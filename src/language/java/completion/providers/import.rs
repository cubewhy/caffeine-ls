use crate::{
    completion::{CompletionCandidate, provider::CompletionProvider},
    index::{IndexScope, IndexView},
    semantic::context::{CursorLocation, SemanticContext},
};

pub struct ImportProvider;

impl CompletionProvider for ImportProvider {
    fn name(&self) -> &'static str {
        "import"
    }

    fn provide(
        &self,
        scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        let prefix = match &ctx.location {
            CursorLocation::Import { prefix } => prefix.as_str(),
            _ => return vec![],
        };
        crate::completion::import_completion::candidates_for_import(prefix, scope, index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::WorkspaceIndex;
    use crate::index::{ClassMetadata, ClassOrigin, IndexScope, IndexView, ModuleId};
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use rust_asm::constants::ACC_PUBLIC;
    use std::sync::Arc;

    fn make_view() -> IndexView {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                make_cls("org/cubewhy", "Main"),
                make_cls("org/cubewhy", "RealMain"),
                make_cls("org/cubewhy/utils", "StringUtil"),
                make_cls("java/util", "ArrayList"),
                make_cls("java/util", "HashMap"),
            ],
        );
        idx.view(IndexScope {
            module: ModuleId::ROOT,
        })
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
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn import_ctx(prefix: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::Import {
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
    fn test_non_import_location_returns_empty() {
        let view = make_view();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "Ma".to_string(),
            },
            "Ma",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        assert!(ImportProvider.provide(scope, &ctx, &view).is_empty());
    }

    #[test]
    fn test_delegates_to_import_completion() {
        let view = make_view();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let results = ImportProvider.provide(scope, &import_ctx("org.cubewhy.Ma"), &view);
        assert!(
            results
                .iter()
                .any(|c| c.label.as_ref() == "org.cubewhy.Main"),
            "{:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }
}
