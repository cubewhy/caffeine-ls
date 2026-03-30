use rust_asm::constants::ACC_ANNOTATION;
use rustc_hash::FxHashSet;

use crate::{
    completion::{
        CandidateKind, CompletionCandidate, fuzzy,
        import_utils::{is_import_needed, source_fqn_of_meta},
        provider::{CompletionProvider, ProviderCompletionResult, ProviderSearchSpace},
    },
    index::{ClassMetadata, IndexScope, IndexView},
    lsp::{request_cancellation::RequestResult, request_context::RequestContext},
    semantic::context::{CursorLocation, SemanticContext},
};
use std::sync::Arc;

pub struct AnnotationProvider;

impl CompletionProvider for AnnotationProvider {
    fn name(&self) -> &'static str {
        "annotation"
    }

    fn is_applicable(&self, ctx: &SemanticContext) -> bool {
        matches!(ctx.location, CursorLocation::Annotation { .. })
    }

    fn search_space(&self, _ctx: &SemanticContext) -> ProviderSearchSpace {
        ProviderSearchSpace::Broad
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
        request: Option<&RequestContext>,
        limit: Option<usize>,
    ) -> RequestResult<ProviderCompletionResult> {
        if limit == Some(0) {
            return Ok(ProviderCompletionResult {
                candidates: Vec::new(),
                is_incomplete: true,
            });
        }

        let (prefix, et) = match &ctx.location {
            CursorLocation::Annotation {
                prefix,
                target_element_type,
            } => (prefix.as_str(), target_element_type),
            _ => return Ok(ProviderCompletionResult::default()),
        };

        let mut results = Vec::new();
        let mut seen_internals: FxHashSet<Arc<str>> = Default::default();
        let mut truncated = false;

        let imported = index.resolve_imports(&ctx.existing_imports);
        for (index_in_pass, meta) in imported.iter().enumerate() {
            maybe_check_cancelled(request, "completion.annotation.imported", index_in_pass)?;
            if reached_limit(results.len(), limit) {
                truncated = true;
                break;
            }

            let Some(score) = annotation_match_score(meta, prefix, et.as_deref()) else {
                continue;
            };
            if !seen_internals.insert(Arc::clone(&meta.internal_name)) {
                continue;
            }

            results.push(make_annotation_candidate(
                meta,
                index,
                self.name(),
                80.0 + score as f32 * 0.1,
                false,
            ));
        }

        if !truncated && let Some(pkg) = ctx.enclosing_package.as_deref() {
            for (index_in_pass, meta) in index.classes_in_package(pkg).into_iter().enumerate() {
                maybe_check_cancelled(
                    request,
                    "completion.annotation.same_package",
                    index_in_pass,
                )?;
                if reached_limit(results.len(), limit) {
                    truncated = true;
                    break;
                }

                let Some(score) = annotation_match_score(&meta, prefix, et.as_deref()) else {
                    continue;
                };
                if !seen_internals.insert(Arc::clone(&meta.internal_name)) {
                    continue;
                }

                results.push(make_annotation_candidate(
                    &meta,
                    index,
                    self.name(),
                    70.0 + score as f32 * 0.1,
                    false,
                ));
            }
        }

        if !truncated {
            for (index_in_pass, meta) in global_annotation_pool(index, prefix, limit)
                .into_iter()
                .enumerate()
            {
                maybe_check_cancelled(request, "completion.annotation.global", index_in_pass)?;
                if reached_limit(results.len(), limit) {
                    truncated = true;
                    break;
                }

                let Some(score) = annotation_match_score(&meta, prefix, et.as_deref()) else {
                    continue;
                };
                if !seen_internals.insert(Arc::clone(&meta.internal_name)) {
                    continue;
                }

                let fqn = source_fqn_of_meta(meta.as_ref(), index);
                let needs_import = is_import_needed(
                    &fqn,
                    &ctx.existing_imports,
                    ctx.enclosing_package.as_deref(),
                );
                results.push(make_annotation_candidate(
                    &meta,
                    index,
                    self.name(),
                    50.0 + score as f32 * 0.1,
                    needs_import,
                ));
            }
        }

        Ok(ProviderCompletionResult {
            candidates: results,
            is_incomplete: truncated,
        })
    }
}

fn global_annotation_pool(
    index: &IndexView,
    prefix: &str,
    limit: Option<usize>,
) -> Vec<Arc<ClassMetadata>> {
    if prefix.is_empty() {
        return index.annotation_classes();
    }

    index.fuzzy_search_classes(prefix, annotation_search_limit(limit))
}

fn annotation_search_limit(limit: Option<usize>) -> usize {
    match limit {
        Some(limit) => limit.saturating_mul(8).min(1024).max(limit),
        None => 1024,
    }
}

fn annotation_match_score(
    meta: &ClassMetadata,
    prefix: &str,
    element_type: Option<&str>,
) -> Option<u32> {
    if !is_annotation_class(meta) || !matches_target(meta, element_type) {
        return None;
    }
    fuzzy::fuzzy_match(prefix, meta.direct_name())
}

fn make_annotation_candidate(
    meta: &Arc<ClassMetadata>,
    index: &IndexView,
    source: &'static str,
    score: f32,
    needs_import: bool,
) -> CompletionCandidate {
    let label = meta.direct_name();
    let fqn = source_fqn_of_meta(meta.as_ref(), index);
    let candidate = CompletionCandidate::new(
        Arc::clone(&meta.name),
        label.to_string(),
        CandidateKind::Annotation,
        source,
    )
    .with_detail(fqn.clone())
    .with_score(score);

    if needs_import {
        candidate.with_import(fqn)
    } else {
        candidate
    }
}

fn maybe_check_cancelled(
    request: Option<&RequestContext>,
    phase: &'static str,
    index: usize,
) -> RequestResult<()> {
    if index.is_multiple_of(32)
        && let Some(request) = request
    {
        request.check_cancelled(phase)?;
    }
    Ok(())
}

fn reached_limit(len: usize, limit: Option<usize>) -> bool {
    limit.is_some_and(|limit| len >= limit)
}

fn matches_target(meta: &ClassMetadata, element_type: Option<&str>) -> bool {
    let et = match element_type {
        None => return true, // unknown target (etc. inside ERROR node), should not filter
        Some(et) => et,
    };
    match meta.annotation_targets() {
        None => true, // No @Target specified in annotation declaration
        Some(targets) => targets.iter().any(|t| t.as_ref() == et),
    }
}

fn is_annotation_class(meta: &crate::index::ClassMetadata) -> bool {
    meta.access_flags & ACC_ANNOTATION != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::CandidateKind;
    use crate::index::WorkspaceIndex;
    use crate::index::{
        AnnotationSummary, AnnotationValue, ClassMetadata, ClassOrigin, IndexScope, ModuleId,
    };
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use rust_asm::constants::{ACC_ANNOTATION, ACC_PUBLIC};
    use rustc_hash::FxHashMap;
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

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

    fn annotation_ctx(prefix: &str, imports: Vec<Arc<str>>, pkg: &str) -> SemanticContext {
        SemanticContext::new(
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
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_class("com/example", "NotAnAnnotation")]);
        idx.add_classes(builtin_java_annotations());
        let ctx = annotation_ctx("Not", vec![], "com/example");
        let results = AnnotationProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results
                .iter()
                .all(|c| c.label.as_ref() != "NotAnAnnotation"),
            "regular class should not appear in annotation completions"
        );
    }

    #[test]
    fn test_annotation_from_import_appears() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(builtin_java_annotations());
        idx.add_classes(vec![make_annotation("org/junit", "Test")]);
        let ctx = annotation_ctx("Te", vec!["org.junit.Test".into()], "com/example");
        let results = AnnotationProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Test"),
            "imported annotation should appear: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_annotation_from_global_index_has_import() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_annotation("org/junit", "Test")]);
        idx.add_classes(builtin_java_annotations());
        let ctx = annotation_ctx("Te", vec![], "com/example");
        let results = AnnotationProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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
        let idx = WorkspaceIndex::new();
        idx.add_classes(builtin_java_annotations());
        let ctx = annotation_ctx("Over", vec![], "com/example");
        let results = AnnotationProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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
        let idx = WorkspaceIndex::new();
        idx.add_classes(builtin_java_annotations());
        let ctx = annotation_ctx("over", vec![], "com/example");
        let results = AnnotationProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Override"),
            "case-insensitive prefix should match Override"
        );
    }

    #[test]
    fn test_target_filter_method_context() {
        let idx = WorkspaceIndex::new();
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

        let ctx = SemanticContext::new(
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
        let results = AnnotationProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.iter().all(|c| c.label.as_ref() != "ClassOnly"));
    }

    #[test]
    fn test_same_package_annotation_not_duplicated_by_global_pass() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_annotation("com/example", "LocalAnn")]);

        let results = AnnotationProvider
            .provide_test(
                root_scope(),
                &annotation_ctx("Local", vec![], "com/example"),
                &idx.view(root_scope()),
                None,
            )
            .candidates;

        assert_eq!(
            results
                .iter()
                .filter(|candidate| candidate.label.as_ref() == "LocalAnn")
                .count(),
            1,
            "same-package annotations should not be duplicated by the global pass"
        );
    }

    #[test]
    fn test_annotation_provider_limit_marks_incomplete() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(builtin_java_annotations());
        idx.add_classes(vec![
            make_annotation("org/junit", "Test"),
            make_annotation("org/junit", "RepeatedTest"),
            make_annotation("org/junit", "Timeout"),
        ]);

        let limited = AnnotationProvider.provide_test(
            root_scope(),
            &annotation_ctx("", vec![], "com/example"),
            &idx.view(root_scope()),
            Some(3),
        );

        assert_eq!(limited.candidates.len(), 3);
        assert!(limited.is_incomplete);
    }
}
