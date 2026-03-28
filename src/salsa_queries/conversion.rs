use std::collections::HashMap;
use std::sync::Arc;

use crate::index::{FieldSummary, IndexView, MethodParams, MethodSummary};
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
use crate::salsa_queries::context::{AccessReceiverKindData, JavaModuleContextKindData};
use crate::salsa_queries::{
    CompletionContextData, CursorLocationData, ExpectedTypeSourceData, FunctionalExprShapeData,
    FunctionalTargetHintData, MethodRefQualifierKindData, MethodSummaryData, StatementLabelData,
    StatementLabelTargetKindData,
};
use crate::semantic::context::{
    CurrentClassMember, ExpectedTypeSource, FunctionalExprShape, FunctionalMethodCallHint,
    FunctionalTargetHint, JavaModuleContextKind, MethodRefQualifierKind, StatementLabel,
    StatementLabelTargetKind,
};
use crate::semantic::types::type_name::TypeName;
use crate::semantic::{CursorLocation, LocalVar, SemanticContext};
use crate::workspace::AnalysisContext;

#[derive(Clone)]
pub struct RequestAnalysisState {
    pub analysis: AnalysisContext,
    pub view: IndexView,
    pub workspace_version: u64,
}

/// Conversion layer between Salsa-compatible data and rich semantic types.
pub trait FromSalsaData<T> {
    fn from_salsa_data(
        data: T,
        db: &dyn Db,
        file: SourceFile,
        workspace: Option<&crate::workspace::Workspace>,
    ) -> Self;
}

pub trait FromSalsaDataWithAnalysis<T> {
    fn from_salsa_data_with_analysis(
        data: T,
        db: &dyn Db,
        file: SourceFile,
        workspace: Option<&crate::workspace::Workspace>,
        analysis: Option<&RequestAnalysisState>,
    ) -> Self;
}

impl FromSalsaData<CompletionContextData> for SemanticContext {
    fn from_salsa_data(
        data: CompletionContextData,
        db: &dyn Db,
        file: SourceFile,
        workspace: Option<&crate::workspace::Workspace>,
    ) -> Self {
        <Self as FromSalsaDataWithAnalysis<CompletionContextData>>::from_salsa_data_with_analysis(
            data, db, file, workspace, None,
        )
    }
}

impl FromSalsaDataWithAnalysis<CompletionContextData> for SemanticContext {
    fn from_salsa_data_with_analysis(
        data: CompletionContextData,
        db: &dyn Db,
        file: SourceFile,
        workspace: Option<&crate::workspace::Workspace>,
        analysis: Option<&RequestAnalysisState>,
    ) -> Self {
        let location = convert_cursor_location(&data.location);
        let imports = crate::salsa_queries::extract_imports(db, file);
        let existing_imports: Vec<Arc<str>> = imports.iter().cloned().collect();

        let mut ctx = SemanticContext::new(
            location,
            data.query.as_ref(),
            vec![],
            data.enclosing_class.clone(),
            data.enclosing_internal_name.clone(),
            data.enclosing_package.clone(),
            existing_imports.clone(),
        )
        .with_java_module_context(
            data.java_module_context
                .map(convert_java_module_context_kind),
        )
        .with_enclosing_class_chain(data.enclosing_class_chain.clone())
        .with_file_uri(data.file_uri.clone())
        .with_language_id(crate::language::LanguageId::new(data.language_id.clone()));

        if let Some(language) = crate::language::lookup_language(data.language_id.as_ref()) {
            ctx = language.enrich_semantic_context_salsa(
                ctx,
                db,
                file,
                workspace,
                &data,
                existing_imports,
                analysis,
            );
        }

        ctx
    }
}

pub(crate) fn enrich_java_semantic_context(
    ctx: SemanticContext,
    db: &dyn Db,
    file: SourceFile,
    workspace: Option<&crate::workspace::Workspace>,
    data: &CompletionContextData,
    existing_imports: Vec<Arc<str>>,
    analysis: Option<&RequestAnalysisState>,
) -> SemanticContext {
    let source = file.content(db);

    let members = crate::salsa_queries::semantic::extract_java_current_class_member_list(
        db,
        file,
        data.cursor_offset,
        workspace,
    );

    let method_map: HashMap<Arc<str>, Arc<MethodSummary>> = members
        .iter()
        .filter_map(|member| match member {
            CurrentClassMember::Method(method) => {
                Some((Arc::clone(&method.name), Arc::clone(method)))
            }
            CurrentClassMember::Field(_) => None,
        })
        .collect();

    let type_ctx = if let Some(request_analysis) = analysis {
        SourceTypeCtx::from_view(
            data.enclosing_package.clone(),
            existing_imports.clone(),
            request_analysis.view.clone(),
        )
    } else {
        SourceTypeCtx::from_overview(
            data.enclosing_package.clone(),
            existing_imports.clone(),
            None,
        )
    };
    let current_class_super_name =
        crate::salsa_queries::semantic::extract_java_enclosing_super_name(
            db,
            file,
            data.cursor_offset,
        );
    let type_ctx = Arc::new(
        type_ctx
            .with_current_class_methods(method_map)
            .with_current_class_super_name(current_class_super_name),
    );

    let local_variables = workspace
        .map(|ws| fetch_locals_from_workspace(db, file, ws, &data))
        .unwrap_or_else(|| {
            crate::salsa_queries::extract_visible_method_locals_from_source(
                source,
                data.cursor_offset,
                Some(&type_ctx),
            )
        });

    let static_imports = fetch_static_imports(db, file);
    let current_java_module_name = workspace
        .and_then(|ws| ws.java_module_descriptor_for_uri(file.file_id(db).uri()))
        .map(|descriptor| Arc::clone(&descriptor.name));
    let java_module_names = workspace
        .zip(analysis.map(|request_analysis| request_analysis.analysis))
        .map(|(workspace, analysis)| workspace.visible_java_module_names(analysis))
        .unwrap_or_default();
    let java_module_packages = analysis
        .map(|request_analysis| {
            db.workspace_index().module_source_package_names(
                request_analysis.analysis.module,
                request_analysis.analysis.classpath,
                request_analysis.analysis.source_root,
            )
        })
        .unwrap_or_default();
    let enclosing_class_member =
        crate::salsa_queries::java::extract_java_enclosing_method(db, file, data.cursor_offset)
            .as_deref()
            .map(convert_method_summary_data_to_member);
    let active_lambda_param_names = workspace
        .map(|_| {
            crate::salsa_queries::extract_active_lambda_param_names_incremental(
                db,
                file,
                data.cursor_offset,
            )
        })
        .unwrap_or_else(|| {
            crate::salsa_queries::extract_active_lambda_param_names_from_source(
                source,
                data.cursor_offset,
            )
        });
    let flow_type_overrides = crate::salsa_queries::materialize_flow_type_overrides(
        crate::salsa_queries::extract_java_flow_type_overrides(db, file, data.cursor_offset)
            .as_ref(),
    );
    let statement_labels = convert_statement_labels(&data.statement_labels);
    let functional_target_hint = data
        .functional_target_hint
        .as_ref()
        .map(convert_functional_target_hint);

    let mut ctx = ctx;
    ctx.local_variables = local_variables;

    ctx.with_static_imports(static_imports)
        .with_current_java_module_name(current_java_module_name)
        .with_java_module_packages(java_module_packages)
        .with_java_module_names(java_module_names)
        .with_class_member_position(data.is_class_member_position)
        .with_class_members(members)
        .with_enclosing_member(enclosing_class_member)
        .with_char_after_cursor(data.char_after_cursor)
        .with_statement_labels(statement_labels)
        .with_active_lambda_param_names(active_lambda_param_names)
        .with_functional_target_hint(functional_target_hint)
        .with_flow_type_overrides(flow_type_overrides)
        .with_extension(type_ctx)
}

fn fetch_static_imports(db: &dyn Db, file: SourceFile) -> Vec<Arc<str>> {
    crate::language::lookup_language(file.language_id(db).as_ref())
        .map(|language| language.extract_static_imports_salsa(db, file))
        .unwrap_or_default()
}

fn convert_cursor_location(data: &CursorLocationData) -> CursorLocation {
    match data {
        CursorLocationData::Expression { prefix } => CursorLocation::Expression {
            prefix: prefix.to_string(),
        },
        CursorLocationData::MemberAccess {
            receiver_kind,
            receiver_expr,
            member_prefix,
            receiver_type_hint,
            arguments,
        } => CursorLocation::MemberAccess {
            receiver_kind: match receiver_kind {
                AccessReceiverKindData::Unknown => crate::semantic::AccessReceiverKind::Unknown,
                AccessReceiverKindData::Expression => {
                    crate::semantic::AccessReceiverKind::Expression
                }
                AccessReceiverKindData::Type {
                    class_internal_name,
                } => crate::semantic::AccessReceiverKind::Type {
                    class_internal_name: Arc::clone(class_internal_name),
                },
            },
            receiver_semantic_type: receiver_type_hint
                .as_ref()
                .map(|s| TypeName::new(Arc::clone(s))),
            receiver_type: receiver_type_hint.clone().or_else(|| match receiver_kind {
                AccessReceiverKindData::Type {
                    class_internal_name,
                } => Some(Arc::clone(class_internal_name)),
                _ => None,
            }),
            member_prefix: member_prefix.to_string(),
            receiver_expr: receiver_expr.to_string(),
            arguments: arguments.as_ref().map(|s| s.to_string()),
        },
        CursorLocationData::Import { prefix } => CursorLocation::Import {
            prefix: prefix.to_string(),
        },
        CursorLocationData::ImportStatic { prefix } => CursorLocation::ImportStatic {
            prefix: prefix.to_string(),
        },
        CursorLocationData::MethodArgument { prefix, .. } => CursorLocation::MethodArgument {
            prefix: prefix.to_string(),
        },
        CursorLocationData::ConstructorCall {
            class_prefix,
            expected_type,
            qualifier_expr,
            qualifier_owner_internal,
        } => CursorLocation::ConstructorCall {
            class_prefix: class_prefix.to_string(),
            expected_type: expected_type.as_ref().map(|s| s.to_string()),
            qualifier_expr: qualifier_expr.as_ref().map(|s| s.to_string()),
            qualifier_owner_internal: qualifier_owner_internal.clone(),
        },
        CursorLocationData::TypeAnnotation { prefix } => CursorLocation::TypeAnnotation {
            prefix: prefix.to_string(),
        },
        CursorLocationData::VariableName { type_name } => CursorLocation::VariableName {
            type_name: type_name.to_string(),
        },
        CursorLocationData::StringLiteral { prefix } => CursorLocation::StringLiteral {
            prefix: prefix.to_string(),
        },
        CursorLocationData::MethodReference {
            qualifier_expr,
            member_prefix,
            is_constructor,
        } => CursorLocation::MethodReference {
            qualifier_expr: qualifier_expr.to_string(),
            member_prefix: member_prefix.to_string(),
            is_constructor: *is_constructor,
        },
        CursorLocationData::Annotation {
            prefix,
            target_element_type,
        } => CursorLocation::Annotation {
            prefix: prefix.to_string(),
            target_element_type: target_element_type.clone(),
        },
        CursorLocationData::AnnotationParam {
            prefix,
            annotation_name,
            used_keys,
            fresh_slot,
        } => CursorLocation::AnnotationParam {
            prefix: prefix.to_string(),
            annotation_name: annotation_name.clone(),
            used_keys: used_keys.clone(),
            fresh_slot: *fresh_slot,
        },
        CursorLocationData::StatementLabel { kind, prefix } => {
            use crate::semantic::context::StatementLabelCompletionKind;

            let completion_kind = match kind {
                crate::salsa_queries::StatementLabelKind::Break => {
                    StatementLabelCompletionKind::Break
                }
                crate::salsa_queries::StatementLabelKind::Continue => {
                    StatementLabelCompletionKind::Continue
                }
            };

            CursorLocation::StatementLabel {
                kind: completion_kind,
                prefix: prefix.to_string(),
            }
        }
        CursorLocationData::Unknown => CursorLocation::Unknown,
    }
}

fn convert_java_module_context_kind(data: JavaModuleContextKindData) -> JavaModuleContextKind {
    match data {
        JavaModuleContextKindData::DirectiveKeyword => JavaModuleContextKind::DirectiveKeyword,
        JavaModuleContextKindData::RequiresModifier => JavaModuleContextKind::RequiresModifier,
        JavaModuleContextKindData::RequiresModule => JavaModuleContextKind::RequiresModule,
        JavaModuleContextKindData::ExportsPackage => JavaModuleContextKind::ExportsPackage,
        JavaModuleContextKindData::OpensPackage => JavaModuleContextKind::OpensPackage,
        JavaModuleContextKindData::TargetModule => JavaModuleContextKind::TargetModule,
        JavaModuleContextKindData::UsesType => JavaModuleContextKind::UsesType,
        JavaModuleContextKindData::ProvidesService => JavaModuleContextKind::ProvidesService,
        JavaModuleContextKindData::ProvidesImplementation => {
            JavaModuleContextKind::ProvidesImplementation
        }
    }
}

fn fetch_locals_from_workspace(
    db: &dyn Db,
    file: SourceFile,
    workspace: &crate::workspace::Workspace,
    context: &CompletionContextData,
) -> Vec<LocalVar> {
    crate::salsa_queries::extract_visible_method_locals_incremental(
        db,
        file,
        context.cursor_offset,
        workspace,
    )
}

pub fn convert_local_var(data: &crate::salsa_queries::LocalVarData) -> LocalVar {
    LocalVar {
        name: Arc::clone(&data.name),
        type_internal: TypeName::new(data.type_internal.as_ref()),
        decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
        init_expr: data.init_expr.as_ref().map(|s| s.to_string()),
    }
}

fn convert_statement_labels(data: &[StatementLabelData]) -> Vec<StatementLabel> {
    data.iter()
        .map(|label| StatementLabel {
            name: Arc::clone(&label.name),
            target_kind: convert_statement_label_target_kind(label.target_kind),
        })
        .collect()
}

fn convert_statement_label_target_kind(
    kind: StatementLabelTargetKindData,
) -> StatementLabelTargetKind {
    match kind {
        StatementLabelTargetKindData::Block => StatementLabelTargetKind::Block,
        StatementLabelTargetKindData::While => StatementLabelTargetKind::While,
        StatementLabelTargetKindData::DoWhile => StatementLabelTargetKind::DoWhile,
        StatementLabelTargetKindData::For => StatementLabelTargetKind::For,
        StatementLabelTargetKindData::EnhancedFor => StatementLabelTargetKind::EnhancedFor,
        StatementLabelTargetKindData::Switch => StatementLabelTargetKind::Switch,
        StatementLabelTargetKindData::Other => StatementLabelTargetKind::Other,
    }
}

fn convert_functional_target_hint(data: &FunctionalTargetHintData) -> FunctionalTargetHint {
    FunctionalTargetHint {
        expected_type_source: data.expected_type_source.as_deref().map(str::to_owned),
        expected_type_context: data
            .expected_type_context
            .as_ref()
            .map(convert_expected_type_source),
        assignment_lhs_expr: data.assignment_lhs_expr.as_deref().map(str::to_owned),
        method_call: data
            .method_call
            .as_ref()
            .map(convert_functional_method_call_hint),
        expr_shape: data.expr_shape.as_ref().map(convert_functional_expr_shape),
    }
}

fn convert_expected_type_source(data: &ExpectedTypeSourceData) -> ExpectedTypeSource {
    match data {
        ExpectedTypeSourceData::VariableInitializer => ExpectedTypeSource::VariableInitializer,
        ExpectedTypeSourceData::AssignmentRhs => ExpectedTypeSource::AssignmentRhs,
        ExpectedTypeSourceData::ReturnExpr => ExpectedTypeSource::ReturnExpr,
        ExpectedTypeSourceData::MethodArgument { arg_index } => {
            ExpectedTypeSource::MethodArgument {
                arg_index: *arg_index,
            }
        }
    }
}

fn convert_functional_method_call_hint(
    data: &crate::salsa_queries::FunctionalMethodCallHintData,
) -> FunctionalMethodCallHint {
    FunctionalMethodCallHint {
        receiver_expr: data.receiver_expr.as_ref().to_string(),
        method_name: data.method_name.as_ref().to_string(),
        arg_index: data.arg_index,
        arg_texts: data
            .arg_texts
            .iter()
            .map(|arg| arg.as_ref().to_string())
            .collect(),
    }
}

fn convert_functional_expr_shape(data: &FunctionalExprShapeData) -> FunctionalExprShape {
    match data {
        FunctionalExprShapeData::MethodReference {
            qualifier_expr,
            member_name,
            is_constructor,
            qualifier_kind,
        } => FunctionalExprShape::MethodReference {
            qualifier_expr: qualifier_expr.as_ref().to_string(),
            member_name: member_name.as_ref().to_string(),
            is_constructor: *is_constructor,
            qualifier_kind: convert_method_ref_qualifier_kind(*qualifier_kind),
        },
        FunctionalExprShapeData::Lambda {
            param_count,
            expression_body,
        } => FunctionalExprShape::Lambda {
            param_count: *param_count,
            expression_body: expression_body.as_deref().map(str::to_owned),
        },
    }
}

fn convert_method_ref_qualifier_kind(data: MethodRefQualifierKindData) -> MethodRefQualifierKind {
    match data {
        MethodRefQualifierKindData::Type => MethodRefQualifierKind::Type,
        MethodRefQualifierKindData::Expr => MethodRefQualifierKind::Expr,
        MethodRefQualifierKindData::Unknown => MethodRefQualifierKind::Unknown,
    }
}

pub fn convert_field_summary(field: &FieldSummary) -> CurrentClassMember {
    CurrentClassMember::Field(Arc::new(field.clone()))
}

fn convert_method_summary_data_to_member(data: &MethodSummaryData) -> CurrentClassMember {
    CurrentClassMember::Method(Arc::new(MethodSummary {
        name: Arc::clone(&data.name),
        params: MethodParams::from_descriptor_and_names(&data.descriptor, &data.param_names),
        annotations: Vec::new(),
        access_flags: data.access_flags,
        is_synthetic: data.is_synthetic,
        generic_signature: data.generic_signature.clone(),
        return_type: data.return_type.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ClassMetadata, ClassOrigin};
    use crate::salsa_db::{Database, FileId, SourceFile};
    use crate::semantic::context::{
        ExpectedTypeSource, FunctionalExprShape, StatementLabelCompletionKind,
        StatementLabelTargetKind,
    };
    use ropey::Rope;
    use tower_lsp::lsp_types::Url;

    fn minimal_class(internal_name: &str) -> ClassMetadata {
        let (package, name) = internal_name
            .rsplit_once('/')
            .map(|(package, name)| (Some(Arc::from(package)), Arc::from(name)))
            .unwrap_or((None, Arc::from(internal_name)));
        ClassMetadata {
            package,
            name,
            internal_name: Arc::from(internal_name),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: 0,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    #[test]
    fn test_convert_cursor_location_expression() {
        let data = CursorLocationData::Expression {
            prefix: Arc::from("test"),
        };

        let location = convert_cursor_location(&data);

        match location {
            CursorLocation::Expression { prefix } => {
                assert_eq!(prefix, "test");
            }
            _ => panic!("Expected Expression location"),
        }
    }

    #[test]
    fn test_convert_cursor_location_member_access() {
        let data = CursorLocationData::MemberAccess {
            receiver_kind: crate::salsa_queries::context::AccessReceiverKindData::Unknown,
            receiver_expr: Arc::from("obj"),
            member_prefix: Arc::from("get"),
            receiver_type_hint: Some(Arc::from("java/lang/Object")),
            arguments: None,
        };

        let location = convert_cursor_location(&data);

        match location {
            CursorLocation::MemberAccess {
                receiver_expr,
                member_prefix,
                receiver_type,
                ..
            } => {
                assert_eq!(receiver_expr, "obj");
                assert_eq!(member_prefix, "get");
                assert_eq!(receiver_type.as_deref(), Some("java/lang/Object"));
            }
            _ => panic!("Expected MemberAccess location"),
        }
    }

    #[test]
    fn test_java_completion_context_conversion_enriches_locals() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let marked_source = indoc::indoc! {r#"
            class Test {
                void demo() {
                    String localValue = "";
                    localV|
                }
            }
        "#};
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);

        assert_eq!(ctx.query, "localV");
        assert!(
            ctx.local_variables
                .iter()
                .any(|local| local.name.as_ref() == "localValue"),
            "expected localValue to be present in the converted semantic context"
        );
    }

    #[test]
    fn test_java_completion_context_conversion_preserves_statement_labels() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let marked_source = indoc::indoc! {r#"
            class Test {
                void demo() {
                    outer: while (true) {
                        break out|
                    }
                }
            }
        "#};
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);

        match &ctx.location {
            CursorLocation::StatementLabel { kind, prefix } => {
                assert_eq!(*kind, StatementLabelCompletionKind::Break);
                assert_eq!(prefix, "out");
            }
            other => panic!("expected statement-label location, got {other:?}"),
        }
        assert_eq!(
            ctx.statement_labels
                .iter()
                .map(|label| (label.name.as_ref(), label.target_kind))
                .collect::<Vec<_>>(),
            vec![("outer", StatementLabelTargetKind::While)]
        );
    }

    #[test]
    fn test_java_completion_context_conversion_preserves_functional_target_hint() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let marked_source = indoc::indoc! {r#"
            class Test {
                void demo() {
                    java.util.function.Function<String, Integer> fn = s -> s.subs|
                }
            }
        "#};
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);
        let hint = ctx
            .functional_target_hint
            .as_ref()
            .expect("expected functional target hint");

        assert_eq!(
            hint.expected_type_context,
            Some(ExpectedTypeSource::VariableInitializer)
        );
        assert!(hint.expected_type_source.is_some());
        match hint.expr_shape.as_ref() {
            Some(FunctionalExprShape::Lambda { param_count, .. }) => {
                assert_eq!(*param_count, 1);
            }
            other => panic!("expected lambda expr shape, got {other:?}"),
        }
    }

    #[test]
    fn test_java_completion_context_conversion_preserves_annotation_target_element_type() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let marked_source = indoc::indoc! {r#"
            class Test {
                @Overri|
                void demo() {}
            }
        "#};
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);

        match &ctx.location {
            CursorLocation::Annotation {
                prefix,
                target_element_type,
            } => {
                assert_eq!(prefix, "Overri");
                assert_eq!(target_element_type.as_deref(), Some("METHOD"));
            }
            other => panic!("expected annotation location, got {other:?}"),
        }
    }

    #[test]
    fn test_java_completion_context_conversion_preserves_enclosing_static_method() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let source = indoc::indoc! {r#"
            class Test {
                static void main(String[] args) {
                    
                }
            }
        "#}
        .to_string();
        let line = 2u32;
        let character = 4u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);

        assert!(
            ctx.is_in_static_context(),
            "expected enclosing static method to survive conversion, got {:?}",
            ctx.enclosing_class_member
        );
        assert_eq!(
            ctx.enclosing_class_member
                .as_ref()
                .map(|member| member.name()),
            Some(Arc::from("main"))
        );
    }

    #[test]
    fn test_java_completion_context_conversion_preserves_flow_type_overrides() {
        let workspace_index =
            crate::index::WorkspaceIndexHandle::new(crate::index::WorkspaceIndex::new());
        workspace_index.update(|index| {
            index.add_jdk_classes(vec![
                minimal_class("java/lang/Object"),
                minimal_class("java/lang/StringBuilder"),
            ]);
        });
        let db = Database::with_workspace_index(workspace_index);
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let marked_source = indoc::indoc! {r#"
            class Test {
                void demo() {
                    Object a = new StringBuilder();
                    if (a instanceof StringBuilder && a.appe|) {
                    }
                }
            }
        "#};
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);

        assert_eq!(
            ctx.flow_override_for_local("a")
                .map(TypeName::erased_internal),
            Some("java/lang/StringBuilder")
        );
    }

    #[test]
    fn test_java_completion_context_conversion_materializes_class_members_without_workspace() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let marked_source = indoc::indoc! {r#"
            class Test {
                Test() {}
                static void helper() {}

                void demo() {
                    hel|
                }
            }
        "#};
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);

        assert!(
            ctx.current_class_members.contains_key("helper"),
            "expected helper() to be materialized from source, got {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
        assert!(
            !ctx.current_class_members.contains_key("<init>"),
            "constructors should be filtered from current_class_members"
        );
    }

    #[test]
    fn test_java_completion_context_conversion_materializes_delegate_members_from_source() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Wrapper.java").unwrap();
        let marked_source = indoc::indoc! {r#"
            import lombok.experimental.Delegate;

            interface Service {
                void doSomething();
                String getName();
            }

            class ServiceImpl implements Service {
                public void doSomething() {}
                public String getName() { return "ServiceImpl"; }
            }

            class Wrapper {
                @Delegate
                private final Service service = new ServiceImpl();

                void demo() {
                    doS|
                }
            }
        "#};
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);

        assert!(
            ctx.current_class_members.contains_key("doSomething"),
            "expected doSomething() to be materialized from plain @Delegate, got {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
        assert!(
            ctx.current_class_members.contains_key("getName"),
            "expected getName() to be materialized from plain @Delegate, got {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_java_completion_context_conversion_recovers_class_members_from_error_source() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let marked_source = indoc::indoc! {r#"
            class Agent {
                private static Object inst;
                public static void agentmain(String args, Object inst) throws Exception {
                    Agent.inst =
                }
                private static Object test() { return null; }

                tes|
            }
        "#};
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        let file = SourceFile::new(&db, FileId::new(uri), source.clone(), Arc::from("java"));

        let data = crate::salsa_queries::java::extract_java_completion_context(
            &db, file, line, character, None,
        );
        let ctx = SemanticContext::from_salsa_data(data.as_ref().clone(), &db, file, None);

        assert!(
            ctx.current_class_members.contains_key("test"),
            "expected test() to survive malformed source, got {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
        assert!(
            ctx.current_class_members.contains_key("agentmain"),
            "expected agentmain() to survive malformed source, got {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
    }
}
