use std::collections::HashMap;
use std::sync::Arc;

use crate::index::{FieldSummary, MethodParams, MethodSummary};
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
use crate::salsa_queries::{CompletionContextData, CursorLocationData};
use crate::semantic::context::{CurrentClassMember, StatementLabel, StatementLabelTargetKind};
use crate::semantic::types::type_name::TypeName;
use crate::semantic::{CursorLocation, LocalVar, SemanticContext};
use crate::workspace::AnalysisContext;

#[derive(Clone)]
pub struct RequestAnalysisState {
    pub analysis: AnalysisContext,
    pub name_table: Arc<crate::index::NameTable>,
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

        let local_variables = workspace
            .map(|ws| fetch_locals_from_workspace(db, file, ws, &data))
            .unwrap_or_default();

        let mut ctx = SemanticContext::new(
            location,
            data.query.as_ref(),
            local_variables,
            data.enclosing_class.clone(),
            data.enclosing_internal_name.clone(),
            data.enclosing_package.clone(),
            existing_imports.clone(),
        )
        .with_file_uri(data.file_uri.clone())
        .with_language_id(crate::language::LanguageId::new(data.language_id.clone()));

        if data.language_id.as_ref() == "java" {
            ctx = enrich_java_semantic_context(
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

fn enrich_java_semantic_context(
    ctx: SemanticContext,
    db: &dyn Db,
    file: SourceFile,
    workspace: Option<&crate::workspace::Workspace>,
    data: &CompletionContextData,
    existing_imports: Vec<Arc<str>>,
    analysis: Option<&RequestAnalysisState>,
) -> SemanticContext {
    let name_table = if let Some(state) = analysis {
        tracing::debug!(
            module = state.analysis.module.0,
            classpath = ?state.analysis.classpath,
            source_root = ?state.analysis.source_root.map(|id| id.0),
            name_table_len = state.name_table.len(),
            "conversion: using request-scoped name table"
        );
        Some(Arc::clone(&state.name_table))
    } else {
        tracing::debug!(
            uri = %file.file_id(db).as_str(),
            "conversion: falling back to workspace analysis for name table"
        );
        workspace.map(|ws| {
            let analysis = ws.analysis_context_for_uri(file.file_id(db).uri());
            crate::salsa_queries::get_name_table_for_context(
                db,
                analysis.module,
                analysis.classpath,
                analysis.source_root,
            )
        })
    };

    let members = workspace
        .map(|ws| fetch_class_members_from_workspace(db, file, ws, data.cursor_offset))
        .unwrap_or_default();

    let method_map: HashMap<Arc<str>, Arc<MethodSummary>> = members
        .values()
        .filter_map(|member| match member {
            CurrentClassMember::Method(method) => {
                Some((Arc::clone(&method.name), Arc::clone(method)))
            }
            CurrentClassMember::Field(_) => None,
        })
        .collect();

    let type_ctx = Arc::new(
        SourceTypeCtx::new(
            data.enclosing_package.clone(),
            existing_imports.clone(),
            name_table,
        )
        .with_current_class_methods(method_map),
    );

    let static_imports = fetch_static_imports(db, file);
    let is_class_member_position = detect_java_class_member_position(db, file, data.cursor_offset);
    let enclosing_class_member =
        detect_java_enclosing_member(db, file, data.cursor_offset, &type_ctx);
    let char_after_cursor = compute_char_after_cursor(file.content(db), data.cursor_offset);
    let statement_labels = infer_statement_labels(&ctx.location);
    let active_lambda_param_names = infer_lambda_params(&ctx.location);

    let mut flow_type_overrides = HashMap::new();
    if let CursorLocation::MemberAccess {
        receiver_type: Some(receiver_type),
        receiver_expr,
        ..
    } = &ctx.location
        && !receiver_expr.is_empty()
    {
        flow_type_overrides.insert(
            Arc::from(receiver_expr.as_str()),
            TypeName::new(Arc::clone(receiver_type)),
        );
    }

    ctx.with_static_imports(static_imports)
        .with_class_member_position(is_class_member_position)
        .with_class_members(members.into_values())
        .with_enclosing_member(enclosing_class_member)
        .with_char_after_cursor(char_after_cursor)
        .with_statement_labels(statement_labels)
        .with_active_lambda_param_names(active_lambda_param_names)
        .with_flow_type_overrides(flow_type_overrides)
        .with_extension(type_ctx)
}

fn fetch_class_members_from_workspace(
    db: &dyn Db,
    file: SourceFile,
    workspace: &crate::workspace::Workspace,
    cursor_offset: usize,
) -> HashMap<Arc<str>, CurrentClassMember> {
    crate::salsa_queries::extract_class_members_incremental(db, file, cursor_offset, workspace)
}

fn fetch_static_imports(db: &dyn Db, file: SourceFile) -> Vec<Arc<str>> {
    crate::salsa_queries::extract_imports(db, file)
        .iter()
        .filter(|imp| imp.starts_with("static "))
        .map(|imp| Arc::from(imp.trim_start_matches("static ").trim()))
        .collect()
}

fn detect_java_class_member_position(db: &dyn Db, file: SourceFile, cursor_offset: usize) -> bool {
    let content = file.content(db);
    let before = &content[..cursor_offset.min(content.len())];
    let line = before.rsplit('\n').next().unwrap_or("").trim();
    if line.is_empty() {
        return true;
    }
    !(line.contains('=') || line.contains('(') || line.contains("return") || line.contains("if "))
}

fn detect_java_enclosing_member(
    db: &dyn Db,
    file: SourceFile,
    cursor_offset: usize,
    type_ctx: &Arc<SourceTypeCtx>,
) -> Option<CurrentClassMember> {
    let content = file.content(db);
    let before = &content[..cursor_offset.min(content.len())];
    let method_name = before.lines().rev().find_map(|line| {
        let line = line.trim();
        if !line.contains('(') || line.ends_with(';') {
            return None;
        }
        let prefix = line.split('(').next()?.split_whitespace().last()?;
        if prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            Some(prefix.to_string())
        } else {
            None
        }
    })?;

    Some(CurrentClassMember::Method(Arc::new(MethodSummary {
        name: Arc::from(method_name),
        params: MethodParams::empty(),
        annotations: Vec::new(),
        access_flags: 0,
        is_synthetic: false,
        generic_signature: None,
        return_type: Some(Arc::from(type_ctx.resolve_simple("void"))),
    })))
}

fn compute_char_after_cursor(content: &str, cursor_offset: usize) -> Option<char> {
    content[cursor_offset.min(content.len())..]
        .chars()
        .find(|c| !(c.is_alphanumeric() || *c == '_'))
}

fn infer_statement_labels(location: &CursorLocation) -> Vec<StatementLabel> {
    match location {
        CursorLocation::StatementLabel { kind, prefix } if !prefix.is_empty() => {
            vec![StatementLabel {
                name: Arc::from(prefix.as_str()),
                target_kind: match kind {
                    crate::semantic::context::StatementLabelCompletionKind::Break => {
                        StatementLabelTargetKind::Block
                    }
                    crate::semantic::context::StatementLabelCompletionKind::Continue => {
                        StatementLabelTargetKind::For
                    }
                },
            }]
        }
        _ => Vec::new(),
    }
}

fn infer_lambda_params(location: &CursorLocation) -> Vec<Arc<str>> {
    match location {
        CursorLocation::MethodArgument { prefix } if !prefix.is_empty() => {
            vec![Arc::from(prefix.as_str())]
        }
        _ => Vec::new(),
    }
}

fn convert_cursor_location(data: &CursorLocationData) -> CursorLocation {
    match data {
        CursorLocationData::Expression { prefix } => CursorLocation::Expression {
            prefix: prefix.to_string(),
        },
        CursorLocationData::MemberAccess {
            receiver_expr,
            member_prefix,
            receiver_type_hint,
            arguments,
        } => CursorLocation::MemberAccess {
            receiver_semantic_type: receiver_type_hint
                .as_ref()
                .map(|s| TypeName::new(Arc::clone(s))),
            receiver_type: receiver_type_hint.clone(),
            member_prefix: member_prefix.to_string(),
            receiver_expr: receiver_expr.to_string(),
            arguments: arguments.as_ref().map(|s| s.to_string()),
        },
        CursorLocationData::StaticAccess {
            class_internal_name,
            member_prefix,
        } => CursorLocation::StaticAccess {
            class_internal_name: Arc::clone(class_internal_name),
            member_prefix: member_prefix.to_string(),
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
        CursorLocationData::Annotation { prefix } => CursorLocation::Annotation {
            prefix: prefix.to_string(),
            target_element_type: None,
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

fn fetch_locals_from_workspace(
    db: &dyn Db,
    file: SourceFile,
    workspace: &crate::workspace::Workspace,
    context: &CompletionContextData,
) -> Vec<LocalVar> {
    crate::salsa_queries::extract_method_locals_incremental(
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
        init_expr: data.init_expr.as_ref().map(|s| s.to_string()),
    }
}

pub fn convert_field_summary(field: &FieldSummary) -> CurrentClassMember {
    CurrentClassMember::Field(Arc::new(field.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
