use std::sync::Arc;
use tower_lsp::lsp_types::*;
use tracing::debug;

use super::super::converters::candidate_to_lsp;
use crate::completion::candidate::{CompletionCandidate, InsertTextMode, ReplacementMode};
use crate::completion::engine::{CompletionEngine, CompletionMetadata, CompletionPolicy};
use crate::language::LanguageRegistry;
use crate::salsa_queries::conversion::{FromSalsaDataWithAnalysis, RequestAnalysisState};
use crate::workspace::Workspace;

pub async fn handle_completion(
    workspace: Arc<Workspace>,
    engine: Arc<CompletionEngine>,
    registry: Arc<LanguageRegistry>,
    params: CompletionParams,
) -> Option<CompletionResponse> {
    let uri = &params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;
    let trigger = params
        .context
        .as_ref()
        .and_then(|ctx| ctx.trigger_character.as_deref())
        .and_then(|s| s.chars().next());

    let lang_id = workspace
        .documents
        .with_doc(uri, |doc| doc.language_id().to_owned())?;

    let lang = registry.find(&lang_id)?;

    let _uri_str = uri.as_str();

    tracing::debug!(
        uri = %uri,
        lang = lang.id(),
        line = position.line,
        character = position.character,
        trigger = ?trigger,
        "completion request"
    );

    workspace.documents.with_doc_mut(uri, |doc| {
        if doc.source().tree.is_none() {
            let tree = lang.parse_tree(doc.source().text(), None);
            doc.set_tree(tree);
        }
    })?;

    let analysis = workspace.analysis_context_for_uri(uri);
    let inferred_package = workspace.infer_java_package_for_uri(uri, analysis.source_root);
    let scope = analysis.scope();

    let request_analysis_t0 = std::time::Instant::now();

    // Use cached IndexView and NameTable via Salsa for better performance
    let (view, name_table) = {
        let db = workspace.salsa_db.lock();

        // Get cached IndexView (memoized)
        let view = crate::salsa_queries::get_index_view_for_context(
            &*db,
            scope.module,
            analysis.classpath,
            analysis.source_root,
        );

        // Get cached NameTable (memoized)
        let name_table = crate::salsa_queries::get_name_table_for_context(
            &*db,
            scope.module,
            analysis.classpath,
            analysis.source_root,
        );

        (view, name_table)
    };

    let request_analysis = RequestAnalysisState {
        analysis,
        name_table: Arc::clone(&name_table),
    };

    let index = workspace.index.read();
    let visible_classpath = index.module_classpath_jars(scope.module, analysis.classpath);

    tracing::debug!(
        uri = %uri,
        module = scope.module.0,
        classpath = ?analysis.classpath,
        source_root = ?analysis.source_root.map(|id| id.0),
        root_kind = ?analysis.root_kind,
        visible_classpath_len = visible_classpath.len(),
        name_table_len = name_table.len(),
        view_layers = view.layer_count(),
        analysis_bundle_ms = request_analysis_t0.elapsed().as_secs_f64() * 1000.0,
        "completion using cached IndexView and NameTable"
    );

    let ctx: crate::semantic::SemanticContext = {
        let salsa_file = workspace.get_or_update_salsa_file(uri)?;

        let context_data = {
            let db = workspace.salsa_db.lock();
            if lang.id() == "java" {
                crate::salsa_queries::java::extract_java_completion_context_with_name_table(
                    &*db,
                    salsa_file,
                    position.line,
                    position.character,
                    trigger,
                    Some(Arc::clone(&request_analysis.name_table)),
                )
            } else {
                lang.extract_completion_context_salsa(
                    &*db,
                    salsa_file,
                    position.line,
                    position.character,
                    trigger,
                )?
            }
        };

        let db = workspace.salsa_db.lock();
        crate::semantic::SemanticContext::from_salsa_data_with_analysis(
            context_data.as_ref().clone(),
            &*db,
            salsa_file,
            Some(&*workspace),
            Some(&request_analysis),
        )
    };

    let source_for_edits = workspace
        .documents
        .with_doc(uri, |doc| doc.source().text().to_owned())?;

    let mut ctx = if let Some(pkg) = inferred_package {
        ctx.with_inferred_package(pkg)
    } else {
        ctx
    };

    // Enrich context with type information (critical for member completion)
    lang.enrich_completion_context(&mut ctx, scope, &view);

    tracing::debug!(location = ?ctx.location, query = %ctx.query, "parsed context");

    const MAX_ITEMS: usize = 100;
    let completion = engine.complete_with_policy(
        scope,
        ctx.clone(),
        lang,
        &view,
        CompletionPolicy {
            broad_provider_limit: 256,
            final_result_limit: Some(MAX_ITEMS),
            short_prefix_len: 1,
        },
    );
    if completion.candidates.is_empty() {
        debug!("no candidates");
        return None;
    }

    let CompletionOutputParts {
        candidates,
        metadata,
    } = CompletionOutputParts::from(completion);
    let candidates = lang.post_process_candidates(candidates, &ctx);
    let completion_list = build_completion_list(
        metadata,
        &candidates,
        &source_for_edits,
        position,
        MAX_ITEMS,
    );

    debug!(
        count = completion_list.items.len(),
        incomplete = completion_list.is_incomplete,
        broad_query = metadata.broad_query,
        broad_provider = metadata.used_broad_provider,
        provider_truncated = metadata.provider_truncated,
        final_truncated = metadata.final_truncated,
        "returning completions"
    );

    Some(CompletionResponse::List(completion_list))
}

fn build_completion_list(
    metadata: CompletionMetadata,
    candidates: &[CompletionCandidate],
    source_for_edits: &str,
    position: Position,
    max_items: usize,
) -> CompletionList {
    let items: Vec<CompletionItem> = candidates
        .iter()
        .take(max_items)
        .map(|c| map_candidate_item(c, source_for_edits, position))
        .collect();

    let is_incomplete = metadata.is_incomplete() || candidates.len() > max_items;
    CompletionList {
        is_incomplete,
        items,
    }
}

struct CompletionOutputParts {
    candidates: Vec<CompletionCandidate>,
    metadata: CompletionMetadata,
}

impl From<crate::completion::engine::CompletionOutput> for CompletionOutputParts {
    fn from(value: crate::completion::engine::CompletionOutput) -> Self {
        Self {
            candidates: value.candidates,
            metadata: value.metadata,
        }
    }
}

fn map_candidate_item(
    c: &CompletionCandidate,
    source_for_edits: &str,
    position: Position,
) -> CompletionItem {
    let mut item = candidate_to_lsp(c, source_for_edits);

    if let Some(edit) = make_text_edit(c, source_for_edits, position) {
        item.text_edit = Some(edit);
        // Keep snippet semantics when new_text contains snippet placeholders.
        item.insert_text_format = match c.insertion.mode {
            InsertTextMode::Snippet => Some(InsertTextFormat::SNIPPET),
            InsertTextMode::PlainText => None,
        };
        item.insert_text = None;
    }

    if let Some(rewrite) = c.insertion.member_access_rewrite.as_ref()
        && let Some(mut rewrite_edits) = make_member_access_cast_additional_edits(
            source_for_edits,
            position,
            &rewrite.receiver_expr,
            &rewrite.cast_type,
        )
    {
        let mut merged = item.additional_text_edits.take().unwrap_or_default();
        merged.append(&mut rewrite_edits);
        item.additional_text_edits = Some(merged);
    }

    if let Some(filter) = c
        .insertion
        .filter_text
        .clone()
        .or_else(|| default_filter_text(c))
    {
        item.filter_text = Some(filter);
    }
    if c.insertion.member_access_rewrite.is_some() && item.filter_text.is_none() {
        item.filter_text = Some(item.label.clone());
    }

    tracing::debug!(
        label = %item.label,
        filter_text = ?item.filter_text,
        insert_text = ?item.insert_text,
        text_edit = ?item.text_edit,
        sort_text = ?item.sort_text,
        kind = ?item.kind,
        additional_text_edits = ?item.additional_text_edits,
        source = c.source,
        "completion item emitted"
    );

    item
}

fn make_text_edit(
    candidate: &CompletionCandidate,
    source: &str,
    position: Position,
) -> Option<CompletionTextEdit> {
    let insertion_text = candidate.insertion.text.as_str();
    match &candidate.insertion.replacement {
        ReplacementMode::Identifier => make_identifier_text_edit(insertion_text, source, position),
        ReplacementMode::MemberSegment => {
            make_member_segment_text_edit(insertion_text, source, position)
        }
        ReplacementMode::PackagePath => {
            make_package_path_text_edit(insertion_text, source, position)
        }
        ReplacementMode::ImportPath => crate::completion::import_completion::make_import_text_edit(
            insertion_text,
            source,
            position,
        ),
        ReplacementMode::AccessModifierPrefix => {
            make_access_modifier_text_edit(insertion_text, source, position)
        }
        ReplacementMode::ClientDefault => None,
    }
}

/// Replace access-modifier prefix before cursor with override stub.
fn make_access_modifier_text_edit(
    insert_text: &str,
    source: &str,
    position: Position,
) -> Option<CompletionTextEdit> {
    let line = source.lines().nth(position.line as usize)?;
    let before_cursor = &line[..position.character as usize];

    let start_char = before_cursor
        .rfind(|c: char| !c.is_alphabetic())
        .map(|p| p + 1)
        .unwrap_or(0) as u32;

    Some(CompletionTextEdit::Edit(TextEdit {
        range: Range {
            start: Position {
                line: position.line,
                character: start_char,
            },
            end: Position {
                line: position.line,
                character: position.character,
            },
        },
        new_text: insert_text.to_string(),
    }))
}

/// Replace package-like path up to cursor (letters, numbers, underscore and dots).
fn make_package_path_text_edit(
    insert_text: &str,
    source: &str,
    position: Position,
) -> Option<CompletionTextEdit> {
    let line = source.lines().nth(position.line as usize)?;
    let before_cursor = &line[..position.character as usize];
    let start_char = before_cursor
        .rfind(|c: char| !c.is_alphanumeric() && c != '.' && c != '_')
        .map(|p| p + 1)
        .unwrap_or(0) as u32;

    Some(CompletionTextEdit::Edit(TextEdit {
        range: Range {
            start: Position {
                line: position.line,
                character: start_char,
            },
            end: Position {
                line: position.line,
                character: position.character,
            },
        },
        new_text: insert_text.to_string(),
    }))
}

fn make_identifier_text_edit(
    insert_text: &str,
    source: &str,
    position: Position,
) -> Option<CompletionTextEdit> {
    make_member_segment_text_edit(insert_text, source, position)
}

/// Replace only member segment after dot (alphanumeric + underscore).
fn make_member_segment_text_edit(
    insert_text: &str,
    source: &str,
    position: Position,
) -> Option<CompletionTextEdit> {
    let line = source.lines().nth(position.line as usize)?;
    let before_cursor = &line[..position.character as usize];
    let start_char = before_cursor
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|p| p + 1)
        .unwrap_or(0) as u32;

    Some(CompletionTextEdit::Edit(TextEdit {
        range: Range {
            start: Position {
                line: position.line,
                character: start_char,
            },
            end: Position {
                line: position.line,
                character: position.character,
            },
        },
        new_text: insert_text.to_string(),
    }))
}

fn make_member_access_cast_additional_edits(
    source: &str,
    position: Position,
    receiver_expr: &str,
    cast_type: &str,
) -> Option<Vec<TextEdit>> {
    let line = source.lines().nth(position.line as usize)?;
    let before_cursor = &line[..position.character as usize];
    let needle = format!("{}.", receiver_expr.trim());
    let receiver_start = before_cursor.rfind(&needle)? as u32;
    let receiver_end = receiver_start + receiver_expr.trim().len() as u32;

    Some(vec![
        TextEdit {
            range: Range {
                start: Position {
                    line: position.line,
                    character: receiver_start,
                },
                end: Position {
                    line: position.line,
                    character: receiver_start,
                },
            },
            new_text: format!("(({}) ", cast_type),
        },
        TextEdit {
            range: Range {
                start: Position {
                    line: position.line,
                    character: receiver_end,
                },
                end: Position {
                    line: position.line,
                    character: receiver_end,
                },
            },
            new_text: ")".to_string(),
        },
    ])
}

fn default_filter_text(c: &CompletionCandidate) -> Option<String> {
    match c.insertion.replacement {
        ReplacementMode::ImportPath => Some(c.insertion.text.clone()),
        ReplacementMode::MemberSegment
        | ReplacementMode::PackagePath
        | ReplacementMode::AccessModifierPrefix => Some(c.label.to_string()),
        ReplacementMode::Identifier | ReplacementMode::ClientDefault => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::candidate::CandidateKind;
    use crate::completion::engine::CompletionMetadata;
    use std::sync::Arc;

    fn edit_range(edit: &CompletionTextEdit) -> Range {
        match edit {
            CompletionTextEdit::Edit(te) => te.range,
            CompletionTextEdit::InsertAndReplace(te) => te.insert,
        }
    }

    fn edit_text(edit: &CompletionTextEdit) -> &str {
        match edit {
            CompletionTextEdit::Edit(te) => te.new_text.as_str(),
            CompletionTextEdit::InsertAndReplace(te) => te.new_text.as_str(),
        }
    }

    #[test]
    fn test_member_access_text_edit_empty_prefix_is_zero_width() {
        let src = "ChainCheck.;";
        let pos = Position {
            line: 0,
            character: "ChainCheck.".len() as u32,
        };
        let edit = make_member_segment_text_edit("Box", src, pos).expect("text edit");
        let range = edit_range(&edit);
        assert_eq!(range.start.character, pos.character);
        assert_eq!(range.end.character, pos.character);
        assert_eq!(edit_text(&edit), "Box");
    }

    #[test]
    fn test_member_access_text_edit_replaces_only_member_segment() {
        let src = "ChainCheck.Bo;";
        let pos = Position {
            line: 0,
            character: "ChainCheck.Bo".len() as u32,
        };
        let edit = make_member_segment_text_edit("Box", src, pos).expect("text edit");
        let range = edit_range(&edit);
        assert_eq!(range.start.character, "ChainCheck.".len() as u32);
        assert_eq!(range.end.character, pos.character);
        assert_eq!(edit_text(&edit), "Box");
    }

    #[test]
    fn test_map_candidate_item_static_access_class_uses_member_edit() {
        let c = CompletionCandidate::new(
            Arc::from("Box"),
            "Box",
            CandidateKind::ClassName,
            "expression",
        )
        .with_replacement_mode(ReplacementMode::MemberSegment);
        let src = "ChainCheck.;";
        let pos = Position {
            line: 0,
            character: "ChainCheck.".len() as u32,
        };
        let item = map_candidate_item(&c, src, pos);
        let edit = item.text_edit.expect("text_edit expected");
        let range = edit_range(&edit);
        assert_eq!(item.label, "Box");
        assert_eq!(item.filter_text.as_deref(), Some("Box"));
        assert_eq!(range.start.character, pos.character);
        assert_eq!(range.end.character, pos.character);
        assert_eq!(edit_text(&edit), "Box");
    }

    #[test]
    fn test_map_candidate_item_snippet_text_edit_keeps_snippet_format() {
        let c = CompletionCandidate::new(
            Arc::from("println"),
            "println(${1:x})$0",
            CandidateKind::Method {
                descriptor: Arc::from("(Ljava/lang/String;)V"),
                defining_class: Arc::from("java/io/PrintStream"),
            },
            "member",
        )
        .with_insert_mode(InsertTextMode::Snippet);
        let src = "System.out.pri";
        let pos = Position {
            line: 0,
            character: "System.out.pri".len() as u32,
        };
        let item = map_candidate_item(&c, src, pos);
        let edit = item.text_edit.expect("text_edit expected");
        assert_eq!(edit_text(&edit), "println(${1:x})$0");
        assert_eq!(item.insert_text, None);
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn test_map_candidate_item_plain_text_edit_has_no_snippet_format() {
        let c = CompletionCandidate::new(
            Arc::from("intValue"),
            "intValue",
            CandidateKind::LocalVariable {
                type_descriptor: Arc::from("I"),
            },
            "local_var",
        );
        let src = "intVa";
        let pos = Position {
            line: 0,
            character: "intVa".len() as u32,
        };
        let item = map_candidate_item(&c, src, pos);
        assert_eq!(item.insert_text_format, None);
    }

    #[test]
    fn test_member_access_cast_rewrite_uses_narrow_primary_edit_and_additional_edits() {
        let c = CompletionCandidate::new(
            Arc::from("append"),
            "append(${1:str})$0",
            CandidateKind::Method {
                descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/StringBuilder;"),
                defining_class: Arc::from("java/lang/StringBuilder"),
            },
            "member",
        )
        .with_insert_mode(InsertTextMode::Snippet)
        .with_member_access_cast_rewrite("sb", "java.lang.StringBuilder");

        let src = "if (sb instanceof StringBuilder) { sb.appe }";
        let pos = Position {
            line: 0,
            character: "if (sb instanceof StringBuilder) { sb.appe".len() as u32,
        };
        let item = map_candidate_item(&c, src, pos);
        match item.text_edit.expect("text_edit expected") {
            CompletionTextEdit::Edit(te) => {
                assert_eq!(
                    te.range.start.character,
                    "if (sb instanceof StringBuilder) { sb.".len() as u32
                );
                assert_eq!(te.range.end.character, pos.character);
                assert_eq!(te.new_text, "append(${1:str})$0");
            }
            CompletionTextEdit::InsertAndReplace(ir) => {
                panic!("expected narrow Edit, got InsertAndReplace({ir:?})");
            }
        }
        let edits = item
            .additional_text_edits
            .expect("cast rewrite additional edits expected");
        assert_eq!(edits.len(), 2);
        assert_eq!(
            edits[0].new_text, "((java.lang.StringBuilder) ",
            "first edit inserts cast prefix"
        );
        assert_eq!(edits[1].new_text, ")", "second edit inserts cast suffix");
        assert!(edits[0].range.end <= edits[1].range.start);
    }

    #[test]
    fn test_append_vs_clone_item_shape_for_vscode_filtering() {
        let append = CompletionCandidate::new(
            Arc::from("append"),
            "append(${1:str})$0",
            CandidateKind::Method {
                descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/StringBuilder;"),
                defining_class: Arc::from("java/lang/StringBuilder"),
            },
            "member",
        )
        .with_insert_mode(InsertTextMode::Snippet)
        .with_member_access_cast_rewrite("sb", "java.lang.StringBuilder");

        let clone = CompletionCandidate::new(
            Arc::from("clone"),
            "clone()",
            CandidateKind::Method {
                descriptor: Arc::from("()Ljava/lang/Object;"),
                defining_class: Arc::from("java/lang/Object"),
            },
            "member",
        );

        let src = "if (sb instanceof StringBuilder) { sb.appe }";
        let pos = Position {
            line: 0,
            character: "if (sb instanceof StringBuilder) { sb.appe".len() as u32,
        };

        let append_item = map_candidate_item(&append, src, pos);
        let clone_item = map_candidate_item(&clone, src, pos);

        assert_eq!(append_item.label, "append");
        assert_eq!(clone_item.label, "clone");
        assert_eq!(append_item.filter_text.as_deref(), Some("append"));
        assert_eq!(clone_item.filter_text, None);
        assert_eq!(append_item.sort_text, clone_item.sort_text);
        assert_eq!(append_item.insert_text, None);
        assert_eq!(clone_item.insert_text, None);
        assert_eq!(
            append_item.insert_text_format,
            Some(InsertTextFormat::SNIPPET)
        );
        assert_eq!(clone_item.insert_text_format, None);

        match append_item.text_edit.expect("append text_edit") {
            CompletionTextEdit::Edit(te) => {
                assert_eq!(
                    te.range.start.character,
                    "if (sb instanceof StringBuilder) { sb.".len() as u32
                );
                assert_eq!(te.range.end.character, pos.character);
                assert_eq!(te.new_text, "append(${1:str})$0");
            }
            CompletionTextEdit::InsertAndReplace(ir) => {
                panic!("append should not use InsertAndReplace({ir:?})");
            }
        }
        let append_additional = append_item
            .additional_text_edits
            .as_ref()
            .expect("append should include cast additional edits");
        assert_eq!(append_additional.len(), 2);
        assert_eq!(append_additional[0].new_text, "((java.lang.StringBuilder) ");
        assert_eq!(append_additional[1].new_text, ")");

        match clone_item.text_edit.expect("clone text_edit") {
            CompletionTextEdit::Edit(te) => {
                assert_eq!(
                    te.range.start.character,
                    "if (sb instanceof StringBuilder) { sb.".len() as u32
                );
                assert_eq!(te.range.end.character, pos.character);
                assert_eq!(te.new_text, "clone()");
            }
            CompletionTextEdit::InsertAndReplace(ir) => {
                panic!("expected clone Edit text_edit, got InsertAndReplace({ir:?})");
            }
        }
        assert!(
            clone_item
                .additional_text_edits
                .as_ref()
                .is_none_or(|edits| edits.is_empty()),
            "clone should not have cast rewrite additional edits"
        );
    }

    #[test]
    fn test_completion_item_does_not_emit_invalid_insert_replace_shape() {
        let c = CompletionCandidate::new(
            Arc::from("append"),
            "append(${1:str})$0",
            CandidateKind::Method {
                descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/StringBuilder;"),
                defining_class: Arc::from("java/lang/StringBuilder"),
            },
            "member",
        )
        .with_insert_mode(InsertTextMode::Snippet)
        .with_member_access_cast_rewrite("sb", "java.lang.StringBuilder");
        let src = "if (sb instanceof StringBuilder) { sb.appe }";
        let pos = Position {
            line: 0,
            character: "if (sb instanceof StringBuilder) { sb.appe".len() as u32,
        };
        let item = map_candidate_item(&c, src, pos);
        if let Some(edit) = item.text_edit
            && let CompletionTextEdit::InsertAndReplace(ir) = edit
        {
            assert_eq!(ir.insert.start, ir.replace.start);
        }
    }

    fn mk_candidate(label: &str) -> CompletionCandidate {
        CompletionCandidate::new(
            Arc::from(label),
            label.to_string(),
            CandidateKind::ClassName,
            "t",
        )
    }

    #[test]
    fn test_build_completion_list_small_result_is_complete() {
        let candidates = vec![mk_candidate("Alpha"), mk_candidate("Beta")];
        let list = build_completion_list(
            CompletionMetadata::default(),
            &candidates,
            "A",
            Position::new(0, 1),
            10,
        );
        assert!(!list.is_incomplete);
        assert_eq!(list.items.len(), 2);
    }

    #[test]
    fn test_build_completion_list_truncated_result_is_incomplete() {
        let candidates: Vec<_> = (0..5).map(|i| mk_candidate(&format!("Item{i}"))).collect();
        let list = build_completion_list(
            CompletionMetadata {
                final_truncated: true,
                ..CompletionMetadata::default()
            },
            &candidates,
            "",
            Position::new(0, 0),
            3,
        );
        assert!(list.is_incomplete);
        assert_eq!(list.items.len(), 3);
    }

    #[test]
    fn test_build_completion_list_threshold_behavior() {
        let candidates = vec![mk_candidate("A"), mk_candidate("B"), mk_candidate("C")];
        let list = build_completion_list(
            CompletionMetadata::default(),
            &candidates,
            "A",
            Position::new(0, 1),
            2,
        );
        assert!(list.is_incomplete);
        assert_eq!(list.items.len(), 2);
    }

    #[test]
    fn test_build_completion_list_provider_partial_marks_incomplete() {
        let candidates = vec![mk_candidate("A"), mk_candidate("B")];
        let list = build_completion_list(
            CompletionMetadata {
                provider_truncated: true,
                ..CompletionMetadata::default()
            },
            &candidates,
            "",
            Position::new(0, 0),
            10,
        );
        assert!(list.is_incomplete);
        assert_eq!(list.items.len(), 2);
    }
}
