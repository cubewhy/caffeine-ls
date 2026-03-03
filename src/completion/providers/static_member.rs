use super::super::{
    candidate::{CandidateKind, CompletionCandidate},
    context::{CompletionContext, CursorLocation},
    scorer,
};
use super::CompletionProvider;
use crate::{
    completion::{scorer::AccessFilter, type_resolver::ContextualResolver},
    index::GlobalIndex,
};
use rust_asm::constants::ACC_STATIC;
use std::sync::Arc;

pub struct StaticMemberProvider;

impl CompletionProvider for StaticMemberProvider {
    fn name(&self) -> &'static str {
        "static_member"
    }

    fn provide(
        &self,
        ctx: &CompletionContext,
        index: &mut GlobalIndex,
    ) -> Vec<CompletionCandidate> {
        let (class_name_raw, member_prefix) = match &ctx.location {
            CursorLocation::StaticAccess {
                class_internal_name,
                member_prefix,
            } => (class_internal_name.as_ref(), member_prefix.as_str()),

            CursorLocation::MemberAccess {
                receiver_expr,
                member_prefix,
                receiver_type: None,
                arguments: None,
            } if is_likely_static_receiver(receiver_expr, ctx) => {
                (receiver_expr.as_str(), member_prefix.as_str())
            }

            _ => return vec![],
        };

        // class_name_raw could be a simple name ("Main") or an internal name ("org/cubewhy/Main")
        // Try searching directly first, then search by simple name if it's not found
        let class_meta = if let Some(m) = index.get_class(class_name_raw) {
            m
        } else {
            let mut candidates = index.get_classes_by_simple_name(class_name_raw).to_vec();
            if candidates.is_empty() {
                // class not in index at all — may be the currently-edited file;
                // fall back to source members if we're accessing our own class
                if is_self_class_by_simple_name(class_name_raw, ctx) {
                    return self.provide_from_source_members(ctx, member_prefix);
                }
                return vec![];
            }
            // same package first
            if let Some(pkg) = ctx.enclosing_package.as_deref()
                && let Some(pos) = candidates
                    .iter()
                    .position(|c| c.package.as_deref() == Some(pkg))
            {
                candidates.swap(0, pos);
            }
            candidates.into_iter().next().unwrap()
        };

        // Determine whether this is a self-access (Main.xxx from inside Main)
        // Compare AFTER resolving to internal name
        let is_same_class = ctx
            .enclosing_internal_name
            .as_deref()
            .is_some_and(|enc| enc == class_meta.internal_name.as_ref());

        // When accessing own class and source members are available, prefer them
        // (handles the case where the current file is not yet compiled into the index)
        if is_same_class && !ctx.current_class_members.is_empty() {
            return self.provide_from_source_members(ctx, member_prefix);
        }

        let filter = if is_same_class {
            AccessFilter::same_class() // private visible
        } else {
            AccessFilter::member_completion()
        };

        let prefix_lower = member_prefix.to_lowercase();
        let class_name = class_meta.internal_name.as_ref();
        let mut results = Vec::new();

        let resolver = ContextualResolver::new(index, ctx);

        for method in &class_meta.methods {
            if method.name.as_ref() == "<init>" || method.name.as_ref() == "<clinit>" {
                continue;
            }
            if method.access_flags & ACC_STATIC == 0 {
                continue;
            }
            if !filter.is_method_accessible(method.access_flags, method.is_synthetic) {
                continue;
            }
            if !prefix_lower.is_empty() && !method.name.to_lowercase().starts_with(&prefix_lower) {
                continue;
            }
            results.push(
                CompletionCandidate::new(
                    Arc::clone(&method.name),
                    if ctx.has_paren_after_cursor() {
                        method.name.to_string()
                    } else {
                        format!("{}(", method.name)
                    },
                    CandidateKind::StaticMethod {
                        descriptor: Arc::clone(&method.descriptor),
                        defining_class: Arc::from(class_name),
                    },
                    self.name(),
                )
                .with_detail(scorer::method_detail(
                    class_name,
                    &class_meta,
                    method,
                    &resolver,
                )),
            );
        }

        let resolver = ContextualResolver::new(index, ctx);

        for field in &class_meta.fields {
            if field.access_flags & ACC_STATIC == 0 {
                continue;
            }
            if !filter.is_field_accessible(field.access_flags, field.is_synthetic) {
                continue;
            }
            if !prefix_lower.is_empty() && !field.name.to_lowercase().starts_with(&prefix_lower) {
                continue;
            }
            results.push(
                CompletionCandidate::new(
                    Arc::clone(&field.name),
                    field.name.to_string(),
                    CandidateKind::StaticField {
                        descriptor: Arc::clone(&field.descriptor),
                        defining_class: Arc::from(class_name),
                    },
                    self.name(),
                )
                .with_detail(scorer::field_detail(
                    class_name,
                    &class_meta,
                    field,
                    &resolver,
                )),
            );
        }

        results
    }
}

impl StaticMemberProvider {
    /// When `Main.` accesses its own class and `current_class_members` is populated
    /// (from source-level parsing), use that — only emit static members.
    fn provide_from_source_members(
        &self,
        ctx: &CompletionContext,
        member_prefix: &str,
    ) -> Vec<CompletionCandidate> {
        use crate::completion::fuzzy;

        let enclosing = ctx.enclosing_internal_name.as_deref().unwrap_or("");

        // Only static members for Cls.xxx access
        let scored = fuzzy::fuzzy_filter_sort(
            member_prefix,
            ctx.current_class_members.values().filter(|m| m.is_static()),
            |m| m.name(),
        );

        scored
            .into_iter()
            .map(|(m, score)| {
                let kind = if m.is_method() {
                    CandidateKind::StaticMethod {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    }
                } else {
                    CandidateKind::StaticField {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    }
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

                let detail = format!(
                    "{} static {}",
                    if m.is_private() { "private" } else { "public" },
                    m.name()
                );

                CompletionCandidate::new(m.name(), insert_text, kind, self.name())
                    .with_detail(detail)
                    .with_score(70.0 + score as f32 * 0.1)
            })
            .collect()
    }
}

/// Check if `class_name_raw` (simple name) refers to the enclosing class,
/// using only the information available in `ctx` (no index lookup).
fn is_self_class_by_simple_name(class_name_raw: &str, ctx: &CompletionContext) -> bool {
    // Match against simple enclosing class name
    ctx.enclosing_class
        .as_deref()
        .is_some_and(|enc| enc == class_name_raw)
}

fn is_likely_static_receiver(expr: &str, ctx: &CompletionContext) -> bool {
    if expr == "this" {
        return false;
    }
    if expr.contains('(') || expr.contains('.') {
        return false;
    }
    if ctx
        .local_variables
        .iter()
        .any(|lv| lv.name.as_ref() == expr)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use rust_asm::constants::{ACC_PRIVATE, ACC_PUBLIC, ACC_STATIC};

    use super::*;
    use crate::completion::context::{CompletionContext, CurrentClassMember, CursorLocation};
    use crate::completion::providers::CompletionProvider;
    use crate::completion::type_resolver::parse_return_type_from_descriptor;
    use crate::index::{
        ClassMetadata, ClassOrigin, FieldSummary, GlobalIndex, MethodParams, MethodSummary,
    };
    use crate::language::{JavaLanguage, Language};
    use std::sync::Arc;

    fn at(src: &str, line: u32, col: u32) -> CompletionContext {
        JavaLanguage
            .parse_completion_context(src, line, col, None)
            .unwrap()
    }

    fn make_method(name: &str, descriptor: &str, flags: u16, is_synthetic: bool) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: flags,
            is_synthetic,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn make_field(name: &str, descriptor: &str, flags: u16, is_synthetic: bool) -> FieldSummary {
        FieldSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            annotations: vec![],
            access_flags: flags,
            is_synthetic,
            generic_signature: None,
        }
    }

    fn make_index_with_main() -> GlobalIndex {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("func"),
                descriptor: Arc::from("()V"),
                annotations: vec![],
                params: MethodParams::empty(),
                access_flags: ACC_PUBLIC | ACC_STATIC,
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
        idx
    }

    fn static_ctx(class_raw: &str, prefix: &str, pkg: &str) -> CompletionContext {
        CompletionContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from(class_raw),
                member_prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from("Main")),
            None,
            Some(Arc::from(pkg)),
            vec![],
        )
    }

    // ── original tests (unchanged) ────────────────────────────────────────

    #[test]
    fn test_static_access_by_simple_name() {
        let mut index = make_index_with_main();
        let ctx = static_ctx("Main", "fun", "org/cubewhy");
        let results = StaticMemberProvider.provide(&ctx, &mut index);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "func"),
            "should find func via simple name lookup: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_static_access_by_internal_name() {
        let mut index = make_index_with_main();
        let ctx = static_ctx("org/cubewhy/Main", "fun", "org/cubewhy");
        let results = StaticMemberProvider.provide(&ctx, &mut index);
        assert!(results.iter().any(|c| c.label.as_ref() == "func"));
    }

    #[test]
    fn test_static_access_empty_prefix_returns_all_static() {
        let mut index = make_index_with_main();
        let ctx = static_ctx("Main", "", "org/cubewhy");
        let results = StaticMemberProvider.provide(&ctx, &mut index);
        assert!(!results.is_empty());
        assert!(results.iter().any(|c| c.label.as_ref() == "func"));
    }

    // ── new tests for self-class static access ────────────────────────────

    /// Build an index that contains Main with a private static field and a
    /// public static method, located in org/cubewhy/a.
    fn make_index_with_self_class() -> GlobalIndex {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/a/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("main"),
                descriptor: Arc::from("()V"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC | ACC_STATIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: None,
            }],
            fields: vec![
                FieldSummary {
                    name: Arc::from("randomField"),
                    descriptor: Arc::from("Lorg/cubewhy/Inst;"),
                    annotations: vec![],
                    access_flags: ACC_PRIVATE | ACC_STATIC,
                    is_synthetic: false,
                    generic_signature: None,
                },
                FieldSummary {
                    name: Arc::from("publicField"),
                    descriptor: Arc::from("I"),
                    access_flags: ACC_PUBLIC | ACC_STATIC,
                    annotations: vec![],
                    is_synthetic: false,
                    generic_signature: None,
                },
            ],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        idx
    }

    fn self_static_ctx(prefix: &str) -> CompletionContext {
        // Simulates: inside org.cubewhy.a.Main, typing "Main.|"
        CompletionContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("Main"), // simple name from parser
                member_prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from("Main")),               // enclosing_class (simple)
            Some(Arc::from("org/cubewhy/a/Main")), // enclosing_internal_name
            Some(Arc::from("org/cubewhy/a")),      // enclosing_package
            vec![],
        )
    }

    #[test]
    fn test_self_class_static_private_field_visible() {
        // Main.| from inside Main — private static field must appear
        let mut idx = make_index_with_self_class();
        let ctx = self_static_ctx("");
        let results = StaticMemberProvider.provide(&ctx, &mut idx);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "randomField"),
            "private static field should be visible when accessing own class: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_self_class_static_public_field_visible() {
        let mut idx = make_index_with_self_class();
        let ctx = self_static_ctx("");
        let results = StaticMemberProvider.provide(&ctx, &mut idx);
        assert!(results.iter().any(|c| c.label.as_ref() == "publicField"));
    }

    #[test]
    fn test_self_class_only_static_members_no_instance() {
        // Even for same-class access, Cls.xxx must only show STATIC members
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/a/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![
                FieldSummary {
                    name: Arc::from("staticF"),
                    descriptor: Arc::from("I"),
                    access_flags: ACC_PUBLIC | ACC_STATIC,
                    annotations: vec![],
                    is_synthetic: false,
                    generic_signature: None,
                },
                FieldSummary {
                    name: Arc::from("instanceF"),
                    descriptor: Arc::from("I"),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC, // NOT static
                    is_synthetic: false,
                    generic_signature: None,
                },
            ],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        let ctx = self_static_ctx("");
        let results = StaticMemberProvider.provide(&ctx, &mut idx);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "staticF"),
            "static field must appear"
        );
        assert!(
            results.iter().all(|c| c.label.as_ref() != "instanceF"),
            "instance field must NOT appear for Cls.xxx access: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_self_class_prefix_filter() {
        let mut idx = make_index_with_self_class();
        let ctx = self_static_ctx("rand");
        let results = StaticMemberProvider.provide(&ctx, &mut idx);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "randomField"),
            "prefix 'rand' should match 'randomField': {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            results.iter().all(|c| c.label.as_ref() != "publicField"),
            "'rand' should not match 'publicField'"
        );
    }

    #[test]
    fn test_self_class_via_source_members_when_not_in_index() {
        // The current file is not compiled yet → class is absent from the index.
        // StaticMemberProvider must fall back to current_class_members.
        let mut idx = GlobalIndex::new(); // empty — class not indexed

        let members = vec![
            // randomField: static + private
            CurrentClassMember::Field(Arc::new(make_field(
                "randomField",
                "Lorg/cubewhy/Inst;",
                ACC_STATIC | ACC_PRIVATE,
                false,
            ))),
            // instanceField: instance + public
            CurrentClassMember::Field(Arc::new(make_field(
                "instanceField",
                "I",
                ACC_PUBLIC,
                false,
            ))),
            // staticHelper: static + public
            CurrentClassMember::Method(Arc::new(make_method(
                "staticHelper",
                "()V",
                ACC_STATIC | ACC_PUBLIC,
                false,
            ))),
        ];

        let ctx = CompletionContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("Main"),
                member_prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_class_members(members);

        let results = StaticMemberProvider.provide(&ctx, &mut idx);

        assert!(
            results.iter().any(|c| c.label.as_ref() == "randomField"),
            "private static field from source members should appear: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            results.iter().any(|c| c.label.as_ref() == "staticHelper"),
            "static method from source members should appear"
        );
        assert!(
            results.iter().all(|c| c.label.as_ref() != "instanceField"),
            "instance field must NOT appear even from source members: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_other_class_private_not_visible() {
        // Accessing a DIFFERENT class's static members → private must be hidden
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Other"),
            internal_name: Arc::from("org/cubewhy/a/Other"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![FieldSummary {
                name: Arc::from("secret"),
                descriptor: Arc::from("I"),
                annotations: vec![],
                access_flags: ACC_PRIVATE | ACC_STATIC,
                is_synthetic: false,
                generic_signature: None,
            }],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        // We are inside Main, accessing Other.secret
        let ctx = CompletionContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("Other"),
                member_prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")), // enclosing is Main, not Other
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        );

        let results = StaticMemberProvider.provide(&ctx, &mut idx);
        assert!(
            results.iter().all(|c| c.label.as_ref() != "secret"),
            "private field of another class must NOT be visible: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    // ── existing parser-based tests (unchanged) ───────────────────────────

    #[test]
    fn test_locals_in_static_method_no_semicolon() {
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                String aVar = "test";
                String str = "a";
                s
            }
        }
    "#};
        let line = 4u32;
        let col = src.lines().nth(4).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "aVar"),
            "aVar should be extracted even without semicolon on current line: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(
            ctx.local_variables.iter().any(|v| v.name.as_ref() == "str"),
            "str should be extracted: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_locals_in_method_argument_no_semicolon() {
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                String aVar = "test";
                System.out.println(aVar)
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.find("aVar").unwrap() as u32 + 4;
        let ctx = at(src, line, col);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "aVar"),
            "aVar should be visible inside method argument: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_char_after_cursor_paren() {
        let src = indoc::indoc! {r#"
        class A {
            void fun() {
                this.priFunc()
            }
            private void priFunc() {}
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("priFunc").unwrap() as u32 + "priFunc".len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.has_paren_after_cursor(),
            "char after cursor should be '(', got {:?}",
            ctx.char_after_cursor
        );
    }

    #[test]
    fn test_char_after_cursor_no_paren() {
        let src = indoc::indoc! {r#"
        class A {
            void fun() {
                this.priFunc
            }
            private void priFunc() {}
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("priFunc").unwrap() as u32 + "priFunc".len() as u32;
        let ctx = at(src, line, col);
        assert!(
            !ctx.has_paren_after_cursor(),
            "no paren after cursor, got {:?}",
            ctx.char_after_cursor
        );
    }

    #[test]
    fn test_lowercase_class_name_static_access_via_provider() {
        use crate::completion::providers::CompletionProvider;
        use crate::completion::providers::static_member::StaticMemberProvider;
        use crate::index::{ClassMetadata, ClassOrigin, FieldSummary, GlobalIndex};
        use rust_asm::constants::{ACC_PUBLIC, ACC_STATIC};

        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("myClass"),
            internal_name: Arc::from("myClass"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![FieldSummary {
                name: Arc::from("FIELD"),
                descriptor: Arc::from("I"),
                annotations: vec![],
                access_flags: ACC_PUBLIC | ACC_STATIC,
                is_synthetic: false,
                generic_signature: None,
            }],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        // Parser 产生 MemberAccess，enrich 后 receiver_type 仍为 None（不是局部变量）
        let ctx = CompletionContext::new(
            CursorLocation::MemberAccess {
                receiver_type: None,
                member_prefix: "FIELD".to_string(),
                receiver_expr: "myClass".to_string(),
                arguments: None,
            },
            "FIELD",
            vec![], // no locals named myClass
            None,
            None,
            None,
            vec![],
        );

        let results = StaticMemberProvider.provide(&ctx, &mut idx);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "FIELD"),
            "lowercase class name static field should be found via provider, got: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }
}
