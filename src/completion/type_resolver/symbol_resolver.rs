use crate::completion::context::{CompletionContext, CursorLocation};
use crate::completion::type_resolver::TypeResolver;
use crate::completion::type_resolver::type_name::TypeName;
use crate::index::{FieldSummary, GlobalIndex, MethodSummary};
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
    pub index: &'a GlobalIndex,
}

impl<'a> SymbolResolver<'a> {
    pub fn new(index: &'a GlobalIndex) -> Self {
        Self { index }
    }

    pub fn resolve(&self, ctx: &CompletionContext) -> Option<ResolvedSymbol> {
        match &ctx.location {
            CursorLocation::MemberAccess {
                receiver_type,
                receiver_expr,
                member_prefix,
                arguments,
                ..
            } => {
                let owner = receiver_type
                    .clone()
                    .or_else(|| self.infer_receiver_type(ctx, receiver_expr));
                tracing::debug!(
                    receiver_expr = %receiver_expr,
                    member = %member_prefix,
                    resolved_owner = ?owner,
                    "resolve: member access"
                );
                self.resolve_member(ctx, &owner?, member_prefix, arguments.as_deref())
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
            _ => None,
        }
    }

    fn resolve_member(
        &self,
        ctx: &CompletionContext,
        owner: &str,
        name: &str,
        arguments: Option<&str>,
    ) -> Option<ResolvedSymbol> {
        if name.is_empty() {
            return None;
        }

        let (methods, fields) = self.index.collect_inherited_members(owner);

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

            let resolver = TypeResolver::new(self.index);
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
                    .find(|m| m.descriptor == best_summary.descriptor)
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

    fn resolve_bare_id(&self, ctx: &CompletionContext, id: &str) -> Option<ResolvedSymbol> {
        // local variable -> return its type
        if let Some(local) = ctx.local_variables.iter().find(|v| v.name.as_ref() == id) {
            let base = local.type_internal.base();
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

    fn infer_receiver_type(&self, ctx: &CompletionContext, expr: &str) -> Option<Arc<str>> {
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
        if self.index.get_class(&as_internal).is_some() {
            return Some(Arc::from(as_internal));
        }

        if expr.is_empty() || expr == "this" {
            return ctx.enclosing_internal_name.clone();
        }

        // local variable
        if let Some(lv) = ctx.local_variables.iter().find(|v| v.name.as_ref() == expr) {
            let t = Arc::from(lv.type_internal.base());
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
    fn resolve_chained(&self, ctx: &CompletionContext, expr: &str) -> Option<Arc<str>> {
        let mut parts = expr.split('.');
        let first = parts.next()?;

        let mut current: Arc<str> = if first == "this" {
            ctx.enclosing_internal_name.clone()?
        } else if let Some(lv) = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == first)
        {
            Arc::from(lv.type_internal.base())
        } else {
            self.resolve_type_name(ctx, first)?
        };

        for part in parts {
            tracing::debug!(owner = %current, field = %part, "resolve: chained field lookup");
            let (_, fields) = self.index.collect_inherited_members(&current);
            let field = fields.iter().find(|f| f.name.as_ref() == part)?;
            current = descriptor_to_internal_arc(&field.descriptor)?;
        }

        tracing::debug!(expr = %expr, result = %current, "resolve: chained resolved");
        Some(current)
    }

    pub fn resolve_type_name(&self, ctx: &CompletionContext, name: &str) -> Option<Arc<str>> {
        // already internal name
        if name.contains('/') {
            return Some(Arc::from(name));
        }

        let as_internal = name.replace('.', "/");
        if let Some(c) = self.index.get_class(&as_internal) {
            return Some(c.internal_name.clone());
        }

        // imports
        for import in &ctx.existing_imports {
            let s = import.as_ref();
            // 精确 import: ends with .ClassName
            if s.ends_with(&format!(".{}", name)) {
                let internal = s.replace('.', "/");
                if let Some(c) = self.index.get_class(&internal) {
                    return Some(c.internal_name.clone());
                }
            }
            // wildcard import: com.example.*
            if s.ends_with(".*") {
                let pkg = s.trim_end_matches(".*").replace('.', "/");
                let candidate = format!("{}/{}", pkg, name);
                if let Some(c) = self.index.get_class(&candidate) {
                    return Some(c.internal_name.clone());
                }
            }
        }
        let java_lang = format!("java/lang/{}", name);
        if let Some(c) = self.index.get_class(&java_lang) {
            return Some(c.internal_name.clone());
        }

        // same package
        if let Some(enc) = &ctx.enclosing_internal_name
            && let Some(slash) = enc.rfind('/')
        {
            let pkg = &enc[..slash];
            let candidate = format!("{}/{}", pkg, name);
            if let Some(c) = self.index.get_class(&candidate) {
                return Some(c.internal_name.clone());
            }
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
    use crate::completion::context::{CompletionContext, CursorLocation};
    use crate::index::{ClassMetadata, ClassOrigin, GlobalIndex, MethodParams, MethodSummary};
    use rust_asm::constants::ACC_PUBLIC;

    #[test]
    fn test_overload_resolution_in_symbol_resolver() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/io")),
            name: Arc::from("PrintStream"),
            internal_name: Arc::from("java/io/PrintStream"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                MethodSummary {
                    name: Arc::from("println"),
                    descriptor: Arc::from("()V"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: None,
                },
                MethodSummary {
                    name: Arc::from("println"),
                    descriptor: Arc::from("(I)V"), // int overload
                    params: MethodParams::from([("I", "x")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: None,
                },
                MethodSummary {
                    name: Arc::from("println"),
                    descriptor: Arc::from("(Ljava/lang/String;)V"), // String overload
                    params: MethodParams::from([("Ljava/lang/String;", "x")]),
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
        }]);

        // 测试 1: 解析 System.out.println(1) 应该落在 (I)V
        let ctx_int = CompletionContext::new(
            CursorLocation::MemberAccess {
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

        let resolver = SymbolResolver::new(&idx);
        let sym_int = resolver.resolve(&ctx_int).unwrap();
        if let ResolvedSymbol::Method { summary, .. } = sym_int {
            assert_eq!(
                summary.descriptor.as_ref(),
                "(I)V",
                "Should resolve to int overload"
            );
        } else {
            panic!("Expected Method");
        }

        // 测试 2: 解析 System.out.println("hello") 应该落在 (Ljava/lang/String;)V
        let ctx_str = CompletionContext::new(
            CursorLocation::MemberAccess {
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
                summary.descriptor.as_ref(),
                "(Ljava/lang/String;)V",
                "Should resolve to String overload"
            );
        } else {
            panic!("Expected Method");
        }
    }
}
