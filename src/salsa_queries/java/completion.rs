use super::common::{build_internal_name, is_in_comment};
use super::indexing::extract_java_static_imports;
use super::scope::{count_java_locals_in_scope, find_java_enclosing_class_name};
use crate::index::{ClasspathId, ModuleId};
use crate::salsa_db::{FileId, SourceFile};
use crate::salsa_queries::Db;
use crate::salsa_queries::context::{
    AccessReceiverKindData, CompletionContextData, CursorLocationData, ExpectedTypeSourceData,
    FunctionalExprShapeData, FunctionalMethodCallHintData, FunctionalTargetHintData,
    JavaModuleContextKindData, MethodRefQualifierKindData, StatementLabelData,
    StatementLabelTargetKindData, line_col_to_offset,
};
use crate::salsa_queries::conversion::{FromSalsaDataWithAnalysis, RequestAnalysisState};
use crate::semantic::{CursorLocation, SemanticContext};
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

    extract_java_completion_context_at_offset(db, file, offset, trigger_char)
}

#[salsa::tracked]
pub fn extract_java_completion_context_at_offset(
    db: &dyn Db,
    file: SourceFile,
    cursor_offset: usize,
    trigger_char: Option<char>,
) -> Arc<CompletionContextData> {
    let content: Arc<str> = Arc::from(file.content(db).as_str());
    let offset = cursor_offset.min(content.len());

    if is_in_comment(content.as_ref(), offset) {
        return Arc::new(empty_context(db, file));
    }

    let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file) else {
        return Arc::new(empty_context(db, file));
    };

    let root = tree.root_node();
    let extractor = crate::language::java::JavaContextExtractor::new_with_overview(
        Arc::clone(&content),
        offset,
        None,
    );
    let cursor_node = extractor.find_cursor_node(root);
    let module_match = crate::language::java::module_info::infer_module_completion_context(
        &extractor,
        root,
        cursor_node,
    );
    let (rich_location, rich_query, java_module_context) = if let Some(module_match) = module_match
    {
        (
            module_match.location,
            module_match.query,
            Some(convert_java_module_context(module_match.context)),
        )
    } else {
        let (location, query) = crate::language::java::location::determine_location(
            &extractor,
            cursor_node,
            trigger_char,
        );
        (location, query, None)
    };
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
    let enclosing_class_chain =
        crate::language::java::scope::extract_enclosing_class_chain(&extractor, cursor_node)
            .or_else(|| {
                crate::language::java::scope::extract_enclosing_class_chain_by_offset(
                    &extractor, root,
                )
            })
            .unwrap_or_default();
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
        java_module_context,
        query,
        cursor_offset: offset,
        enclosing_class,
        enclosing_internal_name,
        enclosing_class_chain,
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
        workspace_version: workspace
            .map(|workspace| workspace.index.load().version())
            .unwrap_or_else(|| db.workspace_index().version()),
    };

    extract_java_semantic_context_at_offset_with_analysis(db, file, offset, &analysis, workspace)
}

pub fn extract_java_semantic_context_at_offset_with_analysis(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
    analysis: &RequestAnalysisState,
    workspace: Option<&crate::workspace::Workspace>,
) -> Option<SemanticContext> {
    let content = file.content(db);
    if is_in_comment(content, offset.min(content.len())) {
        return None;
    }

    let context = extract_java_completion_context_at_offset(db, file, offset, None);

    Some(build_java_semantic_context(
        db,
        file,
        context.as_ref().clone(),
        workspace,
        analysis,
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
    if is_in_comment(source, offset.min(source.len())) {
        return None;
    }

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
    let context = extract_java_completion_context_at_offset(&db, file, offset, None);
    let analysis = RequestAnalysisState {
        analysis: crate::workspace::AnalysisContext {
            module: ModuleId::ROOT,
            classpath: ClasspathId::Main,
            source_root: None,
            root_kind: None,
        },
        view,
        workspace_version: db.workspace_index().version(),
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
            receiver_kind,
            receiver_type,
            member_prefix,
            receiver_expr,
            arguments,
            ..
        } => CursorLocationData::MemberAccess {
            receiver_kind: match receiver_kind {
                crate::semantic::AccessReceiverKind::Unknown => AccessReceiverKindData::Unknown,
                crate::semantic::AccessReceiverKind::Expression => {
                    AccessReceiverKindData::Expression
                }
                crate::semantic::AccessReceiverKind::Type {
                    class_internal_name,
                } => AccessReceiverKindData::Type {
                    class_internal_name: Arc::clone(class_internal_name),
                },
            },
            receiver_expr: Arc::from(receiver_expr.as_str()),
            member_prefix: Arc::from(member_prefix.as_str()),
            receiver_type_hint: receiver_type.clone().or_else(|| match receiver_kind {
                crate::semantic::AccessReceiverKind::Type {
                    class_internal_name,
                } => Some(Arc::clone(class_internal_name)),
                _ => None,
            }),
            arguments: arguments.as_ref().map(|s| Arc::from(s.as_str())),
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
            qualifier_expr,
            qualifier_owner_internal,
        } => CursorLocationData::ConstructorCall {
            class_prefix: Arc::from(class_prefix.as_str()),
            expected_type: expected_type.as_deref().map(Arc::from),
            qualifier_expr: qualifier_expr.as_deref().map(Arc::from),
            qualifier_owner_internal: qualifier_owner_internal.clone(),
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
        CursorLocation::AnnotationParam {
            prefix,
            annotation_name,
            used_keys,
            fresh_slot,
        } => CursorLocationData::AnnotationParam {
            prefix: Arc::from(prefix.as_str()),
            annotation_name: annotation_name.clone(),
            used_keys: used_keys.clone(),
            fresh_slot: *fresh_slot,
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
        java_module_context: None,
        query: Arc::from(""),
        cursor_offset: 0,
        enclosing_class: None,
        enclosing_internal_name: None,
        enclosing_class_chain: vec![],
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

fn convert_java_module_context(
    kind: crate::semantic::context::JavaModuleContextKind,
) -> JavaModuleContextKindData {
    match kind {
        crate::semantic::context::JavaModuleContextKind::DirectiveKeyword => {
            JavaModuleContextKindData::DirectiveKeyword
        }
        crate::semantic::context::JavaModuleContextKind::RequiresModifier => {
            JavaModuleContextKindData::RequiresModifier
        }
        crate::semantic::context::JavaModuleContextKind::RequiresModule => {
            JavaModuleContextKindData::RequiresModule
        }
        crate::semantic::context::JavaModuleContextKind::ExportsPackage => {
            JavaModuleContextKindData::ExportsPackage
        }
        crate::semantic::context::JavaModuleContextKind::OpensPackage => {
            JavaModuleContextKindData::OpensPackage
        }
        crate::semantic::context::JavaModuleContextKind::TargetModule => {
            JavaModuleContextKindData::TargetModule
        }
        crate::semantic::context::JavaModuleContextKind::UsesType => {
            JavaModuleContextKindData::UsesType
        }
        crate::semantic::context::JavaModuleContextKind::ProvidesService => {
            JavaModuleContextKindData::ProvidesService
        }
        crate::semantic::context::JavaModuleContextKind::ProvidesImplementation => {
            JavaModuleContextKindData::ProvidesImplementation
        }
    }
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
