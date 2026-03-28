use crate::completion::CompletionCandidate;
use crate::completion::post_processor;
use crate::completion::provider::{CompletionProvider, ProviderSearchSpace};
use crate::index::{IndexScope, IndexView};
use crate::language::Language;
use crate::lsp::{request_cancellation::RequestResult, request_context::RequestContext};
use crate::semantic::{CursorLocation, SemanticContext};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct CompletionPolicy {
    pub broad_provider_limit: usize,
    pub final_result_limit: Option<usize>,
    pub short_prefix_len: usize,
}

impl Default for CompletionPolicy {
    fn default() -> Self {
        Self {
            broad_provider_limit: 256,
            final_result_limit: None,
            short_prefix_len: 1,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CompletionMetadata {
    pub broad_query: bool,
    pub used_broad_provider: bool,
    pub provider_truncated: bool,
    pub final_truncated: bool,
}

impl CompletionMetadata {
    pub fn is_incomplete(&self) -> bool {
        self.provider_truncated || self.final_truncated
    }
}

#[derive(Debug, Default)]
pub struct CompletionOutput {
    pub candidates: Vec<CompletionCandidate>,
    pub metadata: CompletionMetadata,
}

pub struct CompletionEngine {
    extra_providers: Vec<Box<dyn CompletionProvider>>,
}

impl CompletionEngine {
    pub fn new() -> Self {
        Self {
            extra_providers: Vec::new(),
        }
    }

    pub fn register_provider(&mut self, provider: Box<dyn CompletionProvider>) {
        self.extra_providers.push(provider);
    }

    pub fn complete(
        &self,
        scope: IndexScope,
        ctx: SemanticContext,
        lang: &dyn Language,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        self.complete_with_policy(scope, ctx, lang, index, CompletionPolicy::default())
            .candidates
    }

    pub fn complete_with_policy(
        &self,
        scope: IndexScope,
        mut ctx: SemanticContext,
        lang: &dyn Language,
        index: &IndexView,
        policy: CompletionPolicy,
    ) -> CompletionOutput {
        lang.enrich_completion_context(&mut ctx, scope, index);
        self.complete_prepared_with_policy(scope, ctx, lang, index, policy)
    }

    pub fn complete_prepared_with_policy(
        &self,
        scope: IndexScope,
        ctx: SemanticContext,
        lang: &dyn Language,
        index: &IndexView,
        policy: CompletionPolicy,
    ) -> CompletionOutput {
        self.complete_prepared_with_policy_and_request(scope, ctx, lang, index, policy, None)
            .expect("requestless completion cannot be cancelled")
    }

    pub fn complete_with_policy_and_request(
        &self,
        scope: IndexScope,
        mut ctx: SemanticContext,
        lang: &dyn Language,
        index: &IndexView,
        policy: CompletionPolicy,
        request: Option<&RequestContext>,
    ) -> RequestResult<CompletionOutput> {
        if let Some(request) = request {
            request.check_cancelled("completion.before_enrich")?;
        }
        lang.enrich_completion_context(&mut ctx, scope, index);
        if let Some(request) = request {
            request.check_cancelled("completion.after_enrich")?;
        }
        self.complete_prepared_with_policy_and_request(scope, ctx, lang, index, policy, request)
    }

    pub fn complete_prepared_with_policy_and_request(
        &self,
        scope: IndexScope,
        ctx: SemanticContext,
        lang: &dyn Language,
        index: &IndexView,
        policy: CompletionPolicy,
        request: Option<&RequestContext>,
    ) -> RequestResult<CompletionOutput> {
        let ctx = ctx;

        let t_total = Instant::now();
        let broad_query = is_broad_query(&ctx, policy.short_prefix_len);
        if let Some(request) = request {
            request.check_cancelled("completion.before_provider_discovery")?;
        }

        // Phase 4: Parallel Provider Execution
        // Execute all applicable providers in parallel using rayon
        use rayon::prelude::*;

        let lang_providers: Vec<_> = lang
            .completion_providers()
            .iter()
            .filter(|p| p.is_applicable(&ctx))
            .collect();

        let extra_providers: Vec<_> = self
            .extra_providers
            .iter()
            .filter(|p| p.is_applicable(&ctx))
            .collect();
        if let Some(request) = request {
            request.check_cancelled("completion.before_provider_fanout")?;
        }

        // Execute language providers in parallel
        let lang_results: RequestResult<Vec<_>> = lang_providers
            .par_iter()
            .map(|provider| {
                let tp = Instant::now();
                let use_limit =
                    broad_query && provider.search_space(&ctx) == ProviderSearchSpace::Broad;
                let limit = use_limit.then_some(policy.broad_provider_limit);

                let result = provider.provide(scope, &ctx, index, request, limit)?;

                tracing::debug!(
                    provider = provider.name(),
                    candidates = result.candidates.len(),
                    limited = use_limit,
                    elapsed_ms = tp.elapsed().as_secs_f64() * 1000.0,
                    "completion provider dispatch (parallel)"
                );

                Ok((result, use_limit))
            })
            .collect();
        let lang_results = lang_results?;

        // Execute extra providers in parallel
        let extra_results: RequestResult<Vec<_>> = extra_providers
            .par_iter()
            .map(|provider| {
                let tp = Instant::now();
                let use_limit =
                    broad_query && provider.search_space(&ctx) == ProviderSearchSpace::Broad;
                let limit = use_limit.then_some(policy.broad_provider_limit);

                let result = provider.provide(scope, &ctx, index, request, limit)?;

                tracing::debug!(
                    provider = provider.name(),
                    candidates = result.candidates.len(),
                    limited = use_limit,
                    elapsed_ms = tp.elapsed().as_secs_f64() * 1000.0,
                    "completion extra provider dispatch (parallel)"
                );

                Ok((result, use_limit))
            })
            .collect();
        let extra_results = extra_results?;

        // Collect results
        let mut candidates: Vec<CompletionCandidate> = Vec::new();
        let mut metadata = CompletionMetadata {
            broad_query,
            ..CompletionMetadata::default()
        };

        for (index, (result, use_limit)) in lang_results
            .into_iter()
            .chain(extra_results.into_iter())
            .enumerate()
        {
            if index % 8 == 0
                && let Some(request) = request
            {
                request.check_cancelled("completion.after_provider_result")?;
            }
            if use_limit {
                metadata.used_broad_provider = true;
            }
            metadata.provider_truncated |= result.is_incomplete;
            candidates.extend(result.candidates);
        }

        tracing::debug!(
            elapsed_ms = t_total.elapsed().as_secs_f64() * 1000.0,
            candidates = candidates.len(),
            "completion provider stage total"
        );
        if let Some(request) = request {
            request.check_cancelled("completion.before_post_process")?;
        }
        let mut candidates = post_processor::process(candidates, &ctx.query, &ctx, index, request)?;
        if let Some(limit) = policy.final_result_limit
            && candidates.len() > limit
        {
            candidates.truncate(limit);
            metadata.final_truncated = true;
        }

        Ok(CompletionOutput {
            candidates,
            metadata,
        })
    }
}

fn is_broad_query(ctx: &SemanticContext, short_prefix_len: usize) -> bool {
    match &ctx.location {
        CursorLocation::Expression { prefix }
        | CursorLocation::TypeAnnotation { prefix }
        | CursorLocation::MethodArgument { prefix } => {
            !prefix.contains('.') && prefix.len() <= short_prefix_len
        }
        CursorLocation::ConstructorCall { class_prefix, .. } => {
            !class_prefix.contains('.') && class_prefix.len() <= short_prefix_len
        }
        CursorLocation::MemberAccess { .. }
        | CursorLocation::Import { .. }
        | CursorLocation::ImportStatic { .. }
        | CursorLocation::MethodReference { .. }
        | CursorLocation::StatementLabel { .. }
        | CursorLocation::VariableName { .. }
        | CursorLocation::Annotation { .. }
        | CursorLocation::AnnotationParam { .. }
        | CursorLocation::StringLiteral { .. }
        | CursorLocation::Unknown => false,
    }
}

impl Default for CompletionEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion::parser::parse_chain_from_expr,
        completion::provider::{ProviderCompletionResult, ProviderSearchSpace},
        index::{
            ClassMetadata, ClassOrigin, IndexView, MethodParams, MethodSummary, ModuleId,
            WorkspaceIndex,
        },
        language::{
            JavaLanguage, java::completion_context::ContextEnricher, java::type_ctx::SourceTypeCtx,
        },
        semantic::types::{TypeResolver, type_name::TypeName},
        semantic::{CursorLocation, LocalVar, SemanticContext},
    };
    use rust_asm::constants::ACC_PUBLIC;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tree_sitter::Parser;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn with_type_ctx(ctx: SemanticContext, view: &IndexView) -> SemanticContext {
        let type_ctx = Arc::new(SourceTypeCtx::new(
            ctx.enclosing_package.clone(),
            ctx.existing_imports.clone(),
            Some(view.build_name_table()),
        ));
        ctx.with_extension(type_ctx)
    }

    fn candidate_name(candidate: &CompletionCandidate) -> &str {
        candidate
            .insertion
            .filter_text
            .as_deref()
            .unwrap_or(candidate.label.as_ref())
    }

    fn seg_names(expr: &str) -> Vec<(String, Option<usize>)> {
        parse_chain_from_expr(expr)
            .into_iter()
            .map(|s| (s.name, s.arg_count))
            .collect()
    }

    #[test]
    fn test_chain_simple_variable() {
        assert_eq!(
            seg_names("list.ge"),
            vec![("list".into(), None), ("ge".into(), None)]
        );
    }

    #[test]
    fn test_chain_method_call() {
        assert_eq!(
            seg_names("list.stream().fi"),
            vec![
                ("list".into(), None),
                ("stream".into(), Some(0)),
                ("fi".into(), None)
            ]
        );
    }

    #[test]
    fn test_chain_multiple_methods() {
        assert_eq!(
            seg_names("a.b().c(x, y).d"),
            vec![
                ("a".into(), None),
                ("b".into(), Some(0)),
                ("c".into(), Some(2)),
                ("d".into(), None)
            ]
        );
    }

    #[test]
    fn test_chain_no_dot() {
        assert_eq!(seg_names("someVar"), vec![("someVar".into(), None)]);
    }

    #[test]
    fn test_chain_nested_parens() {
        assert_eq!(
            seg_names("list.get(map.size()).toStr"),
            vec![
                ("list".into(), None),
                ("get".into(), Some(1)),
                ("toStr".into(), None)
            ]
        );
    }

    struct ProbeProvider {
        calls: Arc<AtomicUsize>,
    }

    impl CompletionProvider for ProbeProvider {
        fn name(&self) -> &'static str {
            "probe"
        }

        fn is_applicable(&self, _ctx: &SemanticContext) -> bool {
            false
        }

        fn provide(
            &self,
            _scope: IndexScope,
            _ctx: &SemanticContext,
            _index: &IndexView,
            _request: Option<&crate::lsp::request_context::RequestContext>,
            _limit: Option<usize>,
        ) -> RequestResult<ProviderCompletionResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ProviderCompletionResult::default())
        }
    }

    struct BroadMockProvider {
        count: usize,
    }

    impl CompletionProvider for BroadMockProvider {
        fn name(&self) -> &'static str {
            "broad-mock"
        }

        fn search_space(&self, _ctx: &SemanticContext) -> ProviderSearchSpace {
            ProviderSearchSpace::Broad
        }

        fn provide(
            &self,
            _scope: IndexScope,
            _ctx: &SemanticContext,
            _index: &IndexView,
            _request: Option<&crate::lsp::request_context::RequestContext>,
            limit: Option<usize>,
        ) -> RequestResult<ProviderCompletionResult> {
            let all = (0..self.count)
                .map(|i| {
                    CompletionCandidate::new(
                        Arc::from(format!("Item{i}")),
                        format!("Item{i}"),
                        crate::completion::CandidateKind::ClassName,
                        self.name(),
                    )
                })
                .collect::<Vec<_>>();
            let Some(limit) = limit else {
                return Ok(ProviderCompletionResult {
                    candidates: all,
                    is_incomplete: false,
                });
            };
            let is_incomplete = all.len() > limit;
            Ok(ProviderCompletionResult {
                candidates: all.into_iter().take(limit).collect(),
                is_incomplete,
            })
        }
    }

    struct NarrowMockProvider {
        count: usize,
    }

    impl CompletionProvider for NarrowMockProvider {
        fn name(&self) -> &'static str {
            "narrow-mock"
        }

        fn search_space(&self, _ctx: &SemanticContext) -> ProviderSearchSpace {
            ProviderSearchSpace::Narrow
        }

        fn provide(
            &self,
            _scope: IndexScope,
            _ctx: &SemanticContext,
            _index: &IndexView,
            _request: Option<&crate::lsp::request_context::RequestContext>,
            _limit: Option<usize>,
        ) -> RequestResult<ProviderCompletionResult> {
            Ok((0..self.count)
                .map(|i| {
                    CompletionCandidate::new(
                        Arc::from(format!("N{i}")),
                        format!("N{i}"),
                        crate::completion::CandidateKind::ClassName,
                        self.name(),
                    )
                })
                .collect::<Vec<_>>()
                .into())
        }
    }

    #[derive(Debug)]
    struct DummyLanguage;

    impl crate::language::Language for DummyLanguage {
        fn id(&self) -> &'static str {
            "dummy"
        }

        fn supports(&self, language_id: &str) -> bool {
            language_id == "dummy"
        }

        fn make_parser(&self) -> Parser {
            crate::language::java::make_java_parser()
        }
    }

    #[test]
    fn test_engine_skips_provider_when_not_applicable() {
        let idx = make_index_with_random_class();
        let view = idx.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "Ran".to_string(),
            },
            "Ran",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        )));

        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = CompletionEngine::new();
        engine.register_provider(Box::new(ProbeProvider {
            calls: Arc::clone(&calls),
        }));
        let _ = engine.complete(root_scope(), ctx, &JavaLanguage, &view);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_small_narrow_results_stay_complete() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from("org/cubewhy/Owner")),
                member_prefix: "m".to_string(),
                receiver_expr: "owner".to_string(),
                arguments: None,
            },
            "m",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        let mut engine = CompletionEngine::new();
        engine.register_provider(Box::new(NarrowMockProvider { count: 3 }));
        let out = engine.complete_with_policy(
            root_scope(),
            ctx,
            &DummyLanguage,
            &view,
            CompletionPolicy {
                broad_provider_limit: 2,
                final_result_limit: Some(10),
                short_prefix_len: 1,
            },
        );
        assert_eq!(out.candidates.len(), 3);
        assert!(!out.metadata.is_incomplete());
    }

    #[test]
    fn test_broad_truncated_results_mark_incomplete() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        let mut engine = CompletionEngine::new();
        engine.register_provider(Box::new(BroadMockProvider { count: 12 }));
        let out = engine.complete_with_policy(
            root_scope(),
            ctx,
            &DummyLanguage,
            &view,
            CompletionPolicy {
                broad_provider_limit: 5,
                final_result_limit: Some(8),
                short_prefix_len: 1,
            },
        );
        assert_eq!(out.candidates.len(), 5);
        assert!(out.metadata.provider_truncated);
        assert!(out.metadata.is_incomplete());
    }

    #[test]
    fn test_threshold_controls_final_truncation() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "abc".to_string(),
            },
            "abc",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        let mut engine = CompletionEngine::new();
        engine.register_provider(Box::new(NarrowMockProvider { count: 6 }));
        let out = engine.complete_with_policy(
            root_scope(),
            ctx,
            &DummyLanguage,
            &view,
            CompletionPolicy {
                broad_provider_limit: 10,
                final_result_limit: Some(4),
                short_prefix_len: 1,
            },
        );
        assert_eq!(out.candidates.len(), 4);
        assert!(out.metadata.final_truncated);
        assert!(out.metadata.is_incomplete());
    }

    fn make_index_with_random_class() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy")),
            name: Arc::from("RandomClass"),
            internal_name: Arc::from("org/cubewhy/RandomClass"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("f"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: None,
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: None,
            origin: ClassOrigin::Unknown,
        }]);
        idx
    }

    #[test]
    fn test_enrich_context_resolves_simple_name_via_import() {
        let idx = make_index_with_random_class();
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "f".to_string(),
                receiver_expr: "cl".to_string(),

                arguments: None,
            },
            "f",
            vec![LocalVar {
                name: Arc::from("cl"),
                type_internal: TypeName::new("RandomClass"),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.RandomClass".into()],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(
                receiver_type.as_deref(),
                Some("org/cubewhy/RandomClass"),
                "receiver_type should be fully qualified after enrich"
            );
        }
    }

    #[test]
    fn test_enrich_context_resolves_simple_name_via_wildcard_import() {
        let idx = make_index_with_random_class();
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "cl".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("cl"),
                type_internal: TypeName::new("RandomClass"),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.*".into()],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(receiver_type.as_deref(), Some("org/cubewhy/RandomClass"),);
        }
    }

    #[test]
    fn test_complete_returns_f_method() {
        let idx = make_index_with_random_class();
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "f".to_string(),
                receiver_expr: "cl".to_string(),
                arguments: None,
            },
            "f",
            vec![LocalVar {
                name: Arc::from("cl"),
                type_internal: TypeName::new("RandomClass"),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.RandomClass".into()],
        );
        let view = idx.view(root_scope());
        let ctx = with_type_ctx(ctx, &view);
        let engine = CompletionEngine::new();
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &view);
        assert!(
            results.iter().any(|c| candidate_name(c) == "f"),
            "should find method f(): {:?}",
            results
                .iter()
                .map(|c| candidate_name(c))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_static_qualified_member_access_dispatch_runs_expression_and_skips_package() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("ChainCheck"),
                internal_name: Arc::from("org/cubewhy/ChainCheck"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck")),
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

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
            Some(Arc::from("Probe")),
            Some(Arc::from("org/cubewhy/Probe")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );
        let view = idx.view(root_scope());
        let engine = CompletionEngine::new();
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &view);

        assert!(
            results
                .iter()
                .any(|c| candidate_name(c) == "Box" && c.source == "expression"),
            "expected nested type from expression provider, got: {:?}",
            results
                .iter()
                .map(|c| format!("{}@{}", candidate_name(c), c.source))
                .collect::<Vec<_>>()
        );
        assert!(
            results.iter().all(|c| c.source != "package"),
            "package provider should be gated off, got: {:?}",
            results
                .iter()
                .map(|c| format!("{}@{}", candidate_name(c), c.source))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_chain_field_access_resolved() {
        use crate::index::{ClassMetadata, ClassOrigin, FieldSummary};
        use rust_asm::constants::{ACC_PUBLIC, ACC_STATIC};
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("System"),
                internal_name: Arc::from("java/lang/System"),
                super_name: None,
                interfaces: vec![],
                methods: vec![],
                annotations: vec![],
                fields: vec![FieldSummary {
                    name: Arc::from("out"),
                    descriptor: Arc::from("Ljava/io/PrintStream;"),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC | ACC_STATIC,
                    is_synthetic: false,
                    generic_signature: None,
                }],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/io")),
                name: Arc::from("PrintStream"),
                internal_name: Arc::from("java/io/PrintStream"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        // 模拟用户输入了 System.out.|
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "System.out".to_string(),
                arguments: None,
            },
            "",
            vec![],
            None,
            None,
            None,
            vec!["java.lang.System".into()], // 确保 System 能够被解析
        );

        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);

        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(
                receiver_type.as_deref(),
                Some("java/io/PrintStream"),
                "System.out 应该被正确链式推导为 java/io/PrintStream"
            );
        } else {
            panic!("Location changed unexpectedly");
        }
    }

    #[test]
    fn test_expected_type_ranks_first_in_constructor_completion() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;
        let idx = WorkspaceIndex::new();
        for (pkg, name) in [
            ("org/cubewhy/a", "Main"),
            ("org/cubewhy/a", "Main2"),
            ("org/cubewhy", "RandomClass"),
        ] {
            idx.add_classes(vec![ClassMetadata {
                package: Some(Arc::from(pkg)),
                name: Arc::from(name),
                internal_name: Arc::from(format!("{}/{}", pkg, name)),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("<init>"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: None,
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            }]);
        }
        let engine = CompletionEngine::new();
        let ctx = SemanticContext::new(
            CursorLocation::ConstructorCall {
                class_prefix: String::new(),
                expected_type: Some("RandomClass".to_string()),
                qualifier_expr: None,
                qualifier_owner_internal: None,
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.RandomClass".into()],
        );
        let view = idx.view(root_scope());
        let ctx = with_type_ctx(ctx, &view);
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &view);
        assert!(!results.is_empty(), "should have candidates");
        assert_eq!(
            candidate_name(&results[0]),
            "RandomClass",
            "RandomClass should rank first when it matches expected_type, got: {:?}",
            results
                .iter()
                .map(|c| candidate_name(c))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_var_method_return_type_resolved() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("NestedClass"),
            internal_name: Arc::from("NestedClass"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("randomFunction"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: Some(Arc::from("LNestedClass;")),
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "str".to_string(),
                arguments: None,
            },
            "",
            vec![
                LocalVar {
                    name: Arc::from("nc"),
                    type_internal: TypeName::new("NestedClass"),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("str"),
                    type_internal: TypeName::unknown(),
                    decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                    init_expr: Some("nc.randomFunction()".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        let str_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "str")
            .unwrap();
        assert_eq!(str_var.type_internal.erased_internal(), "NestedClass");
    }

    #[test]
    fn test_var_overload_resolved_by_long_arg() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("NestedClass"),
            internal_name: Arc::from("NestedClass"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                MethodSummary {
                    name: Arc::from("randomFunction"),
                    params: MethodParams::from_method_descriptor(
                        "(Ljava/lang/String;I)LRandomClass;",
                    ),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("LRandomClass;")),
                },
                MethodSummary {
                    name: Arc::from("randomFunction"),
                    params: MethodParams::from_method_descriptor("(Ljava/lang/String;J)LMain2;"),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("LMain2;")),
                },
            ],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "str".to_string(),
                arguments: None,
            },
            "",
            vec![
                LocalVar {
                    name: Arc::from("nc"),
                    type_internal: TypeName::new("NestedClass"),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("str"),
                    type_internal: TypeName::unknown(),
                    decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                    init_expr: Some("nc.randomFunction(\"a\", 1l)".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        let str_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "str")
            .unwrap();
        assert_eq!(str_var.type_internal.erased_internal(), "Main2");
    }

    #[test]
    fn test_var_bare_method_call_resolved() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("Main"),
            internal_name: Arc::from("Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("getString"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: Some(Arc::from("Ljava/lang/String;")),
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "str".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("str"),
                type_internal: TypeName::unknown(),
                decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                init_expr: Some("getString()".to_string()),
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        let str_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "str")
            .unwrap();
        assert_eq!(str_var.type_internal.erased_internal(), "java/lang/String");
    }

    #[test]
    fn test_resolve_method_return_walks_mro() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Parent"),
                internal_name: Arc::from("Parent"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("getValue"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("Ljava/lang/String;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: None,
                name: Arc::from("Child"),
                internal_name: Arc::from("Child"),
                super_name: Some("Parent".into()),
                annotations: vec![],
                interfaces: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(root_scope());
        let resolver = TypeResolver::new(&view);
        let result = resolver.resolve_method_return("Child", "getValue", 0, &[]);
        assert_eq!(
            result.as_ref().map(|t| t.erased_internal()),
            Some("java/lang/String")
        );
    }

    #[test]
    fn test_complete_member_after_bare_method_call() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Main"),
                internal_name: Arc::from("Main"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("getMain2"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("LMain2;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: None,
                name: Arc::from("Main2"),
                internal_name: Arc::from("Main2"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("func"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: None,
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        let engine = CompletionEngine::new();
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "getMain2()".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let ctx = with_type_ctx(ctx, &view);
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &view);
        assert!(results.iter().any(|c| candidate_name(c) == "func"));
    }

    #[test]
    fn test_var_array_element_type_resolved() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/lang")),
            name: Arc::from("String"),
            internal_name: Arc::from("java/lang/String"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "a".to_string(),
                arguments: None,
            },
            "",
            vec![
                LocalVar {
                    name: Arc::from("args"),
                    type_internal: TypeName::new("String[]"),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::unknown(),
                    decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                    init_expr: Some("args[0]".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        let a_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "a")
            .unwrap();
        assert_eq!(a_var.type_internal.erased_internal(), "java/lang/String");
    }

    #[test]
    fn test_var_primitive_array_element_not_resolved() {
        let idx = WorkspaceIndex::new();
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "x".to_string(),
                arguments: None,
            },
            "",
            vec![
                LocalVar {
                    name: Arc::from("nums"),
                    type_internal: TypeName::new("int[]"),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("x"),
                    type_internal: TypeName::unknown(),
                    decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                    init_expr: Some("nums[0]".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        let x_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "x")
            .unwrap();
        assert_ne!(x_var.type_internal.erased_internal_with_arrays(), "int[]");
    }

    #[test]
    fn test_enrich_context_array_access_receiver() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/lang")),
            name: Arc::from("String"),
            internal_name: Arc::from("java/lang/String"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("length"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: Some(Arc::from("I")),
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "b[0]".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("b"),
                type_internal: TypeName::new("String[]"),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(receiver_type.as_deref(), Some("java/lang/String"));
        }
    }

    #[test]
    fn test_package_path_becomes_import_location() {
        use crate::index::{ClassMetadata, ClassOrigin, WorkspaceIndex};
        use rust_asm::constants::ACC_PUBLIC;
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/util")),
            name: Arc::from("ArrayList"),
            internal_name: Arc::from("java/util/ArrayList"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "ArrayL".to_string(),
                receiver_expr: "java.util".to_string(),
                arguments: None,
            },
            "ArrayL",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        assert!(matches!(
            &ctx.location,
            CursorLocation::Import { prefix } if prefix == "java.util.ArrayL"
        ));
    }

    #[test]
    fn test_unknown_receiver_stays_member_access() {
        let idx = WorkspaceIndex::new();
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "foo".to_string(),
                receiver_expr: "unknownPkg".to_string(),
                arguments: None,
            },
            "foo",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        assert!(matches!(&ctx.location, CursorLocation::MemberAccess { .. }));
    }

    #[test]
    fn test_var_array_initializer_and_access() {
        use crate::index::{ClassMetadata, ClassOrigin};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/lang")),
            name: Arc::from("String"),
            internal_name: Arc::from("java/lang/String"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "strItem".to_string(), // 触发位置
                arguments: None,
            },
            "",
            vec![
                // var arr = new char[]{};
                LocalVar {
                    name: Arc::from("arr"),
                    type_internal: TypeName::new("char[]"),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                },
                // var c = arr[0];
                LocalVar {
                    name: Arc::from("c"),
                    type_internal: TypeName::unknown(),
                    decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                    init_expr: Some("arr[0]".to_string()),
                },
                // var strArr = new String[]{"[1]", "[2]"}; (标准对象数组，带干扰符号)
                LocalVar {
                    name: Arc::from("strArr"),
                    type_internal: TypeName::unknown(),
                    decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                    init_expr: Some("new String[]{\"[1]\", \"[2]\"}".to_string()),
                },
                // var strItem = strArr[1];
                LocalVar {
                    name: Arc::from("strItem"),
                    type_internal: TypeName::unknown(),
                    decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                    init_expr: Some("strArr[1]".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);

        // 校验 c (arr[0]) 被推断为 char
        let c_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "c")
            .unwrap();
        assert_eq!(c_var.type_internal.erased_internal(), "char");

        // 校验 strArr (new String[]...) 被推断为 java/lang/String[]
        let str_arr_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "strArr")
            .unwrap();
        assert_eq!(
            str_arr_var.type_internal.erased_internal_with_arrays(),
            "java/lang/String[]"
        );

        // 校验 strItem (strArr[1]) 被推断为 java/lang/String
        let str_item_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "strItem")
            .unwrap();
        assert_eq!(
            str_item_var.type_internal.erased_internal(),
            "java/lang/String"
        );
    }

    #[test]
    fn test_var_chained_array_access_from_method() {
        // 验证 getArr()[0] 这种通过方法调用拿到数组再取下标的情况
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("Main"),
            internal_name: Arc::from("Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("getArr"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: Some(Arc::from("[Ljava/lang/String;")),
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "item".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("item"),
                type_internal: TypeName::unknown(),
                decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                init_expr: Some("getArr()[0]".to_string()),
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );
        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);
        let item = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "item")
            .unwrap();
        assert_eq!(item.type_internal.erased_internal(), "java/lang/String");
    }

    #[test]
    fn test_enrich_context_resolves_var_receiver_first() {
        use crate::index::{ClassMetadata, ClassOrigin};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/Main"),
            super_name: None,
            annotations: vec![],
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "m".to_string(), // m.a
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("m"),
                type_internal: TypeName::unknown(),
                decl_kind: crate::semantic::LocalVarDeclKind::VarSyntax,
                init_expr: Some("new Main()".to_string()),
            }],
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );

        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);

        // 如果 var 优先被解析，这里就能推导出 receiver_expr 是 org/cubewhy/Main
        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(receiver_type.as_deref(), Some("org/cubewhy/Main"));
        } else {
            panic!("Expected MemberAccess");
        }
    }

    #[test]
    fn test_resolve_multi_dimensional_array_access() {
        use crate::index::{ClassMetadata, ClassOrigin};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/lang")),
            name: Arc::from("String"),
            internal_name: Arc::from("java/lang/String"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "res[0][0]".to_string(), // 多维数组访问
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("res"),
                type_internal: TypeName::new("java/lang/String[][]"),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        );

        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);

        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(
                receiver_type.as_deref(),
                Some("java/lang/String"),
                "res[0][0] should drop two dimensions"
            );
        } else {
            panic!("Expected MemberAccess");
        }
    }

    #[test]
    fn test_resolve_multi_dimensional_field_access() {
        use crate::index::{ClassMetadata, ClassOrigin, FieldSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Main"),
                internal_name: Arc::from("org/cubewhy/Main"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![FieldSummary {
                    name: Arc::from("arr"),
                    // 这里模拟一个 4维数组 String[][][][]
                    descriptor: Arc::from("[[[[Ljava/lang/String;"),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                }],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: None,
                annotations: vec![],
                interfaces: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "m.arr[0][0][0][0]".to_string(), // 4层访问
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("m"),
                type_internal: TypeName::new("org/cubewhy/Main"),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        );

        let view = idx.view(root_scope());
        let mut ctx = with_type_ctx(ctx, &view);
        ContextEnricher::new(&view).enrich(&mut ctx);

        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(
                receiver_type.as_deref(),
                Some("java/lang/String"),
                "m.arr[0][0][0][0] should drop four dimensions successfully"
            );
        } else {
            panic!("Expected MemberAccess");
        }
    }
}
