use rust_asm::constants::ACC_STATIC;
use std::sync::Arc;

use crate::{
    completion::{CandidateKind, CompletionCandidate, provider::CompletionProvider},
    index::{ClassMetadata, IndexScope, IndexView},
    language::java::render,
    semantic::{
        context::{CursorLocation, SemanticContext},
        types::ContextualResolver,
    },
};

pub struct StaticImportMemberProvider;

impl CompletionProvider for StaticImportMemberProvider {
    fn name(&self) -> &'static str {
        "static_import_member"
    }

    fn provide(
        &self,
        scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        if ctx.static_imports.is_empty() {
            return vec![];
        }
        let query_lower = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.to_lowercase(),
            CursorLocation::MethodArgument { prefix } => prefix.to_lowercase(),
            CursorLocation::Unknown => ctx.query.to_lowercase(),
            _ => return vec![],
        };

        let mut results = Vec::new();

        for import in &ctx.static_imports {
            let s = import.as_ref();
            if s.ends_with(".*") {
                // import static java.lang.Math.*
                let class_path = s.trim_end_matches(".*").replace('.', "/");
                if let Some(meta) = index.get_class(&class_path) {
                    results.extend(all_static_members(
                        &meta,
                        &class_path,
                        &query_lower,
                        ctx,
                        self.name(),
                        index,
                        scope,
                    ));
                }
            } else {
                // import static java.lang.Math.abs
                let dot = match s.rfind('.') {
                    Some(p) => p,
                    None => continue,
                };
                let class_path = s[..dot].replace('.', "/");
                let member_name = &s[dot + 1..];
                if !query_lower.is_empty() && !member_name.to_lowercase().starts_with(&query_lower)
                {
                    continue;
                }
                if let Some(meta) = index.get_class(&class_path) {
                    results.extend(specific_static_member(
                        &meta,
                        &class_path,
                        member_name,
                        ctx,
                        self.name(),
                        index,
                        scope,
                    ));
                }
            }
        }

        results
    }
}

fn all_static_members(
    meta: &ClassMetadata,
    class_path: &str,
    query_lower: &str,
    ctx: &SemanticContext,
    source: &'static str,
    index: &IndexView,
    _scope: IndexScope,
) -> Vec<CompletionCandidate> {
    let resolver = ContextualResolver::new(index, ctx);

    let mut out = Vec::new();
    for method in &meta.methods {
        if matches!(method.name.as_ref(), "<init>" | "<clinit>") {
            continue;
        }
        if method.access_flags & ACC_STATIC == 0 {
            continue;
        }
        if !query_lower.is_empty() && !method.name.to_lowercase().starts_with(query_lower) {
            continue;
        }
        out.push(
            CompletionCandidate::new(
                Arc::clone(&method.name),
                if ctx.has_paren_after_cursor() {
                    method.name.to_string()
                } else {
                    format!("{}(", method.name)
                },
                CandidateKind::StaticMethod {
                    descriptor: method.desc(),
                    defining_class: Arc::from(class_path),
                },
                source,
            )
            .with_detail(render::method_detail(class_path, meta, method, &resolver))
            .with_score(75.0),
        );
    }
    for field in &meta.fields {
        if field.access_flags & ACC_STATIC == 0 {
            continue;
        }
        if !query_lower.is_empty() && !field.name.to_lowercase().starts_with(query_lower) {
            continue;
        }
        out.push(
            CompletionCandidate::new(
                Arc::clone(&field.name),
                field.name.to_string(),
                CandidateKind::StaticField {
                    descriptor: Arc::clone(&field.descriptor),
                    defining_class: Arc::from(class_path),
                },
                source,
            )
            .with_detail(render::field_detail(class_path, meta, field, &resolver))
            .with_score(75.0),
        );
    }
    out
}

fn specific_static_member(
    meta: &ClassMetadata,
    class_path: &str,
    member_name: &str,
    ctx: &SemanticContext,
    source: &'static str,
    index: &IndexView,
    _scope: IndexScope,
) -> Vec<CompletionCandidate> {
    let resolver = ContextualResolver::new(index, ctx);

    let mut out = Vec::new();
    for method in &meta.methods {
        if method.name.as_ref() != member_name {
            continue;
        }
        if method.access_flags & ACC_STATIC == 0 {
            continue;
        }
        out.push(
            CompletionCandidate::new(
                Arc::clone(&method.name),
                if ctx.has_paren_after_cursor() {
                    method.name.to_string()
                } else {
                    format!("{}(", method.name)
                },
                CandidateKind::StaticMethod {
                    descriptor: method.desc(),
                    defining_class: Arc::from(class_path),
                },
                source,
            )
            .with_detail(render::method_detail(class_path, meta, method, &resolver))
            .with_score(80.0),
        );
    }

    for field in &meta.fields {
        if field.name.as_ref() != member_name {
            continue;
        }
        if field.access_flags & ACC_STATIC == 0 {
            continue;
        }
        out.push(
            CompletionCandidate::new(
                Arc::clone(&field.name),
                field.name.to_string(),
                CandidateKind::StaticField {
                    descriptor: Arc::clone(&field.descriptor),
                    defining_class: Arc::from(class_path),
                },
                source,
            )
            .with_detail(render::field_detail(class_path, meta, field, &resolver))
            .with_score(80.0),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::WorkspaceIndex;
    use crate::index::{
        ClassMetadata, ClassOrigin, FieldSummary, IndexScope, MethodParams, MethodSummary, ModuleId,
    };
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use rust_asm::constants::{ACC_PUBLIC, ACC_STATIC};
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn math_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            root_scope(),
            vec![ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Math"),
                internal_name: Arc::from("java/lang/Math"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("abs"),
                        params: MethodParams::from([("I", "i")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC | ACC_STATIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
                    },
                    MethodSummary {
                        name: Arc::from("pow"),
                        params: MethodParams::from([("D", "d0"), ("D", "d1")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC | ACC_STATIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("D")),
                    },
                ],
                fields: vec![FieldSummary {
                    name: Arc::from("PI"),
                    descriptor: Arc::from("D"),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC | ACC_STATIC,
                    is_synthetic: false,
                    generic_signature: None,
                }],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            }],
        );

        idx
    }

    fn expr_ctx(prefix: &str, static_imports: Vec<Arc<str>>) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            None,
            None,
            None,
            vec![],
        )
        .with_static_imports(static_imports)
    }

    #[test]
    fn test_wildcard_static_import_provides_all_static_members() {
        let idx = math_index();
        let ctx = expr_ctx("", vec![Arc::from("java.lang.Math.*")]);
        let results =
            StaticImportMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"abs"), "abs should appear: {:?}", labels);
        assert!(labels.contains(&"pow"), "pow should appear: {:?}", labels);
        assert!(labels.contains(&"PI"), "PI should appear: {:?}", labels);
    }

    #[test]
    fn test_wildcard_static_import_filters_by_prefix() {
        let idx = math_index();
        let ctx = expr_ctx("ab", vec![Arc::from("java.lang.Math.*")]);
        let results =
            StaticImportMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"abs"), "abs should match prefix 'ab'");
        assert!(!labels.contains(&"pow"), "pow should not match 'ab'");
    }

    #[test]
    fn test_specific_static_import_provides_named_member() {
        let idx = math_index();
        let ctx = expr_ctx("", vec![Arc::from("java.lang.Math.abs")]);
        let results =
            StaticImportMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            labels.contains(&"abs"),
            "abs should appear for specific import"
        );
        assert!(
            !labels.contains(&"pow"),
            "pow should NOT appear for specific import of abs"
        );
    }

    #[test]
    fn test_no_static_imports_returns_empty() {
        let idx = math_index();
        let ctx = expr_ctx("ab", vec![]);
        assert!(
            StaticImportMemberProvider
                .provide(root_scope(), &ctx, &idx.view(root_scope()))
                .is_empty()
        );
    }

    #[test]
    fn test_wrong_location_returns_empty() {
        let idx = math_index();
        let ctx = SemanticContext::new(
            CursorLocation::Import {
                prefix: "java.lang.Math".to_string(),
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        )
        .with_static_imports(vec![Arc::from("java.lang.Math.*")]);
        assert!(
            StaticImportMemberProvider
                .provide(root_scope(), &ctx, &idx.view(root_scope()))
                .is_empty()
        );
    }
}
