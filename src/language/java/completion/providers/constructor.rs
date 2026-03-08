use crate::{
    completion::{
        CandidateKind, CompletionCandidate,
        import_utils::{is_import_needed, source_fqn_of_meta},
        provider::CompletionProvider,
        scorer::AccessFilter,
    },
    index::{IndexScope, IndexView},
    language::java::completion::providers::type_lookup::qualified_nested_type_matches,
    language::java::location::normalize_top_level_generic_base,
    semantic::context::{CursorLocation, SemanticContext},
};
use std::sync::Arc;

pub struct ConstructorProvider;

impl CompletionProvider for ConstructorProvider {
    fn name(&self) -> &'static str {
        "constructor"
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        let (class_prefix, expected_type) = match &ctx.location {
            CursorLocation::ConstructorCall {
                class_prefix,
                expected_type,
            } => (class_prefix.as_str(), expected_type.as_deref()),
            _ => return vec![],
        };
        let class_prefix = normalize_top_level_generic_base(class_prefix);

        // When prefix is empty but expected_type exists, search for expected_type first
        let search_prefix = if class_prefix.is_empty() {
            expected_type.unwrap_or("")
        } else {
            class_prefix
        };
        let metas = if class_prefix.contains('.') {
            qualified_nested_type_matches(class_prefix, ctx, index)
        } else {
            index
                .fuzzy_search_classes(search_prefix, 50)
                .into_iter()
                .filter(|meta| is_constructor_type_visible(meta, ctx, index))
                .collect()
        };

        let mut results = Vec::new();
        for meta in metas {
            let fqn = source_fqn_of_meta(&meta, index);
            let needs_import = is_import_needed(
                &fqn,
                &ctx.existing_imports,
                ctx.enclosing_package.as_deref(),
            );

            // Score boost when class name matches the expected LHS type
            let type_score_boost = expected_type
                .map(|et| type_match_score(et, &meta.name))
                .unwrap_or(0.0);

            let filter = AccessFilter::member_completion();
            let constructors: Vec<_> = meta
                .methods
                .iter()
                .filter(|m| {
                    m.name.as_ref() == "<init>"
                        && filter.is_method_accessible(m.access_flags, m.is_synthetic)
                })
                .collect();

            if constructors.is_empty() {
                // Synthesise a default no-arg constructor
                let candidate = CompletionCandidate::new(
                    Arc::clone(&meta.name),
                    format!("{}()", meta.name),
                    CandidateKind::Constructor {
                        descriptor: Arc::from("()V"),
                        defining_class: Arc::clone(&meta.name),
                    },
                    self.name(),
                )
                .with_detail(format!("new {}()", fqn))
                .with_score(type_score_boost);

                let candidate = if needs_import {
                    candidate.with_import(fqn)
                } else {
                    candidate
                };
                results.push(candidate);
                continue;
            }

            for ctor in constructors {
                let readable_params = descriptor_params_to_readable(&ctor.desc());
                let insert_text = format!("{}(", meta.name);
                let detail = format!("new {}({})", fqn, readable_params);
                let candidate = CompletionCandidate::new(
                    Arc::clone(&meta.name),
                    insert_text,
                    CandidateKind::Constructor {
                        descriptor: Arc::clone(&ctor.desc()),
                        defining_class: Arc::clone(&meta.name),
                    },
                    self.name(),
                )
                .with_detail(detail)
                .with_score(type_score_boost);

                let candidate = if needs_import {
                    candidate.with_import(fqn.clone())
                } else {
                    candidate
                };
                results.push(candidate);
            }
        }
        results
    }
}

fn is_constructor_type_visible(
    meta: &crate::index::ClassMetadata,
    ctx: &SemanticContext,
    index: &IndexView,
) -> bool {
    if meta.inner_class_of.is_none() {
        return true;
    }
    let Some(enclosing) = ctx.enclosing_internal_name.as_deref() else {
        return false;
    };
    index
        .resolve_scoped_inner_class(enclosing, meta.name.as_ref())
        .is_some_and(|resolved| resolved.internal_name == meta.internal_name)
}

/// Score boost based on how well the class name matches the expected type.
/// - Exact match (case-insensitive): high boost → sorted to top
/// - No match: 0
///
/// Future: extend to check super_name / interfaces for inheritance bonus.
fn type_match_score(expected: &str, class_name: &str) -> f32 {
    if class_name.eq_ignore_ascii_case(expected) {
        // Exact match — push to the very top
        200.0
    } else if class_name
        .to_lowercase()
        .starts_with(&expected.to_lowercase())
    {
        // Prefix match (e.g. expected="Array", class="ArrayList")
        50.0
    } else {
        0.0
    }
}

pub fn descriptor_params_to_readable(descriptor: &str) -> String {
    let inner = match descriptor.find('(').zip(descriptor.find(')')) {
        Some((l, r)) => &descriptor[l + 1..r],
        None => return String::new(),
    };
    parse_type_list(inner)
        .into_iter()
        .map(|t| jvm_type_to_readable(&t))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_type_list(mut s: &str) -> Vec<String> {
    let mut result = Vec::new();
    while !s.is_empty() {
        let (ty, rest) = consume_one_type(s);
        result.push(ty);
        s = rest;
    }
    result
}

fn consume_one_type(s: &str) -> (String, &str) {
    match s.chars().next() {
        Some('L') => {
            // object type, like Ljava/lang/String;
            if let Some(end) = s.find(';') {
                (s[..=end].to_string(), &s[end + 1..])
            } else {
                (s.to_string(), "")
            }
        }
        Some('[') => {
            let (inner, rest) = consume_one_type(&s[1..]);
            (format!("[{}", inner), rest)
        }
        Some(c) => (c.to_string(), &s[1..]),
        None => (String::new(), ""),
    }
}

pub fn jvm_type_to_readable(ty: &str) -> String {
    if let Some(stripped) = ty.strip_prefix('[') {
        return format!("{}[]", jvm_type_to_readable(stripped));
    }
    if ty.starts_with('L') && ty.ends_with(';') {
        let class_path = &ty[1..ty.len() - 1];
        return class_path
            .rsplit('/')
            .next()
            .unwrap_or(class_path)
            .to_string();
    }
    match ty {
        "B" => "byte",
        "C" => "char",
        "D" => "double",
        "F" => "float",
        "I" => "int",
        "J" => "long",
        "S" => "short",
        "Z" => "boolean",
        "V" => "void",
        other => other,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::WorkspaceIndex;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, ModuleId,
    };
    use crate::language::java::type_ctx::SourceTypeCtx;
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use rust_asm::constants::ACC_PUBLIC;
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_index_with(pkg: &str, name: &str, has_init: bool) -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        let methods = if has_init {
            vec![MethodSummary {
                name: Arc::from("<init>"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: None,
            }]
        } else {
            vec![]
        };
        idx.add_classes(vec![ClassMetadata {
            package: if pkg.is_empty() {
                None
            } else {
                Some(Arc::from(pkg))
            },
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).trim_start_matches('/')),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods,
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        idx
    }

    fn make_ctx(prefix: &str, expected: Option<&str>, imports: Vec<Arc<str>>) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::ConstructorCall {
                class_prefix: prefix.to_string(),
                expected_type: expected.map(|s| s.to_string()),
            },
            prefix,
            vec![],
            Some(Arc::from("Main")),
            None,
            Some(Arc::from("org/cubewhy/a")),
            imports,
        )
    }

    fn make_nested_index() -> WorkspaceIndex {
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
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
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
                inner_class_of: Some(Arc::from("ChainCheck")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("BoxV"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box$BoxV"),
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
                inner_class_of: Some(Arc::from("Box")),
                origin: ClassOrigin::Unknown,
            },
        ]);
        idx
    }

    fn make_nested_ctx(prefix: &str, enclosing_internal: Option<&str>) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::ConstructorCall {
                class_prefix: prefix.to_string(),
                expected_type: None,
            },
            prefix,
            vec![],
            Some(Arc::from("Probe")),
            enclosing_internal.map(Arc::from),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
    }

    // ── empty prefix returns candidates ──────────────────────────────────

    #[test]
    fn test_empty_prefix_returns_candidates() {
        let idx = make_index_with("org/cubewhy", "RandomClass", true);
        let ctx = make_ctx("", None, vec![]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            !results.is_empty(),
            "empty prefix should return constructor candidates"
        );
    }

    #[test]
    fn test_empty_prefix_includes_known_class() {
        let idx = make_index_with("org/cubewhy", "RandomClass", true);
        let ctx = make_ctx("", None, vec![]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "RandomClass"),
            "RandomClass should appear with empty prefix: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    // ── import logic (was inverted) ───────────────────────────────────────

    #[test]
    fn test_no_import_when_already_exact_imported() {
        let idx = make_index_with("org/cubewhy", "RandomClass", true);
        let ctx = make_ctx("RandomClass", None, vec!["org.cubewhy.RandomClass".into()]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| c.required_import.is_none()),
            "should not add import when already imported: {:?}",
            results
                .iter()
                .map(|c| &c.required_import)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_import_when_wildcard_imported() {
        let idx = make_index_with("org/cubewhy", "RandomClass", true);
        let ctx = make_ctx("RandomClass", None, vec!["org.cubewhy.*".into()]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| c.required_import.is_none()),
            "should not add import under wildcard: {:?}",
            results
                .iter()
                .map(|c| &c.required_import)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_import_when_same_package() {
        let idx = make_index_with("org/cubewhy/a", "Helper", true);
        // enclosing package is org/cubewhy/a — same as Helper
        let ctx = make_ctx("Helper", None, vec![]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| c.required_import.is_none()),
            "same-package class should not need import: {:?}",
            results
                .iter()
                .map(|c| &c.required_import)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_generic_constructor_prefix_is_normalized_for_search() {
        let idx = make_index_with("org/cubewhy", "ArrayList", true);
        let ctx = make_ctx("ArrayList<String>", None, vec![]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "ArrayList"),
            "generic prefix should still match ArrayList: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_non_generic_constructor_prefix_unchanged() {
        let idx = make_index_with("org/cubewhy", "ArrayList", true);
        let ctx = make_ctx("ArrayList", None, vec![]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "ArrayList"),
            "non-generic prefix should keep existing behavior: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_import_added_when_not_imported() {
        let idx = make_index_with("org/cubewhy", "RandomClass", true);
        let ctx = make_ctx("RandomClass", None, vec![]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results
                .iter()
                .any(|c| c.required_import.as_deref() == Some("org.cubewhy.RandomClass")),
            "should add import when class not imported: {:?}",
            results
                .iter()
                .map(|c| &c.required_import)
                .collect::<Vec<_>>()
        );
    }

    // ── expected_type score boost ─────────────────────────────────────────

    #[test]
    fn test_expected_type_exact_match_scores_highest() {
        let idx = WorkspaceIndex::new();
        // Add both String and StringBuilder
        for (pkg, name) in [("java/lang", "String"), ("java/lang", "StringBuilder")] {
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

        // expected_type = "String" → String should score higher than StringBuilder
        let ctx = make_ctx("S", Some("String"), vec![]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));

        let string_score = results
            .iter()
            .find(|c| c.label.as_ref() == "String")
            .map(|c| c.score)
            .unwrap_or(0.0);
        let sb_score = results
            .iter()
            .find(|c| c.label.as_ref() == "StringBuilder")
            .map(|c| c.score)
            .unwrap_or(0.0);

        assert!(
            string_score > sb_score,
            "String (exact match) should score higher than StringBuilder: {} vs {}",
            string_score,
            sb_score
        );
    }

    #[test]
    fn test_no_expected_type_no_score_boost() {
        let idx = make_index_with("org/cubewhy", "RandomClass", true);
        let ctx = make_ctx("RandomClass", None, vec![]);
        let results = ConstructorProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        // score should be 0.0 (set by provider; Scorer adds on top in engine)
        assert!(
            results.iter().all(|c| c.score == 0.0),
            "no expected_type → no boost, scores: {:?}",
            results.iter().map(|c| c.score).collect::<Vec<_>>()
        );
    }

    // ── type_match_score unit tests ───────────────────────────────────────

    #[test]
    fn test_type_match_exact() {
        assert_eq!(type_match_score("String", "String"), 200.0);
        assert_eq!(type_match_score("string", "String"), 200.0); // case-insensitive
    }

    #[test]
    fn test_type_match_prefix() {
        assert!(type_match_score("Array", "ArrayList") > 0.0);
        assert!(type_match_score("Array", "ArrayList") < 200.0);
    }

    #[test]
    fn test_type_match_none() {
        assert_eq!(type_match_score("String", "HashMap"), 0.0);
    }

    // ── descriptor helpers ────────────────────────────────────────────────

    #[test]
    fn test_descriptor_params_readable() {
        assert_eq!(
            descriptor_params_to_readable("(Ljava/lang/String;I)V"),
            "String, int"
        );
        assert_eq!(descriptor_params_to_readable("()V"), "");
        assert_eq!(
            descriptor_params_to_readable("([Ljava/lang/String;)V"),
            "String[]"
        );
    }

    #[test]
    fn test_constructor_completion_inside_enclosing_class_sees_nested() {
        let idx = make_nested_index();
        let view = idx.view(root_scope());
        let ctx = make_nested_ctx("BoxV", Some("org/cubewhy/ChainCheck$Box")).with_extension(
            Arc::new(SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy")),
                vec![],
                Some(view.build_name_table()),
            )),
        );
        let results = ConstructorProvider.provide(root_scope(), &ctx, &view);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "BoxV"),
            "{:?}",
            results
                .iter()
                .map(|c| format!("{} detail={:?}", c.label, c.detail))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_constructor_completion_owner_qualified_nested() {
        let idx = make_nested_index();
        let view = idx.view(root_scope());
        let ctx = make_nested_ctx("ChainCheck.Box", Some("org/cubewhy/Probe")).with_extension(
            Arc::new(SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy")),
                vec![],
                Some(view.build_name_table()),
            )),
        );
        let results = ConstructorProvider.provide(root_scope(), &ctx, &view);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "Box"),
            "{:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_constructor_completion_deep_owner_qualified_nested() {
        let idx = make_nested_index();
        let view = idx.view(root_scope());
        let ctx = make_nested_ctx("ChainCheck.Box.BoxV", Some("org/cubewhy/Probe")).with_extension(
            Arc::new(SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy")),
                vec![],
                Some(view.build_name_table()),
            )),
        );
        let results = ConstructorProvider.provide(root_scope(), &ctx, &view);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "BoxV"),
            "{:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        let boxv = results
            .iter()
            .find(|c| c.label.as_ref() == "BoxV")
            .expect("BoxV candidate");
        assert_eq!(
            boxv.detail.as_deref(),
            Some("new org.cubewhy.ChainCheck.Box.BoxV()")
        );
        assert_eq!(
            boxv.required_import.as_deref(),
            Some("org.cubewhy.ChainCheck.Box.BoxV")
        );
    }

    #[test]
    fn test_constructor_completion_unqualified_does_not_leak_unrelated_nested() {
        let idx = make_nested_index();
        let view = idx.view(root_scope());
        let ctx = make_nested_ctx("Box", Some("org/cubewhy/Unrelated")).with_extension(Arc::new(
            SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy")),
                vec![],
                Some(view.build_name_table()),
            ),
        ));
        let results = ConstructorProvider.provide(root_scope(), &ctx, &view);
        assert!(
            results.iter().all(|c| c.label.as_ref() != "Box"),
            "{:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }
}
