use crate::index::{FieldSummary, IndexView, MethodSummary};
use crate::language::java::super_support::{is_super_receiver_expr, resolve_direct_super_owner};
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{CursorLocation, SemanticContext};
use crate::semantic::types::TypeResolver;
use crate::semantic::types::type_name::TypeName;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum ResolvedSymbol {
    Class(Arc<str>),
    Method {
        owner: Arc<str>,
        summary: Arc<MethodSummary>,
    },
    Field {
        owner: Arc<str>,
        summary: Arc<FieldSummary>,
    },
}

pub struct SymbolResolver<'a> {
    pub view: &'a IndexView,
    caches: RefCell<SymbolResolverCaches>,
}

#[derive(Default)]
struct SymbolResolverCaches {
    type_names: HashMap<TypeNameCacheKey, Option<Arc<str>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TypeNameCacheKey {
    name: Arc<str>,
    enclosing_internal: Option<Arc<str>>,
    package: Option<Arc<str>>,
}

impl<'a> SymbolResolver<'a> {
    pub fn new(view: &'a IndexView) -> Self {
        Self {
            view,
            caches: RefCell::new(SymbolResolverCaches::default()),
        }
    }

    pub fn resolve(&self, ctx: &SemanticContext) -> Option<ResolvedSymbol> {
        match &ctx.location {
            CursorLocation::MemberAccess { .. } => {
                let receiver_expr = ctx.location.member_access_expr()?;
                let member_prefix = ctx.location.member_access_prefix()?;
                let arguments = ctx.location.member_access_arguments();
                let owner = ctx
                    .location
                    .member_access_receiver_owner_internal()
                    .map(Arc::from)
                    .or_else(|| self.infer_receiver_type(ctx, receiver_expr));
                tracing::debug!(
                    receiver_expr = %receiver_expr,
                    member = %member_prefix,
                    resolved_owner = ?owner,
                    "resolve: member access"
                );
                self.resolve_member(ctx, &owner?, member_prefix, arguments)
            }
            CursorLocation::StaticAccess {
                class_internal_name,
                member_prefix,
            } => {
                if let Some(inner) = self
                    .view
                    .resolve_direct_inner_class(class_internal_name, member_prefix)
                {
                    return Some(ResolvedSymbol::Class(Arc::clone(&inner.internal_name)));
                }
                self.resolve_member(ctx, class_internal_name, member_prefix, None)
            }
            CursorLocation::Expression { prefix } => {
                if prefix.is_empty() {
                    return None;
                }
                self.resolve_bare_id(ctx, prefix)
            }
            CursorLocation::ConstructorCall { class_prefix, .. } => {
                if class_prefix.is_empty() {
                    return None;
                }
                tracing::debug!(class = %class_prefix, "resolve: constructor call");
                self.resolve_type_name(ctx, class_prefix)
                    .map(ResolvedSymbol::Class)
            }
            CursorLocation::TypeAnnotation { prefix } => {
                if prefix.is_empty() {
                    return None;
                }
                self.resolve_type_name(ctx, prefix)
                    .map(ResolvedSymbol::Class)
            }
            CursorLocation::Annotation { prefix, .. } => {
                if prefix.is_empty() {
                    return None;
                }
                self.resolve_type_name(ctx, prefix)
                    .map(ResolvedSymbol::Class)
            }
            _ => None,
        }
    }

    fn resolve_member(
        &self,
        ctx: &SemanticContext,
        owner: &str,
        name: &str,
        arguments: Option<&str>,
    ) -> Option<ResolvedSymbol> {
        if name.is_empty() {
            return None;
        }

        let methods = self.view.lookup_methods_in_hierarchy(owner, name);
        let named_candidates: Vec<&Arc<MethodSummary>> = methods.iter().collect();

        if let Some(args) = arguments {
            let arg_text = args.trim().trim_start_matches('(').trim_end_matches(')');
            let arg_texts = split_args(arg_text);
            let arg_count = if arg_text.trim().is_empty() {
                0
            } else {
                arg_texts.len()
            };

            let resolver = TypeResolver::new(self.view);
            let arg_types: Vec<TypeName> = arg_texts
                .iter()
                .map(|arg| {
                    let resolved = resolver.resolve(
                        arg.trim(),
                        &ctx.local_variables,
                        ctx.enclosing_internal_name.as_ref(),
                    );
                    resolved.unwrap_or_else(|| TypeName::new("unknown"))
                })
                .collect();

            if !named_candidates.is_empty() {
                let summaries: Vec<&MethodSummary> =
                    named_candidates.iter().map(|m| m.as_ref()).collect();
                let best_summary = resolver
                    .select_overload_match(&summaries, arg_count, &arg_types)?
                    .method;

                if let Some(found_arc) = named_candidates
                    .iter()
                    .find(|m| m.desc() == best_summary.desc())
                {
                    return Some(ResolvedSymbol::Method {
                        owner: Arc::from(owner),
                        summary: (*found_arc).clone(),
                    });
                }
            }

            // 如果带参数调用但没找到任何匹配的方法，直接在这里结束，不要去尝试字段或无参兜底
            return None;
        }

        if let Some(f) = self.view.lookup_field_in_hierarchy(owner, name) {
            return Some(ResolvedSymbol::Field {
                owner: Arc::from(owner),
                summary: f,
            });
        }

        // 最后才尝试返回第一个匹配的同名方法
        if let Some(m) = named_candidates.first() {
            return Some(ResolvedSymbol::Method {
                owner: Arc::from(owner),
                summary: (*m).clone(),
            });
        }

        None
    }

    fn resolve_bare_id(&self, ctx: &SemanticContext, id: &str) -> Option<ResolvedSymbol> {
        if is_super_receiver_expr(id) {
            return resolve_direct_super_owner(ctx, self.view).map(ResolvedSymbol::Class);
        }

        // local variable -> return its type
        if let Some(local) = ctx.local_variables.iter().find(|v| v.name.as_ref() == id) {
            let base = local.type_internal.erased_internal();
            let resolved_type = self
                .resolve_type_name(ctx, base)
                .unwrap_or_else(|| Arc::from(base));
            return Some(ResolvedSymbol::Class(resolved_type));
        }

        // this member
        if let Some(enclosing) = &ctx.enclosing_internal_name {
            tracing::debug!(
                enclosing = %enclosing,
                id = %id,
                "resolve: bare id in enclosing class"
            );

            if let Some(res) = self.resolve_member(ctx, enclosing, id, None) {
                return Some(res);
            }
        } else {
            tracing::debug!(id = %id, "resolve: enclosing_internal_name is None");
        }

        // type name
        tracing::debug!(id = %id, "resolve: trying as type name");
        self.resolve_type_name(ctx, id).map(ResolvedSymbol::Class)
    }

    fn infer_receiver_type(&self, ctx: &SemanticContext, expr: &str) -> Option<Arc<str>> {
        // handle constructor calls
        if let Some(rest) = expr.strip_prefix("new ") {
            let boundary = rest.find(['(', '<', '[', '{']).unwrap_or(rest.len());
            let ty = rest[..boundary].trim();
            if !ty.is_empty()
                && let Some(internal) = self.resolve_type_name(ctx, ty)
            {
                return Some(internal);
            }
        }

        let as_internal = expr.replace('.', "/");
        if self.view.get_class(&as_internal).is_some() {
            return Some(Arc::from(as_internal));
        }

        if expr.is_empty() || expr == "this" {
            return ctx.enclosing_internal_name.clone();
        }
        if is_super_receiver_expr(expr) {
            return resolve_direct_super_owner(ctx, self.view);
        }

        // local variable
        if let Some(lv) = ctx.local_variables.iter().find(|v| v.name.as_ref() == expr) {
            let t = Arc::from(lv.type_internal.erased_internal());
            tracing::debug!(expr = %expr, type_ = %t, "resolve: receiver type from local var");
            return Some(t);
        }
        if !expr.contains('.') {
            // Simple identifier: used as a type name (for accessing static fields such as System.xxx)
            return self.resolve_type_name(ctx, expr);
        }
        // Chained field access: System.out -> java/lang/System -> field out -> java/io/PrintStream
        self.resolve_chained(ctx, expr)
    }

    /// Iterate through each field in the dotted expression and return the internal name of the final type.
    fn resolve_chained(&self, ctx: &SemanticContext, expr: &str) -> Option<Arc<str>> {
        let mut parts = expr.split('.');
        let first = parts.next()?;

        let mut current: Arc<str> = if first == "this" {
            ctx.enclosing_internal_name.clone()?
        } else if is_super_receiver_expr(first) {
            resolve_direct_super_owner(ctx, self.view)?
        } else if let Some(lv) = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == first)
        {
            Arc::from(lv.type_internal.erased_internal())
        } else {
            self.resolve_type_name(ctx, first)?
        };

        for part in parts {
            if let Some(inner) = self.view.resolve_direct_inner_class(&current, part) {
                current = Arc::clone(&inner.internal_name);
                continue;
            }
            tracing::debug!(owner = %current, field = %part, "resolve: chained field lookup");
            let field = self.view.lookup_field_in_hierarchy(&current, part)?;
            current = descriptor_to_internal_arc(&field.descriptor)?;
        }

        tracing::debug!(expr = %expr, result = %current, "resolve: chained resolved");
        Some(current)
    }

    pub fn resolve_type_name(&self, ctx: &SemanticContext, name: &str) -> Option<Arc<str>> {
        let cache_key = TypeNameCacheKey {
            name: Arc::from(name),
            enclosing_internal: ctx.enclosing_internal_name.clone(),
            package: ctx
                .enclosing_package
                .clone()
                .or_else(|| ctx.inferred_package.clone()),
        };
        if let Some(cached) = self.caches.borrow().type_names.get(&cache_key) {
            return cached.clone();
        }

        let resolve_head = |head: &str| self.resolve_type_head(ctx, head);
        let resolved = self
            .view
            .resolve_qualified_type_path(name, &resolve_head)
            .map(|c| Arc::clone(&c.internal_name));
        if resolved.is_none() {
            tracing::debug!(name = %name, "resolve: type not found in index");
        }
        self.caches
            .borrow_mut()
            .type_names
            .insert(cache_key, resolved.clone());
        resolved
    }

    fn resolve_type_head(&self, ctx: &SemanticContext, head: &str) -> Option<Arc<str>> {
        if head.contains('/') {
            return self.view.get_class(head).map(|c| c.internal_name.clone());
        }
        if head.contains('.') {
            let as_internal = head.replace('.', "/");
            if let Some(cls) = self.view.get_class(&as_internal) {
                return Some(Arc::clone(&cls.internal_name));
            }
        }
        if let Some(type_ctx) = ctx.extension::<SourceTypeCtx>() {
            if let Some(resolved) = type_ctx.resolve_simple_strict(head) {
                return Some(Arc::from(resolved));
            }
            if let Some(resolved) = type_ctx.resolve_type_name_strict(head) {
                return Some(Arc::from(resolved.erased_internal()));
            }
        }
        if let Some(enclosing) = ctx.enclosing_internal_name.as_deref()
            && let Some(inner) = self.view.resolve_scoped_inner_class(enclosing, head)
        {
            return Some(Arc::clone(&inner.internal_name));
        }
        None
    }
}

/// `Ljava/io/PrintStream;` -> `java/io/PrintStream`
fn descriptor_to_internal_arc(desc: &str) -> Option<Arc<str>> {
    if desc.starts_with('L') && desc.ends_with(';') {
        Some(Arc::from(&desc[1..desc.len() - 1]))
    } else {
        // Primitive types, arrays: not navigable to classes
        None
    }
}

fn split_args(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '<' | '[' | '{' => depth += 1,
            ')' | '>' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                result.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        result.push(s[start..].to_string());
    }
    result
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, ModuleId,
        WorkspaceIndex,
    };
    use crate::language::java::class_parser::parse_java_source_with_test_jdk;
    use crate::language::java::type_ctx::SourceTypeCtx;
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use rust_asm::constants::ACC_PUBLIC;

    #[test]
    fn test_overload_resolution_in_symbol_resolver() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![ClassMetadata {
                package: Some(Arc::from("java/io")),
                name: Arc::from("PrintStream"),
                internal_name: Arc::from("java/io/PrintStream"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("println"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: None,
                    },
                    MethodSummary {
                        name: Arc::from("println"),
                        params: MethodParams::from([("I", "x")]), // int overload
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: None,
                    },
                    MethodSummary {
                        name: Arc::from("println"),
                        params: MethodParams::from([("Ljava/lang/String;", "x")]), // string overload
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: None,
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            }],
        );

        // 测试 1: 解析 System.out.println(1) 应该落在 (I)V
        let ctx_int = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from("java/io/PrintStream")),
                receiver_expr: "out".to_string(),
                member_prefix: "println".to_string(),
                arguments: Some("(1)".to_string()),
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let sym_int = resolver.resolve(&ctx_int).unwrap();
        if let ResolvedSymbol::Method { summary, .. } = sym_int {
            assert_eq!(
                summary.desc().as_ref(),
                "(I)V",
                "Should resolve to int overload"
            );
        } else {
            panic!("Expected Method");
        }

        // 测试 2: 解析 System.out.println("hello") 应该落在 (Ljava/lang/String;)V
        let ctx_str = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from("java/io/PrintStream")),
                receiver_expr: "out".to_string(),
                member_prefix: "println".to_string(),
                arguments: Some("(\"hello\")".to_string()),
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let sym_str = resolver.resolve(&ctx_str).unwrap();
        if let ResolvedSymbol::Method { summary, .. } = sym_str {
            assert_eq!(
                summary.desc().as_ref(),
                "(Ljava/lang/String;)V",
                "Should resolve to String overload"
            );
        } else {
            panic!("Expected Method");
        }

        let ctx_invalid = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from("java/io/PrintStream")),
                receiver_expr: "out".to_string(),
                member_prefix: "println".to_string(),
                arguments: Some("(\"a\", \"b\", \"c\")".to_string()),
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        assert!(
            resolver.resolve(&ctx_invalid).is_none(),
            "no applicable overload should not resolve arbitrarily"
        );
    }

    #[test]
    fn test_overload_resolution_in_symbol_resolver_source_varargs_join_many_args() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let src = indoc::indoc! {r#"
            public class VarargsExample {
                public static String join(String separator, String... parts) {
                    return "";
                }
            }
        "#};
        let origin = ClassOrigin::SourceFile(Arc::from("file:///tmp/VarargsExample.java"));
        let classes = parse_java_source_with_test_jdk(
            src,
            origin.clone(),
            &["java/lang/Object", "java/lang/String"],
        );
        idx.update_source(scope, origin, classes);
        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                receiver_expr: "".to_string(),
                member_prefix: "join".to_string(),
                arguments: Some("(\"-\", \"java\", \"lsp\", \"test\")".to_string()),
            },
            "join",
            vec![],
            Some(Arc::from("VarargsExample")),
            Some(Arc::from("VarargsExample")),
            None,
            vec![],
        );
        let resolved = resolver.resolve(&ctx);
        let ResolvedSymbol::Method { summary, .. } = resolved.expect("resolved varargs method")
        else {
            panic!("expected method");
        };
        assert!(
            summary.desc().as_ref().contains("[Ljava/lang/String;"),
            "expected varargs descriptor shape, got {}",
            summary.desc()
        );
    }

    #[test]
    fn test_source_resolution_is_independent_of_method_declaration_order() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let src = indoc::indoc! {r#"
            public class VarargsExample {
                public static void main(String[] args) {
                    join("-", "java", "lsp", "test");
                }

                public static String join(String separator, String... parts) {
                    return "";
                }
            }
        "#};
        let origin = ClassOrigin::SourceFile(Arc::from("file:///tmp/VarargsExample.java"));
        let classes = parse_java_source_with_test_jdk(
            src,
            origin.clone(),
            &["java/lang/Object", "java/lang/String"],
        );
        idx.update_source(scope, origin, classes);
        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                receiver_expr: "".to_string(),
                member_prefix: "join".to_string(),
                arguments: Some("(\"-\", \"java\", \"lsp\", \"test\")".to_string()),
            },
            "join",
            vec![],
            Some(Arc::from("VarargsExample")),
            Some(Arc::from("VarargsExample")),
            None,
            vec![],
        );
        let resolved = resolver.resolve(&ctx).expect("resolved source method");
        let ResolvedSymbol::Method { summary, .. } = resolved else {
            panic!("expected method");
        };
        assert_eq!(summary.name.as_ref(), "join");
    }

    #[test]
    fn test_member_access_resolve_prefers_semantic_owner_over_legacy_receiver_type() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("size"),
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
            }],
        );

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::with_args(
                    "java/util/List",
                    vec![TypeName::new("java/lang/String")],
                )),
                receiver_type: Some(Arc::from("legacy/Wrong")),
                receiver_expr: "xs".to_string(),
                member_prefix: "size".to_string(),
                arguments: None,
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let resolved = resolver.resolve(&ctx).expect("should resolve method");
        match resolved {
            ResolvedSymbol::Method { owner, summary } => {
                assert_eq!(owner.as_ref(), "java/util/List");
                assert_eq!(summary.name.as_ref(), "size");
            }
            _ => panic!("Expected method resolution"),
        }
    }

    #[test]
    fn test_member_access_resolve_falls_back_to_legacy_receiver_type() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("size"),
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
            }],
        );

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from("java/util/List")),
                receiver_expr: "xs".to_string(),
                member_prefix: "size".to_string(),
                arguments: None,
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let resolved = resolver.resolve(&ctx).expect("should resolve method");
        match resolved {
            ResolvedSymbol::Method { owner, summary } => {
                assert_eq!(owner.as_ref(), "java/util/List");
                assert_eq!(summary.name.as_ref(), "size");
            }
            _ => panic!("Expected method resolution"),
        }
    }

    #[test]
    fn test_resolve_type_name_prefers_scoped_inner_class_when_strict_rules_fail() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("ClassWithGenerics"),
                    internal_name: Arc::from("org/cubewhy/ClassWithGenerics"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/ClassWithGenerics$Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: Some(Arc::from("ClassWithGenerics")),
                    generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
            ],
        );
        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        ));
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(type_ctx);

        let resolved = resolver.resolve_type_name(&ctx, "Box");
        assert_eq!(
            resolved.as_deref(),
            Some("org/cubewhy/ClassWithGenerics$Box"),
            "scoped inner class should resolve when strict rules cannot prove top-level Box"
        );
    }

    #[test]
    fn test_resolve_type_name_keeps_top_level_resolution_before_inner_fallback() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("ClassWithGenerics"),
                    internal_name: Arc::from("org/cubewhy/ClassWithGenerics"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/ClassWithGenerics$Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: Some(Arc::from("ClassWithGenerics")),
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
            ],
        );
        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        ));
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(type_ctx);

        let resolved = resolver.resolve_type_name(&ctx, "Box");
        assert_eq!(
            resolved.as_deref(),
            Some("org/cubewhy/Box"),
            "top-level same-package class should still win before scoped inner fallback"
        );
    }

    #[test]
    fn test_resolve_type_name_does_not_pick_unrelated_inner_class() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("ClassWithGenerics"),
                    internal_name: Arc::from("org/cubewhy/ClassWithGenerics"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Other"),
                    internal_name: Arc::from("org/cubewhy/Other"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/Other$Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: Some(Arc::from("Other")),
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
            ],
        );
        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        ));
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(type_ctx);

        let resolved = resolver.resolve_type_name(&ctx, "Box");
        assert!(
            resolved.is_none(),
            "unrelated inner class should not be guessed across enclosing scope"
        );
    }

    #[test]
    fn test_static_access_resolves_nested_type_member() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![
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
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: Some(Arc::from("ChainCheck")),
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
            ],
        );
        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("org/cubewhy/ChainCheck"),
                member_prefix: "Box".to_string(),
            },
            "Box",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );

        let resolved = resolver
            .resolve(&ctx)
            .expect("resolve nested type from static access");
        match resolved {
            ResolvedSymbol::Class(owner) => {
                assert_eq!(owner.as_ref(), "org/cubewhy/ChainCheck$Box");
            }
            _ => panic!("expected class resolution"),
        }
    }

    #[test]
    fn test_resolve_type_name_supports_qualified_nested_type_path() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![
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
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: Some(Arc::from("ChainCheck")),
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("BoxV"),
                    internal_name: Arc::from("org/cubewhy/ChainCheck$Box$BoxV"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: Some(Arc::from("Box")),
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
            ],
        );
        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        ));
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(type_ctx);

        let resolved = resolver.resolve_type_name(&ctx, "ChainCheck.Box.BoxV");
        assert_eq!(resolved.as_deref(), Some("org/cubewhy/ChainCheck$Box$BoxV"));
    }

    #[test]
    fn test_resolve_bare_super_to_source_hint_superclass() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![
                ClassMetadata {
                    package: Some(Arc::from("java/lang")),
                    name: Arc::from("Object"),
                    internal_name: Arc::from("java/lang/Object"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Base"),
                    internal_name: Arc::from("org/cubewhy/Base"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
            ],
        );

        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let type_ctx = Arc::new(
            SourceTypeCtx::from_view(Some(Arc::from("org/cubewhy")), vec![], view.clone())
                .with_current_class_super_name(Some(Arc::from("Base"))),
        );
        let ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "super".to_string(),
            },
            "super",
            vec![],
            Some(Arc::from("Child")),
            Some(Arc::from("org/cubewhy/Child")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(type_ctx);

        let resolved = resolver.resolve(&ctx).expect("resolve bare super");
        match resolved {
            ResolvedSymbol::Class(owner) => {
                assert_eq!(owner.as_ref(), "org/cubewhy/Base");
            }
            other => panic!("expected class resolution for super, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_member_access_from_super_receiver() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_jar_classes(
            scope,
            vec![
                ClassMetadata {
                    package: Some(Arc::from("java/lang")),
                    name: Arc::from("Object"),
                    internal_name: Arc::from("java/lang/Object"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Base"),
                    internal_name: Arc::from("org/cubewhy/Base"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("baseWork"),
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
                },
            ],
        );

        let view = idx.view(scope);
        let resolver = SymbolResolver::new(&view);
        let type_ctx = Arc::new(
            SourceTypeCtx::from_view(Some(Arc::from("org/cubewhy")), vec![], view.clone())
                .with_current_class_super_name(Some(Arc::from("Base"))),
        );
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                receiver_expr: "super".to_string(),
                member_prefix: "baseWork".to_string(),
                arguments: None,
            },
            "baseWork",
            vec![],
            Some(Arc::from("Child")),
            Some(Arc::from("org/cubewhy/Child")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(type_ctx);

        let resolved = resolver.resolve(&ctx).expect("resolve super member");
        match resolved {
            ResolvedSymbol::Method { owner, summary } => {
                assert_eq!(owner.as_ref(), "org/cubewhy/Base");
                assert_eq!(summary.name.as_ref(), "baseWork");
            }
            other => panic!("expected method resolution for super member, got {other:?}"),
        }
    }
}
