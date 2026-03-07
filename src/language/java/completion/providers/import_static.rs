use std::sync::Arc;

use rust_asm::constants::ACC_STATIC;

use crate::{
    completion::{CandidateKind, CompletionCandidate, provider::CompletionProvider},
    index::{ClassMetadata, IndexScope, IndexView},
    semantic::context::{CursorLocation, SemanticContext},
};

pub struct ImportStaticProvider;

impl CompletionProvider for ImportStaticProvider {
    fn name(&self) -> &'static str {
        "import_static"
    }

    fn provide(
        &self,
        scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        let prefix = match &ctx.location {
            CursorLocation::ImportStatic { prefix } => prefix.as_str(),
            _ => return vec![],
        };

        if let Some(dot_pos) = prefix.rfind('.') {
            let class_part = &prefix[..dot_pos];
            let member_prefix = &prefix[dot_pos + 1..];
            let class_internal = class_part.replace('.', "/");

            if let Some(meta) = index.get_class(&class_internal) {
                return static_members_for_import(
                    &meta,
                    &class_internal,
                    member_prefix,
                    self.name(),
                );
            }
        }

        crate::completion::import_completion::candidates_for_import(prefix, scope, index)
    }
}

/// Returns all static methods and fields in `meta`, filtered by `member_prefix` (case-insensitive).
/// label / insert_text contains only member names, with detail indicating the class to which it belongs.
fn static_members_for_import(
    meta: &ClassMetadata,
    class_internal: &str,
    member_prefix: &str,
    source: &'static str,
) -> Vec<CompletionCandidate> {
    let prefix_lower = member_prefix.to_lowercase();
    let mut out = Vec::new();

    for method in &meta.methods {
        if matches!(method.name.as_ref(), "<init>" | "<clinit>") {
            continue;
        }
        if method.access_flags & ACC_STATIC == 0 {
            continue;
        }
        if !prefix_lower.is_empty() && !method.name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        out.push(
            CompletionCandidate::new(
                Arc::clone(&method.name),
                method.name.to_string(),
                CandidateKind::StaticMethod {
                    descriptor: Arc::clone(&method.desc()),
                    defining_class: Arc::from(class_internal),
                },
                source,
            )
            .with_detail(format!(
                "{} (static method)",
                class_internal.replace('/', ".")
            ))
            .with_score(80.0),
        );
    }

    for field in &meta.fields {
        if field.access_flags & ACC_STATIC == 0 {
            continue;
        }
        if !prefix_lower.is_empty() && !field.name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        out.push(
            CompletionCandidate::new(
                Arc::clone(&field.name),
                field.name.to_string(),
                CandidateKind::StaticField {
                    descriptor: Arc::clone(&field.descriptor),
                    defining_class: Arc::from(class_internal),
                },
                source,
            )
            .with_detail(format!(
                "{} (static field)",
                class_internal.replace('/', ".")
            ))
            .with_score(80.0),
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::WorkspaceIndex;
    use crate::index::{
        ClassMetadata, ClassOrigin, FieldSummary, IndexScope, MethodParams, MethodSummary, ModuleId,
    };
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use rust_asm::constants::{ACC_PUBLIC, ACC_STATIC};
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn math_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Math"),
                internal_name: Arc::from("java/lang/Math"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("abs"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC | ACC_STATIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
                    },
                    MethodSummary {
                        name: Arc::from("pow"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC | ACC_STATIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("D")),
                    },
                    MethodSummary {
                        name: Arc::from("instanceMethod"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: None,
                    },
                ],
                fields: vec![
                    FieldSummary {
                        name: Arc::from("PI"),
                        descriptor: Arc::from("D"),
                        access_flags: ACC_PUBLIC | ACC_STATIC,
                        annotations: vec![],
                        is_synthetic: false,
                        generic_signature: None,
                    },
                    FieldSummary {
                        name: Arc::from("instanceField"),
                        descriptor: Arc::from("I"),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                    },
                ],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            }],
        );

        idx
    }

    fn import_static_ctx(prefix: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::ImportStatic {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    #[test]
    fn test_wrong_location_returns_empty() {
        let idx = math_index();
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "abs".to_string(),
            },
            "abs",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        assert!(
            ImportStaticProvider
                .provide(root_scope(), &ctx, &idx.view(root_scope()))
                .is_empty()
        );
    }

    #[test]
    fn test_class_path_stage_returns_class_candidates() {
        let idx = math_index();
        // "java.lang.Ma" - Not yet at the Math layer, should go through candidates_for_import
        let ctx = import_static_ctx("java.lang.Ma");
        let results = ImportStaticProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref().contains("Math")),
            "should suggest Math at class path stage: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_member_stage_empty_prefix_returns_all_static() {
        let idx = math_index();
        // "java.lang.Math." - Empty member prefix, returns all static members
        let ctx = import_static_ctx("java.lang.Math.");
        let results = ImportStaticProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"abs"), "abs should appear: {:?}", labels);
        assert!(labels.contains(&"pow"), "pow should appear: {:?}", labels);
        assert!(labels.contains(&"PI"), "PI should appear: {:?}", labels);
    }

    #[test]
    fn test_member_stage_filters_by_prefix() {
        let idx = math_index();
        let ctx = import_static_ctx("java.lang.Math.a");
        let results = ImportStaticProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"abs"), "abs should match prefix 'a'");
        assert!(!labels.contains(&"pow"), "pow should not match 'a'");
        assert!(
            !labels.contains(&"PI"),
            "PI should not match 'a' (case-insensitive)"
        );
    }

    #[test]
    fn test_member_stage_prefix_case_insensitive() {
        let idx = math_index();
        let ctx = import_static_ctx("java.lang.Math.p");
        let results = ImportStaticProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"pow"), "pow should match prefix 'p'");
        assert!(
            labels.contains(&"PI"),
            "PI should match prefix 'p' case-insensitively"
        );
    }

    #[test]
    fn test_member_stage_excludes_non_static() {
        let idx = math_index();
        let ctx = import_static_ctx("java.lang.Math.");
        let results = ImportStaticProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            !labels.contains(&"instanceMethod"),
            "instance method must not appear: {:?}",
            labels
        );
        assert!(
            !labels.contains(&"instanceField"),
            "instance field must not appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_member_stage_no_init_methods() {
        let idx = math_index();
        let ctx = import_static_ctx("java.lang.Math.");
        let results = ImportStaticProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results
                .iter()
                .all(|c| !matches!(c.label.as_ref(), "<init>" | "<clinit>")),
            "constructor-like names must not appear"
        );
    }

    #[test]
    fn test_insert_text_has_no_parentheses() {
        // Parentheses should not be added when completing import static.
        let idx = math_index();
        let ctx = import_static_ctx("java.lang.Math.");
        let results = ImportStaticProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let method = results.iter().find(|c| c.label.as_ref() == "abs").unwrap();
        assert_eq!(
            method.insert_text, "abs",
            "insert_text for import static should not contain '('"
        );
    }

    #[test]
    fn test_unknown_class_falls_back_to_path_completion() {
        let idx = math_index();
        // "com.example.Unknown." - The class does not exist;
        // you should call candidates_for_import (which returns an empty string or the package path).
        let ctx = import_static_ctx("com.example.Unknown.");
        // No panic, no crash.
        let _results = ImportStaticProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
    }
}
