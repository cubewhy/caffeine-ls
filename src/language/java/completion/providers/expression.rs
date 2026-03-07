use crate::{
    completion::{
        CandidateKind, CompletionCandidate, fuzzy, import_utils::is_import_needed,
        provider::CompletionProvider,
    },
    index::{IndexScope, IndexView},
    semantic::context::{CursorLocation, SemanticContext},
};
use std::sync::Arc;

pub struct ExpressionProvider;

impl CompletionProvider for ExpressionProvider {
    fn name(&self) -> &'static str {
        "expression"
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        let prefix = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.as_str(),
            CursorLocation::TypeAnnotation { prefix } => prefix.as_str(),
            CursorLocation::MethodArgument { prefix } => prefix.as_str(),
            _ => return vec![],
        };

        if prefix.contains('.') {
            return vec![];
        }

        let prefix_lower = prefix.to_lowercase();

        // Package name of the current file (used to determine if it is in the same package)
        let current_pkg = ctx.enclosing_package.as_deref();

        let mut results = Vec::new();

        // Classes that have already been imported in current context
        let imported = index.resolve_imports(&ctx.existing_imports);
        for meta in &imported {
            // Theoretically, it is not possible to import nested classes.
            if meta.inner_class_of.is_some() {
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
            let fqn = fqn_of(meta);
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

        let imported_internals: std::collections::HashSet<Arc<str>> = imported
            .iter()
            .map(|m| Arc::clone(&m.internal_name))
            .collect();

        // Same package
        if let Some(pkg) = current_pkg {
            for meta in index.classes_in_package(pkg) {
                if meta.inner_class_of.is_some() {
                    continue;
                }
                if imported_internals.contains(&meta.internal_name) {
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
                let fqn = fqn_of(&meta);
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

        // Other classes (global, require auto-import)
        if !prefix.is_empty() {
            for meta in index.iter_all_classes() {
                // skip nested classes
                if meta.inner_class_of.is_some() {
                    continue;
                }
                if imported_internals.contains(&meta.internal_name) {
                    continue;
                }
                let score = match fuzzy::fuzzy_match(&prefix_lower, &meta.name.to_lowercase()) {
                    Some(s) => s,
                    None => continue,
                };
                let fqn = fqn_of(&meta);

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

        results
    }
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

fn fqn_of(meta: &crate::index::ClassMetadata) -> String {
    match &meta.package {
        Some(pkg) => format!("{}.{}", pkg.replace('/', "."), meta.name),
        None => meta.name.to_string(),
    }
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
}
