use rust_asm::constants::ACC_STATIC;

use crate::index::{IndexScope, IndexView};
use crate::semantic::context::{CursorLocation, SemanticContext};
use crate::semantic::types::ContextualResolver;
use crate::{
    completion::{
        candidate::{CandidateKind, CompletionCandidate},
        fuzzy,
        provider::CompletionProvider,
    },
    language::java::render,
};
use std::sync::Arc;

pub struct ThisMemberProvider;

impl CompletionProvider for ThisMemberProvider {
    fn name(&self) -> &'static str {
        "this_member"
    }

    fn provide(
        &self,
        _scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        tracing::debug!(
            "ThisMemberProvider: enclosing={:?}",
            ctx.enclosing_internal_name
        );
        let prefix = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.as_str(),
            CursorLocation::MethodArgument { prefix } => prefix.as_str(),
            _ => return vec![],
        };

        if ctx.current_class_members.is_empty() && ctx.enclosing_internal_name.is_none() {
            return vec![];
        }

        let in_static = ctx.is_in_static_context();
        let enclosing = ctx.enclosing_internal_name.as_deref().unwrap_or("");

        tracing::debug!("  in_static={}", in_static);
        tracing::debug!(
            "  current_class_members: {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );

        // ── 1. 当前类 source members（含 private，不依赖 index）──────────────
        let scored = fuzzy::fuzzy_filter_sort(
            prefix,
            ctx.current_class_members
                .values()
                .filter(|m| !in_static || m.is_static()),
            |m| m.name(),
        );

        let resolver = ContextualResolver::new(index, ctx);

        let mut results: Vec<CompletionCandidate> = scored
            .into_iter()
            .map(|(m, score)| {
                let kind = match (m.is_method(), m.is_static()) {
                    (true, true) => CandidateKind::StaticMethod {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (true, false) => CandidateKind::Method {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (false, true) => CandidateKind::StaticField {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (false, false) => CandidateKind::Field {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                };
                let insert_text = if m.is_method() {
                    if ctx.has_paren_after_cursor() {
                        m.name().to_string()
                    } else {
                        format!("{}(", m.name())
                    }
                } else {
                    m.name().to_string()
                };
                let detail = render::source_member_detail(enclosing, m, &resolver);

                CompletionCandidate::new(Arc::clone(&m.name()), insert_text, kind, self.name())
                    .with_detail(detail)
                    .with_score(60.0 + score as f32 * 0.1)
            })
            .collect();

        // Inheritance chain members (search from index MRO, skipping those already existing in the current class)
        if !enclosing.is_empty() {
            // Avoid duplicate names that already exist in the current class source.
            let source_names: std::collections::HashSet<Arc<str>> =
                ctx.current_class_members.keys().map(Arc::clone).collect();

            let prefix_lower = prefix.to_lowercase();

            let mro = index.mro(enclosing);
            let resolver = ContextualResolver::new(index, ctx);

            tracing::debug!(
                "  mro: {:?}",
                mro.iter()
                    .map(|c| c.internal_name.as_ref())
                    .collect::<Vec<_>>()
            );
            // mro[0] 是当前类自身，跳过；从 super 开始
            for class_meta in mro.iter().skip(1) {
                for method in &class_meta.methods {
                    if method.name.as_ref() == "<init>" || method.name.as_ref() == "<clinit>" {
                        continue;
                    }
                    // 静态上下文只显示 static 成员
                    let is_static = method.access_flags & ACC_STATIC != 0;
                    // if in_static && !is_static {
                    //     continue;
                    // }
                    // 跳过 private（继承不可见）
                    use rust_asm::constants::ACC_PRIVATE;
                    if method.access_flags & ACC_PRIVATE != 0 {
                        continue;
                    }
                    // 跳过 synthetic
                    if method.is_synthetic {
                        continue;
                    }
                    // 当前类 source 已声明同名方法，不重复
                    if source_names.contains(&method.name) {
                        continue;
                    }
                    // 前缀过滤
                    if !prefix_lower.is_empty()
                        && !method.name.to_lowercase().contains(&prefix_lower)
                    {
                        continue;
                    }
                    let kind = if is_static {
                        CandidateKind::StaticMethod {
                            descriptor: method.desc(),
                            defining_class: Arc::clone(&class_meta.internal_name),
                        }
                    } else {
                        CandidateKind::Method {
                            descriptor: method.desc(),
                            defining_class: Arc::clone(&class_meta.internal_name),
                        }
                    };
                    let insert_text = if ctx.has_paren_after_cursor() {
                        method.name.to_string()
                    } else {
                        format!("{}(", method.name)
                    };
                    results.push(
                        CompletionCandidate::new(
                            Arc::clone(&method.name),
                            insert_text,
                            kind,
                            self.name(),
                        )
                        .with_detail(render::method_detail(
                            class_meta.internal_name.as_ref(),
                            class_meta,
                            method,
                            &resolver,
                        ))
                        .with_score(50.0),
                    );
                }

                for field in &class_meta.fields {
                    use rust_asm::constants::ACC_PRIVATE;
                    if field.access_flags & ACC_PRIVATE != 0 {
                        continue;
                    }
                    if field.is_synthetic {
                        continue;
                    }
                    let is_static = field.access_flags & ACC_STATIC != 0;
                    if in_static && !is_static {
                        continue;
                    }
                    if source_names.contains(&field.name) {
                        continue;
                    }
                    if !prefix_lower.is_empty()
                        && !field.name.to_lowercase().contains(&prefix_lower)
                    {
                        continue;
                    }
                    let kind = if is_static {
                        CandidateKind::StaticField {
                            descriptor: Arc::clone(&field.descriptor),
                            defining_class: Arc::clone(&class_meta.internal_name),
                        }
                    } else {
                        CandidateKind::Field {
                            descriptor: Arc::clone(&field.descriptor),
                            defining_class: Arc::clone(&class_meta.internal_name),
                        }
                    };
                    results.push(
                        CompletionCandidate::new(
                            Arc::clone(&field.name),
                            field.name.to_string(),
                            kind,
                            self.name(),
                        )
                        .with_detail(render::field_detail(
                            class_meta.internal_name.as_ref(),
                            class_meta,
                            field,
                            &resolver,
                        ))
                        .with_score(50.0),
                    );
                }
            }
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use crate::index::WorkspaceIndex;
    use rust_asm::constants::{ACC_PRIVATE, ACC_PUBLIC};

    use super::*;
    use crate::index::{FieldSummary, IndexScope, MethodParams, MethodSummary, ModuleId};
    use crate::semantic::context::{CurrentClassMember, CursorLocation, SemanticContext};
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_member(
        name: &str,
        is_method: bool,
        is_static: bool,
        is_private: bool,
    ) -> CurrentClassMember {
        let mut flags = if is_private { ACC_PRIVATE } else { ACC_PUBLIC };
        if is_static {
            flags |= ACC_STATIC;
        }

        if is_method {
            CurrentClassMember::Method(Arc::new(MethodSummary {
                name: Arc::from(name),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: flags,
                is_synthetic: false,
                generic_signature: None,
                return_type: None,
            }))
        } else {
            CurrentClassMember::Field(Arc::new(FieldSummary {
                name: Arc::from(name),
                descriptor: Arc::from("I"),
                annotations: vec![],
                access_flags: flags,
                is_synthetic: false,
                generic_signature: None,
            }))
        }
    }

    fn ctx_with_members(prefix: &str, members: Vec<CurrentClassMember>) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_class_members(members)
    }

    fn ctx_with_members_static(
        prefix: &str,
        members: Vec<CurrentClassMember>,
        enclosing: CurrentClassMember,
    ) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_class_members(members)
        .with_enclosing_member(Some(enclosing))
    }

    #[test]
    fn test_prefix_match() {
        let members = vec![
            make_member("func", true, false, false),
            make_member("fun", true, false, false),
            make_member("pri", true, true, true),
            make_member("other", true, false, false),
        ];
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members("fu", members);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "func"));
        assert!(results.iter().any(|c| c.label.as_ref() == "fun"));
        assert!(results.iter().all(|c| c.label.as_ref() != "other"));
        assert!(results.iter().all(|c| c.label.as_ref() != "pri"));
    }

    #[test]
    fn test_private_method_visible() {
        // Private methods of the same type should be visible
        let members = vec![make_member("pri", true, true, true)];
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members("pr", members);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "pri"),
            "private method should be visible: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_private_static_method_visible() {
        let members = vec![make_member("pri", true, true, true)];
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members("pr", members);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "pri"));
        assert!(matches!(
            results
                .iter()
                .find(|c| c.label.as_ref() == "pri")
                .unwrap()
                .kind,
            CandidateKind::StaticMethod { .. }
        ));
    }

    #[test]
    fn test_field_no_paren() {
        let members = vec![make_member("count", false, false, true)];
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members("co", members);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let c = results
            .iter()
            .find(|c| c.label.as_ref() == "count")
            .unwrap();
        assert!(!c.insert_text.contains('('));
        assert!(matches!(c.kind, CandidateKind::Field { .. }));
    }

    #[test]
    fn test_empty_prefix_returns_all() {
        let members = vec![make_member("func", true, false, false)];
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members("", members);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].label.as_ref(), "func");
    }

    #[test]
    fn test_no_members_returns_nothing() {
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members("fu", vec![]);
        assert!(
            ThisMemberProvider
                .provide(root_scope(), &ctx, &idx.view(root_scope()))
                .is_empty()
        );
    }

    #[test]
    fn test_method_before_cursor_accessible() {
        // Verification method definition can be completed even after the cursor (full-text scan)
        // Here, it is injected directly through current_class_members to simulate full-text parsing
        let members = vec![
            make_member("afterMethod", true, false, false), // Defined after the cursor
            make_member("beforeMethod", true, false, false), // Defined before the cursor
        ];
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members("a", members);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "afterMethod"),
            "method defined after cursor should still be completable"
        );
    }

    #[test]
    fn test_kind_static_field() {
        let members = vec![make_member("CONST", false, true, false)];
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members("CO", members);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(matches!(
            results
                .iter()
                .find(|c| c.label.as_ref() == "CONST")
                .unwrap()
                .kind,
            CandidateKind::StaticField { .. }
        ));
    }

    #[test]
    fn test_static_method_no_this() {
        // In a static method, ThisMemberProvider should not return any result.
        let members = vec![
            make_member("helper", true, false, false),
            make_member("CONST", false, true, false),
        ];
        let enclosing = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("staticEntry"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_STATIC | ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members_static("he", members, enclosing);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.is_empty(),
            "ThisMemberProvider should return nothing inside a static method, got: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_instance_method_has_this() {
        // In the instance method, ThisMemberProvider should return normally.
        let members = vec![make_member("helper", true, false, false)];
        let enclosing = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("instanceEntry"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members_static("he", members, enclosing);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "helper"),
            "ThisMemberProvider should work inside an instance method"
        );
    }

    #[test]
    fn test_this_dot_in_static_method() {
        // Even if `this.xxx` is explicitly written, it should not be completed in the static method.
        let members = vec![make_member("field", false, false, false)];
        let enclosing = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("staticFn"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_STATIC | ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let idx = WorkspaceIndex::new();
        let mut ctx = ctx_with_members_static("", members, enclosing);
        ctx.location = CursorLocation::MemberAccess {
            receiver_semantic_type: None,
            receiver_type: None,
            member_prefix: "fi".to_string(),
            receiver_expr: "this".to_string(),
            arguments: None,
        };
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.is_empty(),
            "this.xxx should not complete inside a static method"
        );
    }

    #[test]
    fn test_static_context_only_shows_static_members() {
        // Static methods should only display static members
        let members = vec![
            make_member("pri", true, true, false),     // static method
            make_member("fun", true, false, false),    // instance method
            make_member("CONST", false, true, false),  // static field
            make_member("count", false, false, false), // instance field
        ];
        let idx = WorkspaceIndex::new();

        // Construct a static context
        let enclosing_method = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("main"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_STATIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_class_members(members)
        .with_enclosing_member(Some(enclosing_method));

        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));

        assert!(
            results.iter().any(|c| c.label.as_ref() == "pri"),
            "static method should be visible in static context"
        );
        assert!(
            results.iter().any(|c| c.label.as_ref() == "CONST"),
            "static field should be visible in static context"
        );
        assert!(
            results.iter().all(|c| c.label.as_ref() != "fun"),
            "instance method should NOT be visible in static context"
        );
        assert!(
            results.iter().all(|c| c.label.as_ref() != "count"),
            "instance field should NOT be visible in static context"
        );
    }

    #[test]
    fn test_static_context_prefix_filter() {
        // Static methods use the "pr" prefix; you should be able to find pri
        let members = vec![
            make_member("pri", true, true, false),
            make_member("fun", true, false, false),
        ];
        let idx = WorkspaceIndex::new();

        let enclosing_method = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("main"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_STATIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "pr".to_string(),
            },
            "pr",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_class_members(members)
        .with_enclosing_member(Some(enclosing_method));

        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "pri"),
            "should find static method 'pri' with prefix 'pr' in static context: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_method_arg_empty_prefix_returns_locals_and_members() {
        // println(|) with an empty prefix should return local variables and class members
        let members = vec![make_member("pri", true, true, false)];
        let idx = WorkspaceIndex::new();

        let enclosing_method = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("main"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_STATIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let ctx = SemanticContext::new(
            CursorLocation::MethodArgument {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_class_members(members)
        .with_enclosing_member(Some(enclosing_method));

        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "pri"),
            "empty prefix in method arg should return static members in static context: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_static_method_no_instance_members() {
        let members = vec![
            make_member("helper", true, false, false), // instance
            make_member("CONST", false, true, false),  // static
        ];
        let enclosing_method = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("staticEntry"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC | ACC_STATIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let idx = WorkspaceIndex::new();
        let ctx = ctx_with_members_static("he", members, enclosing_method);
        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| c.label.as_ref() != "helper"),
            "instance method should not appear in static context: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        // CONST is static, so "he" cannot match "CONST", and it's normal for the result to be empty.
    }

    #[test]
    fn test_inherited_instance_method_visible_in_instance_context() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::ACC_PUBLIC;

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("BaseClass"),
                internal_name: Arc::from("org/cubewhy/BaseClass"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("funcA"),
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
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Main2"),
                internal_name: Arc::from("org/cubewhy/Main2"),
                super_name: Some("org/cubewhy/BaseClass".into()),
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

        // 模拟在 Main2 的实例方法里补全
        let enclosing_method = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("func"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Main2")),
            Some(Arc::from("org/cubewhy/Main2")), // enclosing_internal_name
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_class_members(vec![make_member("func", true, false, false)])
        .with_enclosing_member(Some(enclosing_method));

        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "funcA"),
            "funcA inherited from BaseClass should be visible inside Main2 instance method, got: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_private_super_method_not_inherited() {
        use crate::index::{ClassMetadata, ClassOrigin, MethodSummary};
        use rust_asm::constants::{ACC_PRIVATE, ACC_PUBLIC};

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Base"),
                internal_name: Arc::from("Base"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("superPrivate"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PRIVATE,
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
            ClassMetadata {
                package: None,
                name: Arc::from("Child"),
                internal_name: Arc::from("Child"),
                super_name: Some(Arc::from("Base")),
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

        let enclosing_method = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("doWork"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Child")),
            Some(Arc::from("Child")),
            None,
            vec![],
        )
        .with_enclosing_member(Some(enclosing_method));

        let results = ThisMemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| c.label.as_ref() != "superPrivate"),
            "private super method should NOT be visible in subclass"
        );
    }
}
