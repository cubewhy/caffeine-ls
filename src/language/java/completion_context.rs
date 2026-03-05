use std::sync::Arc;

use crate::completion::parser::parse_chain_from_expr;
use crate::index::IndexView;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::types::symbol_resolver::SymbolResolver;
use crate::semantic::types::type_name::TypeName;
use crate::semantic::types::{
    ChainSegment, TypeResolver, parse_single_type_to_internal, singleton_descriptor_to_type,
};
use crate::semantic::{CursorLocation, LocalVar, SemanticContext};

pub struct ContextEnricher<'a> {
    view: &'a IndexView,
}

impl<'a> ContextEnricher<'a> {
    pub fn new(view: &'a IndexView) -> Self {
        Self { view }
    }

    pub fn enrich(&self, ctx: &mut SemanticContext) {
        let type_ctx = match ctx.extension_arc::<SourceTypeCtx>() {
            Some(ctx) => ctx,
            None => {
                tracing::debug!("enrich_context: missing SourceTypeCtx");
                return;
            }
        };
        {
            let resolver = TypeResolver::new(self.view);
            let to_resolve: Vec<(usize, String)> = ctx
                .local_variables
                .iter()
                .enumerate()
                .filter_map(|(i, lv)| {
                    if lv.type_internal.as_ref() == "var" {
                        lv.init_expr.as_deref().map(|e| (i, e.to_string()))
                    } else {
                        None
                    }
                })
                .collect();

            for (idx_in_vec, init_expr) in to_resolve {
                if let Some(resolved) = resolve_var_init_expr(
                    &init_expr,
                    &ctx.local_variables,
                    ctx.enclosing_internal_name.as_ref(),
                    &resolver,
                    &type_ctx,
                    self.view,
                ) {
                    ctx.local_variables[idx_in_vec].type_internal = resolved;
                }
            }
        }

        if let CursorLocation::MemberAccess { .. } = &ctx.location
            && let CursorLocation::MemberAccess {
                receiver_type,
                receiver_expr,
                ..
            } = &mut ctx.location
                && receiver_type.is_none()
                && !receiver_expr.is_empty()
            {
                let resolver = TypeResolver::new(self.view);
                let resolved = if looks_like_array_access(receiver_expr) {
                    resolve_array_access_type(
                        receiver_expr,
                        &ctx.local_variables,
                        ctx.enclosing_internal_name.as_ref(),
                        &resolver,
                        &type_ctx,
                        self.view,
                    )
                } else {
                    let chain = parse_chain_from_expr(receiver_expr);
                    tracing::debug!(?chain, receiver_expr, "enrich_context: parsed chain");

                    if chain.is_empty() {
                        let r = resolver.resolve(
                            receiver_expr,
                            &ctx.local_variables,
                            ctx.enclosing_internal_name.as_ref(),
                        );
                        tracing::debug!(
                            ?r,
                            receiver_expr,
                            "enrich_context: chain is empty, resolver.resolve returned"
                        );
                        r
                    } else {
                        let r = evaluate_chain(
                            &chain,
                            &ctx.local_variables,
                            ctx.enclosing_internal_name.as_ref(),
                            &resolver,
                            &type_ctx,
                            self.view,
                        );
                        tracing::debug!(?r, "enrich_context: evaluate_chain returned");
                        r
                    }
                };

                tracing::debug!(?resolved, "enrich_context: resolved before final match");

                // If the result is a simple name (without '/'), it needs to be further parsed into an internal name.
                *receiver_type = match resolved {
                    None => {
                        tracing::debug!("enrich_context: final match -> None");
                        None
                    }
                    Some(ref ty) if ty.contains_slash() => Some(Arc::from(ty.base())),
                    Some(ty) => {
                        let r = type_ctx.resolve_type_name_strict(ty.as_str());
                        tracing::debug!(?r, ?ty, "enrich_context: final match -> resolve strict");
                        r.map(|t| Arc::from(t.base()))
                    }
                };
            }

        // receiver_expr 是已知包名 -> 转成 Import
        let import_location: Option<(CursorLocation, String)> =
            if let CursorLocation::MemberAccess {
                receiver_type,
                receiver_expr,
                member_prefix,
                ..
            } = &ctx.location
                && receiver_type.is_none()
            {
                let pkg_normalized = receiver_expr.replace('.', "/");
                if self.view.has_package(&pkg_normalized) {
                    let prefix = format!("{}.{}", receiver_expr, member_prefix);
                    let query = member_prefix.clone();
                    Some((CursorLocation::Import { prefix }, query))
                } else {
                    None
                }
            } else {
                None
            };

        if let Some((loc, query)) = import_location {
            ctx.location = loc;
            ctx.query = query;
        }

        // Resolve `var` local variables
        {
            let resolver = TypeResolver::new(self.view);
            let to_resolve: Vec<(usize, String)> = ctx
                .local_variables
                .iter()
                .enumerate()
                .filter_map(|(i, lv)| {
                    if lv.type_internal.as_ref() == "var" {
                        lv.init_expr.as_deref().map(|e| (i, e.to_string()))
                    } else {
                        None
                    }
                })
                .collect();

            // infer `var`
            for (idx_in_vec, init_expr) in to_resolve {
                if let Some(resolved) = resolve_var_init_expr(
                    &init_expr,
                    &ctx.local_variables,
                    ctx.enclosing_internal_name.as_ref(),
                    &resolver,
                    &type_ctx,
                    self.view,
                ) {
                    ctx.local_variables[idx_in_vec].type_internal = resolved;
                }
            }

            let sym = SymbolResolver::new(self.view);
            let new_types: Vec<TypeName> = ctx
                .local_variables
                .iter()
                .map(|lv| expand_local_type_strict(&sym, ctx, &type_ctx, &lv.type_internal))
                .collect();

            for (lv, new_ty) in ctx.local_variables.iter_mut().zip(new_types) {
                lv.type_internal = new_ty;
            }
        }
    }
}

fn looks_like_array_access(expr: &str) -> bool {
    expr.contains('[') && expr.trim_end().ends_with(']')
}

fn find_matching_angle(s: &str, start: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices().skip(start) {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn expand_local_type_strict(
    sym: &SymbolResolver,
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    ty: &TypeName,
) -> TypeName {
    let s = ty.as_str();

    // primitives/unknown/var 不动
    if matches!(
        s,
        "var"
            | "unknown"
            | "byte"
            | "short"
            | "int"
            | "long"
            | "float"
            | "double"
            | "boolean"
            | "char"
            | "void"
    ) {
        return ty.clone();
    }

    // array dims: Foo[][]
    let mut base = s;
    let mut dims = 0usize;
    while let Some(stripped) = base.strip_suffix("[]") {
        dims += 1;
        base = stripped.trim();
    }

    if let Some(resolved) = type_ctx.resolve_type_name_strict(s) {
        return resolved;
    }
    let base = base.split('<').next().unwrap_or(base).trim();

    // already internal or index match
    if base.contains('/') || sym.view.get_class(base).is_some() {
        return ty.clone();
    }

    let mut out = if let Some(internal) = sym.resolve_type_name(ctx, base) {
        TypeName::from(internal)
    } else {
        ty.clone()
    };

    for _ in 0..dims {
        out = out.wrap_array();
    }
    out
}

fn resolve_array_access_type(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let bracket = expr.rfind('[')?;
    if !expr.trim_end().ends_with(']') {
        return None;
    }
    let array_expr = expr[..bracket].trim();
    if array_expr.is_empty() {
        return None;
    }

    // 统一走解析链，让 evaluate_chain 去应对多级调用
    let chain = parse_chain_from_expr(array_expr);
    let array_type = if chain.is_empty() {
        resolver.resolve(array_expr, locals, enclosing_internal)
    } else {
        evaluate_chain(&chain, locals, enclosing_internal, resolver, type_ctx, view)
    }?;

    array_type.element_type()
}

fn resolve_var_init_expr(
    expr: &str,
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let expr = expr.trim();
    if let Some(rest) = expr.strip_prefix("new ") {
        // 寻找类型声明的边界：可能是普通构造函数 '('、泛型 '<'，或者是数组的 '['、'{'
        let mut boundary_idx = rest.find(['(', '[', '{']).unwrap_or(rest.len());
        if let Some(gen_start) = rest.find('<')
            && gen_start < boundary_idx {
                if let Some(gen_end) = find_matching_angle(rest, gen_start) {
                    boundary_idx = gen_end + 1;
                } else {
                    return None;
                }
            }
        let type_name = rest[..boundary_idx].trim();

        // 解析基础类型，同时为 primitive 类型做白名单兜底
        let resolved_base: TypeName = match type_name {
            "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char" => {
                TypeName::new(type_name)
            }
            _ => type_ctx.resolve_type_name_strict(type_name)?,
        };

        let after_type = rest[boundary_idx..].trim_start();

        if after_type.starts_with('[') || after_type.starts_with('{') {
            let brace_idx = after_type.find('{').unwrap_or(after_type.len());
            let dimensions = after_type[..brace_idx].matches('[').count();
            let mut array_ty = resolved_base;
            for _ in 0..dimensions {
                array_ty = array_ty.wrap_array();
            }
            return Some(array_ty);
        }

        return Some(resolved_base);
    }

    let chain = parse_chain_from_expr(expr);
    if !chain.is_empty() {
        return evaluate_chain(&chain, locals, enclosing_internal, resolver, type_ctx, view);
    }

    resolve_array_access_type(
        expr,
        locals,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
    )
}

/// 统一且健壮的调用链类型推导逻辑 (支持连缀方法调用和字段读取)
fn evaluate_chain(
    chain: &[ChainSegment],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let mut current: Option<TypeName> = None;
    for (i, seg) in chain.iter().enumerate() {
        // 提取 base_name 和 数组维度 (彻底解决 parser 不拆分 [0] 的问题)
        let bracket_idx = seg.name.find('[');
        let base_name = if let Some(idx) = bracket_idx {
            &seg.name[..idx]
        } else {
            &seg.name
        };
        let dimensions = seg.name.matches('[').count();

        if i == 0 {
            if seg.arg_count.is_some() {
                let recv_internal = enclosing_internal?;
                let arg_types: Vec<TypeName> = seg
                    .arg_texts
                    .iter()
                    .filter_map(|t| resolver.resolve(t.trim(), locals, enclosing_internal))
                    .collect();
                let arg_types_ref: &[TypeName] = if arg_types.len() == seg.arg_texts.len() {
                    &arg_types
                } else {
                    &[]
                };
                current = resolver.resolve_method_return(
                    recv_internal.as_ref(),
                    base_name,
                    seg.arg_count.unwrap_or(-1),
                    arg_types_ref,
                );
            } else {
                current = resolver.resolve(base_name, locals, enclosing_internal);
                if current.is_none() {
                    if let Some(enclosing) = enclosing_internal {
                        let enclosing_simple = enclosing
                            .rsplit('/')
                            .next()
                            .unwrap_or(enclosing)
                            .rsplit('$')
                            .next()
                            .unwrap_or(enclosing);

                        if base_name == enclosing_simple {
                            current = Some(TypeName::new(enclosing.as_ref()));
                        }
                    }

                    if current.is_none() {
                        current = type_ctx.resolve_type_name_strict(base_name);
                    }
                }
            }
        } else {
            let recv = current.as_ref()?;

            // 处理形如 `getArr()[0]` 被解析为独立的无名 segment 的情况
            if base_name.is_empty() {
                current = Some(recv.clone());
            } else {
                let recv_str = recv.as_str();
                let recv_full: TypeName = if recv_str.contains('/') {
                    recv.clone()
                } else {
                    type_ctx.resolve_type_name_strict(recv_str)?
                };

                if seg.arg_count.is_some() {
                    let arg_types: Vec<TypeName> = seg
                        .arg_texts
                        .iter()
                        .filter_map(|t| resolver.resolve(t.trim(), locals, enclosing_internal))
                        .collect();
                    let arg_types_ref: &[TypeName] = if arg_types.len() == seg.arg_texts.len() {
                        &arg_types
                    } else {
                        &[]
                    };
                    current = resolver.resolve_method_return(
                        recv_full.as_str(),
                        base_name,
                        seg.arg_count.unwrap_or(-1),
                        arg_types_ref,
                    );
                } else {
                    let (methods, fields) = view.collect_inherited_members(recv_full.base());

                    if let Some(f) = fields.iter().find(|f| f.name.as_ref() == base_name) {
                        if let Some(ty) = singleton_descriptor_to_type(&f.descriptor) {
                            current = Some(TypeName::new(ty));
                        } else {
                            current = parse_single_type_to_internal(&f.descriptor);
                        }
                    } else if methods.iter().any(|m| m.name.as_ref() == base_name) {
                        current = None;
                    } else {
                        current = None;
                    }
                }
            }
        }

        // handle array access dims on segment
        if dimensions > 0 {
            // 使用 take() 拿走所有权，此时 current 自动变为 None
            if let Some(mut ty) = current.take() {
                let mut success = true;
                for _ in 0..dimensions {
                    if let Some(el) = ty.element_type() {
                        ty = el;
                    } else {
                        success = false; // 超出数组维度访问
                        break;
                    }
                }
                // 只有成功降维完毕，才把新的类型装回去
                // 如果失败了，current 保持为 take() 留下的 None
                if success {
                    current = Some(ty);
                }
            }
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::parser::parse_chain_from_expr;
    use crate::index::ModuleId;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, WorkspaceIndex,
    };
    use rust_asm::constants::ACC_PUBLIC;

    fn seg_names(expr: &str) -> Vec<(String, Option<i32>)> {
        parse_chain_from_expr(expr)
            .into_iter()
            .map(|s| (s.name, s.arg_count))
            .collect()
    }

    #[test]
    fn test_chain_simple_variable() {
        // [修复点] "list.ge" -> 应当解析为前后两个完整的 variable
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
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![ClassMetadata {
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
            }],
        );
        idx
    }

    #[test]
    fn test_enrich_context_resolves_simple_name_via_import() {
        let idx = make_index_with_random_class();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.RandomClass".into()],
            Some(name_table),
        ));
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
        )
        .with_extension(type_ctx);
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
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.*".into()],
            Some(name_table),
        ));
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
        )
        .with_extension(type_ctx);
        ContextEnricher::new(&view).enrich(&mut ctx);
        if let CursorLocation::MemberAccess { receiver_type, .. } = &ctx.location {
            assert_eq!(receiver_type.as_deref(), Some("org/cubewhy/RandomClass"),);
        }
    }
}
