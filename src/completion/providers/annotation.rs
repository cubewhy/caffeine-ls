use rust_asm::constants::ACC_ANNOTATION;

use super::super::{
    candidate::{CandidateKind, CompletionCandidate},
    context::{CompletionContext, CursorLocation},
    fuzzy,
    import_utils::is_import_needed,
};
use super::CompletionProvider;
use crate::index::{ClassMetadata, GlobalIndex};
use std::sync::Arc;

pub struct AnnotationProvider;

impl CompletionProvider for AnnotationProvider {
    fn name(&self) -> &'static str {
        "annotation"
    }

    fn provide(
        &self,
        ctx: &CompletionContext,
        index: &mut GlobalIndex,
    ) -> Vec<CompletionCandidate> {
        let (prefix, et) = match &ctx.location {
            CursorLocation::Annotation {
                prefix,
                target_element_type,
            } => (prefix.as_str(), target_element_type),
            _ => return vec![],
        };

        let prefix_lower = prefix.to_lowercase();
        let mut results = Vec::new();

        // Annotations from imports
        let imported = index.resolve_imports(&ctx.existing_imports);
        for meta in &imported {
            if !is_annotation_class(meta) {
                continue;
            }
            let score = match fuzzy::fuzzy_match(&prefix_lower, &meta.name.to_lowercase()) {
                Some(s) => s,
                None => continue,
            };
            let fqn = fqn_of(meta);
            if !matches_target(meta, et.as_deref()) {
                continue;
            }
            results.push(
                CompletionCandidate::new(
                    Arc::clone(&meta.name),
                    meta.name.to_string(),
                    CandidateKind::Annotation,
                    self.name(),
                )
                .with_detail(fqn)
                .with_score(80.0 + score as f32 * 0.1),
            );
        }

        let imported_internals: std::collections::HashSet<Arc<str>> = imported
            .iter()
            .map(|m| Arc::clone(&m.internal_name))
            .collect();

        // Same package annotations
        if let Some(pkg) = ctx.enclosing_package.as_deref() {
            for meta in index.classes_in_package(pkg) {
                if imported_internals.contains(&meta.internal_name) {
                    continue;
                }
                if !is_annotation_class(meta) {
                    continue;
                }
                let score = match fuzzy::fuzzy_match(&prefix_lower, &meta.name.to_lowercase()) {
                    Some(s) => s,
                    None => continue,
                };
                if !matches_target(meta, et.as_deref()) {
                    continue;
                }
                results.push(
                    CompletionCandidate::new(
                        Arc::clone(&meta.name),
                        meta.name.to_string(),
                        CandidateKind::Annotation,
                        self.name(),
                    )
                    .with_detail(fqn_of(meta))
                    .with_score(70.0 + score as f32 * 0.1),
                );
            }
        }

        // Global index — all annotation classes (require auto-import)
        for meta in index.iter_all_classes() {
            if imported_internals.contains(&meta.internal_name) {
                continue;
            }
            if !is_annotation_class(meta) {
                continue;
            }
            let score = match fuzzy::fuzzy_match(&prefix_lower, &meta.name.to_lowercase()) {
                Some(s) => s,
                None => continue,
            };
            let fqn = fqn_of(meta);
            let needs_import = is_import_needed(
                &fqn,
                &ctx.existing_imports,
                ctx.enclosing_package.as_deref(),
            );
            let candidate = CompletionCandidate::new(
                Arc::clone(&meta.name),
                meta.name.to_string(),
                CandidateKind::Annotation,
                self.name(),
            )
            .with_detail(fqn.clone())
            .with_score(50.0 + score as f32 * 0.1);
            if !matches_target(meta, et.as_deref()) {
                continue;
            }

            results.push(if needs_import {
                candidate.with_import(fqn)
            } else {
                candidate
            });
        }

        results
    }
}

fn matches_target(meta: &ClassMetadata, element_type: Option<&str>) -> bool {
    let et = match element_type {
        None => return true, // 位置未知，不过滤
        Some(et) => et,
    };
    match meta.annotation_targets() {
        None => true, // 无 @Target，适用所有位置
        Some(targets) => targets.iter().any(|t| t.as_ref() == et),
    }
}

fn is_annotation_class(meta: &crate::index::ClassMetadata) -> bool {
    meta.access_flags & ACC_ANNOTATION != 0
}

fn fqn_of(meta: &crate::index::ClassMetadata) -> String {
    match &meta.package {
        Some(pkg) => format!("{}.{}", pkg.replace('/', "."), meta.name),
        None => meta.name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::context::{CompletionContext, CursorLocation};
    use crate::completion::providers::CompletionProvider;
    use crate::index::{
        AnnotationSummary, AnnotationValue, ClassMetadata, ClassOrigin, GlobalIndex,
    };
    use rust_asm::constants::{ACC_ANNOTATION, ACC_PUBLIC};
    use rustc_hash::FxHashMap;
    use std::sync::Arc;

    fn make_annotation(pkg: &str, name: &str) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).as_str()),
            super_name: None,
            annotations: vec![],
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC | ACC_ANNOTATION,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn make_class(pkg: &str, name: &str) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).as_str()),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC, // not an annotation
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn annotation_ctx(prefix: &str, imports: Vec<Arc<str>>, pkg: &str) -> CompletionContext {
        CompletionContext::new(
            CursorLocation::Annotation {
                prefix: prefix.to_string(),
                target_element_type: None,
            },
            prefix,
            vec![],
            None,
            None,
            Some(Arc::from(pkg)),
            imports,
        )
    }

    fn make_target_annotation(targets: &[&str]) -> AnnotationSummary {
        let items: Vec<AnnotationValue> = targets
            .iter()
            .map(|t| AnnotationValue::Enum {
                type_name: Arc::from("Ljava/lang/annotation/ElementType;"),
                const_name: Arc::from(*t),
            })
            .collect();

        let mut elements = FxHashMap::default();
        elements.insert(
            Arc::from("value"),
            if items.len() == 1 {
                items.into_iter().next().unwrap()
            } else {
                AnnotationValue::Array(items)
            },
        );

        AnnotationSummary {
            internal_name: Arc::from("java/lang/annotation/Target"),
            runtime_visible: true,
            elements,
        }
    }

    fn builtin_annotation(pkg: &str, name: &str, targets: &[&str]) -> ClassMetadata {
        let internal = format!("{}/{}", pkg, name);
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(internal.as_str()),
            super_name: None,
            interfaces: vec![],
            annotations: if targets.is_empty() {
                vec![]
            } else {
                vec![make_target_annotation(targets)]
            },
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC | ACC_ANNOTATION,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    /// Returns ClassMetadata for all built-in Java annotations that are always
    /// available without an explicit import. Call this at index initialization.
    pub fn builtin_java_annotations() -> Vec<ClassMetadata> {
        vec![
            builtin_annotation("java/lang", "Override", &["METHOD"]),
            builtin_annotation("java/lang", "Deprecated", &[]),
            builtin_annotation("java/lang", "SuppressWarnings", &[]),
            builtin_annotation("java/lang", "FunctionalInterface", &["TYPE"]),
            builtin_annotation("java/lang", "SafeVarargs", &["METHOD", "CONSTRUCTOR"]),
        ]
    }

    #[test]
    fn test_non_annotation_class_excluded() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![make_class("com/example", "NotAnAnnotation")]);
        idx.add_classes(builtin_java_annotations());
        let ctx = annotation_ctx("Not", vec![], "com/example");
        let results = AnnotationProvider.provide(&ctx, &mut idx);
        assert!(
            results
                .iter()
                .all(|c| c.label.as_ref() != "NotAnAnnotation"),
            "regular class should not appear in annotation completions"
        );
    }

    #[test]
    fn test_annotation_from_import_appears() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(builtin_java_annotations());
        idx.add_classes(vec![make_annotation("org/junit", "Test")]);
        let ctx = annotation_ctx("Te", vec!["org.junit.Test".into()], "com/example");
        let results = AnnotationProvider.provide(&ctx, &mut idx);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Test"),
            "imported annotation should appear: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_annotation_from_global_index_has_import() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![make_annotation("org/junit", "Test")]);
        idx.add_classes(builtin_java_annotations());
        let ctx = annotation_ctx("Te", vec![], "com/example");
        let results = AnnotationProvider.provide(&ctx, &mut idx);
        let test_candidate = results.iter().find(|c| c.label.as_ref() == "Test");
        assert!(test_candidate.is_some(), "Test annotation should appear");
        assert_eq!(
            test_candidate.unwrap().required_import.as_deref(),
            Some("org.junit.Test"),
            "should carry auto-import"
        );
    }

    #[test]
    fn test_annotation_kind_is_annotation() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(builtin_java_annotations());
        let ctx = annotation_ctx("Over", vec![], "com/example");
        let results = AnnotationProvider.provide(&ctx, &mut idx);
        let c = results
            .iter()
            .find(|c| c.label.as_ref() == "Override")
            .unwrap();
        assert!(
            matches!(c.kind, CandidateKind::Annotation),
            "kind should be Annotation"
        );
    }

    #[test]
    fn test_prefix_filter_case_insensitive() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(builtin_java_annotations());
        let ctx = annotation_ctx("over", vec![], "com/example");
        let results = AnnotationProvider.provide(&ctx, &mut idx);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Override"),
            "case-insensitive prefix should match Override"
        );
    }

    #[test]
    fn test_target_filter_method_context() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(builtin_java_annotations());
        let mut type_only = make_annotation("com/example", "ClassOnly");
        type_only.annotations = vec![AnnotationSummary {
            internal_name: Arc::from("java/lang/annotation/Target"),
            runtime_visible: true,
            elements: {
                let mut m = FxHashMap::default();
                m.insert(
                    Arc::from("value"),
                    AnnotationValue::Enum {
                        type_name: Arc::from("Ljava/lang/annotation/ElementType;"),
                        const_name: Arc::from("TYPE"),
                    },
                );
                m
            },
        }];
        idx.add_classes(vec![type_only]);

        let ctx = CompletionContext::new(
            CursorLocation::Annotation {
                prefix: "Class".to_string(),
                target_element_type: Some(Arc::from("METHOD")),
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let results = AnnotationProvider.provide(&ctx, &mut idx);
        assert!(results.iter().all(|c| c.label.as_ref() != "ClassOnly"));
    }
}
