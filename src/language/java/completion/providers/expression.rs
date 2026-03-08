use crate::{
    completion::{
        CandidateKind, CompletionCandidate, fuzzy, import_utils::is_import_needed,
        provider::CompletionProvider,
    },
    index::{IndexScope, IndexView},
    language::java::completion::providers::type_lookup::qualified_nested_type_matches,
    semantic::context::{CursorLocation, SemanticContext},
    semantic::types::symbol_resolver::SymbolResolver,
};
use std::sync::Arc;
use std::time::Instant;

pub struct ExpressionProvider;

impl CompletionProvider for ExpressionProvider {
    fn name(&self) -> &'static str {
        "expression"
    }

    fn is_applicable(&self, ctx: &SemanticContext) -> bool {
        matches!(
            &ctx.location,
            CursorLocation::Expression { .. }
                | CursorLocation::TypeAnnotation { .. }
                | CursorLocation::MethodArgument { .. }
                | CursorLocation::MemberAccess { .. }
                | CursorLocation::StaticAccess { .. }
        )
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        let trace_enabled = tracing::enabled!(tracing::Level::DEBUG);
        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            receiver_expr,
            member_prefix,
            ..
        } = &ctx.location
        {
            let t0 = Instant::now();
            let resolver = SymbolResolver::new(index);
            let owner = receiver_semantic_type
                .as_ref()
                .map(|t| Arc::from(t.erased_internal()))
                .or_else(|| receiver_type.clone())
                .or_else(|| resolver.resolve_type_name(ctx, receiver_expr));
            let Some(owner_internal) = owner else {
                return vec![];
            };
            let mut out = Vec::new();
            let member_prefix_lower =
                (!member_prefix.is_empty()).then(|| member_prefix.to_lowercase());
            for inner in index.direct_inner_classes_of(&owner_internal) {
                let Some(score) =
                    fuzzy_score_if_matches(member_prefix_lower.as_deref(), inner.name.as_ref())
                else {
                    continue;
                };
                out.push(
                    CompletionCandidate::new(
                        Arc::clone(&inner.name),
                        inner.name.to_string(),
                        CandidateKind::ClassName,
                        self.name(),
                    )
                    .with_detail(inner.source_name())
                    .with_score(88.0 + score),
                );
            }
            if trace_enabled {
                tracing::debug!(
                    provider = self.name(),
                    member_access = true,
                    receiver_expr,
                    owner_internal = %owner_internal,
                    candidates = out.len(),
                    elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0,
                    "completion provider timing"
                );
            }
            return out;
        }

        if let CursorLocation::StaticAccess {
            class_internal_name,
            member_prefix,
        } = &ctx.location
        {
            let t0 = Instant::now();
            let mut out = Vec::new();
            let member_prefix_lower =
                (!member_prefix.is_empty()).then(|| member_prefix.to_lowercase());
            for inner in index.direct_inner_classes_of(class_internal_name.as_ref()) {
                let Some(score) =
                    fuzzy_score_if_matches(member_prefix_lower.as_deref(), inner.name.as_ref())
                else {
                    continue;
                };
                out.push(
                    CompletionCandidate::new(
                        Arc::clone(&inner.name),
                        inner.name.to_string(),
                        CandidateKind::ClassName,
                        self.name(),
                    )
                    .with_detail(inner.source_name())
                    .with_score(88.0 + score),
                );
            }
            if trace_enabled {
                tracing::debug!(
                    provider = self.name(),
                    static_access = true,
                    owner_internal = %class_internal_name,
                    candidates = out.len(),
                    elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0,
                    "completion provider timing"
                );
            }
            return out;
        }

        let prefix = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.as_str(),
            CursorLocation::TypeAnnotation { prefix } => prefix.as_str(),
            CursorLocation::MethodArgument { prefix } => prefix.as_str(),
            _ => return vec![],
        };
        let t0 = Instant::now();
        let is_type_annotation = matches!(&ctx.location, CursorLocation::TypeAnnotation { .. });

        if prefix.contains('.') {
            let results = provide_qualified_type_prefix(prefix, ctx, index, self.name());
            if trace_enabled {
                tracing::debug!(
                    provider = self.name(),
                    prefix,
                    qualified = true,
                    candidates = results.len(),
                    elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0,
                    "completion provider timing"
                );
            }
            return results;
        }

        let prefix_lower = prefix.to_lowercase();

        // Package name of the current file (used to determine if it is in the same package)
        let current_pkg = ctx.enclosing_package.as_deref();

        let mut results = Vec::new();
        let mut seen_internals: std::collections::HashSet<Arc<str>> = Default::default();
        let t_imports = Instant::now();

        // Classes that have already been imported in current context
        let imported = index.resolve_imports(&ctx.existing_imports);
        for meta in &imported {
            if !is_type_visible_in_context(meta, ctx, index, is_type_annotation) {
                continue;
            }
            if !seen_internals.insert(Arc::clone(&meta.internal_name)) {
                continue;
            }
            let score = if prefix.is_empty() {
                0
            } else {
                match fuzzy::fuzzy_match(&prefix_lower, &meta.name.to_lowercase()) {
                    Some(s) => s,
                    None => continue,
                }
            };
            let fqn = source_fqn_of(meta, index);
            results.push(
                CompletionCandidate::new(
                    Arc::clone(&meta.name),
                    meta.name.to_string(),
                    CandidateKind::ClassName,
                    self.name(),
                )
                .with_detail(fqn)
                .with_score(80.0 + score as f32 * 0.1),
            );
        }
        let imports_ms = t_imports.elapsed().as_secs_f64() * 1000.0;

        let imported_internals: std::collections::HashSet<Arc<str>> = imported
            .iter()
            .map(|m| Arc::clone(&m.internal_name))
            .collect();

        let t_fuzzy = Instant::now();
        let fuzzy_pool = if prefix.is_empty() {
            Vec::new()
        } else {
            index.fuzzy_search_classes(prefix, 1024)
        };
        let fuzzy_ms = t_fuzzy.elapsed().as_secs_f64() * 1000.0;

        // Same package
        let t_same_pkg = Instant::now();
        if let Some(pkg) = current_pkg {
            let iter: Vec<_> = if prefix.is_empty() {
                index.classes_in_package(pkg)
            } else {
                fuzzy_pool
                    .iter()
                    .filter(|m| m.package.as_deref() == Some(pkg))
                    .cloned()
                    .collect()
            };
            for meta in iter {
                if !is_type_visible_in_context(&meta, ctx, index, is_type_annotation) {
                    continue;
                }
                if imported_internals.contains(&meta.internal_name) {
                    continue;
                }
                if !seen_internals.insert(Arc::clone(&meta.internal_name)) {
                    continue;
                }
                let score = if prefix.is_empty() {
                    0
                } else {
                    match fuzzy::fuzzy_match(&prefix_lower, &meta.name.to_lowercase()) {
                        Some(s) => s,
                        None => continue,
                    }
                };
                let fqn = source_fqn_of(&meta, index);
                results.push(
                    CompletionCandidate::new(
                        Arc::clone(&meta.name),
                        meta.name.to_string(),
                        CandidateKind::ClassName,
                        self.name(),
                    )
                    .with_detail(fqn)
                    .with_score(90.0 + score as f32 * 0.1),
                );
            }
        }
        let same_pkg_ms = t_same_pkg.elapsed().as_secs_f64() * 1000.0;

        // Other classes (global, require auto-import)
        let t_global = Instant::now();
        if !prefix.is_empty() {
            for meta in fuzzy_pool {
                if current_pkg.is_some_and(|pkg| meta.package.as_deref() == Some(pkg)) {
                    continue;
                }
                if !is_type_visible_in_context(&meta, ctx, index, is_type_annotation) {
                    continue;
                }
                if imported_internals.contains(&meta.internal_name) {
                    continue;
                }
                if !seen_internals.insert(Arc::clone(&meta.internal_name)) {
                    continue;
                }
                let score = match fuzzy::fuzzy_match(&prefix_lower, &meta.name.to_lowercase()) {
                    Some(s) => s,
                    None => continue,
                };
                let fqn = source_fqn_of(&meta, index);

                let boost = calculate_boost(meta.package.as_deref());
                let base_score = 40.0;

                let length_penalty = meta.name.len() as f32 * 0.05;

                let candidate = CompletionCandidate::new(
                    Arc::clone(&meta.name),
                    meta.name.to_string(),
                    CandidateKind::ClassName,
                    self.name(),
                )
                .with_detail(fqn.clone())
                .with_score(base_score + score as f32 * 0.1 - length_penalty + boost);

                let needs_import = is_import_needed(
                    &fqn,
                    &ctx.existing_imports,
                    ctx.enclosing_package.as_deref(),
                );
                let candidate = if needs_import {
                    candidate.with_import(fqn)
                } else {
                    candidate
                };

                results.push(candidate);
            }
        }
        let global_ms = t_global.elapsed().as_secs_f64() * 1000.0;

        if trace_enabled {
            tracing::debug!(
                provider = self.name(),
                prefix,
                qualified = false,
                candidates = results.len(),
                imports_ms,
                fuzzy_ms,
                same_pkg_ms,
                global_ms,
                elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0,
                "completion provider timing"
            );
        }
        results
    }
}

fn provide_qualified_type_prefix(
    prefix: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    source: &'static str,
) -> Vec<CompletionCandidate> {
    let mut out = Vec::new();
    let member_prefix = prefix.rsplit('.').next().unwrap_or("").trim();
    let member_prefix_lower = (!member_prefix.is_empty()).then(|| member_prefix.to_lowercase());
    for inner in qualified_nested_type_matches(prefix, ctx, index) {
        let Some(score) =
            fuzzy_score_if_matches(member_prefix_lower.as_deref(), inner.name.as_ref())
        else {
            continue;
        };
        out.push(
            CompletionCandidate::new(
                Arc::clone(&inner.name),
                inner.name.to_string(),
                CandidateKind::ClassName,
                source,
            )
            .with_detail(inner.source_name())
            .with_score(85.0 + score),
        );
    }
    out
}

fn fuzzy_score_if_matches(prefix_lower: Option<&str>, candidate_name: &str) -> Option<f32> {
    let Some(prefix) = prefix_lower else {
        return Some(0.0);
    };
    let score = fuzzy::fuzzy_match(prefix, &candidate_name.to_lowercase())?;
    Some(score as f32 * 0.1)
}

fn calculate_boost(pkg: Option<&str>) -> f32 {
    let pkg = match pkg {
        Some(p) => p,
        None => return 0.0,
    };

    let rules = [
        ("jdk/", -60.0),
        ("sun/", -60.0),
        ("com/sun/", -60.0),
        ("java/util/", 10.0),
    ];

    for (prefix, score) in rules {
        if pkg.starts_with(prefix) {
            return score;
        }
    }
    0.0
}

fn source_fqn_of(meta: &crate::index::ClassMetadata, index: &IndexView) -> String {
    crate::completion::import_utils::source_fqn_of_meta(meta, index)
}

fn is_type_visible_in_context(
    meta: &crate::index::ClassMetadata,
    ctx: &SemanticContext,
    index: &IndexView,
    is_type_annotation: bool,
) -> bool {
    if meta.inner_class_of.is_none() {
        return true;
    }
    if !is_type_annotation {
        return false;
    }
    let Some(enclosing) = ctx.enclosing_internal_name.as_deref() else {
        return false;
    };
    index
        .resolve_scoped_inner_class(enclosing, meta.name.as_ref())
        .is_some_and(|resolved| resolved.internal_name == meta.internal_name)
}

#[cfg(test)]
mod tests {
    use crate::index::WorkspaceIndex;
    use rust_asm::constants::ACC_PUBLIC;

    use super::*;
    use crate::index::{ClassMetadata, ClassOrigin, IndexScope, ModuleId};
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_cls(pkg: &str, name: &str) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).as_str()),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn make_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_cls("org/cubewhy", "Main"),
            make_cls("org/cubewhy", "Main2"),
            make_cls("java/util", "ArrayList"),
            make_cls("java/util", "HashMap"),
        ]);
        idx
    }

    fn ctx(
        prefix: &str,
        enclosing_class: &str,
        enclosing_pkg: &str,
        imports: Vec<Arc<str>>,
    ) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from(enclosing_class)),
            None, // enclosing_internal_name
            Some(Arc::from(enclosing_pkg)),
            imports,
        )
    }

    #[test]
    fn test_same_name_different_package_not_filtered() {
        // There is a Main package in another package, which should not be filtered.
        let index = make_index();
        index.add_classes(vec![make_cls("com/other", "Main")]);
        let ctx = ctx("Main", "Main", "org/cubewhy", vec![]);
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        // com/other/Main should appear (with auto-import)
        assert!(
            results.iter().any(|c| {
                c.label.as_ref() == "Main" && c.required_import.as_deref() == Some("com.other.Main")
            }),
            "should suggest Main from other package with import"
        );
    }

    #[test]
    fn test_self_class_appears_in_same_package() {
        // The class itself should appear in the completion (this can be used for type annotations, static access, etc.)
        let index = make_index();
        let ctx = ctx("Main", "Main", "org/cubewhy", vec![]);
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Main"),
            "enclosing class itself should appear as a completion candidate: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_same_name_different_package_both_appear() {
        let index = make_index();
        index.add_classes(vec![make_cls("com/other", "Main")]);
        let ctx = ctx("Main", "Main", "org/cubewhy", vec![]);
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        // Both Main (no import) and com/other/Main (requires import) in the same package should appear.
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Main"),
            "should suggest Main from same package"
        );
        assert!(
            results.iter().any(|c| {
                c.label.as_ref() == "Main" && c.required_import.as_deref() == Some("com.other.Main")
            }),
            "should also suggest Main from other package with import"
        );
    }

    #[test]
    fn test_nested_classes_are_filtered() {
        let index = WorkspaceIndex::new();
        let mut nested_cls = make_cls("java/util", "Entry");
        nested_cls.inner_class_of = Some(Arc::from("java/util/Map"));

        index.add_classes(vec![nested_cls, make_cls("java/util", "Map")]);

        let ctx = ctx("Map", "Test", "app", vec![]);
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));

        // Map 应该出现，但 Map$Entry 不应该出现
        assert!(results.iter().any(|c| c.label.as_ref() == "Map"));
        assert!(!results.iter().any(|c| c.label.as_ref() == "Entry"));
    }

    #[test]
    fn test_duplicate_simple_names_from_different_packages() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![
            make_cls("java/util", "List"),
            make_cls("java/awt", "List"),
        ]);

        let ctx = ctx("List", "Test", "app", vec![]);
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));

        // 验证两个 List 都存在
        let list_candidates: Vec<_> = results
            .iter()
            .filter(|c| c.label.as_ref() == "List")
            .collect();

        assert_eq!(
            list_candidates.len(),
            2,
            "Both java.util.List and java.awt.List should be present"
        );
    }

    #[test]
    fn test_type_annotation_includes_scoped_inner_box() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("org/cubewhy", "ClassWithGenerics"), {
            let mut c = make_cls("org/cubewhy", "Box");
            c.internal_name = Arc::from("org/cubewhy/ClassWithGenerics$Box");
            c.inner_class_of = Some(Arc::from("ClassWithGenerics"));
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::TypeAnnotation {
                prefix: "Bo".to_string(),
            },
            "Bo",
            vec![],
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Box"),
            "in-scope inner Box should be visible for TypeAnnotation"
        );
    }

    #[test]
    fn test_type_annotation_excludes_unrelated_inner_box() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![
            make_cls("org/cubewhy", "ClassWithGenerics"),
            make_cls("org/cubewhy", "Other"),
            {
                let mut c = make_cls("org/cubewhy", "Box");
                c.internal_name = Arc::from("org/cubewhy/Other$Box");
                c.inner_class_of = Some(Arc::from("Other"));
                c
            },
        ]);
        let ctx = SemanticContext::new(
            CursorLocation::TypeAnnotation {
                prefix: "Bo".to_string(),
            },
            "Bo",
            vec![],
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        assert!(
            !results.iter().any(|c| c.label.as_ref() == "Box"),
            "unrelated inner Box should remain hidden"
        );
    }

    #[test]
    fn test_qualified_expression_prefix_exposes_nested_types() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("org/cubewhy", "ChainCheck"), {
            let mut c = make_cls("org/cubewhy", "Box");
            c.internal_name = Arc::from("org/cubewhy/ChainCheck$Box");
            c.inner_class_of = Some(Arc::from("ChainCheck"));
            c
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "ChainCheck.".to_string(),
            },
            "ChainCheck.",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(Arc::new(
            crate::language::java::type_ctx::SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy")),
                vec![],
                Some(index.view(root_scope()).build_name_table()),
            ),
        ));
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Box"),
            "{results:?}"
        );
    }

    #[test]
    fn test_qualified_nested_prefix_exposes_nested_nested_types() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![
            make_cls("org/cubewhy", "ChainCheck"),
            {
                let mut c = make_cls("org/cubewhy", "Box");
                c.internal_name = Arc::from("org/cubewhy/ChainCheck$Box");
                c.inner_class_of = Some(Arc::from("ChainCheck"));
                c
            },
            {
                let mut c = make_cls("org/cubewhy", "BoxV");
                c.internal_name = Arc::from("org/cubewhy/ChainCheck$Box$BoxV");
                c.inner_class_of = Some(Arc::from("Box"));
                c
            },
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "ChainCheck.Box.".to_string(),
            },
            "ChainCheck.Box.",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(Arc::new(
            crate::language::java::type_ctx::SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy")),
                vec![],
                Some(index.view(root_scope()).build_name_table()),
            ),
        ));
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "BoxV"),
            "{results:?}"
        );
    }

    #[test]
    fn test_member_access_with_semantic_owner_exposes_nested_types() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("org/cubewhy", "ChainCheck"), {
            let mut c = make_cls("org/cubewhy", "Box");
            c.internal_name = Arc::from("org/cubewhy/ChainCheck$Box");
            c.inner_class_of = Some(Arc::from("ChainCheck"));
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(crate::semantic::types::type_name::TypeName::new(
                    "org/cubewhy/ChainCheck",
                )),
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "ChainCheck".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Box"),
            "{results:?}"
        );
    }

    #[test]
    fn test_static_access_exposes_nested_types_and_is_applicable() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("org/cubewhy", "ChainCheck"), {
            let mut c = make_cls("org/cubewhy", "Box");
            c.internal_name = Arc::from("org/cubewhy/ChainCheck$Box");
            c.inner_class_of = Some(Arc::from("ChainCheck"));
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("org/cubewhy/ChainCheck"),
                member_prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        assert!(ExpressionProvider.is_applicable(&ctx));
        let results = ExpressionProvider.provide(root_scope(), &ctx, &index.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Box"),
            "{results:?}"
        );
    }

    #[test]
    fn test_global_scan_fast_path_beats_full_scan_baseline() {
        let index = WorkspaceIndex::new();
        let mut classes = Vec::new();
        for i in 0..20_000 {
            classes.push(make_cls("bench/p", &format!("Class{i:05}")));
        }
        classes.push(make_cls("java/util", "ArrayList"));
        classes.push(make_cls("java/util", "AbstractList"));
        index.add_classes(classes);
        let view = index.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "Array".to_string(),
            },
            "Array",
            vec![],
            Some(Arc::from("Bench")),
            None,
            Some(Arc::from("bench/p")),
            vec![],
        );

        let t_slow = Instant::now();
        let mut slow = 0usize;
        for meta in view.classes_in_package("bench/p") {
            if fuzzy::fuzzy_match("array", &meta.name.to_lowercase()).is_some() {
                slow += 1;
            }
        }
        for meta in view.iter_all_classes() {
            if fuzzy::fuzzy_match("array", &meta.name.to_lowercase()).is_some() {
                slow += 1;
            }
        }
        let slow_ms = t_slow.elapsed().as_secs_f64() * 1000.0;

        let t_fast = Instant::now();
        let fast_candidates = ExpressionProvider.provide(root_scope(), &ctx, &view);
        let fast_ms = t_fast.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "expression_provider_perf_baseline: slow_scan_ms={slow_ms:.3} fast_provider_ms={fast_ms:.3} slow_hits={slow} fast_hits={}",
            fast_candidates.len()
        );
        assert!(fast_ms < slow_ms);
        assert!(
            fast_candidates
                .iter()
                .any(|c| c.label.as_ref().contains("ArrayList"))
        );
    }

    #[test]
    fn test_member_access_zero_result_fast_path_beats_full_scan_baseline() {
        let index = WorkspaceIndex::new();
        let mut classes = Vec::new();
        classes.push(make_cls("bench/p", "Owner"));
        for i in 0..30_000 {
            classes.push(make_cls("bench/p", &format!("Class{i:05}")));
        }
        for i in 0..5_000 {
            let mut inner = make_cls("bench/p", &format!("Inner{i:05}"));
            inner.internal_name = Arc::from(format!("bench/p/Other$Inner{i:05}"));
            inner.inner_class_of = Some(Arc::from("Other"));
            classes.push(inner);
        }
        index.add_classes(classes);
        let view = index.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(crate::semantic::types::type_name::TypeName::new(
                    "bench/p/Owner",
                )),
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "Owner".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("Owner")),
            Some(Arc::from("bench/p/Owner")),
            Some(Arc::from("bench/p")),
            vec![],
        );

        let t_slow = Instant::now();
        let mut slow = 0usize;
        for meta in view.iter_all_classes() {
            if meta.package.as_deref() == Some("bench/p")
                && meta.inner_class_of.as_deref() == Some("Owner")
            {
                slow += 1;
            }
        }
        let slow_ms = t_slow.elapsed().as_secs_f64() * 1000.0;

        let t_fast = Instant::now();
        let fast_candidates = ExpressionProvider.provide(root_scope(), &ctx, &view);
        let fast_ms = t_fast.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "expression_member_zero_result_perf: slow_scan_ms={slow_ms:.3} fast_provider_ms={fast_ms:.3} slow_hits={slow} fast_hits={}",
            fast_candidates.len()
        );

        assert!(fast_candidates.is_empty(), "{fast_candidates:?}");
        assert!(slow == 0);
        assert!(fast_ms < slow_ms);
    }
}
