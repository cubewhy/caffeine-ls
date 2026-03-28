use crate::{
    completion::{
        CandidateKind, CompletionCandidate,
        candidate::ReplacementMode,
        fuzzy,
        import_utils::is_import_needed,
        provider::{CompletionProvider, ProviderCompletionResult, ProviderSearchSpace},
    },
    index::{IndexScope, IndexView},
    language::java::completion::providers::type_lookup::{
        qualified_nested_type_matches, visible_direct_inner_classes,
    },
    semantic::context::{AccessReceiverKind, CursorLocation, SemanticContext},
    semantic::types::symbol_resolver::SymbolResolver,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

pub struct ExpressionProvider;

impl CompletionProvider for ExpressionProvider {
    fn name(&self) -> &'static str {
        "expression"
    }

    fn search_space(&self, _ctx: &SemanticContext) -> ProviderSearchSpace {
        ProviderSearchSpace::Broad
    }

    fn is_applicable(&self, ctx: &SemanticContext) -> bool {
        if let Some(module_context) = ctx.java_module_context {
            return matches!(
                module_context,
                crate::semantic::context::JavaModuleContextKind::UsesType
                    | crate::semantic::context::JavaModuleContextKind::ProvidesService
                    | crate::semantic::context::JavaModuleContextKind::ProvidesImplementation
            ) && matches!(&ctx.location, CursorLocation::TypeAnnotation { .. });
        }

        matches!(
            &ctx.location,
            CursorLocation::Expression { .. }
                | CursorLocation::TypeAnnotation { .. }
                | CursorLocation::MethodArgument { .. }
                | CursorLocation::MemberAccess { .. }
        )
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
        request: Option<&crate::lsp::request_context::RequestContext>,
        limit: Option<usize>,
    ) -> crate::lsp::request_cancellation::RequestResult<ProviderCompletionResult> {
        let trace_enabled = tracing::enabled!(tracing::Level::DEBUG);
        if limit == Some(0) {
            return Ok(ProviderCompletionResult {
                candidates: Vec::new(),
                is_incomplete: true,
            });
        }

        if let CursorLocation::MemberAccess {
            receiver_kind,
            receiver_semantic_type,
            receiver_type,
            receiver_expr,
            member_prefix,
            ..
        } = &ctx.location
        {
            let t0 = Instant::now();
            let owner = member_access_nested_type_owner(
                ctx,
                index,
                receiver_expr,
                receiver_kind,
                receiver_semantic_type,
                receiver_type,
            );
            let Some(owner_internal) = owner else {
                return Ok(ProviderCompletionResult {
                    candidates: vec![],
                    is_incomplete: false,
                });
            };

            let mut out = Vec::new();
            let member_prefix_lower =
                (!member_prefix.is_empty()).then(|| member_prefix.to_lowercase());
            for (i, inner) in visible_direct_inner_classes(&owner_internal, ctx, index)
                .into_iter()
                .enumerate()
            {
                maybe_check_cancelled(request, "completion.expression.member_access", i)?;
                let inner_name = inner.direct_name();
                let Some(score) =
                    fuzzy_score_if_matches(member_prefix_lower.as_deref(), inner_name)
                else {
                    continue;
                };
                out.push(
                    CompletionCandidate::new(
                        Arc::from(inner_name),
                        inner_name.to_string(),
                        CandidateKind::ClassName,
                        self.name(),
                    )
                    .with_replacement_mode(ReplacementMode::MemberSegment)
                    .with_filter_text(inner_name.to_string())
                    .with_detail(source_fqn_of(inner.as_ref(), index))
                    .with_score(88.0 + score),
                );
            }

            if trace_enabled {
                tracing::debug!(
                    provider = self.name(),
                    member_access = true,
                    limit = ?limit,
                    receiver_expr,
                    owner_internal = %owner_internal,
                    candidates = out.len(),
                    elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0,
                    "completion provider timing"
                );
            }
            return Ok(ProviderCompletionResult {
                candidates: out,
                is_incomplete: false,
            });
        }

        let prefix = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.as_str(),
            CursorLocation::TypeAnnotation { prefix } => prefix.as_str(),
            CursorLocation::MethodArgument { prefix } => prefix.as_str(),
            _ => {
                return Ok(ProviderCompletionResult {
                    candidates: vec![],
                    is_incomplete: false,
                });
            }
        };

        let t0 = Instant::now();

        if prefix.contains('.') {
            let results = provide_qualified_type_prefix(prefix, ctx, index, self.name(), request)?;
            if trace_enabled {
                tracing::debug!(
                    provider = self.name(),
                    prefix,
                    limit = ?limit,
                    qualified = true,
                    candidates = results.len(),
                    elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0,
                    "completion provider timing"
                );
            }
            return Ok(ProviderCompletionResult {
                candidates: results,
                is_incomplete: false,
            });
        }

        let prefix_lower = prefix.to_lowercase();
        let current_pkg = ctx.enclosing_package.as_deref();
        let mut seeds = Vec::new();
        let mut seen_internals: std::collections::HashSet<Arc<str>> = Default::default();
        let reached_limit = |len: usize, lim: Option<usize>| {
            lim.is_some_and(|effective_limit| len >= effective_limit)
        };
        let mut visibility_filter = VisibilityFilter::new(ctx, index);

        let t_imports = Instant::now();
        let imported = index.resolve_imports(&ctx.existing_imports);
        for (i, meta) in imported.iter().enumerate() {
            maybe_check_cancelled(request, "completion.expression.imported", i)?;
            if reached_limit(seeds.len(), limit) {
                break;
            }
            if !visibility_filter.allows(meta) {
                continue;
            }
            if !seen_internals.insert(Arc::clone(&meta.internal_name)) {
                continue;
            }
            let score = if prefix.is_empty() {
                0
            } else {
                match fuzzy::fuzzy_match(&prefix_lower, &meta.direct_name().to_lowercase()) {
                    Some(s) => s,
                    _ => continue,
                }
            };
            seeds.push(CandidateSeed {
                meta: Arc::clone(meta),
                score: 80.0 + score as f32 * 0.1,
                source_group: CandidateSourceGroup::Imported,
            });
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
            index.fuzzy_search_classes(prefix, limit.unwrap_or(1024))
        };
        let fuzzy_ms = t_fuzzy.elapsed().as_secs_f64() * 1000.0;

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

            for (i, meta) in iter.into_iter().enumerate() {
                maybe_check_cancelled(request, "completion.expression.same_package", i)?;
                if reached_limit(seeds.len(), limit) {
                    break;
                }
                if !visibility_filter.allows(&meta) {
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
                    match fuzzy::fuzzy_match(&prefix_lower, &meta.direct_name().to_lowercase()) {
                        Some(s) => s,
                        None => continue,
                    }
                };
                seeds.push(CandidateSeed {
                    meta,
                    score: 90.0 + score as f32 * 0.1,
                    source_group: CandidateSourceGroup::SamePackage,
                });
            }
        }
        let same_pkg_ms = t_same_pkg.elapsed().as_secs_f64() * 1000.0;

        let t_global = Instant::now();
        if !prefix.is_empty() {
            for (i, meta) in fuzzy_pool.into_iter().enumerate() {
                maybe_check_cancelled(request, "completion.expression.global", i)?;
                if reached_limit(seeds.len(), limit) {
                    break;
                }
                if current_pkg.is_some_and(|pkg| meta.package.as_deref() == Some(pkg)) {
                    continue;
                }
                if !visibility_filter.allows(&meta) {
                    continue;
                }
                if imported_internals.contains(&meta.internal_name) {
                    continue;
                }
                if !seen_internals.insert(Arc::clone(&meta.internal_name)) {
                    continue;
                }
                let score =
                    match fuzzy::fuzzy_match(&prefix_lower, &meta.direct_name().to_lowercase()) {
                        Some(s) => s,
                        None => continue,
                    };
                let boost = calculate_boost(meta.package.as_deref());
                let base_score = 40.0;
                let length_penalty = meta.direct_name().len() as f32 * 0.05;

                seeds.push(CandidateSeed {
                    meta,
                    score: base_score + score as f32 * 0.1 - length_penalty + boost,
                    source_group: CandidateSourceGroup::Global,
                });
            }
        }
        let global_ms = t_global.elapsed().as_secs_f64() * 1000.0;

        let t_decorate = Instant::now();
        let mut results = Vec::with_capacity(seeds.len());
        for (i, seed) in seeds.into_iter().enumerate() {
            maybe_check_cancelled(request, "completion.expression.decorate", i)?;
            let fqn = source_fqn_of(&seed.meta, index);
            let direct_name = seed.meta.direct_name();
            let mut candidate = CompletionCandidate::new(
                Arc::from(direct_name),
                direct_name.to_string(),
                CandidateKind::ClassName,
                self.name(),
            )
            .with_detail(fqn.clone())
            .with_score(seed.score);
            if matches!(seed.source_group, CandidateSourceGroup::Global) {
                let needs_import = is_import_needed(
                    &fqn,
                    &ctx.existing_imports,
                    ctx.enclosing_package.as_deref(),
                );
                if needs_import {
                    candidate = candidate.with_import(fqn);
                }
            }
            results.push(candidate);
        }
        let decoration_ms = t_decorate.elapsed().as_secs_f64() * 1000.0;

        let is_incomplete = reached_limit(results.len(), limit);
        if trace_enabled {
            tracing::debug!(
                provider = self.name(),
                prefix,
                limit = ?limit,
                qualified = false,
                candidates = results.len(),
                incomplete = is_incomplete,
                imports_ms,
                fuzzy_ms,
                visibility_ms = visibility_filter.visibility_ms(),
                visibility_checks = visibility_filter.total_checks,
                nested_visibility_checks = visibility_filter.nested_checks,
                same_pkg_ms,
                global_ms,
                decoration_ms,
                elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0,
                "completion provider timing"
            );
        }

        Ok(ProviderCompletionResult {
            candidates: results,
            is_incomplete,
        })
    }
}

#[derive(Clone)]
struct CandidateSeed {
    meta: Arc<crate::index::ClassMetadata>,
    score: f32,
    source_group: CandidateSourceGroup,
}

#[derive(Clone, Copy)]
enum CandidateSourceGroup {
    Imported,
    SamePackage,
    Global,
}

struct VisibilityFilter {
    visible_inner_by_simple_name: HashMap<Arc<str>, Arc<str>>,
    total_checks: usize,
    nested_checks: usize,
    visibility_ns: u128,
}

impl VisibilityFilter {
    fn new(ctx: &SemanticContext, index: &IndexView) -> Self {
        let mut visible_inner_by_simple_name = HashMap::new();

        // Always collect inner classes of the enclosing class, not just for type annotations
        if let Some(enclosing_internal) = ctx.enclosing_internal_name.as_deref() {
            for inner in index.direct_inner_classes_of(enclosing_internal) {
                visible_inner_by_simple_name
                    .entry(Arc::from(inner.direct_name()))
                    .or_insert_with(|| Arc::clone(&inner.internal_name));
            }
        }

        Self {
            visible_inner_by_simple_name,
            total_checks: 0,
            nested_checks: 0,
            visibility_ns: 0,
        }
    }

    fn allows(&mut self, meta: &crate::index::ClassMetadata) -> bool {
        let start = Instant::now();
        self.total_checks += 1;
        let visible = if meta.inner_class_of.is_none() {
            true // Top-level classes always visible
        } else {
            // Allow nested classes if they're in the visible_inner_by_simple_name map
            // This includes direct inner classes of the enclosing class
            self.nested_checks += 1;
            self.visible_inner_by_simple_name
                .get(meta.direct_name())
                .is_some_and(|internal| internal.as_ref() == meta.internal_name.as_ref())
        };
        self.visibility_ns += start.elapsed().as_nanos();
        visible
    }

    fn visibility_ms(&self) -> f64 {
        self.visibility_ns as f64 / 1_000_000.0
    }
}

fn provide_qualified_type_prefix(
    prefix: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    source: &'static str,
    request: Option<&crate::lsp::request_context::RequestContext>,
) -> crate::lsp::request_cancellation::RequestResult<Vec<CompletionCandidate>> {
    let mut out = Vec::new();
    let member_prefix = prefix.rsplit('.').next().unwrap_or("").trim();
    let member_prefix_lower = (!member_prefix.is_empty()).then(|| member_prefix.to_lowercase());
    for (i, inner) in qualified_nested_type_matches(prefix, ctx, index)
        .into_iter()
        .enumerate()
    {
        maybe_check_cancelled(request, "completion.expression.qualified", i)?;
        let inner_name = inner.direct_name();
        let Some(score) = fuzzy_score_if_matches(member_prefix_lower.as_deref(), inner_name) else {
            continue;
        };
        out.push(
            CompletionCandidate::new(
                Arc::from(inner_name),
                inner_name.to_string(),
                CandidateKind::ClassName,
                source,
            )
            .with_detail(source_fqn_of(inner.as_ref(), index))
            .with_score(85.0 + score),
        );
    }
    Ok(out)
}

fn member_access_nested_type_owner(
    ctx: &SemanticContext,
    index: &IndexView,
    receiver_expr: &str,
    receiver_kind: &AccessReceiverKind,
    receiver_semantic_type: &Option<crate::semantic::types::type_name::TypeName>,
    receiver_type: &Option<Arc<str>>,
) -> Option<Arc<str>> {
    if let AccessReceiverKind::Type {
        class_internal_name,
    } = receiver_kind
    {
        return Some(Arc::clone(class_internal_name));
    }

    if !is_type_qualifier_expr(ctx, index, receiver_expr) {
        return None;
    }

    receiver_semantic_type
        .as_ref()
        .map(|t| Arc::from(t.erased_internal()))
        .or_else(|| receiver_type.clone())
        .or_else(|| SymbolResolver::new(index).resolve_type_name(ctx, receiver_expr))
}

fn is_type_qualifier_expr(ctx: &SemanticContext, index: &IndexView, receiver_expr: &str) -> bool {
    let expr = receiver_expr.trim();
    if expr.is_empty() || matches!(expr, "this" | "super") {
        return false;
    }
    if !expr
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.')
    {
        return false;
    }

    let mut parts = expr.split('.').filter(|s| !s.is_empty());
    let Some(head) = parts.next() else {
        return false;
    };
    if ctx
        .local_variables
        .iter()
        .any(|lv| lv.name.as_ref() == head)
    {
        return false;
    }

    SymbolResolver::new(index)
        .resolve_type_name(ctx, expr)
        .is_some()
}

fn maybe_check_cancelled(
    request: Option<&crate::lsp::request_context::RequestContext>,
    phase: &'static str,
    index: usize,
) -> crate::lsp::request_cancellation::RequestResult<()> {
    if index % 64 == 0
        && let Some(request) = request
    {
        request.check_cancelled(phase)?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use crate::index::WorkspaceIndex;
    use rust_asm::constants::ACC_PUBLIC;

    use super::*;
    use crate::index::{ClassMetadata, ClassOrigin, IndexScope, ModuleId};
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use std::sync::Arc;
    use tracing_subscriber::{EnvFilter, fmt};

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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;

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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;

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
            c.inner_class_of = Some(Arc::from("org/cubewhy/ClassWithGenerics"));
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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
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
                c.inner_class_of = Some(Arc::from("org/cubewhy/Other"));
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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
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
            c.inner_class_of = Some(Arc::from("org/cubewhy/ChainCheck"));
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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
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
                c.inner_class_of = Some(Arc::from("org/cubewhy/ChainCheck"));
                c
            },
            {
                let mut c = make_cls("org/cubewhy", "BoxV");
                c.internal_name = Arc::from("org/cubewhy/ChainCheck$Box$BoxV");
                c.inner_class_of = Some(Arc::from("org/cubewhy/ChainCheck$Box"));
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
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "BoxV"),
            "{results:?}"
        );
    }

    #[test]
    fn test_member_access_with_type_qualifier_expr_exposes_nested_types() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("org/cubewhy", "ChainCheck"), {
            let mut c = make_cls("org/cubewhy", "Box");
            c.internal_name = Arc::from("org/cubewhy/ChainCheck$Box");
            c.inner_class_of = Some(Arc::from("org/cubewhy/ChainCheck"));
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
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
        )
        .with_extension(Arc::new(
            crate::language::java::type_ctx::SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy")),
                vec![],
                Some(index.view(root_scope()).build_name_table()),
            ),
        ));
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Box"),
            "{results:?}"
        );
    }

    #[test]
    fn test_member_access_hides_nested_types_for_this_receiver() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("app", "Test"), {
            let mut c = make_cls("app", "Nested");
            c.internal_name = Arc::from("app/Test$Nested");
            c.inner_class_of = Some(Arc::from("app/Test"));
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: Some(crate::semantic::types::type_name::TypeName::new(
                    "app/Test",
                )),
                receiver_type: None,
                member_prefix: "N".to_string(),
                receiver_expr: "this".to_string(),
                arguments: None,
            },
            "N",
            vec![],
            Some(Arc::from("Test")),
            Some(Arc::from("app/Test")),
            Some(Arc::from("app")),
            vec![],
        );
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        assert!(
            !results.iter().any(|c| c.label.as_ref() == "Nested"),
            "{results:?}"
        );
    }

    #[test]
    fn test_member_access_hides_nested_types_for_local_instance_receiver() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("app", "Test"), {
            let mut c = make_cls("app", "NestedNonStatic");
            c.internal_name = Arc::from("app/Test$NestedNonStatic");
            c.inner_class_of = Some(Arc::from("app/Test"));
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: Some(crate::semantic::types::type_name::TypeName::new(
                    "app/Test",
                )),
                receiver_type: None,
                member_prefix: "N".to_string(),
                receiver_expr: "t".to_string(),
                arguments: None,
            },
            "N",
            vec![crate::semantic::LocalVar {
                name: Arc::from("t"),
                type_internal: crate::semantic::types::type_name::TypeName::new("app/Test"),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            }],
            Some(Arc::from("Test")),
            Some(Arc::from("app/Test")),
            Some(Arc::from("app")),
            vec![],
        );
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        assert!(
            !results
                .iter()
                .any(|c| c.label.as_ref() == "NestedNonStatic"),
            "{results:?}"
        );
    }

    #[test]
    fn test_static_access_exposes_nested_types_and_is_applicable() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("org/cubewhy", "ChainCheck"), {
            let mut c = make_cls("org/cubewhy", "Box");
            c.internal_name = Arc::from("org/cubewhy/ChainCheck$Box");
            c.inner_class_of = Some(Arc::from("org/cubewhy/ChainCheck"));
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Type {
                    class_internal_name: Arc::from("org/cubewhy/ChainCheck"),
                },
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from("org/cubewhy/ChainCheck")),
                member_prefix: "".to_string(),
                receiver_expr: "org/cubewhy/ChainCheck".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        assert!(ExpressionProvider.is_applicable(&ctx));
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Box"),
            "{results:?}"
        );
    }

    #[test]
    fn test_static_access_uses_direct_name_for_raw_bytecode_nested_type() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("java/lang", "Integer"), {
            let mut c = make_cls("java/lang", "PublicCache");
            c.internal_name = Arc::from("java/lang/Integer$PublicCache");
            c.inner_class_of = Some(Arc::from("java/lang/Integer"));
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Type {
                    class_internal_name: Arc::from("java/lang/Integer"),
                },
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from("java/lang/Integer")),
                member_prefix: "".to_string(),
                receiver_expr: "java/lang/Integer".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("Test")),
            Some(Arc::from("app/Test")),
            Some(Arc::from("app")),
            vec![],
        );

        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"PublicCache"), "{labels:?}");
        assert!(!labels.contains(&"Integer$PublicCache"), "{labels:?}");
    }

    #[test]
    fn test_static_access_hides_non_public_nested_type_from_other_package() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("java/lang", "Integer"), {
            let mut c = make_cls("java/lang", "IntegerCache");
            c.internal_name = Arc::from("java/lang/Integer$IntegerCache");
            c.inner_class_of = Some(Arc::from("java/lang/Integer"));
            c.access_flags = 0;
            c
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Type {
                    class_internal_name: Arc::from("java/lang/Integer"),
                },
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from("java/lang/Integer")),
                member_prefix: "".to_string(),
                receiver_expr: "java/lang/Integer".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("Test")),
            Some(Arc::from("app/Test")),
            Some(Arc::from("app")),
            vec![],
        );

        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(!labels.contains(&"IntegerCache"), "{labels:?}");
        assert!(!labels.contains(&"Integer$IntegerCache"), "{labels:?}");
    }

    #[test]
    fn test_qualified_prefix_hides_non_public_nested_type_from_other_package() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![make_cls("java/lang", "Integer"), {
            let mut c = make_cls("java/lang", "Integer$IntegerCache");
            c.internal_name = Arc::from("java/lang/Integer$IntegerCache");
            c.inner_class_of = Some(Arc::from("java/lang/Integer"));
            c.access_flags = 0;
            c
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "Integer.".to_string(),
            },
            "Integer.",
            vec![],
            Some(Arc::from("Test")),
            Some(Arc::from("app/Test")),
            Some(Arc::from("app")),
            vec![Arc::from("java.lang.Integer")],
        )
        .with_extension(Arc::new(
            crate::language::java::type_ctx::SourceTypeCtx::new(
                Some(Arc::from("app")),
                vec![Arc::from("java.lang.Integer")],
                Some(index.view(root_scope()).build_name_table()),
            ),
        ));

        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(!labels.contains(&"IntegerCache"), "{labels:?}");
        assert!(!labels.contains(&"Integer$IntegerCache"), "{labels:?}");
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
            inner.inner_class_of = Some(Arc::from("org/cubewhy/Other"));
            classes.push(inner);
        }
        index.add_classes(classes);
        let view = index.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
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
        let fast_candidates = ExpressionProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates;
        let fast_ms = t_fast.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "expression_member_zero_result_perf: slow_scan_ms={slow_ms:.3} fast_provider_ms={fast_ms:.3} slow_hits={slow} fast_hits={}",
            fast_candidates.len()
        );

        assert!(fast_candidates.is_empty(), "{fast_candidates:?}");
        assert!(slow == 0);
        assert!(fast_ms < slow_ms);
    }

    #[test]
    fn test_expression_provider_remains_applicable_in_class_member_position() {
        let index = WorkspaceIndex::new();
        index.add_classes(vec![
            make_cls("bench/p", "ProcessBuilder"),
            make_cls("bench/p", "Process"),
        ]);
        let view = index.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "pro".to_string(),
            },
            "pro",
            vec![],
            Some(Arc::from("A")),
            Some(Arc::from("bench/p/A")),
            Some(Arc::from("bench/p")),
            vec![],
        )
        .with_class_member_position(true);

        assert!(ExpressionProvider.is_applicable(&ctx));
        let results = ExpressionProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates;
        assert!(
            !results.is_empty(),
            "provider should still produce valid expression candidates"
        );
    }

    #[test]
    fn test_broad_path_emits_timing_breakdown() {
        let _ = fmt()
            .with_env_filter(EnvFilter::new("debug"))
            .with_test_writer()
            .try_init();

        let index = WorkspaceIndex::new();
        let mut classes = Vec::new();
        for i in 0..10_000 {
            classes.push(make_cls("bench/p", &format!("Class{i:05}")));
        }
        for i in 0..2_000 {
            let mut inner = make_cls("bench/p", &format!("Inner{i:05}"));
            inner.internal_name = Arc::from(format!("bench/p/Outer$Inner{i:05}"));
            inner.inner_class_of = Some(Arc::from("bench/p/Outer"));
            classes.push(inner);
        }
        classes.push(make_cls("bench/p", "Owner"));
        classes.push(make_cls("java/util", "ArrayList"));
        classes.push(make_cls("java/util", "ArrayDeque"));
        index.add_classes(classes);
        let view = index.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "Array".to_string(),
            },
            "Array",
            vec![],
            Some(Arc::from("Owner")),
            Some(Arc::from("bench/p/Owner")),
            Some(Arc::from("bench/p")),
            vec![
                Arc::from("java.util.ArrayList"),
                Arc::from("java.util.ArrayDeque"),
            ],
        );

        let results = ExpressionProvider.provide_test(root_scope(), &ctx, &view, Some(256));
        assert!(
            !results.candidates.is_empty(),
            "broad path should still produce candidates"
        );
    }
}
