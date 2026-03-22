use super::common::{build_internal_name, is_in_comment};
use super::indexing::extract_java_static_imports;
use super::scope::{count_java_locals_in_scope, find_java_enclosing_class_name};
use crate::index::{ClasspathId, ModuleId};
use crate::salsa_db::{FileId, SourceFile};
use crate::salsa_queries::Db;
use crate::salsa_queries::context::{
    CompletionContextData, CursorLocationData, ExpectedTypeSourceData, FunctionalExprShapeData,
    FunctionalMethodCallHintData, FunctionalTargetHintData, MethodRefQualifierKindData,
    StatementLabelData, StatementLabelTargetKindData, line_col_to_offset,
};
use crate::salsa_queries::conversion::{FromSalsaDataWithAnalysis, RequestAnalysisState};
use crate::semantic::{CursorLocation, SemanticContext};
use ropey::Rope;
use std::sync::Arc;
use tower_lsp::lsp_types::Url;

#[salsa::tracked]
pub fn extract_java_completion_context(
    db: &dyn Db,
    file: SourceFile,
    line: u32,
    character: u32,
    trigger_char: Option<char>,
) -> Arc<CompletionContextData> {
    let content = file.content(db);
    let Some(offset) = line_col_to_offset(content, line, character) else {
        return Arc::new(empty_context(db, file));
    };

    if is_in_comment(content, offset) {
        return Arc::new(empty_context(db, file));
    }

    let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file) else {
        return Arc::new(empty_context(db, file));
    };

    let root = tree.root_node();
    let extractor = crate::language::java::JavaContextExtractor::new_with_overview(
        content.to_string(),
        offset,
        None,
    );
    let cursor_node = extractor.find_cursor_node(root);
    let (rich_location, rich_query) =
        crate::language::java::location::determine_location(&extractor, cursor_node, trigger_char);
    let location = convert_rich_location(&rich_location);
    let query = Arc::from(rich_query.as_str());
    let statement_labels = convert_statement_labels(
        crate::language::java::scope::extract_enclosing_statement_labels(&extractor, cursor_node),
    );
    let char_after_cursor = content[offset.min(content.len())..]
        .chars()
        .find(|c| !(c.is_alphanumeric() || *c == '_'));
    let is_class_member_position =
        crate::language::java::scope::is_cursor_in_class_member_position(cursor_node);
    let functional_target_hint = convert_functional_target_hint(
        crate::language::java::location::infer_functional_target_hint(&extractor, cursor_node),
    );

    let package = crate::salsa_queries::parse::extract_package(db, file);
    let imports = crate::salsa_queries::parse::extract_imports(db, file);
    let static_imports = extract_java_static_imports(db, file);
    let enclosing_class = find_java_enclosing_class_name(db, file, offset);
    let enclosing_internal_name = crate::language::java::scope::extract_enclosing_internal_name(
        &extractor,
        cursor_node,
        package.as_ref(),
    )
    .or_else(|| build_internal_name(&package, &enclosing_class));
    let local_var_count = count_java_locals_in_scope(db, file, offset);
    let content_hash = crate::salsa_queries::context::compute_scope_content_hash(db, file, offset);

    Arc::new(CompletionContextData {
        location,
        query,
        cursor_offset: offset,
        enclosing_class,
        enclosing_internal_name,
        enclosing_package: package,
        local_var_count,
        import_count: imports.len(),
        static_import_count: static_imports.len(),
        statement_labels,
        char_after_cursor,
        is_class_member_position,
        functional_target_hint,
        content_hash,
        file_uri: Arc::from(file.file_id(db).as_str()),
        language_id: Arc::from("java"),
    })
}

pub fn extract_java_semantic_context_at_offset(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
    view: crate::index::IndexView,
    workspace: Option<&crate::workspace::Workspace>,
) -> Option<SemanticContext> {
    let content = file.content(db);
    if is_in_comment(content, offset.min(content.len())) {
        return None;
    }

    let rope = Rope::from_str(content);
    let (line, character) = offset_to_line_col_utf16(&rope, offset);
    let context = extract_java_completion_context(db, file, line, character, None);
    let analysis = RequestAnalysisState {
        analysis: workspace
            .map(|workspace| workspace.analysis_context_for_uri(file.file_id(db).uri()))
            .unwrap_or(crate::workspace::AnalysisContext {
                module: ModuleId::ROOT,
                classpath: ClasspathId::Main,
                source_root: None,
                root_kind: None,
            }),
        view,
    };

    Some(build_java_semantic_context(
        db,
        file,
        context.as_ref().clone(),
        workspace,
        &analysis,
    ))
}

pub fn build_java_semantic_context(
    db: &dyn Db,
    file: SourceFile,
    context: CompletionContextData,
    workspace: Option<&crate::workspace::Workspace>,
    analysis: &RequestAnalysisState,
) -> SemanticContext {
    let mut ctx = SemanticContext::from_salsa_data_with_analysis(
        context,
        db,
        file,
        workspace,
        Some(analysis),
    );
    crate::language::java::completion_context::ContextEnricher::new(&analysis.view)
        .enrich(&mut ctx);
    ctx
}

pub fn extract_java_semantic_context_from_source_at_offset(
    source: &str,
    offset: usize,
    view: crate::index::IndexView,
) -> Option<SemanticContext> {
    let db = crate::salsa_db::Database::default();
    let file = SourceFile::new(
        &db,
        FileId::new(
            Url::parse("file:///__java_analyzer__/ephemeral/semantic_context.java")
                .expect("valid static url"),
        ),
        source.to_string(),
        Arc::from("java"),
    );
    let rope = Rope::from_str(source);
    let (line, character) = offset_to_line_col_utf16(&rope, offset);
    let context = extract_java_completion_context(&db, file, line, character, None);
    let analysis = RequestAnalysisState {
        analysis: crate::workspace::AnalysisContext {
            module: ModuleId::ROOT,
            classpath: ClasspathId::Main,
            source_root: None,
            root_kind: None,
        },
        view,
    };

    Some(SemanticContext::from_salsa_data_with_analysis(
        context.as_ref().clone(),
        &db,
        file,
        None,
        Some(&analysis),
    ))
}

fn convert_rich_location(location: &CursorLocation) -> CursorLocationData {
    match location {
        CursorLocation::Expression { prefix } => CursorLocationData::Expression {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::MemberAccess {
            receiver_type,
            member_prefix,
            receiver_expr,
            arguments,
            ..
        } => CursorLocationData::MemberAccess {
            receiver_expr: Arc::from(receiver_expr.as_str()),
            member_prefix: Arc::from(member_prefix.as_str()),
            receiver_type_hint: receiver_type.clone(),
            arguments: arguments.as_ref().map(|s| Arc::from(s.as_str())),
        },
        CursorLocation::StaticAccess {
            class_internal_name,
            member_prefix,
        } => CursorLocationData::StaticAccess {
            class_internal_name: Arc::clone(class_internal_name),
            member_prefix: Arc::from(member_prefix.as_str()),
        },
        CursorLocation::Import { prefix } => CursorLocationData::Import {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::ImportStatic { prefix } => CursorLocationData::ImportStatic {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::MethodArgument { prefix } => CursorLocationData::MethodArgument {
            prefix: Arc::from(prefix.as_str()),
            method_name: None,
            arg_index: None,
        },
        CursorLocation::ConstructorCall {
            class_prefix,
            expected_type,
        } => CursorLocationData::ConstructorCall {
            class_prefix: Arc::from(class_prefix.as_str()),
            expected_type: expected_type.as_deref().map(Arc::from),
        },
        CursorLocation::TypeAnnotation { prefix } => CursorLocationData::TypeAnnotation {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::VariableName { type_name } => CursorLocationData::VariableName {
            type_name: Arc::from(type_name.as_str()),
        },
        CursorLocation::StringLiteral { prefix } => CursorLocationData::StringLiteral {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::MethodReference {
            qualifier_expr,
            member_prefix,
            is_constructor,
        } => CursorLocationData::MethodReference {
            qualifier_expr: Arc::from(qualifier_expr.as_str()),
            member_prefix: Arc::from(member_prefix.as_str()),
            is_constructor: *is_constructor,
        },
        CursorLocation::Annotation {
            prefix,
            target_element_type,
        } => CursorLocationData::Annotation {
            prefix: Arc::from(prefix.as_str()),
            target_element_type: target_element_type.clone(),
        },
        CursorLocation::StatementLabel { kind, prefix } => CursorLocationData::StatementLabel {
            kind: match kind {
                crate::semantic::context::StatementLabelCompletionKind::Break => {
                    crate::salsa_queries::StatementLabelKind::Break
                }
                crate::semantic::context::StatementLabelCompletionKind::Continue => {
                    crate::salsa_queries::StatementLabelKind::Continue
                }
            },
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::Unknown => CursorLocationData::Unknown,
    }
}

fn empty_context(db: &dyn Db, file: SourceFile) -> CompletionContextData {
    CompletionContextData {
        location: CursorLocationData::Unknown,
        query: Arc::from(""),
        cursor_offset: 0,
        enclosing_class: None,
        enclosing_internal_name: None,
        enclosing_package: None,
        local_var_count: 0,
        import_count: 0,
        static_import_count: 0,
        statement_labels: vec![],
        char_after_cursor: None,
        is_class_member_position: false,
        functional_target_hint: None,
        content_hash: 0,
        file_uri: Arc::from(file.file_id(db).as_str()),
        language_id: Arc::from("java"),
    }
}

fn offset_to_line_col_utf16(rope: &Rope, offset: usize) -> (u32, u32) {
    let offset = offset.min(rope.len_bytes());
    let line = rope.byte_to_line(offset);
    let line_start_char = rope.line_to_char(line);
    let target_char = rope.byte_to_char(offset);
    let character = rope
        .slice(line_start_char..target_char)
        .chars()
        .map(|ch| if (ch as u32) >= 0x10000 { 2 } else { 1 })
        .sum::<usize>() as u32;
    (line as u32, character)
}

fn convert_statement_labels(
    labels: Vec<crate::semantic::context::StatementLabel>,
) -> Vec<StatementLabelData> {
    labels
        .into_iter()
        .map(|label| StatementLabelData {
            name: label.name,
            target_kind: convert_statement_label_target_kind(label.target_kind),
        })
        .collect()
}

fn convert_statement_label_target_kind(
    kind: crate::semantic::context::StatementLabelTargetKind,
) -> StatementLabelTargetKindData {
    match kind {
        crate::semantic::context::StatementLabelTargetKind::Block => {
            StatementLabelTargetKindData::Block
        }
        crate::semantic::context::StatementLabelTargetKind::While => {
            StatementLabelTargetKindData::While
        }
        crate::semantic::context::StatementLabelTargetKind::DoWhile => {
            StatementLabelTargetKindData::DoWhile
        }
        crate::semantic::context::StatementLabelTargetKind::For => {
            StatementLabelTargetKindData::For
        }
        crate::semantic::context::StatementLabelTargetKind::EnhancedFor => {
            StatementLabelTargetKindData::EnhancedFor
        }
        crate::semantic::context::StatementLabelTargetKind::Switch => {
            StatementLabelTargetKindData::Switch
        }
        crate::semantic::context::StatementLabelTargetKind::Other => {
            StatementLabelTargetKindData::Other
        }
    }
}

fn convert_functional_target_hint(
    hint: Option<crate::semantic::context::FunctionalTargetHint>,
) -> Option<FunctionalTargetHintData> {
    hint.map(|hint| FunctionalTargetHintData {
        expected_type_source: hint.expected_type_source.map(Arc::from),
        expected_type_context: hint.expected_type_context.map(convert_expected_type_source),
        assignment_lhs_expr: hint.assignment_lhs_expr.map(Arc::from),
        method_call: hint.method_call.map(convert_functional_method_call_hint),
        expr_shape: hint.expr_shape.map(convert_functional_expr_shape),
    })
}

fn convert_expected_type_source(
    source: crate::semantic::context::ExpectedTypeSource,
) -> ExpectedTypeSourceData {
    match source {
        crate::semantic::context::ExpectedTypeSource::VariableInitializer => {
            ExpectedTypeSourceData::VariableInitializer
        }
        crate::semantic::context::ExpectedTypeSource::AssignmentRhs => {
            ExpectedTypeSourceData::AssignmentRhs
        }
        crate::semantic::context::ExpectedTypeSource::ReturnExpr => {
            ExpectedTypeSourceData::ReturnExpr
        }
        crate::semantic::context::ExpectedTypeSource::MethodArgument { arg_index } => {
            ExpectedTypeSourceData::MethodArgument { arg_index }
        }
    }
}

fn convert_functional_method_call_hint(
    hint: crate::semantic::context::FunctionalMethodCallHint,
) -> FunctionalMethodCallHintData {
    FunctionalMethodCallHintData {
        receiver_expr: Arc::from(hint.receiver_expr),
        method_name: Arc::from(hint.method_name),
        arg_index: hint.arg_index,
        arg_texts: hint.arg_texts.into_iter().map(Arc::from).collect(),
    }
}

fn convert_functional_expr_shape(
    shape: crate::semantic::context::FunctionalExprShape,
) -> FunctionalExprShapeData {
    match shape {
        crate::semantic::context::FunctionalExprShape::MethodReference {
            qualifier_expr,
            member_name,
            is_constructor,
            qualifier_kind,
        } => FunctionalExprShapeData::MethodReference {
            qualifier_expr: Arc::from(qualifier_expr),
            member_name: Arc::from(member_name),
            is_constructor,
            qualifier_kind: convert_method_ref_qualifier_kind(qualifier_kind),
        },
        crate::semantic::context::FunctionalExprShape::Lambda {
            param_count,
            expression_body,
        } => FunctionalExprShapeData::Lambda {
            param_count,
            expression_body: expression_body.map(Arc::from),
        },
    }
}

fn convert_method_ref_qualifier_kind(
    kind: crate::semantic::context::MethodRefQualifierKind,
) -> MethodRefQualifierKindData {
    match kind {
        crate::semantic::context::MethodRefQualifierKind::Type => MethodRefQualifierKindData::Type,
        crate::semantic::context::MethodRefQualifierKind::Expr => MethodRefQualifierKindData::Expr,
        crate::semantic::context::MethodRefQualifierKind::Unknown => {
            MethodRefQualifierKindData::Unknown
        }
    }
}
