use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
use crate::salsa_queries::{CompletionContextData, CursorLocationData};
use crate::semantic::types::type_name::TypeName;
use crate::semantic::{CursorLocation, LocalVar, SemanticContext};
/// Conversion layer between Salsa-compatible data and rich semantic types
///
/// This module provides conversions from lightweight Salsa data structures
/// to the full-featured types used by the completion engine and LSP handlers.
use std::sync::Arc;

/// Trait for converting from Salsa data to rich types
pub trait FromSalsaData<T> {
    fn from_salsa_data(
        data: T,
        db: &dyn Db,
        file: SourceFile,
        workspace: Option<&crate::workspace::Workspace>,
    ) -> Self;
}

impl FromSalsaData<CompletionContextData> for SemanticContext {
    fn from_salsa_data(
        data: CompletionContextData,
        db: &dyn Db,
        file: SourceFile,
        workspace: Option<&crate::workspace::Workspace>,
    ) -> Self {
        // Convert cursor location
        let location = convert_cursor_location(&data.location);

        // Fetch locals from workspace cache if available
        let local_variables = if let Some(ws) = workspace {
            if data.local_var_count > 0 {
                fetch_locals_from_workspace(db, file, ws, &data)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Fetch imports (already cached by Salsa)
        let existing_imports = crate::salsa_queries::extract_imports(db, file);
        let existing_imports: Vec<Arc<str>> = existing_imports.iter().cloned().collect();

        SemanticContext::new(
            location,
            data.query.as_ref(),
            local_variables,
            data.enclosing_class.clone(),
            data.enclosing_internal_name.clone(),
            data.enclosing_package.clone(),
            existing_imports,
        )
        .with_file_uri(data.file_uri.clone())
        .with_language_id(crate::language::LanguageId::new(data.language_id.clone()))
    }
}

/// Convert Salsa cursor location to rich CursorLocation
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
            receiver_semantic_type: None,
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
            target_element_type: None, // Will be refined by context enrichment
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

/// Fetch locals from workspace using the incremental cache
fn fetch_locals_from_workspace(
    _db: &dyn Db,
    _file: SourceFile,
    workspace: &crate::workspace::Workspace,
    context: &CompletionContextData,
) -> Vec<LocalVar> {
    // Use the content hash to check if we have cached locals
    if let Some(cached) = workspace.get_cached_method_locals(context.content_hash) {
        return cached;
    }

    // If not cached, return empty - the old extraction logic will populate this
    Vec::new()
}

/// Convert Salsa LocalVarData to rich LocalVar
pub fn convert_local_var(data: &crate::salsa_queries::LocalVarData) -> LocalVar {
    LocalVar {
        name: Arc::clone(&data.name),
        type_internal: TypeName::new(data.type_internal.as_ref()),
        init_expr: data.init_expr.as_ref().map(|s| s.to_string()),
    }
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
            receiver_type_hint: None,
            arguments: None,
        };

        let location = convert_cursor_location(&data);

        match location {
            CursorLocation::MemberAccess {
                receiver_expr,
                member_prefix,
                ..
            } => {
                assert_eq!(receiver_expr, "obj");
                assert_eq!(member_prefix, "get");
            }
            _ => panic!("Expected MemberAccess location"),
        }
    }
}
