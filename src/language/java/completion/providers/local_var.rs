use crate::{
    completion::{
        CandidateKind, CompletionCandidate, fuzzy,
        provider::{CompletionProvider, ProviderCompletionResult},
    },
    index::{IndexScope, IndexView},
    language::java::render,
    semantic::{
        context::{CursorLocation, SemanticContext},
        types::ContextualResolver,
    },
};
use std::sync::Arc;

pub struct LocalVarProvider;

impl CompletionProvider for LocalVarProvider {
    fn name(&self) -> &'static str {
        "local_var"
    }

    fn is_applicable(&self, ctx: &SemanticContext) -> bool {
        if ctx.java_module_context.is_some() {
            return false;
        }

        matches!(
            ctx.location,
            CursorLocation::Expression { .. } | CursorLocation::MethodArgument { .. }
        )
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
        _request: Option<&crate::lsp::request_context::RequestContext>,
        _limit: Option<usize>,
    ) -> crate::lsp::request_cancellation::RequestResult<ProviderCompletionResult> {
        let prefix = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.as_str(),
            CursorLocation::MethodArgument { prefix } => prefix.as_str(),
            _ => return Ok(ProviderCompletionResult::default()),
        };
        let resolver = ContextualResolver::new(index, ctx);

        let scored =
            fuzzy::fuzzy_filter_sort(prefix, ctx.local_variables.iter(), |lv| lv.name.clone());

        Ok(scored
            .into_iter()
            .map(|(lv, score)| {
                CompletionCandidate::new(
                    Arc::clone(&lv.name),
                    lv.name.to_string(),
                    CandidateKind::LocalVariable {
                        type_descriptor: Arc::from(lv.type_internal.to_internal_with_generics()),
                    },
                    self.name(),
                )
                .with_detail(render::local_variable_detail(
                    lv.name.as_ref(),
                    &lv.type_internal,
                    &resolver,
                ))
                .with_score(50.0 + score as f32 * 0.1)
            })
            .collect::<Vec<_>>()
            .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::WorkspaceIndex;
    use crate::index::{IndexScope, ModuleId};
    use crate::semantic::context::{CursorLocation, LocalVar, SemanticContext};
    use crate::semantic::types::type_name::TypeName;
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_ctx(prefix: &str, vars: Vec<(&str, &str)>) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vars.into_iter()
                .map(|(name, ty)| LocalVar {
                    name: Arc::from(name),
                    type_internal: TypeName::new(ty),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                })
                .collect(),
            None,
            None,
            None,
            vec![],
        )
    }

    #[test]
    fn test_prefix_match() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let ctx = make_ctx(
            "str",
            vec![("str", "java/lang/String"), ("aVar", "java/lang/String")],
        );
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.iter().any(|c| c.label.as_ref() == "str"));
        assert!(results.iter().all(|c| c.label.as_ref() != "aVar"));
    }

    #[test]
    fn test_partial_prefix_match() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let ctx = make_ctx(
            "aV",
            vec![("aVar", "java/lang/String"), ("str", "java/lang/String")],
        );
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.iter().any(|c| c.label.as_ref() == "aVar"));
        assert!(results.iter().all(|c| c.label.as_ref() != "str"));
    }

    #[test]
    fn test_empty_prefix_returns_all_locals() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let ctx = make_ctx(
            "",
            vec![("aVar", "java/lang/String"), ("str", "java/lang/String")],
        );
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert_eq!(
            results.len(),
            2,
            "empty prefix should return all locals: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_case_insensitive() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let ctx = make_ctx("AVAR", vec![("aVar", "java/lang/String")]);
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.iter().any(|c| c.label.as_ref() == "aVar"));
    }

    #[test]
    fn test_method_argument_location() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let ctx = SemanticContext::new(
            CursorLocation::MethodArgument {
                prefix: "aV".to_string(),
            },
            "aV",
            vec![LocalVar {
                name: Arc::from("aVar"),
                type_internal: TypeName::new("java/lang/String"),
                decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        );
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "aVar"),
            "should complete locals inside method arguments"
        );
    }

    #[test]
    fn test_fuzzy_match_var_finds_a_var() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let ctx = make_ctx(
            "var",
            vec![("aVar", "java/lang/String"), ("str", "java/lang/String")],
        );
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "aVar"),
            "fuzzy: 'var' should match 'aVar': {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_match_returns_empty() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let ctx = make_ctx("xyz", vec![("aVar", "java/lang/String")]);
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.is_empty(), "no fuzzy match should return empty");
    }

    #[test]
    fn test_member_access_does_not_return_locals() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
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
            None,
            None,
            None,
            vec![],
        );
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.is_empty(),
            "LocalVarProvider should not return locals for MemberAccess: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_array_local_detail_and_descriptor_preserve_dims() {
        let idx = WorkspaceIndex::new();
        let scope = root_scope();
        let ctx = make_ctx(
            "",
            vec![("numbers", "int[]"), ("parts", "java/lang/String[]")],
        );
        let results = LocalVarProvider
            .provide_test(scope, &ctx, &idx.view(root_scope()), None)
            .candidates;
        let numbers = results
            .iter()
            .find(|c| c.label.as_ref() == "numbers")
            .expect("numbers candidate");
        let parts = results
            .iter()
            .find(|c| c.label.as_ref() == "parts")
            .expect("parts candidate");

        match &numbers.kind {
            CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "int[]");
            }
            other => panic!("unexpected candidate kind: {other:?}"),
        }
        let numbers_detail = numbers.detail.as_deref().unwrap_or("");
        assert!(
            numbers_detail.contains("[]"),
            "array local detail should indicate array-ness: {numbers_detail}"
        );

        match &parts.kind {
            CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "java/lang/String[]");
            }
            other => panic!("unexpected candidate kind: {other:?}"),
        }
        let parts_detail = parts.detail.as_deref().unwrap_or("");
        assert!(
            parts_detail.contains("[]"),
            "array local detail should indicate array-ness: {parts_detail}"
        );
    }
}
