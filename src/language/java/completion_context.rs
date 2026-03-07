use std::collections::HashSet;
use std::sync::Arc;

use crate::completion::parser::parse_chain_from_expr;
use crate::index::IndexView;
use crate::index::MethodSummary;
use crate::language::java::location::normalize_top_level_generic_base;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{
    ExpectedType, ExpectedTypeConfidence, ExpectedTypeSource, FunctionalMethodCallHint,
    SamSignature, TypedExpressionContext,
};
use crate::semantic::types::symbol_resolver::SymbolResolver;
use crate::semantic::types::type_name::TypeName;
use crate::semantic::types::{
    ChainSegment, TypeResolver, parse_single_type_to_internal, singleton_descriptor_to_type,
};
use crate::semantic::{CursorLocation, LocalVar, SemanticContext};
use rust_asm::constants::{ACC_ABSTRACT, ACC_STATIC};

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
        let method_ref = match &ctx.location {
            CursorLocation::MethodReference {
                qualifier_expr,
                member_prefix,
                is_constructor,
            } => Some((
                qualifier_expr.clone(),
                member_prefix.clone(),
                *is_constructor,
            )),
            _ => None,
        };
        if let Some((qualifier_expr, member_prefix, is_constructor)) = method_ref {
            if is_constructor {
                ctx.location = CursorLocation::ConstructorCall {
                    class_prefix: qualifier_expr.clone(),
                    expected_type: None,
                };
                ctx.query = qualifier_expr;
            } else {
                ctx.location = CursorLocation::MemberAccess {
                    receiver_semantic_type: None,
                    receiver_type: None,
                    member_prefix: member_prefix.clone(),
                    receiver_expr: qualifier_expr,
                    arguments: None,
                };
                ctx.query = member_prefix;
            }
        }
        {
            let resolver = TypeResolver::new(self.view);
            let to_resolve: Vec<(usize, String)> = ctx
                .local_variables
                .iter()
                .enumerate()
                .filter_map(|(i, lv)| {
                    if lv.type_internal.erased_internal() == "var" {
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
                receiver_semantic_type,
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

            // Normalize to a canonical semantic receiver type before writing either field.
            let resolved_semantic = canonicalize_receiver_semantic(resolved, &type_ctx);

            if receiver_semantic_type.is_none() {
                *receiver_semantic_type = resolved_semantic.clone();
            }

            *receiver_type = resolved_semantic
                .as_ref()
                .map(|t| Arc::from(t.erased_internal()));
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
                    if lv.type_internal.erased_internal() == "var" {
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

        enrich_expected_type_context(ctx, self.view, &type_ctx);
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

fn canonicalize_receiver_semantic(
    resolved: Option<TypeName>,
    type_ctx: &SourceTypeCtx,
) -> Option<TypeName> {
    match resolved {
        None => {
            tracing::debug!("enrich_context: final match -> None");
            None
        }
        Some(ty) if ty.contains_slash() => Some(ty),
        Some(ty) => {
            let ty_str = ty.erased_internal_with_arrays();
            let r = type_ctx.resolve_type_name_strict(&ty_str);
            tracing::debug!(?r, ?ty, "enrich_context: final match -> resolve strict");
            r
        }
    }
}

fn expand_local_type_strict(
    sym: &SymbolResolver,
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    ty: &TypeName,
) -> TypeName {
    // primitives/unknown/var 不动
    if matches!(
        ty.erased_internal(),
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

    let base = ty.erased_internal();

    if ty.args.is_empty() && base.contains('<') {
        if let Some(mut resolved) = type_ctx.resolve_type_name_strict(base) {
            resolved.array_dims = ty.array_dims;
            return resolved;
        }
    }

    let expanded_args: Vec<TypeName> = ty
        .args
        .iter()
        .map(|a| expand_local_type_strict(sym, ctx, type_ctx, a))
        .collect();

    if ty.contains_slash() || sym.view.get_class(base).is_some() {
        return TypeName {
            base_internal: ty.base_internal.clone(),
            args: expanded_args,
            array_dims: ty.array_dims,
        };
    }

    if let Some(mut resolved) = type_ctx.resolve_type_name_strict(base) {
        resolved.args = expanded_args;
        resolved.array_dims = ty.array_dims;
        return resolved;
    }

    if let Some(internal) = sym.resolve_type_name(ctx, base) {
        return TypeName {
            base_internal: internal,
            args: expanded_args,
            array_dims: ty.array_dims,
        };
    }

    ty.clone()
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
            && gen_start < boundary_idx
        {
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

    resolve_array_access_type(expr, locals, enclosing_internal, resolver, type_ctx, view)
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
                let recv_full: TypeName = if recv.contains_slash() {
                    recv.clone()
                } else {
                    type_ctx.resolve_type_name_strict(recv.erased_internal())?
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
                    let receiver_internal = recv_full.to_internal_with_generics();
                    current = resolver.resolve_method_return(
                        &receiver_internal,
                        base_name,
                        seg.arg_count.unwrap_or(-1),
                        arg_types_ref,
                    );
                } else {
                    let (methods, fields) =
                        view.collect_inherited_members(recv_full.erased_internal());

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

fn enrich_expected_type_context(
    ctx: &mut SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
) {
    let Some(hint) = ctx.functional_target_hint.clone() else {
        ctx.typed_expr_ctx = None;
        ctx.expected_functional_interface = None;
        ctx.expected_sam = None;
        return;
    };

    let mut expected = hint
        .expected_type_source
        .as_deref()
        .and_then(|src| resolve_source_type_hint(type_ctx, src))
        .map(|(ty, confidence)| ExpectedType {
            ty,
            source: ExpectedTypeSource::AssignmentRhs,
            confidence,
        });
    let mut receiver_type: Option<TypeName> = None;

    if expected.is_none()
        && let Some(call_hint) = hint.method_call.as_ref()
    {
        let (expected_from_arg, receiver) =
            resolve_expected_type_from_method_argument(ctx, view, type_ctx, call_hint);
        expected = expected_from_arg.map(|(ty, confidence)| ExpectedType {
            ty,
            source: ExpectedTypeSource::MethodArgument {
                arg_index: call_hint.arg_index,
            },
            confidence,
        });
        receiver_type = receiver;
    }

    ctx.typed_expr_ctx = Some(TypedExpressionContext {
        expected_type: expected.clone(),
        receiver_type,
    });

    let expected_ty = expected.as_ref().map(|e| e.ty.clone());
    ctx.expected_functional_interface = expected_ty.clone();
    ctx.expected_sam = expected_ty
        .as_ref()
        .and_then(|ty| extract_sam_signature(view, ty));
}

fn resolve_source_type_hint(
    type_ctx: &SourceTypeCtx,
    src: &str,
) -> Option<(TypeName, ExpectedTypeConfidence)> {
    type_ctx
        .resolve_type_name_relaxed(src)
        .map(|r| {
            let confidence = match r.quality {
                crate::language::java::type_ctx::TypeResolveQuality::Exact => {
                    ExpectedTypeConfidence::Exact
                }
                crate::language::java::type_ctx::TypeResolveQuality::Partial => {
                    ExpectedTypeConfidence::Partial
                }
            };
            (r.ty, confidence)
        })
        .or_else(|| {
            let base = normalize_top_level_generic_base(src);
            type_ctx
                .resolve_simple_strict(base)
                .map(TypeName::new)
                .map(|ty| (ty, ExpectedTypeConfidence::Partial))
        })
}

fn resolve_expected_type_from_method_argument(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    hint: &FunctionalMethodCallHint,
) -> (Option<(TypeName, ExpectedTypeConfidence)>, Option<TypeName>) {
    let resolver = TypeResolver::new(view);
    let receiver =
        match resolve_hint_receiver_type(ctx, type_ctx, view, &resolver, &hint.receiver_expr) {
            Some(r) => r,
            None => return (None, None),
        };
    let receiver_owner = receiver.erased_internal();

    let (methods, _) = view.collect_inherited_members(receiver_owner);
    let candidates: Vec<&MethodSummary> = methods
        .iter()
        .filter(|m| m.name.as_ref() == hint.method_name)
        .map(|m| m.as_ref())
        .collect();
    if candidates.is_empty() {
        return (None, Some(receiver));
    }

    let arg_types: Vec<TypeName> = hint
        .arg_texts
        .iter()
        .map(|arg| {
            resolver
                .resolve(
                    arg,
                    &ctx.local_variables,
                    ctx.enclosing_internal_name.as_ref(),
                )
                .unwrap_or_else(|| TypeName::new("unknown"))
        })
        .collect();
    let arg_count = hint.arg_texts.len() as i32;
    let selected = match resolver.select_overload(&candidates, arg_count, &arg_types) {
        Some(s) => s,
        None => return (None, Some(receiver)),
    };
    let Some(param) = selected.params.items.get(hint.arg_index) else {
        return (None, Some(receiver));
    };
    let expected =
        descriptor_to_type_name(&param.descriptor).map(|ty| (ty, ExpectedTypeConfidence::Exact));
    (expected, Some(receiver))
}

fn resolve_hint_receiver_type(
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolver: &TypeResolver,
    expr: &str,
) -> Option<TypeName> {
    let resolved = if looks_like_array_access(expr) {
        resolve_array_access_type(
            expr,
            &ctx.local_variables,
            ctx.enclosing_internal_name.as_ref(),
            resolver,
            type_ctx,
            view,
        )
    } else {
        let chain = parse_chain_from_expr(expr);
        if chain.is_empty() {
            resolver.resolve(
                expr,
                &ctx.local_variables,
                ctx.enclosing_internal_name.as_ref(),
            )
        } else {
            evaluate_chain(
                &chain,
                &ctx.local_variables,
                ctx.enclosing_internal_name.as_ref(),
                resolver,
                type_ctx,
                view,
            )
        }
    };

    canonicalize_receiver_semantic(resolved, type_ctx)
}

fn extract_sam_signature(view: &IndexView, interface_ty: &TypeName) -> Option<SamSignature> {
    let owner = interface_ty.erased_internal();
    let (methods, _) = view.collect_inherited_members(owner);
    let mut seen = HashSet::new();
    let mut abstract_methods: Vec<Arc<MethodSummary>> = Vec::new();

    for method in methods {
        if (method.access_flags & ACC_ABSTRACT) == 0 {
            continue;
        }
        if (method.access_flags & ACC_STATIC) != 0 {
            continue;
        }
        if is_object_method(method.name.as_ref(), method.desc().as_ref()) {
            continue;
        }
        let key = format!("{}#{}", method.name, method.desc());
        if seen.insert(key) {
            abstract_methods.push(method);
        }
    }

    if abstract_methods.len() != 1 {
        return None;
    }

    let sam = abstract_methods.pop()?;
    let param_types = sam
        .params
        .items
        .iter()
        .map(|p| descriptor_to_type_name(&p.descriptor).unwrap_or_else(|| TypeName::new("unknown")))
        .collect::<Vec<_>>();
    let return_type = sam.return_type.as_deref().and_then(descriptor_to_type_name);

    Some(SamSignature {
        method_name: sam.name.clone(),
        param_types,
        return_type,
    })
}

fn descriptor_to_type_name(desc: &str) -> Option<TypeName> {
    parse_single_type_to_internal(desc)
        .or_else(|| singleton_descriptor_to_type(desc).map(TypeName::new))
}

fn is_object_method(name: &str, desc: &str) -> bool {
    matches!(
        (name, desc),
        ("equals", "(Ljava/lang/Object;)Z")
            | ("hashCode", "()I")
            | ("toString", "()Ljava/lang/String;")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::parser::parse_chain_from_expr;
    use crate::index::ModuleId;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, WorkspaceIndex,
    };
    use rust_asm::constants::{ACC_ABSTRACT, ACC_PUBLIC};

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

    fn make_index_with_functional_types() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: Some(Arc::from("java/lang")),
                    name: Arc::from("Object"),
                    internal_name: Arc::from("java/lang/Object"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![
                        MethodSummary {
                            name: Arc::from("toString"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("Ljava/lang/String;")),
                        },
                        MethodSummary {
                            name: Arc::from("hashCode"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("I")),
                        },
                        MethodSummary {
                            name: Arc::from("equals"),
                            params: MethodParams::from([("Ljava/lang/Object;", "obj")]),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("Z")),
                        },
                    ],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("java/lang")),
                    name: Arc::from("String"),
                    internal_name: Arc::from("java/lang/String"),
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
                ClassMetadata {
                    package: Some(Arc::from("java/util/function")),
                    name: Arc::from("Function"),
                    internal_name: Arc::from("java/util/function/Function"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![
                        MethodSummary {
                            name: Arc::from("apply"),
                            params: MethodParams::from([("Ljava/lang/Object;", "t")]),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("Ljava/lang/Object;")),
                        },
                        MethodSummary {
                            name: Arc::from("andThen"),
                            params: MethodParams::from([(
                                "Ljava/util/function/Function;",
                                "after",
                            )]),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("Ljava/util/function/Function;")),
                        },
                    ],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("java/util/stream")),
                    name: Arc::from("Stream"),
                    internal_name: Arc::from("java/util/stream/Stream"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("map"),
                        params: MethodParams::from([("Ljava/util/function/Function;", "mapper")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/util/stream/Stream;")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
            ],
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
                receiver_semantic_type: None,
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
        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            ..
        } = &ctx.location
        {
            assert_eq!(
                receiver_semantic_type.as_ref().map(|t| t.erased_internal()),
                Some("org/cubewhy/RandomClass"),
                "receiver_semantic_type should preserve resolved TypeName"
            );
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
                receiver_semantic_type: None,
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
        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            ..
        } = &ctx.location
        {
            assert_eq!(
                receiver_semantic_type.as_ref().map(|t| t.erased_internal()),
                Some("org/cubewhy/RandomClass")
            );
            assert_eq!(receiver_type.as_deref(), Some("org/cubewhy/RandomClass"),);
        }
    }

    #[test]
    fn test_enrich_context_does_not_overwrite_existing_receiver_semantic_type() {
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
                receiver_semantic_type: Some(TypeName::new("java/lang/Object")),
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

        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            ..
        } = &ctx.location
        {
            assert_eq!(
                receiver_semantic_type.as_ref().map(|t| t.erased_internal()),
                Some("java/lang/Object")
            );
            assert_eq!(receiver_type.as_deref(), Some("org/cubewhy/RandomClass"));
        }
    }

    #[test]
    fn test_enrich_context_routes_method_reference_to_member_access() {
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
            CursorLocation::MethodReference {
                qualifier_expr: "this".to_string(),
                member_prefix: "toString".to_string(),
                is_constructor: false,
            },
            "toString",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert!(
            matches!(
                ctx.location,
                CursorLocation::MemberAccess {
                    receiver_expr: ref r,
                    member_prefix: ref p,
                    ..
                } if r == "this" && p == "toString"
            ),
            "Expected method reference to normalize to MemberAccess, got {:?}",
            ctx.location
        );
        assert_eq!(ctx.query, "toString");
    }

    #[test]
    fn test_enrich_context_routes_constructor_reference_to_constructor_call() {
        let idx = make_index_with_random_class();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.*".into()],
            Some(name_table),
        ));
        let mut ctx = SemanticContext::new(
            CursorLocation::MethodReference {
                qualifier_expr: "ArrayList".to_string(),
                member_prefix: "new".to_string(),
                is_constructor: true,
            },
            "ArrayList",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert!(
            matches!(
                ctx.location,
                CursorLocation::ConstructorCall {
                    class_prefix: ref c,
                    expected_type: None
                } if c == "ArrayList"
            ),
            "Expected constructor reference to normalize to ConstructorCall, got {:?}",
            ctx.location
        );
        assert_eq!(ctx.query, "ArrayList");
    }

    #[test]
    fn test_enrich_context_populates_expected_functional_type_from_assignment_rhs() {
        let idx = make_index_with_functional_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.function.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::MethodReference {
                qualifier_expr: "String".to_string(),
                member_prefix: "length".to_string(),
                is_constructor: false,
            },
            "length",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.function.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: Some("Function<String, Integer>".to_string()),
            method_call: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert!(matches!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.expected_type.as_ref()),
            Some(crate::semantic::context::ExpectedType {
                source: crate::semantic::context::ExpectedTypeSource::AssignmentRhs,
                confidence: crate::semantic::context::ExpectedTypeConfidence::Partial,
                ..
            })
        ));
        assert_eq!(
            ctx.expected_functional_interface
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("java/util/function/Function")
        );
        assert_eq!(
            ctx.expected_sam.as_ref().map(|s| s.method_name.as_ref()),
            Some("apply")
        );
    }

    #[test]
    fn test_enrich_context_populates_expected_functional_type_from_method_argument() {
        let idx = make_index_with_functional_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.stream.*".into(), "java.util.function.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::MethodReference {
                qualifier_expr: "String".to_string(),
                member_prefix: "trim".to_string(),
                is_constructor: false,
            },
            "trim",
            vec![LocalVar {
                name: Arc::from("stream"),
                type_internal: TypeName::new("java/util/stream/Stream"),
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.stream.*".into(), "java.util.function.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: None,
            method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                receiver_expr: "stream".to_string(),
                method_name: "map".to_string(),
                arg_index: 0,
                arg_texts: vec!["String::trim".to_string()],
            }),
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert!(matches!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.expected_type.as_ref()),
            Some(crate::semantic::context::ExpectedType {
                source: crate::semantic::context::ExpectedTypeSource::MethodArgument {
                    arg_index: 0
                },
                confidence: crate::semantic::context::ExpectedTypeConfidence::Exact,
                ..
            })
        ));
        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.receiver_type.as_ref())
                .map(|t| t.erased_internal()),
            Some("java/util/stream/Stream")
        );
        assert_eq!(
            ctx.expected_functional_interface
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("java/util/function/Function")
        );
        assert_eq!(
            ctx.expected_sam.as_ref().map(|s| s.method_name.as_ref()),
            Some("apply")
        );
    }

    #[test]
    fn test_enrich_context_leaves_expected_sam_none_for_non_functional_type() {
        let idx = make_index_with_functional_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.lang.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "x".to_string(),
            },
            "x",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.lang.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: Some("String".to_string()),
            method_call: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        assert_eq!(
            ctx.expected_functional_interface
                .as_ref()
                .map(|t| t.erased_internal()),
            Some("java/lang/String")
        );
        assert!(
            ctx.expected_sam.is_none(),
            "non-functional type should not produce SAM"
        );
    }

    #[test]
    fn test_enrich_context_assignment_partial_expected_type_preserved() {
        let idx = make_index_with_functional_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.function.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "x".to_string(),
            },
            "x",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.function.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: Some("Function<? super T, ? extends K>".to_string()),
            method_call: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        let expected = ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.expected_type.as_ref())
            .expect("expected type should be present as partial");
        assert_eq!(expected.ty.erased_internal(), "java/util/function/Function");
        assert_eq!(
            expected.confidence,
            crate::semantic::context::ExpectedTypeConfidence::Partial
        );
    }
}
