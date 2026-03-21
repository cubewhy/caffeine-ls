use crate::completion::provider::{CompletionProvider, ProviderCompletionResult};
use crate::completion::{CandidateKind, CompletionCandidate, candidate::ReplacementMode};
use crate::index::{IndexScope, IndexView};
use crate::semantic::context::{JavaIntrinsicAccessKind, SemanticContext};
use std::sync::Arc;

pub struct IntrinsicMemberProvider;

impl CompletionProvider for IntrinsicMemberProvider {
    fn name(&self) -> &'static str {
        "intrinsic_member"
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        _index: &IndexView,
        _limit: Option<usize>,
    ) -> ProviderCompletionResult {
        match ctx.java_intrinsic_access.as_ref().map(|a| a.kind) {
            Some(JavaIntrinsicAccessKind::ClassLiteral) => vec![
                CompletionCandidate::new(
                    Arc::from("class"),
                    "class",
                    CandidateKind::Keyword,
                    self.name(),
                )
                .with_replacement_mode(ReplacementMode::MemberSegment)
                .with_detail("class literal")
                .with_score(95.0),
            ],
            _ => vec![],
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, MethodSummary, ModuleId, WorkspaceIndex,
    };
    use crate::language::java::completion_context::ContextEnricher;
    use crate::language::java::type_ctx::SourceTypeCtx;
    use crate::language::{ParseEnv, java::extract_java_semantic_context_for_test};
    use crate::semantic::context::CursorLocation;
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("toString"),
                    params: crate::index::MethodParams::empty(),
                    annotations: vec![],
                    access_flags: 0,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("Ljava/lang/String;")),
                }],
                fields: vec![],
                access_flags: 0,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("pkg")),
                name: Arc::from("Outer"),
                internal_name: Arc::from("pkg/Outer"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("pkg")),
                name: Arc::from("Inner"),
                internal_name: Arc::from("pkg/Outer$Inner"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                inner_class_of: Some(Arc::from("Outer")),
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        idx
    }

    fn java_ctx_from_marked_source_with_view(
        marked_source: &str,
        trigger_char: Option<char>,
        view: &IndexView,
    ) -> crate::semantic::SemanticContext {
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = ropey::Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let col = (marker - rope.line_to_byte(line as usize)) as u32;
        extract_java_semantic_context_for_test(
            &source,
            line,
            col,
            trigger_char,
            &ParseEnv {
                name_table: Some(view.build_name_table()),
                view: Some(view.clone()),
                workspace: None,
                file_uri: None,
                metrics: None,
            },
        )
        .expect("java semantic context with view")
    }

    fn complete(src: &str) -> (crate::semantic::SemanticContext, Vec<CompletionCandidate>) {
        let idx = make_index();
        let view = idx.view(root_scope());
        let name_table = view.build_name_table();
        let mut ctx = java_ctx_from_marked_source_with_view(src, Some('.'), &view);
        ctx = ctx.with_extension(Arc::new(
            SourceTypeCtx::new(
                Some(Arc::from("pkg")),
                vec!["java.lang.*".into(), "pkg.*".into()],
                Some(name_table),
            )
            .with_view(view.clone()),
        ));
        ContextEnricher::new(&view).enrich(&mut ctx);
        let out = IntrinsicMemberProvider
            .provide(root_scope(), &ctx, &view, None)
            .candidates;
        (ctx, out)
    }

    #[test]
    fn suggests_class_for_reference_primitive_nested_and_array_type_receivers() {
        for src in [
            "class T { void m() { String.| } }",
            "class T { void m() { int.| } }",
            "class T { void m() { Outer.Inner.| } }",
            "class T { void m() { String[].| } }",
        ] {
            let (_, out) = complete(src);
            assert!(
                out.iter().any(|c| c.label.as_ref() == "class"),
                "expected class completion for {src:?}, got {:?}",
                out.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn does_not_suggest_class_for_value_receiver() {
        let (ctx, out) = complete("class T { void m(String obj) { obj.| } }");
        assert!(
            matches!(ctx.location, CursorLocation::MemberAccess { .. }),
            "expected member access, got {:?}",
            ctx.location
        );
        assert!(
            out.iter().all(|c| c.label.as_ref() != "class"),
            "class must not be suggested for value receivers: {:?}",
            out.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn suggests_class_for_incomplete_class_literal_prefix_on_type_operands() {
        let (_, out) = complete("class T { void m() { String.cl| } }");
        assert!(
            out.iter().any(|c| c.label.as_ref() == "class"),
            "expected class completion, got {:?}",
            out.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }
}
