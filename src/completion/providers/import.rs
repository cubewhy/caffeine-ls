use super::super::{
    candidate::CompletionCandidate,
    context::{CompletionContext, CursorLocation},
};
use super::CompletionProvider;
use crate::index::GlobalIndex;

pub struct ImportProvider;

impl CompletionProvider for ImportProvider {
    fn name(&self) -> &'static str {
        "import"
    }

    fn provide(
        &self,
        ctx: &CompletionContext,
        index: &mut GlobalIndex,
    ) -> Vec<CompletionCandidate> {
        let prefix = match &ctx.location {
            CursorLocation::Import { prefix } => prefix.as_str(),
            _ => return vec![],
        };
        crate::completion::import_completion::candidates_for_import(prefix, index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::context::{CompletionContext, CursorLocation};
    use crate::index::{ClassMetadata, ClassOrigin, GlobalIndex};
    use rust_asm::constants::ACC_PUBLIC;
    use std::sync::Arc;

    fn make_index() -> GlobalIndex {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_cls("org/cubewhy", "Main"),
            make_cls("org/cubewhy", "RealMain"),
            make_cls("org/cubewhy/utils", "StringUtil"),
            make_cls("java/util", "ArrayList"),
            make_cls("java/util", "HashMap"),
        ]);
        idx
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

    fn import_ctx(prefix: &str) -> CompletionContext {
        CompletionContext::new(
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
        let mut idx = make_index();
        let ctx = CompletionContext::new(
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
        assert!(ImportProvider.provide(&ctx, &mut idx).is_empty());
    }

    #[test]
    fn test_delegates_to_import_completion() {
        let mut idx = make_index();
        let results = ImportProvider.provide(&import_ctx("org.cubewhy.Ma"), &mut idx);
        assert!(
            results
                .iter()
                .any(|c| c.label.as_ref() == "org.cubewhy.Main"),
            "{:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }
}
