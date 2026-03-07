use std::collections::HashSet;
use std::sync::Arc;

use rust_asm::constants::{ACC_FINAL, ACC_PRIVATE, ACC_STATIC};

use crate::{
    completion::{CandidateKind, CompletionCandidate, provider::CompletionProvider},
    index::{IndexScope, IndexView, MethodSummary},
    semantic::{
        context::{CursorLocation, SemanticContext},
        types::ContextualResolver,
    },
};

pub struct OverrideProvider;

impl CompletionProvider for OverrideProvider {
    fn name(&self) -> &'static str {
        "override"
    }

    fn provide(
        &self,
        scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        let prefix = match &ctx.location {
            CursorLocation::Expression { prefix } => prefix.as_str(),
            _ => return vec![],
        };

        if !is_access_modifier_prefix(prefix) {
            return vec![];
        }

        let enclosing = match ctx.enclosing_internal_name.as_deref() {
            Some(e) => e,
            None => return vec![],
        };

        // A collection of (name, descriptor) methods that have been overridden in the current class
        // current_class_members is a HashMap<name, member>, which only keeps the last overridden method
        // Therefore, it additionally retrieves the current class's own methods from the index for precise deduplication.
        let already_overridden: HashSet<(Arc<str>, Arc<str>)> =
            self.collect_current_class_methods(enclosing, index, scope);

        // (name, descriptor) already appearing in this candidate list, to avoid the same method appearing repeatedly from multiple ancestors.
        let mut emitted: HashSet<(Arc<str>, Arc<str>)> = HashSet::new();

        let mut mro = index.mro(enclosing);
        let has_object = mro
            .iter()
            .any(|c| c.internal_name.as_ref() == "java/lang/Object");
        if !has_object && let Some(object_meta) = index.get_class("java/lang/Object") {
            mro.push(object_meta);
        }

        let mut candidates = Vec::new();

        for (i, class_meta) in mro.iter().enumerate() {
            if i == 0 {
                continue;
            }
            for method in &class_meta.methods {
                if !is_overridable(method) {
                    continue;
                }
                let key = (Arc::clone(&method.name), method.desc());
                if already_overridden.contains(&key) {
                    continue;
                }

                // Source-level member
                let candidate_param_count = method.params.len();
                let blocked_by_source = ctx.current_class_members.values().any(|m| {
                    if !m.is_method() || m.name() != method.name {
                        return false;
                    }
                    // bad descriptor ast
                    if m.descriptor().is_empty() {
                        return true;
                    }
                    crate::semantic::types::count_params(&m.descriptor()) == candidate_param_count
                });

                if blocked_by_source {
                    continue;
                }
                if !emitted.insert(key) {
                    // It has been generated from a more recent ancestor.
                    continue;
                }

                let resolver = ContextualResolver::new(index, ctx);

                let Some((params_source, return_type_source)) =
                    crate::semantic::types::parse_strict_method_signature(
                        &method.desc(),
                        &resolver,
                    )
                else {
                    continue;
                };

                let candidate = build_candidate(
                    method,
                    &return_type_source,
                    &params_source,
                    &class_meta.internal_name,
                    ctx,
                    index,
                    scope,
                    self.name(),
                );
                candidates.push(candidate);
            }
        }

        candidates
    }
}

impl OverrideProvider {
    /// Accurately collect the (name, descriptor) members already present in the current class:
    // Prioritize index (if the current file has been compiled into index),
    // Then overlay current_class_members (members resolved at the source level).
    fn collect_current_class_methods(
        &self,
        enclosing: &str,
        index: &IndexView,
        _scope: IndexScope,
    ) -> HashSet<(Arc<str>, Arc<str>)> {
        let mut set: HashSet<(Arc<str>, Arc<str>)> = HashSet::new();
        if let Some(meta) = index.get_class(enclosing) {
            for m in &meta.methods {
                set.insert((Arc::clone(&m.name), m.desc()));
            }
        }
        set
    }
}

fn is_overridable(method: &MethodSummary) -> bool {
    // Constructor / Static Initialization Block
    if matches!(method.name.as_ref(), "<init>" | "<clinit>") {
        return false;
    }
    // Compiler-generated synthesis method
    if method.is_synthetic {
        return false;
    }
    // private, cannot be overridden
    if method.access_flags & ACC_PRIVATE != 0 {
        return false;
    }
    // Static methods can only be hidden, not overridden.
    if method.access_flags & ACC_STATIC != 0 {
        return false;
    }
    // final cannot be overridden
    if method.access_flags & ACC_FINAL != 0 {
        return false;
    }
    true
}

/// The current input prefix is ​​a valid start (at least 2 characters) of "public" or "protected".
fn is_access_modifier_prefix(prefix: &str) -> bool {
    let p = prefix.trim();
    if p.len() < 2 {
        return false;
    }
    "public".starts_with(p) || "protected".starts_with(p)
}

fn build_candidate(
    method: &MethodSummary,
    return_type_source: &str,
    params_source: &[String],
    defining_class_internal: &Arc<str>,
    ctx: &SemanticContext,
    index: &IndexView,
    _scope: IndexScope,
    source: &'static str,
) -> CompletionCandidate {
    use rust_asm::constants::{ACC_PROTECTED, ACC_PUBLIC};

    let visibility = if method.access_flags & ACC_PUBLIC != 0 {
        "public"
    } else if method.access_flags & ACC_PROTECTED != 0 {
        "protected"
    } else {
        // package-private
        ""
    };

    // 组装参数列表：如 "java.lang.Object arg0, int arg1"
    let params_str = params_source
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} arg{}", t, i))
        .collect::<Vec<_>>()
        .join(", ");

    let label_text = format!(
        "{} {} {}({})",
        visibility, return_type_source, method.name, params_str
    )
    .trim()
    .to_string();

    let body_line = r#"throw new RuntimeException("Not implemented yet");"#;

    let vis_prefix = if visibility.is_empty() {
        String::new()
    } else {
        format!("{} ", visibility)
    };

    let insert_text = if ctx.has_paren_after_cursor() {
        format!(
            "@Override\n{}{}  {}(",
            vis_prefix, return_type_source, method.name
        )
    } else {
        format!(
            "@Override\n{}{} {}({}) {{\n    {}\n}}",
            vis_prefix, return_type_source, method.name, params_str, body_line
        )
    };

    // 查找展示名称，如果因为某些极端的并发原因没查到，做个兜底展示
    let defining_class_display = index
        .get_source_type_name(defining_class_internal)
        .unwrap_or_else(|| defining_class_internal.replace(['/', '$'], "."));

    let detail = format!("@Override — {}", defining_class_display);

    CompletionCandidate::new(
        Arc::from(label_text.as_str()),
        insert_text,
        CandidateKind::Method {
            descriptor: method.desc(),
            defining_class: Arc::clone(defining_class_internal),
        },
        source,
    )
    .with_detail(detail)
    .with_score(65.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::WorkspaceIndex;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, ModuleId,
    };
    use crate::semantic::context::{CurrentClassMember, CursorLocation, SemanticContext};
    use crate::semantic::types::parse_return_type_from_descriptor;
    use rust_asm::constants::{ACC_PROTECTED, ACC_PUBLIC, ACC_STATIC};
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn method(name: &str, descriptor: &str, flags: u16) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: flags,
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn synthetic_method(name: &str, descriptor: &str) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: true,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn make_class(
        pkg: &str,
        name: &str,
        super_name: Option<&str>,
        methods: Vec<MethodSummary>,
    ) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).as_str()),
            super_name: super_name.map(Arc::from),
            interfaces: vec![],
            annotations: vec![],
            methods,
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn ctx_with_prefix(prefix: &str, enclosing_internal: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::Expression {
                prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from(
                enclosing_internal.rsplit('/').next().unwrap_or(""),
            )),
            Some(Arc::from(enclosing_internal)),
            None,
            vec![],
        )
    }

    #[test]
    fn test_prefix_public_triggers() {
        assert!(is_access_modifier_prefix("pu"));
        assert!(is_access_modifier_prefix("pub"));
        assert!(is_access_modifier_prefix("publ"));
        assert!(is_access_modifier_prefix("publi"));
        assert!(is_access_modifier_prefix("public"));
    }

    #[test]
    fn test_prefix_protected_triggers() {
        assert!(is_access_modifier_prefix("pr"));
        assert!(is_access_modifier_prefix("pro"));
        assert!(is_access_modifier_prefix("prot"));
        assert!(is_access_modifier_prefix("protected"));
    }

    #[test]
    fn test_prefix_too_short_no_trigger() {
        assert!(!is_access_modifier_prefix(""));
        assert!(!is_access_modifier_prefix("p")); // 单字符太模糊
    }

    #[test]
    fn test_unrelated_prefix_no_trigger() {
        assert!(!is_access_modifier_prefix("vo")); // void
        assert!(!is_access_modifier_prefix("pri")); // private
        assert!(!is_access_modifier_prefix("abc"));
    }

    #[test]
    fn test_full_word_with_space_no_trigger() {
        // "public void" cannot match "public".starts_with("public void")
        assert!(!is_access_modifier_prefix("public void"));
    }

    #[test]
    fn test_basic_override_from_superclass() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();

        assert!(
            labels.iter().any(|l| l.contains("doWork")),
            "doWork should appear as overridable: {:?}",
            labels
        );
    }

    #[test]
    fn test_insert_text_contains_override_annotation() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let candidate = results.iter().find(|c| c.label.contains("doWork")).unwrap();

        assert!(
            candidate.insert_text.contains("@Override"),
            "insert_text must contain @Override: {:?}",
            candidate.insert_text
        );
    }

    #[test]
    fn test_insert_text_contains_method_body_stub() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let c = results.iter().find(|c| c.label.contains("doWork")).unwrap();

        assert!(c.insert_text.contains('{'), "should have opening brace");
        assert!(c.insert_text.contains('}'), "should have closing brace");
    }

    #[test]
    fn test_already_overridden_excluded_via_index() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class(
                "com/example",
                "Child",
                Some("com/example/Parent"),
                // Child 已经 override doWork
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();

        assert!(
            labels.iter().all(|l| !l.contains("doWork")),
            "doWork already overridden, must not appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_already_overridden_excluded_via_source_members() {
        // Child is not compiled into the index, but current_class_members has doWork.
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_class(
            "com/example",
            "Parent",
            None,
            vec![method("doWork", "()V", ACC_PUBLIC)],
        )]);
        // Child only exists at the source level (without add_classes)
        let child_meta = make_class("com/example", "Child", Some("com/example/Parent"), vec![]);
        // Manually let the index know the superclass of Child (otherwise the MRO cannot find the Parent).
        idx.add_classes(vec![child_meta.clone()]);

        let source_member = CurrentClassMember::Method(Arc::new(MethodSummary {
            name: Arc::from("doWork"),
            params: MethodParams::empty(),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: None,
        }));
        let ctx = ctx_with_prefix("pub", "com/example/Child")
            .with_class_members(std::iter::once(source_member));

        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            labels.iter().all(|l| !l.contains("doWork")),
            "doWork in source members must be excluded: {:?}",
            labels
        );
    }

    #[test]
    fn test_overloads_both_shown_when_none_overridden() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            make_class(
                "com/example",
                "Parent",
                None,
                vec![
                    method("compute", "(I)I", ACC_PUBLIC),
                    method("compute", "(Ljava/lang/String;)I", ACC_PUBLIC),
                ],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let compute_count = results
            .iter()
            .filter(|c| c.label.contains("compute"))
            .count();
        assert_eq!(
            compute_count,
            2,
            "both overloads should appear: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_overloads_only_unoverridden_shown() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            make_class(
                "com/example",
                "Parent",
                None,
                vec![
                    method("compute", "(I)I", ACC_PUBLIC),
                    method("compute", "(Ljava/lang/String;)I", ACC_PUBLIC),
                ],
            ),
            make_class(
                "com/example",
                "Child",
                Some("com/example/Parent"),
                // 只 override 了 int 版本
                vec![method("compute", "(I)I", ACC_PUBLIC)],
            ),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let compute: Vec<_> = results
            .iter()
            .filter(|c| c.label.contains("compute"))
            .collect();

        assert_eq!(
            compute.len(),
            1,
            "only unoverridden overload should remain: {:?}",
            compute.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        // 剩下的应该是 String 参数版本
        assert!(
            compute[0].insert_text.contains("String"),
            "remaining overload should be String variant: {:?}",
            compute[0].insert_text
        );
    }

    #[test]
    fn test_private_method_not_overridable() {
        use rust_asm::constants::ACC_PRIVATE;
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("secret", "()V", ACC_PRIVATE)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| !c.label.contains("secret")),
            "private method must not appear"
        );
    }

    #[test]
    fn test_static_method_not_overridable() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("staticFn", "()V", ACC_PUBLIC | ACC_STATIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| !c.label.contains("staticFn")),
            "static method must not appear"
        );
    }

    #[test]
    fn test_final_method_not_overridable() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("locked", "()V", ACC_PUBLIC | ACC_FINAL)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| !c.label.contains("locked")),
            "final method must not appear"
        );
    }

    #[test]
    fn test_synthetic_method_excluded() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![synthetic_method("access$000", "(Lcom/example/Parent;)V")],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| !c.label.contains("access$")),
            "synthetic method must not appear"
        );
    }

    #[test]
    fn test_constructor_excluded() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("<init>", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| !c.label.contains("<init>")),
            "<init> must not appear"
        );
    }

    #[test]
    fn test_no_enclosing_class_returns_empty() {
        let idx = WorkspaceIndex::new();
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "pub".to_string(),
            },
            "pub",
            vec![],
            None,
            None, // enclosing_internal_name = None
            None,
            vec![],
        );
        assert!(
            OverrideProvider
                .provide(root_scope(), &ctx, &idx.view(root_scope()))
                .is_empty()
        );
    }

    #[test]
    fn test_protected_method_visibility_preserved() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("hook", "()V", ACC_PROTECTED)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pro", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let c = results.iter().find(|c| c.label.contains("hook")).unwrap();
        assert!(
            c.insert_text.contains("protected"),
            "protected visibility should be preserved: {:?}",
            c.insert_text
        );
    }

    #[test]
    fn test_grandparent_method_appears() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "GrandParent",
                None,
                vec![method("ancientMethod", "()V", ACC_PUBLIC)],
            ),
            make_class(
                "com/example",
                "Parent",
                Some("com/example/GrandParent"),
                vec![],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.contains("ancientMethod")),
            "grandparent method should be overridable: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_duplicate_from_multiple_ancestors() {
        // GrandParent 和 Parent 都声明了同一方法（Parent 没有 override，走继承）
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "GrandParent",
                None,
                vec![method("shared", "()V", ACC_PUBLIC)],
            ),
            make_class(
                "com/example",
                "Parent",
                Some("com/example/GrandParent"),
                vec![method("shared", "()V", ACC_PUBLIC)], // 重复声明（模拟 index 含两份）
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let count = results
            .iter()
            .filter(|c| c.label.contains("shared"))
            .count();
        assert_eq!(
            count,
            1,
            "same method must not appear twice: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_wrong_location_returns_empty() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class(
                "com/example",
                "Parent",
                None,
                vec![method("doWork", "()V", ACC_PUBLIC)],
            ),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "pub".to_string(),
                receiver_expr: "obj".to_string(),
                arguments: None,
            },
            "pub",
            vec![],
            Some(Arc::from("Child")),
            Some(Arc::from("com/example/Child")),
            None,
            vec![],
        );
        assert!(
            OverrideProvider
                .provide(root_scope(), &ctx, &idx.view(root_scope()))
                .is_empty()
        );
    }

    #[test]
    fn test_object_methods_appear_when_no_explicit_superclass() {
        // Object 的 toString / equals / hashCode 应当出现
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            // Object 本身
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                annotations: vec![],
                super_name: None,
                interfaces: vec![],
                methods: vec![
                    method("toString", "()Ljava/lang/String;", ACC_PUBLIC),
                    method("equals", "(Ljava/lang/Object;)Z", ACC_PUBLIC),
                    method("hashCode", "()I", ACC_PUBLIC),
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            // 没有显式 super_name 的类
            make_class("com/example", "Plain", None, vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Plain");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();

        assert!(
            labels.iter().any(|l| l.contains("toString")),
            "toString should appear: {:?}",
            labels
        );
        assert!(
            labels.iter().any(|l| l.contains("equals")),
            "equals should appear: {:?}",
            labels
        );
        assert!(
            labels.iter().any(|l| l.contains("hashCode")),
            "hashCode should appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_object_methods_not_duplicated_when_already_in_mro() {
        // 如果 mro 里已经有 Object（通过显式继承链走到），不应重复
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                annotations: vec![],
                super_name: None,
                interfaces: vec![],
                methods: vec![method("toString", "()Ljava/lang/String;", ACC_PUBLIC)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            make_class("com/example", "Parent", Some("java/lang/Object"), vec![]),
            make_class("com/example", "Child", Some("com/example/Parent"), vec![]),
        ]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let count = results
            .iter()
            .filter(|c| c.label.contains("toString"))
            .count();
        assert_eq!(
            count,
            1,
            "toString must appear exactly once: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    fn make_interface(pkg: &str, name: &str, methods: Vec<MethodSummary>) -> ClassMetadata {
        use rust_asm::constants::ACC_INTERFACE;
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name).as_str()),
            annotations: vec![],
            super_name: None,
            interfaces: vec![],
            methods,
            fields: vec![],
            // interface class 自身的 access_flags 带 ACC_INTERFACE，但不影响方法遍历
            access_flags: ACC_PUBLIC | ACC_INTERFACE,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn abstract_method(name: &str, descriptor: &str) -> MethodSummary {
        use rust_asm::constants::ACC_ABSTRACT;
        MethodSummary {
            name: Arc::from(name),
            annotations: vec![],
            params: MethodParams::from_method_descriptor(descriptor),
            access_flags: ACC_PUBLIC | ACC_ABSTRACT,
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn default_method(name: &str, descriptor: &str) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: ACC_PUBLIC, // default method: public, non-abstract, non-static
            is_synthetic: false,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    #[test]
    fn test_interface_abstract_method_shown() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        // 一个实现了 Runnable 但尚未实现 run() 的类
        let mut cls = make_class("com/example", "MyTask", None, vec![]);
        cls.interfaces = vec![Arc::from("com/example/Runnable")];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/MyTask");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            labels.iter().any(|l| l.contains("run")),
            "abstract interface method run() should appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_interface_default_method_shown() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "String", None, vec![]),
            make_interface(
                "com/example",
                "Greeter",
                vec![default_method("greet", "()Ljava/lang/String;")],
            ),
        ]);
        let mut cls = make_class("com/example", "HelloGreeter", None, vec![]);
        cls.interfaces = vec![Arc::from("com/example/Greeter")];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/HelloGreeter");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.contains("greet")),
            "default interface method should be overridable: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_interface_method_excluded_when_already_implemented_in_index() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        let mut cls = make_class(
            "com/example",
            "MyTask",
            None,
            vec![
                method("run", "()V", ACC_PUBLIC), // 已实现
            ],
        );
        cls.interfaces = vec![Arc::from("com/example/Runnable")];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/MyTask");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| !c.label.contains("run")),
            "already implemented run() must not appear: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_interface_method_excluded_via_source_members() {
        // 未编译，只有 source members
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        let mut cls = make_class("com/example", "MyTask", None, vec![]);
        cls.interfaces = vec![Arc::from("com/example/Runnable")];
        idx.add_classes(vec![cls]);

        let source_member = CurrentClassMember::Method(Arc::new(method("run", "()V", ACC_PUBLIC)));
        let ctx = ctx_with_prefix("pub", "com/example/MyTask")
            .with_class_members(std::iter::once(source_member));

        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().all(|c| !c.label.contains("run")),
            "run() in source members must be excluded: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_multiple_interfaces_all_shown() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_interface(
                "com/example",
                "Runnable",
                vec![abstract_method("run", "()V")],
            ),
            make_interface(
                "com/example",
                "Closeable",
                vec![abstract_method("close", "()V")],
            ),
        ]);
        let mut cls = make_class("com/example", "Resource", None, vec![]);
        cls.interfaces = vec![
            Arc::from("com/example/Runnable"),
            Arc::from("com/example/Closeable"),
        ];
        idx.add_classes(vec![cls]);

        let ctx = ctx_with_prefix("pub", "com/example/Resource");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<_> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            labels.iter().any(|l| l.contains("run")),
            "run should appear: {:?}",
            labels
        );
        assert!(
            labels.iter().any(|l| l.contains("close")),
            "close should appear: {:?}",
            labels
        );
    }

    #[test]
    fn test_interface_method_not_duplicated_via_superclass_and_interface() {
        // Parent 实现了 Runnable，Child extends Parent —— run() 只应出现一次
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_interface(
            "com/example",
            "Runnable",
            vec![abstract_method("run", "()V")],
        )]);
        let mut parent = make_class("com/example", "Parent", None, vec![]);
        parent.interfaces = vec![Arc::from("com/example/Runnable")];
        let child = make_class("com/example", "Child", Some("com/example/Parent"), vec![]);
        idx.add_classes(vec![parent, child]);

        let ctx = ctx_with_prefix("pub", "com/example/Child");
        let results = OverrideProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let count = results.iter().filter(|c| c.label.contains("run")).count();
        assert_eq!(
            count,
            1,
            "run() must not be duplicated: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }
}
