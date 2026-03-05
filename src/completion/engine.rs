use crate::completion::CompletionCandidate;
use crate::completion::post_processor;
use crate::completion::provider::CompletionProvider;
use crate::index::{IndexScope, IndexView};
use crate::language::Language;
use crate::semantic::SemanticContext;

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
        mut ctx: SemanticContext,
        lang: &dyn Language,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        lang.enrich_completion_context(&mut ctx, scope, index);

        let mut candidates: Vec<CompletionCandidate> = lang
            .completion_providers()
            .iter()
            .flat_map(|p| p.provide(scope, &ctx, index))
            .collect();

        for provider in &self.extra_providers {
            candidates.extend(provider.provide(scope, &ctx, index));
        }

        post_processor::process(candidates, &ctx.query)
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
        index::{
            ClassMetadata, ClassOrigin, MethodParams, MethodSummary, ModuleId, WorkspaceIndex,
        },
        language::{JavaLanguage, java::completion_context::ContextEnricher},
        semantic::types::{TypeResolver, type_name::TypeName},
        semantic::{CursorLocation, LocalVar, SemanticContext},
    };
    use rust_asm::constants::ACC_PUBLIC;
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn seg_names(expr: &str) -> Vec<(String, Option<i32>)> {
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "f".to_string(),
                receiver_expr: "cl".to_string(),

                arguments: None,
            },
            "f",
            vec![LocalVar {
                name: Arc::from("cl"),
                type_internal: TypeName::new("RandomClass"),
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.RandomClass".into()],
        );
        let view = idx.view(root_scope());
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "cl".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("cl"),
                type_internal: TypeName::new("RandomClass"),
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.*".into()],
        );
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(receiver_type.as_deref(), Some("org/cubewhy/RandomClass"),);
        }
    }

    #[test]
    fn test_complete_returns_f_method() {
        let idx = make_index_with_random_class();
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "f".to_string(),
                receiver_expr: "cl".to_string(),
                arguments: None,
            },
            "f",
            vec![LocalVar {
                name: Arc::from("cl"),
                type_internal: TypeName::new("RandomClass"),
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.RandomClass".into()],
        );
        let engine = CompletionEngine::new();
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "f"),
            "should find method f(): {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
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

        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);

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
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.RandomClass".into()],
        );
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &idx.view(root_scope()));
        assert!(!results.is_empty(), "should have candidates");
        assert_eq!(
            results[0].label.as_ref(),
            "RandomClass",
            "RandomClass should rank first when it matches expected_type, got: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
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
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("str"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("nc.randomFunction()".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
        let str_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "str")
            .unwrap();
        assert_eq!(str_var.type_internal.as_ref(), "NestedClass");
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
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
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("str"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("nc.randomFunction(\"a\", 1l)".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
        let str_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "str")
            .unwrap();
        assert_eq!(str_var.type_internal.as_ref(), "Main2");
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "str".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("str"),
                type_internal: TypeName::new("var"),
                init_expr: Some("getString()".to_string()),
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
        let str_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "str")
            .unwrap();
        assert_eq!(str_var.type_internal.as_ref(), "java/lang/String");
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
        assert_eq!(result.as_deref(), Some("java/lang/String"));
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
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "func"));
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
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
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("args[0]".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
        let a_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "a")
            .unwrap();
        assert_eq!(a_var.type_internal.as_ref(), "java/lang/String");
    }

    #[test]
    fn test_var_primitive_array_element_not_resolved() {
        let idx = WorkspaceIndex::new();
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
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
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("x"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("nums[0]".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![],
        );
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
        let x_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "x")
            .unwrap();
        assert_ne!(x_var.type_internal.as_ref(), "int[]");
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "b[0]".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("b"),
                type_internal: TypeName::new("String[]"),
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        );
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
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
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
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
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
        assert!(matches!(
            &ctx.location,
            CursorLocation::Import { prefix } if prefix == "java.util.ArrayL"
        ));
    }

    #[test]
    fn test_unknown_receiver_stays_member_access() {
        let idx = WorkspaceIndex::new();
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
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
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
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

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
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
                    init_expr: None,
                },
                // var c = arr[0];
                LocalVar {
                    name: Arc::from("c"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("arr[0]".to_string()),
                },
                // var strArr = new String[]{"[1]", "[2]"}; (标准对象数组，带干扰符号)
                LocalVar {
                    name: Arc::from("strArr"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("new String[]{\"[1]\", \"[2]\"}".to_string()),
                },
                // var strItem = strArr[1];
                LocalVar {
                    name: Arc::from("strItem"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("strArr[1]".to_string()),
                },
            ],
            None,
            None,
            None,
            vec![], // 没传 import，String 可以被兜底或者需要完整包名？
        );

        // 注入默认 java.lang.* import 来确保 String 能正常被 resolve_simple_to_internal 解析
        ctx.existing_imports.push(Arc::from("java.lang.*"));

        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);

        // 校验 c (arr[0]) 被推断为 char
        let c_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "c")
            .unwrap();
        assert_eq!(c_var.type_internal.as_ref(), "char");

        // 校验 strArr (new String[]...) 被推断为 java/lang/String[]
        let str_arr_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "strArr")
            .unwrap();
        assert_eq!(str_arr_var.type_internal.as_ref(), "java/lang/String[]");

        // 校验 strItem (strArr[1]) 被推断为 java/lang/String
        let str_item_var = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "strItem")
            .unwrap();
        assert_eq!(str_item_var.type_internal.as_ref(), "java/lang/String");
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

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "item".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("item"),
                type_internal: TypeName::new("var"),
                init_expr: Some("getArr()[0]".to_string()),
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );
        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);
        let item = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "item")
            .unwrap();
        assert_eq!(item.type_internal.as_ref(), "java/lang/String");
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

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "m".to_string(), // m.a
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("m"),
                type_internal: TypeName::new("var"),
                init_expr: Some("new Main()".to_string()),
            }],
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );

        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);

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

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "res[0][0]".to_string(), // 多维数组访问
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("res"),
                type_internal: TypeName::new("java/lang/String[][]"),
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        );

        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);

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

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "m.arr[0][0][0][0]".to_string(), // 4层访问
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("m"),
                type_internal: TypeName::new("org/cubewhy/Main"),
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        );

        ContextEnricher::new(&idx.view(root_scope())).enrich(&mut ctx);

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
