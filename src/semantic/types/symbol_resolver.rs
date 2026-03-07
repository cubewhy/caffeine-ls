use crate::index::{FieldSummary, IndexView, MethodSummary};
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{CursorLocation, SemanticContext};
use crate::semantic::types::TypeResolver;
use crate::semantic::types::type_name::TypeName;
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
}

impl<'a> SymbolResolver<'a> {
    pub fn new(view: &'a IndexView) -> Self {
        Self { view }
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
            } => self.resolve_member(ctx, class_internal_name, member_prefix, None),
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

        let (methods, fields) = self.view.collect_inherited_members(owner);

        let named_candidates: Vec<&Arc<MethodSummary>> =
            methods.iter().filter(|m| m.name.as_ref() == name).collect();

        if let Some(args) = arguments {
            let arg_text = args.trim().trim_start_matches('(').trim_end_matches(')');
            let arg_texts = split_args(arg_text);
            let arg_count = if arg_text.trim().is_empty() {
                0
            } else {
                arg_texts.len() as i32
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
                let best_summary = resolver.select_overload(&summaries, arg_count, &arg_types)?;

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

        // 如果没有参数 (arguments == None)，例如跳转到方法引用或字段
        // 优先匹配字段 (Field)
        if let Some(f) = fields.iter().find(|f| f.name.as_ref() == name) {
            return Some(ResolvedSymbol::Field {
                owner: Arc::from(owner),
                summary: f.clone(),
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
            tracing::debug!(owner = %current, field = %part, "resolve: chained field lookup");
            let (_, fields) = self.view.collect_inherited_members(&current);
            let field = fields.iter().find(|f| f.name.as_ref() == part)?;
            current = descriptor_to_internal_arc(&field.descriptor)?;
        }

        tracing::debug!(expr = %expr, result = %current, "resolve: chained resolved");
        Some(current)
    }

    pub fn resolve_type_name(&self, ctx: &SemanticContext, name: &str) -> Option<Arc<str>> {
        if name.contains('/') {
            return self.view.get_class(name).map(|c| c.internal_name.clone());
        }

        if name.contains('.') {
            let as_internal = name.replace('.', "/");
            return self
                .view
                .get_class(&as_internal)
                .map(|c| c.internal_name.clone());
        }

        if let Some(type_ctx) = ctx.extension::<SourceTypeCtx>()
            && let Some(resolved) = type_ctx.resolve_simple_strict(name)
        {
            return Some(Arc::from(resolved));
        }

        tracing::debug!(name = %name, "resolve: type not found in index");
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
}
