use std::sync::Arc;
use tower_lsp::jsonrpc::{self, Result as LspResult};
use tower_lsp::lsp_types::*;
use tracing::debug;

use super::super::converters::candidate_to_lsp;
use crate::completion::candidate::{CompletionCandidate, InsertTextMode, ReplacementMode};
use crate::completion::engine::{CompletionEngine, CompletionMetadata, CompletionPolicy};
use crate::language::LanguageRegistry;
use crate::lsp::request_context::{PreparedRequest, RequestContext};
use crate::workspace::Workspace;

pub async fn handle_completion(
    workspace: Arc<Workspace>,
    engine: Arc<CompletionEngine>,
    registry: Arc<LanguageRegistry>,
    params: CompletionParams,
    request: Arc<RequestContext>,
) -> LspResult<Option<CompletionResponse>> {
    let task = tokio::task::spawn_blocking(move || {
        handle_completion_blocking(workspace, engine, registry, params, request)
    });

    match task.await {
        Ok(result) => result.map_err(|cancelled| cancelled.into_lsp_error()),
        Err(error) => {
            tracing::error!(%error, "completion worker panicked");
            Err(jsonrpc::Error::internal_error())
        }
    }
}

fn handle_completion_blocking(
    workspace: Arc<Workspace>,
    engine: Arc<CompletionEngine>,
    registry: Arc<LanguageRegistry>,
    params: CompletionParams,
    request: Arc<RequestContext>,
) -> crate::lsp::request_cancellation::RequestResult<Option<CompletionResponse>> {
    let started = std::time::Instant::now();
    let uri = &params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;
    let trigger = params
        .context
        .as_ref()
        .and_then(|ctx| ctx.trigger_character.as_deref())
        .and_then(|s| s.chars().next());

    let Some(prepared) = PreparedRequest::prepare(
        Arc::clone(&workspace),
        registry.as_ref(),
        uri,
        Arc::clone(&request),
    )?
    else {
        return Ok(None);
    };
    let lang = prepared.lang();
    let analysis = prepared.analysis();
    let scope = prepared.scope();
    let view = prepared.view();
    let log_summary = || {
        prepared.metrics().log_summary(
            analysis.module.0,
            analysis.classpath,
            analysis.source_root.map(|id| id.0),
            started.elapsed().as_secs_f64() * 1000.0,
        );
    };

    let _uri_str = uri.as_str();

    tracing::debug!(
        uri = %uri,
        lang = lang.id(),
        line = position.line,
        character = position.character,
        trigger = ?trigger,
        "completion request"
    );

    let request_analysis_t0 = std::time::Instant::now();

    let index = workspace.index.load();
    let visible_classpath = index.module_classpath_jars(scope.module, analysis.classpath);

    tracing::debug!(
        uri = %uri,
        module = scope.module.0,
        classpath = ?analysis.classpath,
        source_root = ?analysis.source_root.map(|id| id.0),
        root_kind = ?analysis.root_kind,
        visible_classpath_len = visible_classpath.len(),
        view_layers = view.layer_count(),
        analysis_bundle_ms = request_analysis_t0.elapsed().as_secs_f64() * 1000.0,
        "completion using analysis IndexView"
    );

    let Some(ctx) = prepared.semantic_context(position, trigger)? else {
        log_summary();
        return Ok(None);
    };

    let source_for_edits = prepared.source_text().to_owned();

    tracing::debug!(location = ?ctx.location, query = %ctx.query, "parsed context");

    const MAX_ITEMS: usize = 100;
    let completion = engine.complete_prepared_with_policy_and_request(
        scope,
        ctx.clone(),
        lang,
        view,
        CompletionPolicy {
            broad_provider_limit: 256,
            final_result_limit: Some(MAX_ITEMS),
            short_prefix_len: 1,
        },
        Some(&request),
    )?;
    if completion.candidates.is_empty() {
        debug!("no candidates");
        log_summary();
        return Ok(None);
    }

    let CompletionOutputParts {
        candidates,
        metadata,
    } = CompletionOutputParts::from(completion);
    request.check_cancelled("completion.before_lang_post_process")?;
    let candidates = lang.post_process_candidates(candidates, &ctx);
    request.check_cancelled("completion.before_lsp_conversion")?;
    let completion_list = build_completion_list(
        metadata,
        &candidates,
        &source_for_edits,
        position,
        MAX_ITEMS,
    );

    tracing::debug!(
        count = completion_list.items.len(),
        incomplete = completion_list.is_incomplete,
        broad_query = metadata.broad_query,
        broad_provider = metadata.used_broad_provider,
        provider_truncated = metadata.provider_truncated,
        final_truncated = metadata.final_truncated,
        "returning completions"
    );
    log_summary();

    Ok(Some(CompletionResponse::List(completion_list)))
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
    use crate::completion::engine::{CompletionEngine, CompletionMetadata};
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, IndexedArchiveData, IndexedJavaModule,
        MethodParams, MethodSummary, ModuleId,
    };
    use crate::language::LanguageRegistry;
    use crate::language::java::module_info::JavaModuleDescriptor;
    use crate::lsp::request_cancellation::{CancellationToken, RequestFamily};
    use crate::lsp::request_context::RequestContext;
    use crate::semantic::types::parse_return_type_from_descriptor;
    use crate::workspace::document::Document;
    use crate::workspace::{SourceFile, Workspace};
    use rust_asm::constants::{ACC_PUBLIC, ACC_STATIC};
    use std::sync::Arc;
    use tower_lsp::lsp_types::{
        CompletionContext, CompletionTriggerKind, PartialResultParams, TextDocumentIdentifier,
        TextDocumentPositionParams, Url, WorkDoneProgressParams,
    };

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

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn strip_cursor_marker(marked_source: &str) -> (String, Position) {
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = ropey::Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        (source, Position::new(line, character))
    }

    fn make_class(package: &str, name: &str, origin: ClassOrigin) -> ClassMetadata {
        let internal_name = if package.is_empty() {
            name.to_string()
        } else {
            format!("{package}/{name}")
        };
        ClassMetadata {
            package: (!package.is_empty()).then(|| Arc::from(package)),
            name: Arc::from(name),
            internal_name: Arc::from(internal_name),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: None,
            origin,
        }
    }

    fn make_bytecode_module(name: &str) -> IndexedJavaModule {
        IndexedJavaModule {
            descriptor: Arc::new(JavaModuleDescriptor {
                name: Arc::from(name),
                is_open: false,
                requires: vec![],
                exports: vec![],
                opens: vec![],
                uses: vec![],
                provides: vec![],
            }),
            origin: ClassOrigin::Jar(Arc::from("/tmp/modules.jar")),
        }
    }

    fn make_method(name: &str, descriptor: &str, flags: u16) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: flags,
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn open_java_document(workspace: &Workspace, uri: &Url, source: &str) {
        workspace.documents.open(Document::new(SourceFile::new(
            uri.clone(),
            "java",
            1,
            source.to_string(),
            None,
        )));
        let salsa_file = workspace
            .get_or_update_salsa_file(uri)
            .expect("salsa file for document");
        let db = workspace.salsa_db.lock();
        workspace.refresh_java_module_descriptor_for_salsa_file(&db, salsa_file);
    }

    fn completion_labels_from_marked_source(
        workspace: Arc<Workspace>,
        uri: Url,
        marked_source: &str,
    ) -> Vec<String> {
        let (source, position) = strip_cursor_marker(marked_source);
        open_java_document(workspace.as_ref(), &uri, &source);

        let response = handle_completion_blocking(
            Arc::clone(&workspace),
            Arc::new(CompletionEngine::new()),
            Arc::new(LanguageRegistry::new()),
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: Some(CompletionContext {
                    trigger_kind: CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                }),
            },
            RequestContext::new(
                "test_completion",
                &uri,
                RequestFamily::Completion,
                1,
                CancellationToken::new(),
            ),
        )
        .expect("completion result")
        .expect("completion response");

        let mut labels: Vec<String> = match response {
            CompletionResponse::Array(items) => items.into_iter().map(|item| item.label).collect(),
            CompletionResponse::List(list) => {
                list.items.into_iter().map(|item| item.label).collect()
            }
        };
        labels.sort();
        labels
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
    fn test_map_candidate_item_annotation_element_uses_property_kind_and_assignment_edit() {
        let c = CompletionCandidate::new(
            Arc::from("name"),
            "name = ",
            CandidateKind::AnnotationElement,
            "annotation_param",
        )
        .with_filter_text("name");
        let src = "@ConfigAnno(value = \"x\", )";
        let pos = Position {
            line: 0,
            character: "@ConfigAnno(value = \"x\", ".len() as u32,
        };

        let item = map_candidate_item(&c, src, pos);
        let edit = item.text_edit.expect("text_edit expected");
        let range = edit_range(&edit);

        assert_eq!(item.kind, Some(CompletionItemKind::PROPERTY));
        assert_eq!(item.filter_text.as_deref(), Some("name"));
        assert_eq!(range.start.character, pos.character);
        assert_eq!(range.end.character, pos.character);
        assert_eq!(edit_text(&edit), "name = ");
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

    #[test]
    fn test_module_info_completion_suggests_directive_keywords() {
        let workspace = Arc::new(Workspace::new());
        let uri = Url::parse("file:///workspace/module-info.java").expect("uri");

        let labels =
            completion_labels_from_marked_source(workspace, uri, "module com.example.app { req| }");

        assert!(labels.iter().any(|label| label == "requires"), "{labels:?}");
        assert!(
            !labels.iter().any(|label| label == "return"),
            "module body keyword completion should stay JPMS-specific: {labels:?}"
        );
    }

    #[test]
    fn test_module_info_requires_completion_uses_workspace_module_registry() {
        let workspace = Arc::new(Workspace::new());
        let shared_uri = Url::parse("file:///workspace/shared/module-info.java").expect("uri");
        open_java_document(
            workspace.as_ref(),
            &shared_uri,
            "module com.example.shared { }",
        );

        let app_uri = Url::parse("file:///workspace/app/module-info.java").expect("uri");
        let labels = completion_labels_from_marked_source(
            workspace,
            app_uri,
            "module com.example.app { requires com.example.|; }",
        );

        assert!(
            labels.iter().any(|label| label == "com.example.shared"),
            "{labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label == "com.example.app"),
            "current module should not be suggested in requires completion: {labels:?}"
        );
    }

    #[test]
    fn test_module_info_target_module_completion_includes_bytecode_modules() {
        let workspace = Arc::new(Workspace::new());
        workspace.index.update(|index| {
            index.add_jdk_archive(IndexedArchiveData {
                classes: vec![],
                modules: vec![make_bytecode_module("com.example.runtime")],
            });
        });

        let app_uri = Url::parse("file:///workspace/app/module-info.java").expect("uri");
        let labels = completion_labels_from_marked_source(
            workspace,
            app_uri,
            "module com.example.app { exports com.example.api to com.example.run|time; }",
        );

        assert!(
            labels.iter().any(|label| label == "com.example.runtime"),
            "{labels:?}"
        );
    }

    #[test]
    fn test_module_info_exports_completion_uses_current_module_source_packages() {
        let workspace = Arc::new(Workspace::new());
        let api_uri = Url::parse("file:///workspace/src/com/example/api/Api.java").expect("uri");
        let internal_uri =
            Url::parse("file:///workspace/src/com/example/internal/Internal.java").expect("uri");
        workspace.index.update(|index| {
            let api_origin = ClassOrigin::SourceFile(Arc::from(api_uri.as_str()));
            index.update_source(
                root_scope(),
                api_origin.clone(),
                vec![make_class("com/example/api", "Api", api_origin)],
            );
            let internal_origin = ClassOrigin::SourceFile(Arc::from(internal_uri.as_str()));
            index.update_source(
                root_scope(),
                internal_origin.clone(),
                vec![make_class(
                    "com/example/internal",
                    "Internal",
                    internal_origin,
                )],
            );
        });

        let module_uri = Url::parse("file:///workspace/module-info.java").expect("uri");
        let labels = completion_labels_from_marked_source(
            workspace,
            module_uri,
            "module com.example.app { exports com.example.|; }",
        );

        assert!(
            labels.iter().any(|label| label == "com.example.api"),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|label| label == "com.example.internal"),
            "{labels:?}"
        );
    }

    #[test]
    fn test_module_info_uses_completion_routes_to_type_candidates() {
        let workspace = Arc::new(Workspace::new());
        workspace.index.update(|index| {
            index.add_classes(vec![make_class(
                "com/example/spi",
                "Service",
                ClassOrigin::Unknown,
            )]);
        });

        let module_uri = Url::parse("file:///workspace/module-info.java").expect("uri");
        let labels = completion_labels_from_marked_source(
            workspace,
            module_uri,
            "module com.example.app { uses com.example.spi.Se|; }",
        );

        assert!(
            labels
                .iter()
                .any(|label| label == "com.example.spi.Service"),
            "{labels:?}"
        );
    }

    #[test]
    fn test_static_method_completion_survives_existing_call_parentheses() {
        let workspace = Arc::new(Workspace::new());
        workspace.index.update(|index| {
            index.add_classes(vec![ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Thread"),
                internal_name: Arc::from("java/lang/Thread"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method(
                    "currentThread",
                    "()Ljava/lang/Thread;",
                    ACC_PUBLIC | ACC_STATIC,
                )],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            }]);
        });

        let uri = Url::parse("file:///workspace/Test.java").expect("uri");
        let labels = completion_labels_from_marked_source(
            workspace,
            uri,
            "class Test { void m() { Thread.currentThread|(); } }",
        );

        assert!(
            labels
                .iter()
                .any(|label| label.starts_with("currentThread")),
            "{labels:?}"
        );
    }
}
