use std::collections::HashSet;
use std::sync::Arc;

use crate::completion::parser::parse_chain_from_expr;
use crate::index::IndexView;
use crate::index::MethodSummary;
use crate::language::java::location::normalize_top_level_generic_base;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{
    ExpectedType, ExpectedTypeConfidence, ExpectedTypeSource, FunctionalCompat,
    FunctionalCompatStatus, FunctionalExprShape, FunctionalMethodCallHint, MethodRefQualifierKind,
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

        // If the receiver text is a known package, reinterpret this as import completion.
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
            match r {
                Some(mut canonical) => {
                    if !ty.args.is_empty() {
                        canonical.args = ty.args;
                    }
                    canonical.array_dims = ty.array_dims;
                    Some(canonical)
                }
                None => None,
            }
        }
    }
}

fn expand_local_type_strict(
    sym: &SymbolResolver,
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    ty: &TypeName,
) -> TypeName {
    // Leave primitive/unknown/var unchanged.
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

    if ty.args.is_empty()
        && base.contains('<')
    {
        if let Some(mut resolved) = resolve_source_like_type_with_scope(ctx, type_ctx, sym, base) {
            resolved.array_dims = ty.array_dims;
            return resolved;
        }

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

    // Route through chain evaluation so nested calls resolve consistently.
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
        // Find the type boundary before constructor args / generics / array suffix.
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

        // Resolve the base type, with explicit primitive fallback.
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

/// Shared chain type-resolution logic for method calls and field reads.
fn evaluate_chain(
    chain: &[ChainSegment],
    locals: &[LocalVar],
    enclosing_internal: Option<&Arc<str>>,
    resolver: &TypeResolver,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
) -> Option<TypeName> {
    let mut current: Option<TypeName> = None;
    let resolve_qualifier = |q: &str| type_ctx.resolve_type_name_strict(q);
    for (i, seg) in chain.iter().enumerate() {
        // Split `name[index]` into base segment and trailing index dimensions.
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
                current = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                    recv_internal.as_ref(),
                    base_name,
                    seg.arg_count.unwrap_or(-1),
                    arg_types_ref,
                    &seg.arg_texts,
                    locals,
                    enclosing_internal,
                    Some(&resolve_qualifier),
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

            // Handle parser output where `[0]` is emitted as a standalone segment.
            if base_name.is_empty() {
                current = Some(recv.clone());
            } else {
                let recv_full: TypeName = if recv.contains_slash() {
                    recv.clone()
                } else {
                    let mut canonical =
                        type_ctx.resolve_type_name_strict(recv.erased_internal())?;
                    if !recv.args.is_empty() {
                        canonical.args = recv.args.clone();
                    }
                    canonical.array_dims = recv.array_dims;
                    canonical
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
                    current = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                        &receiver_internal,
                        base_name,
                        seg.arg_count.unwrap_or(-1),
                        arg_types_ref,
                        &seg.arg_texts,
                        locals,
                        enclosing_internal,
                        Some(&resolve_qualifier),
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
            // `take()` lets us mutate the current type while preserving failure semantics.
            if let Some(mut ty) = current.take() {
                let mut success = true;
                for _ in 0..dimensions {
                    if let Some(el) = ty.element_type() {
                        ty = el;
                    } else {
                        success = false; // Indexing past array depth.
                        break;
                    }
                }
                // Only commit on successful dimensional reduction; otherwise keep `None`.
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

    let expected_ty = expected.as_ref().map(|e| e.ty.clone());
    let expected_sam = expected_ty
        .as_ref()
        .and_then(|ty| extract_sam_signature(view, ty));
    let functional_compat =
        match (
            hint.expr_shape.as_ref(),
            expected_sam.as_ref(),
            expected.as_ref(),
        ) {
            (Some(shape), Some(sam), Some(expected_type)) => Some(
                evaluate_functional_compatibility(ctx, view, type_ctx, shape, sam, expected_type),
            ),
            _ => None,
        };

    ctx.typed_expr_ctx = Some(TypedExpressionContext {
        expected_type: expected.clone(),
        receiver_type,
        functional_compat,
    });

    ctx.expected_functional_interface = expected_ty;
    ctx.expected_sam = expected_sam;
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
    if selected.params.items.get(hint.arg_index).is_none() {
        return (None, Some(receiver));
    }
    let receiver_internal = receiver.to_internal_with_generics_for_substitution();
    let expected = resolver
        .resolve_selected_param_type_from_generic_signature(
            &receiver_internal,
            selected,
            hint.arg_index,
        )
        .map(|(ty, exact)| {
            (
                ty,
                if exact {
                    ExpectedTypeConfidence::Exact
                } else {
                    ExpectedTypeConfidence::Partial
                },
            )
        })
        .or_else(|| {
            selected
                .params
                .items
                .get(hint.arg_index)
                .and_then(|param| descriptor_to_type_name(&param.descriptor))
                .map(|ty| (ty, ExpectedTypeConfidence::Exact))
        });
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

    if let Some(canonical) = canonicalize_receiver_semantic(resolved.clone(), type_ctx) {
        return Some(canonical);
    }

    let ty = resolved?;
    if ty.contains_slash() || !ty.args.is_empty() {
        return Some(ty);
    }

    let mut scoped = resolve_type_name_with_scoped_inner_fallback(ctx, type_ctx, view, &ty)?;
    scoped.array_dims = ty.array_dims;
    Some(scoped)
}

fn resolve_type_name_with_scoped_inner_fallback(
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    ty: &TypeName,
) -> Option<TypeName> {
    let src = ty.erased_internal();
    if src.is_empty() {
        return None;
    }

    let sym = SymbolResolver::new(view);
    resolve_source_like_type_with_scope(ctx, type_ctx, &sym, src)
}

fn resolve_source_like_type_with_scope(
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    sym: &SymbolResolver<'_>,
    ty: &str,
) -> Option<TypeName> {
    let ty = ty.trim();
    if ty.is_empty() {
        return None;
    }

    let mut base = ty;
    let mut dims = 0usize;
    while let Some(stripped) = base.strip_suffix("[]") {
        dims += 1;
        base = stripped.trim();
    }

    let (base_name, args_str) = split_source_generic_base(base)?;
    let base_internal = resolve_source_base_with_scope(ctx, type_ctx, sym, base_name)?.to_string();

    let mut out = if let Some(args) = args_str {
        let arg_types = split_source_generic_args(args)
            .into_iter()
            .map(|arg| resolve_source_type_arg_with_scope(ctx, type_ctx, sym, arg))
            .collect::<Option<Vec<TypeName>>>()?;
        if arg_types.is_empty() {
            TypeName::new(base_internal)
        } else {
            TypeName::with_args(base_internal, arg_types)
        }
    } else {
        TypeName::new(base_internal)
    };

    if dims > 0 {
        out = out.with_array_dims(dims);
    }
    Some(out)
}

fn resolve_source_type_arg_with_scope(
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    sym: &SymbolResolver<'_>,
    arg: &str,
) -> Option<TypeName> {
    let arg = arg.trim();
    if arg.is_empty() {
        return None;
    }
    if arg == "?" {
        return Some(TypeName::new("*"));
    }
    if let Some(bound) = arg.strip_prefix("? extends ") {
        let inner = resolve_source_type_arg_with_scope(ctx, type_ctx, sym, bound)
            .or_else(|| resolve_source_like_type_with_scope(ctx, type_ctx, sym, bound))?;
        return Some(TypeName::with_args("+", vec![inner]));
    }
    if let Some(bound) = arg.strip_prefix("? super ") {
        let inner = resolve_source_type_arg_with_scope(ctx, type_ctx, sym, bound)
            .or_else(|| resolve_source_like_type_with_scope(ctx, type_ctx, sym, bound))?;
        return Some(TypeName::with_args("-", vec![inner]));
    }

    resolve_source_like_type_with_scope(ctx, type_ctx, sym, arg)
}

fn resolve_source_base_with_scope(
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    sym: &SymbolResolver<'_>,
    base_name: &str,
) -> Option<Arc<str>> {
    let base_name = base_name.trim();
    if base_name.is_empty() {
        return None;
    }
    if base_name.contains('/') {
        return Some(Arc::from(base_name));
    }
    if base_name.contains('.') {
        return Some(Arc::from(base_name.replace('.', "/")));
    }
    if let Some(strict) = type_ctx.resolve_simple_strict(base_name) {
        return Some(Arc::from(strict));
    }
    sym.resolve_type_name(ctx, base_name)
}

fn split_source_generic_base(ty: &str) -> Option<(&str, Option<&str>)> {
    if let Some(start) = ty.find('<') {
        let mut depth = 0i32;
        for (i, c) in ty.char_indices().skip(start) {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        let base = ty[..start].trim();
                        let args = ty[start + 1..i].trim();
                        return Some((base, Some(args)));
                    }
                }
                _ => {}
            }
        }
        None
    } else {
        Some((ty.trim(), None))
    }
}

fn split_source_generic_args(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                result.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        result.push(s[start..].trim());
    }
    result.into_iter().filter(|x| !x.is_empty()).collect()
}

fn evaluate_functional_compatibility(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    shape: &FunctionalExprShape,
    sam: &SamSignature,
    expected: &ExpectedType,
) -> FunctionalCompat {
    match shape {
        FunctionalExprShape::MethodReference {
            qualifier_expr,
            member_name,
            is_constructor,
            qualifier_kind,
        } => evaluate_method_reference_compatibility(
            ctx,
            view,
            type_ctx,
            qualifier_expr,
            member_name,
            *is_constructor,
            qualifier_kind,
            sam,
            expected,
        ),
        FunctionalExprShape::Lambda {
            param_count,
            expression_body,
        } => {
            evaluate_lambda_compatibility(ctx, view, *param_count, expression_body.as_deref(), sam)
        }
    }
}

fn evaluate_method_reference_compatibility(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    qualifier_expr: &str,
    member_name: &str,
    is_constructor: bool,
    qualifier_kind: &MethodRefQualifierKind,
    sam: &SamSignature,
    expected: &ExpectedType,
) -> FunctionalCompat {
    let resolver = TypeResolver::new(view);
    let mut candidates = Vec::new();

    let type_owner = resolve_method_ref_qualifier_as_type(type_ctx, qualifier_expr);
    let expr_owner = resolve_hint_receiver_type(ctx, type_ctx, view, &resolver, qualifier_expr);

    if is_constructor {
        if let Some(owner) = type_owner.clone() {
            candidates.extend(evaluate_constructor_ref_candidates(
                view, &resolver, &owner, sam, expected,
            ));
        }
    } else {
        if matches!(
            qualifier_kind,
            MethodRefQualifierKind::Type | MethodRefQualifierKind::Unknown
        ) && let Some(owner) = type_owner.clone()
        {
            candidates.extend(evaluate_type_method_ref_candidates(
                view,
                &resolver,
                &owner,
                member_name,
                sam,
                expected,
            ));
        }
        if matches!(
            qualifier_kind,
            MethodRefQualifierKind::Expr | MethodRefQualifierKind::Unknown
        ) && let Some(owner) = expr_owner.clone()
        {
            candidates.extend(evaluate_expr_method_ref_candidates(
                view,
                &resolver,
                &owner,
                member_name,
                sam,
                expected,
            ));
        }
    }

    reduce_compat_candidates(candidates).unwrap_or_else(|| FunctionalCompat {
        status: FunctionalCompatStatus::Partial,
        resolved_owner: type_owner.or(expr_owner),
        resolved_return: None,
    })
}

fn evaluate_constructor_ref_candidates(
    view: &IndexView,
    resolver: &TypeResolver,
    owner: &TypeName,
    sam: &SamSignature,
    expected: &ExpectedType,
) -> Vec<FunctionalCompat> {
    let mut out = Vec::new();
    let Some(class) = view.get_class(owner.erased_internal()) else {
        return out;
    };
    for method in &class.methods {
        if method.name.as_ref() != "<init>" {
            continue;
        }
        let method_params = method.params.len();
        if method_params != sam.param_types.len() {
            continue;
        }
        let actual_desc = format!("L{};", owner.erased_internal());
        let return_compat = check_return_compat(
            resolver,
            &actual_desc,
            Some(owner.clone()),
            sam.return_type.as_ref(),
            expected,
        );
        out.push(FunctionalCompat {
            status: return_compat,
            resolved_owner: Some(owner.clone()),
            resolved_return: Some(owner.clone()),
        });
    }
    out
}

fn evaluate_type_method_ref_candidates(
    view: &IndexView,
    resolver: &TypeResolver,
    owner: &TypeName,
    member_name: &str,
    sam: &SamSignature,
    expected: &ExpectedType,
) -> Vec<FunctionalCompat> {
    let mut out = Vec::new();
    let (methods, _) = view.collect_inherited_members(owner.erased_internal());
    for method in methods {
        if method.name.as_ref() != member_name {
            continue;
        }
        let method_params = method.params.len();
        let static_form =
            (method.access_flags & ACC_STATIC) != 0 && method_params == sam.param_types.len();
        let unbound_form =
            (method.access_flags & ACC_STATIC) == 0 && method_params + 1 == sam.param_types.len();
        if !static_form && !unbound_form {
            continue;
        }
        let status = check_method_return_compatibility(resolver, method.as_ref(), sam, expected);
        out.push(FunctionalCompat {
            status,
            resolved_owner: Some(owner.clone()),
            resolved_return: method_return_type_from_descriptor(method.desc().as_ref()),
        });
    }
    out
}

fn evaluate_expr_method_ref_candidates(
    view: &IndexView,
    resolver: &TypeResolver,
    owner: &TypeName,
    member_name: &str,
    sam: &SamSignature,
    expected: &ExpectedType,
) -> Vec<FunctionalCompat> {
    let mut out = Vec::new();
    let (methods, _) = view.collect_inherited_members(owner.erased_internal());
    for method in methods {
        if method.name.as_ref() != member_name {
            continue;
        }
        if (method.access_flags & ACC_STATIC) != 0 {
            continue;
        }
        if method.params.len() != sam.param_types.len() {
            continue;
        }
        let status = check_method_return_compatibility(resolver, method.as_ref(), sam, expected);
        out.push(FunctionalCompat {
            status,
            resolved_owner: Some(owner.clone()),
            resolved_return: method_return_type_from_descriptor(method.desc().as_ref()),
        });
    }
    out
}

fn reduce_compat_candidates(candidates: Vec<FunctionalCompat>) -> Option<FunctionalCompat> {
    if candidates.is_empty() {
        return None;
    }
    if let Some(exact) = candidates
        .iter()
        .find(|c| c.status == FunctionalCompatStatus::Exact)
    {
        return Some(exact.clone());
    }
    if let Some(partial) = candidates
        .iter()
        .find(|c| c.status == FunctionalCompatStatus::Partial)
    {
        return Some(partial.clone());
    }
    candidates.into_iter().next()
}

fn evaluate_lambda_compatibility(
    ctx: &SemanticContext,
    view: &IndexView,
    param_count: usize,
    expression_body: Option<&str>,
    sam: &SamSignature,
) -> FunctionalCompat {
    if param_count != sam.param_types.len() {
        return FunctionalCompat {
            status: FunctionalCompatStatus::Incompatible,
            resolved_owner: None,
            resolved_return: None,
        };
    }

    let Some(body) = expression_body.map(str::trim).filter(|b| !b.is_empty()) else {
        return FunctionalCompat {
            status: FunctionalCompatStatus::Partial,
            resolved_owner: None,
            resolved_return: None,
        };
    };

    let resolver = TypeResolver::new(view);
    let body_ty = resolver.resolve(
        body,
        &ctx.local_variables,
        ctx.enclosing_internal_name.as_ref(),
    );
    let status =
        if let (Some(actual), Some(expected)) = (body_ty.as_ref(), sam.return_type.as_ref()) {
            descriptor_compat_status(&resolver, &actual.to_jvm_signature(), expected)
        } else {
            FunctionalCompatStatus::Partial
        };

    FunctionalCompat {
        status,
        resolved_owner: None,
        resolved_return: body_ty,
    }
}

fn resolve_method_ref_qualifier_as_type(
    type_ctx: &SourceTypeCtx,
    qualifier_expr: &str,
) -> Option<TypeName> {
    type_ctx
        .resolve_type_name_relaxed(qualifier_expr)
        .map(|r| r.ty)
        .or_else(|| {
            type_ctx
                .resolve_simple_strict(qualifier_expr)
                .map(TypeName::new)
        })
}

fn check_method_return_compatibility(
    resolver: &TypeResolver,
    method: &MethodSummary,
    sam: &SamSignature,
    expected: &ExpectedType,
) -> FunctionalCompatStatus {
    let desc = method.desc();
    let Some(ret_idx) = desc.find(')') else {
        return FunctionalCompatStatus::Partial;
    };
    let ret_desc = &desc[ret_idx + 1..];
    let actual_return = method_return_type_from_descriptor(desc.as_ref());
    check_return_compat(
        resolver,
        ret_desc,
        actual_return,
        sam.return_type.as_ref(),
        expected,
    )
}

fn check_return_compat(
    resolver: &TypeResolver,
    actual_desc: &str,
    actual_return: Option<TypeName>,
    sam_return: Option<&TypeName>,
    expected: &ExpectedType,
) -> FunctionalCompatStatus {
    match sam_return {
        Some(expected_return) => descriptor_compat_status(resolver, actual_desc, expected_return),
        None => match expected.confidence {
            ExpectedTypeConfidence::Exact => {
                if actual_return.is_none() {
                    FunctionalCompatStatus::Exact
                } else {
                    FunctionalCompatStatus::Partial
                }
            }
            ExpectedTypeConfidence::Partial => FunctionalCompatStatus::Partial,
        },
    }
}

fn descriptor_compat_status(
    resolver: &TypeResolver,
    actual_desc: &str,
    expected_ty: &TypeName,
) -> FunctionalCompatStatus {
    let Some(actual_ty) = descriptor_to_type_name(actual_desc) else {
        return FunctionalCompatStatus::Partial;
    };
    let expected_desc = expected_ty.to_jvm_signature();
    let score =
        resolver.score_single_descriptor(&expected_desc, &actual_ty.erased_internal_with_arrays());
    if score >= 10 {
        FunctionalCompatStatus::Exact
    } else if score >= 0 {
        FunctionalCompatStatus::Partial
    } else {
        FunctionalCompatStatus::Incompatible
    }
}

fn method_return_type_from_descriptor(desc: &str) -> Option<TypeName> {
    let ret_idx = desc.find(')')?;
    let ret_desc = &desc[ret_idx + 1..];
    if ret_desc == "V" {
        return None;
    }
    descriptor_to_type_name(ret_desc)
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
        // "list.ge" should parse as two variable-like segments.
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
                    methods: vec![
                        MethodSummary {
                            name: Arc::from("length"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("I")),
                        },
                        MethodSummary {
                            name: Arc::from("trim"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("Ljava/lang/String;")),
                        },
                    ],
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
                    package: Some(Arc::from("java/util/function")),
                    name: Arc::from("ToIntFunction"),
                    internal_name: Arc::from("java/util/function/ToIntFunction"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("applyAsInt"),
                        params: MethodParams::from([("Ljava/lang/Object;", "value")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
                    }],
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
                ClassMetadata {
                    package: Some(Arc::from("java/util")),
                    name: Arc::from("ArrayList"),
                    internal_name: Arc::from("java/util/ArrayList"),
                    super_name: Some(Arc::from("java/lang/Object")),
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
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
            ],
        );
        idx
    }

    fn make_index_with_box_map_get_list_size() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: None,
                    name: Arc::from("Box"),
                    internal_name: Arc::from("Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![
                        MethodSummary {
                            name: Arc::from("map"),
                            params: MethodParams::from([("Ljava/util/function/Function;", "fn")]),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: Some(Arc::from(
                                "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)LBox<TR;>;",
                            )),
                            return_type: Some(Arc::from("LBox;")),
                        },
                        MethodSummary {
                            name: Arc::from("get"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: Some(Arc::from("()TT;")),
                            return_type: Some(Arc::from("Ljava/lang/Object;")),
                        },
                    ],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: None,
                    name: Arc::from("List"),
                    internal_name: Arc::from("List"),
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
                        return_type: Some(Arc::from("I")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
            ],
        );
        idx
    }

    fn make_index_with_box_map_get_ambiguous_list_size() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: None,
                    name: Arc::from("Box"),
                    internal_name: Arc::from("Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![
                        MethodSummary {
                            name: Arc::from("map"),
                            params: MethodParams::from([("Ljava/util/function/Function;", "fn")]),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: Some(Arc::from(
                                "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)LBox<TR;>;",
                            )),
                            return_type: Some(Arc::from("LBox;")),
                        },
                        MethodSummary {
                            name: Arc::from("get"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: Some(Arc::from("()TT;")),
                            return_type: Some(Arc::from("Ljava/lang/Object;")),
                        },
                    ],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
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
                        return_type: Some(Arc::from("I")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("java/awt")),
                    name: Arc::from("List"),
                    internal_name: Arc::from("java/awt/List"),
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
                        return_type: Some(Arc::from("I")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
            ],
        );
        idx
    }

    fn make_index_with_list_add_box_for_expected_arg() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: Some(Arc::from("java/util")),
                    name: Arc::from("List"),
                    internal_name: Arc::from("java/util/List"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("add"),
                        params: MethodParams::from([("LBox;", "item")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("(LBox<+Ljava/lang/Number;>;)Z")),
                        return_type: Some(Arc::from("Z")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: None,
                    name: Arc::from("Box"),
                    internal_name: Arc::from("Box"),
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
                    package: Some(Arc::from("java/lang")),
                    name: Arc::from("Number"),
                    internal_name: Arc::from("java/lang/Number"),
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
            ],
        );
        idx
    }

    fn make_index_with_list_add_generic_and_scoped_inner_box() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: Some(Arc::from("java/util")),
                    name: Arc::from("List"),
                    internal_name: Arc::from("java/util/List"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("add"),
                        params: MethodParams::from([("Ljava/lang/Object;", "item")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("(TE;)Z")),
                        return_type: Some(Arc::from("Z")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
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
                ClassMetadata {
                    package: Some(Arc::from("java/lang")),
                    name: Arc::from("Number"),
                    internal_name: Arc::from("java/lang/Number"),
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
            ],
        );
        idx
    }

    fn make_index_with_list_add_generic_and_top_level_box() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: Some(Arc::from("java/util")),
                    name: Arc::from("List"),
                    internal_name: Arc::from("java/util/List"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("add"),
                        params: MethodParams::from([("Ljava/lang/Object;", "item")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("(TE;)Z")),
                        return_type: Some(Arc::from("Z")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
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
                    generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("java/lang")),
                    name: Arc::from("Number"),
                    internal_name: Arc::from("java/lang/Number"),
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
            ],
        );
        idx
    }

    fn make_index_with_box_map_get_trim_and_constructor_chain() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: None,
                    name: Arc::from("Box"),
                    internal_name: Arc::from("Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![
                        MethodSummary {
                            name: Arc::from("map"),
                            params: MethodParams::from([("Ljava/util/function/Function;", "fn")]),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: Some(Arc::from(
                                "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)LBox<TR;>;",
                            )),
                            return_type: Some(Arc::from("LBox;")),
                        },
                        MethodSummary {
                            name: Arc::from("get"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: Some(Arc::from("()TT;")),
                            return_type: Some(Arc::from("Ljava/lang/Object;")),
                        },
                    ],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("java/lang")),
                    name: Arc::from("String"),
                    internal_name: Arc::from("java/lang/String"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![
                        MethodSummary {
                            name: Arc::from("trim"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("Ljava/lang/String;")),
                        },
                        MethodSummary {
                            name: Arc::from("substring"),
                            params: MethodParams::from([("I", "beginIndex")]),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: Some(Arc::from("Ljava/lang/String;")),
                        },
                    ],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("java/util")),
                    name: Arc::from("ArrayList"),
                    internal_name: Arc::from("java/util/ArrayList"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![
                        MethodSummary {
                            name: Arc::from("<init>"),
                            params: MethodParams::empty(),
                            annotations: vec![],
                            access_flags: ACC_PUBLIC,
                            is_synthetic: false,
                            generic_signature: None,
                            return_type: None,
                        },
                        MethodSummary {
                            name: Arc::from("add"),
                            params: MethodParams::from([("Ljava/lang/Object;", "e")]),
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
            ],
        );
        idx
    }

    fn make_index_with_var_local_generic_types() -> WorkspaceIndex {
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
                    methods: vec![],
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
                    package: Some(Arc::from("java/util")),
                    name: Arc::from("HashMap"),
                    internal_name: Arc::from("java/util/HashMap"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("put"),
                        params: MethodParams::from([
                            ("Ljava/lang/Object;", "key"),
                            ("Ljava/lang/Object;", "value"),
                        ]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("(TK;TV;)TV;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from(
                        "<K:Ljava/lang/Object;V:Ljava/lang/Object;>Ljava/lang/Object;",
                    )),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("java/util")),
                    name: Arc::from("List"),
                    internal_name: Arc::from("java/util/List"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("size"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("java/util")),
                    name: Arc::from("ArrayList"),
                    internal_name: Arc::from("java/util/ArrayList"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("get"),
                        params: MethodParams::from([("I", "index")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("(I)TE;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
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
    fn test_canonicalize_receiver_semantic_preserves_existing_type_args() {
        let idx = make_index_with_random_class();
        let view = idx.view(IndexScope {
            module: ModuleId::ROOT,
        });
        let name_table = view.build_name_table();
        let type_ctx = SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.*".into()],
            Some(name_table),
        );

        let resolved = Some(TypeName::with_args("RandomClass", vec![TypeName::new("R")]));
        let canonical =
            super::canonicalize_receiver_semantic(resolved, &type_ctx).expect("canonicalized type");

        assert_eq!(canonical.erased_internal(), "org/cubewhy/RandomClass");
        assert_eq!(canonical.args.len(), 1);
        assert_eq!(canonical.args[0].erased_internal(), "R");
    }

    #[test]
    fn test_canonicalize_receiver_semantic_keeps_failure_behavior() {
        let idx = make_index_with_random_class();
        let view = idx.view(IndexScope {
            module: ModuleId::ROOT,
        });
        let name_table = view.build_name_table();
        let type_ctx = SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["org.cubewhy.*".into()],
            Some(name_table),
        );

        let resolved = Some(TypeName::with_args(
            "DefinitelyUnknownType",
            vec![TypeName::new("R")],
        ));
        let canonical = super::canonicalize_receiver_semantic(resolved, &type_ctx);
        assert!(canonical.is_none());
    }

    #[test]
    fn test_enrich_context_keeps_map_receiver_semantic_args_for_list_size() {
        let idx = make_index_with_box_map_get_list_size();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(None, vec![], Some(name_table)));
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "ge".to_string(),
                receiver_expr: "box.map(List::size)".to_string(),
                arguments: Some("()".to_string()),
            },
            "ge",
            vec![LocalVar {
                name: Arc::from("box"),
                type_internal: TypeName::with_args(
                    "Box",
                    vec![TypeName::with_args(
                        "List",
                        vec![TypeName::new("java/lang/String")],
                    )],
                ),
                init_expr: None,
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec![],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            ..
        } = &ctx.location
        {
            let sem = receiver_semantic_type
                .as_ref()
                .expect("semantic receiver should be populated");
            assert_eq!(sem.erased_internal(), "Box");
            assert!(
                !sem.args.is_empty(),
                "map(List::size) receiver semantic args should be preserved"
            );
            assert_eq!(receiver_type.as_deref(), Some("Box"));
        } else {
            panic!("expected member access location");
        }
    }

    #[test]
    fn test_enrich_context_resolves_list_method_ref_via_import_context_for_binding() {
        let idx = make_index_with_box_map_get_ambiguous_list_size();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into()],
            Some(name_table),
        ));
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "ge".to_string(),
                receiver_expr: "box.map(List::size)".to_string(),
                arguments: Some("()".to_string()),
            },
            "ge",
            vec![LocalVar {
                name: Arc::from("box"),
                type_internal: TypeName::with_args(
                    "Box",
                    vec![TypeName::with_args(
                        "java/util/List",
                        vec![TypeName::new("java/lang/String")],
                    )],
                ),
                init_expr: None,
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.util.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            ..
        } = &ctx.location
        {
            let sem = receiver_semantic_type
                .as_ref()
                .expect("semantic receiver should be populated");
            assert_eq!(sem.erased_internal(), "Box");
            assert_eq!(sem.args.len(), 1);
            assert_eq!(
                sem.args[0].erased_internal(),
                "int",
                "List::size should bind map<R> return to int via import-aware qualifier resolution"
            );
        } else {
            panic!("expected member access location");
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
            expr_shape: None,
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
        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.functional_compat.as_ref())
                .map(|c| c.status),
            None
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
            expr_shape: None,
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
        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.functional_compat.as_ref())
                .map(|c| c.status),
            None
        );
    }

    #[test]
    fn test_method_argument_expected_type_uses_generic_signature_with_receiver_substitution_exact()
    {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![ClassMetadata {
                package: None,
                name: Arc::from("Holder"),
                internal_name: Arc::from("Holder"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("set"),
                    params: MethodParams::from([("Ljava/lang/Object;", "v")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;)V")),
                    return_type: Some(Arc::from("V")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            }],
        );
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(None, vec![], Some(name_table)));
        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![LocalVar {
                name: Arc::from("holder"),
                type_internal: TypeName::with_args(
                    "Holder",
                    vec![TypeName::new("java/lang/String")],
                ),
                init_expr: None,
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec![],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: None,
            method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                receiver_expr: "holder".to_string(),
                method_name: "set".to_string(),
                arg_index: 0,
                arg_texts: vec!["\"x\"".to_string()],
            }),
            expr_shape: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let expected = ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.expected_type.as_ref())
            .expect("expected type should be present");
        assert_eq!(expected.ty.erased_internal(), "java/lang/String");
        assert_eq!(expected.confidence, ExpectedTypeConfidence::Exact);
    }

    #[test]
    fn test_method_argument_expected_type_generic_wildcard_is_partial() {
        let idx = make_index_with_list_add_box_for_expected_arg();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into()],
            Some(name_table),
        ));
        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![LocalVar {
                name: Arc::from("nums"),
                type_internal: TypeName::with_args(
                    "java/util/List",
                    vec![TypeName::with_args(
                        "Box",
                        vec![TypeName::with_args(
                            "+",
                            vec![TypeName::new("java/lang/Number")],
                        )],
                    )],
                ),
                init_expr: None,
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.util.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: None,
            method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                receiver_expr: "nums".to_string(),
                method_name: "add".to_string(),
                arg_index: 0,
                arg_texts: vec!["\"x\"".to_string()],
            }),
            expr_shape: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let expected = ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.expected_type.as_ref())
            .expect("expected type should be present");
        assert_eq!(expected.ty.erased_internal(), "Box");
        assert!(
            !expected.ty.args.is_empty(),
            "wildcard generic structure should be preserved"
        );
        assert_eq!(
            expected.ty.args[0].base_internal.as_ref(),
            "+",
            "expected wildcard-bound generic to be preserved"
        );
        assert_eq!(expected.confidence, ExpectedTypeConfidence::Partial);
    }

    #[test]
    fn test_method_argument_expected_type_preserves_scoped_inner_box_wildcard() {
        let idx = make_index_with_list_add_generic_and_scoped_inner_box();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
            Some(name_table),
        ));
        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "".to_string(),
            },
            "",
            vec![LocalVar {
                name: Arc::from("nums"),
                type_internal: TypeName::with_args(
                    "java/util/List",
                    vec![TypeName::with_args(
                        "Box",
                        vec![TypeName::with_args(
                            "+",
                            vec![TypeName::new("java/lang/Number")],
                        )],
                    )],
                ),
                init_expr: None,
            }],
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: None,
            method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                receiver_expr: "nums".to_string(),
                method_name: "add".to_string(),
                arg_index: 0,
                arg_texts: vec!["new Box()".to_string()],
            }),
            expr_shape: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let expected = ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.expected_type.as_ref())
            .expect("expected type should be present");
        assert_eq!(
            expected.ty.to_internal_with_generics(),
            "org/cubewhy/ClassWithGenerics$Box<+Ljava/lang/Number;>"
        );
        assert_eq!(expected.confidence, ExpectedTypeConfidence::Partial);
    }

    #[test]
    fn test_local_declared_generic_type_is_structured_for_scoped_inner_box() {
        use crate::completion::provider::CompletionProvider;
        use crate::language::java::completion::providers::member::MemberProvider;

        let idx = make_index_with_list_add_generic_and_scoped_inner_box();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "add".to_string(),
                receiver_expr: "nums".to_string(),
                arguments: Some("(".to_string()),
            },
            "add",
            vec![LocalVar {
                name: Arc::from("nums"),
                type_internal: TypeName::new("List<Box<? extends Number>>"),
                init_expr: None,
            }],
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let nums = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "nums")
            .expect("nums local should exist");
        assert_eq!(
            nums.type_internal.to_internal_with_generics(),
            "java/util/List<Lorg/cubewhy/ClassWithGenerics$Box<+Ljava/lang/Number;>;>"
        );

        let add = MemberProvider
            .provide(scope, &ctx, &view)
            .into_iter()
            .find(|c| c.label.as_ref() == "add")
            .expect("add candidate should exist");
        let detail = add.detail.unwrap_or_default();
        assert!(
            detail.contains("Box<? extends"),
            "add detail should preserve wildcard bound after local normalization, got: {}",
            detail
        );
    }

    #[test]
    fn test_nums_add_expected_type_and_detail_preserve_wildcard_structure() {
        use crate::completion::provider::CompletionProvider;
        use crate::language::java::completion::providers::member::MemberProvider;

        let idx = make_index_with_list_add_box_for_expected_arg();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into()],
            Some(name_table),
        ));

        let receiver_semantic = TypeName::with_args(
            "java/util/List",
            vec![TypeName::with_args(
                "Box",
                vec![TypeName::with_args(
                    "+",
                    vec![TypeName::new("java/lang/Number")],
                )],
            )],
        );

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(receiver_semantic.clone()),
                receiver_type: Some(Arc::from("java/util/List")),
                member_prefix: "add".to_string(),
                receiver_expr: "nums".to_string(),
                arguments: None,
            },
            "add",
            vec![LocalVar {
                name: Arc::from("nums"),
                type_internal: receiver_semantic,
                init_expr: None,
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.util.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: None,
            method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                receiver_expr: "nums".to_string(),
                method_name: "add".to_string(),
                arg_index: 0,
                arg_texts: vec!["new Box()".to_string()],
            }),
            expr_shape: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let expected = ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.expected_type.as_ref())
            .expect("expected type should be present");
        assert_eq!(
            expected.ty.to_internal_with_generics(),
            "Box<+Ljava/lang/Number;>"
        );

        let provider = MemberProvider;
        let results = provider.provide(scope, &ctx, &view);
        let add = results
            .iter()
            .find(|c| c.label.as_ref() == "add")
            .expect("expected add candidate");
        let detail = add.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("Box<? extends"),
            "detail should preserve wildcard bound structure, got: {}",
            detail
        );
    }

    #[test]
    fn test_snapshot_nums_add_substitution_old_vs_new_receiver_forms() {
        use crate::semantic::types::generics::{JvmType, parse_method_signature_types, substitute_type};

        let idx = make_index_with_list_add_box_for_expected_arg();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let resolver = TypeResolver::new(&view);

        let list_meta = view
            .get_class("java/util/List")
            .expect("List class should exist");
        let add = list_meta
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "add")
            .expect("add method should exist");
        let sig = add
            .generic_signature
            .as_deref()
            .expect("add should have generic signature");
        let (params, _ret) =
            parse_method_signature_types(sig).expect("parse add generic signature");
        let param0 = params.first().expect("first param should exist");
        let param_token = param0.to_signature_string();

        let receiver_structured = TypeName::with_args(
            "java/util/List",
            vec![TypeName::with_args(
                "Box",
                vec![TypeName::with_args(
                    "+",
                    vec![TypeName::new("java/lang/Number")],
                )],
            )],
        );
        let receiver_old = receiver_structured.to_internal_with_generics();
        let receiver_new = receiver_structured.to_internal_with_generics_for_substitution();

        let old_sub = substitute_type(&receiver_old, list_meta.generic_signature.as_deref(), &param_token)
            .map(|t| t.to_internal_with_generics());
        let new_sub = substitute_type(&receiver_new, list_meta.generic_signature.as_deref(), &param_token)
            .map(|t| t.to_internal_with_generics());

        let old_resolved = resolver
            .resolve_selected_param_type_from_generic_signature(&receiver_old, add, 0)
            .map(|(t, exact)| (t.to_internal_with_generics(), exact));
        let new_resolved = resolver
            .resolve_selected_param_type_from_generic_signature(&receiver_new, add, 0)
            .map(|(t, exact)| (t.to_internal_with_generics(), exact));

        let unresolved_old = "List<Box<? extends Number>>".to_string();
        let unresolved_new = unresolved_old.clone();
        let unresolved_old_resolved = resolver
            .resolve_selected_param_type_from_generic_signature(&unresolved_old, add, 0)
            .map(|(t, exact)| (t.to_internal_with_generics(), exact));
        let unresolved_new_resolved = resolver
            .resolve_selected_param_type_from_generic_signature(&unresolved_new, add, 0)
            .map(|(t, exact)| (t.to_internal_with_generics(), exact));

        let unresolved_old_sub =
            substitute_type(&unresolved_old, list_meta.generic_signature.as_deref(), &param_token)
                .map(|t| t.to_internal_with_generics());
        let unresolved_new_sub =
            substitute_type(&unresolved_new, list_meta.generic_signature.as_deref(), &param_token)
                .map(|t| t.to_internal_with_generics());

        let parsed_new_arg = crate::semantic::types::generics::split_internal_name(&receiver_new)
            .1
            .first()
            .map(|j| j.to_signature_string());
        let parsed_old_arg = crate::semantic::types::generics::split_internal_name(&receiver_old)
            .1
            .first()
            .map(|j| j.to_signature_string());
        let parsed_param = JvmType::parse(&param_token).map(|(j, _)| j.to_signature_string());

        insta::assert_snapshot!(
            "nums_add_substitution_old_vs_new_receiver_forms",
            format!(
                "method_sig={:?}\nclass_sig={:?}\nparam_token={}\nparsed_param={:?}\nreceiver_old={}\nreceiver_new={}\nparsed_old_arg={:?}\nparsed_new_arg={:?}\nold_sub={:?}\nnew_sub={:?}\nold_resolved={:?}\nnew_resolved={:?}\nunresolved_old={}\nunresolved_old_sub={:?}\nunresolved_old_resolved={:?}\nunresolved_new_sub={:?}\nunresolved_new_resolved={:?}\n",
                add.generic_signature,
                list_meta.generic_signature,
                param_token,
                parsed_param,
                receiver_old,
                receiver_new,
                parsed_old_arg,
                parsed_new_arg,
                old_sub,
                new_sub,
                old_resolved,
                new_resolved,
                unresolved_old,
                unresolved_old_sub,
                unresolved_old_resolved,
                unresolved_new_sub,
                unresolved_new_resolved
            )
        );
    }

    #[test]
    fn test_snapshot_inner_vs_top_level_box_nums_add_provenance() {
        use crate::completion::provider::CompletionProvider;
        use crate::language::java::completion::providers::member::MemberProvider;
        use crate::language::java::render;
        use crate::semantic::types::ContextualResolver;
        use crate::semantic::types::generics::{parse_method_signature_types, substitute_type};

        let run_case = |idx: WorkspaceIndex, title: &str| -> String {
            let scope = IndexScope {
                module: ModuleId::ROOT,
            };
            let view = idx.view(scope);
            let name_table = view.build_name_table();
            let type_ctx = Arc::new(SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy")),
                vec!["java.util.*".into()],
                Some(name_table),
            ));

            let mut ctx = SemanticContext::new(
                CursorLocation::Expression {
                    prefix: "".to_string(),
                },
                "",
                vec![LocalVar {
                    name: Arc::from("nums"),
                    type_internal: TypeName::new("List<Box<? extends Number>>"),
                    init_expr: None,
                }],
                Some(Arc::from("ClassWithGenerics")),
                Some(Arc::from("org/cubewhy/ClassWithGenerics")),
                Some(Arc::from("org/cubewhy")),
                vec!["java.util.*".into()],
            )
            .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
                expected_type_source: None,
                method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                    receiver_expr: "nums".to_string(),
                    method_name: "add".to_string(),
                    arg_index: 0,
                    arg_texts: vec!["new Box()".to_string()],
                }),
                expr_shape: None,
            }))
            .with_extension(type_ctx.clone());

            let resolver = TypeResolver::new(&view);
            let recv_before = resolve_hint_receiver_type(
                &ctx,
                &type_ctx,
                &view,
                &resolver,
                "nums",
            );
            let local_nums_before = ctx
                .local_variables
                .iter()
                .find(|lv| lv.name.as_ref() == "nums")
                .map(|lv| lv.type_internal.to_internal_with_generics())
                .unwrap_or_else(|| "<missing>".to_string());

            ContextEnricher::new(&view).enrich(&mut ctx);

            let local_nums_after = ctx
                .local_variables
                .iter()
                .find(|lv| lv.name.as_ref() == "nums")
                .map(|lv| lv.type_internal.to_internal_with_generics())
                .unwrap_or_else(|| "<missing>".to_string());

            let expected = ctx
                .typed_expr_ctx
                .as_ref()
                .and_then(|t| t.expected_type.as_ref())
                .map(|e| (e.ty.to_internal_with_generics(), e.confidence));

            let list_meta = view.get_class("java/util/List").expect("List class");
            let add_meta = list_meta
                .methods
                .iter()
                .find(|m| m.name.as_ref() == "add")
                .expect("add method");
            let add_desc = add_meta.desc();
            let sig = add_meta
                .generic_signature
                .as_deref()
                .unwrap_or(add_desc.as_ref());
            let (params, _) = parse_method_signature_types(sig).expect("parse add sig");
            let param_token = params
                .first()
                .map(|p| p.to_signature_string())
                .unwrap_or_else(|| "<none>".to_string());

            let recv_for_subst = recv_before
                .as_ref()
                .map(TypeName::to_internal_with_generics_for_substitution)
                .unwrap_or_else(|| "<none>".to_string());
            let subst = if recv_for_subst == "<none>" {
                None
            } else {
                substitute_type(
                    &recv_for_subst,
                    list_meta.generic_signature.as_deref(),
                    &param_token,
                )
                .map(|t| t.to_internal_with_generics())
            };

            let detail = if recv_for_subst == "<none>" {
                "<none>".to_string()
            } else {
                let resolver = ContextualResolver::new(&view, &ctx);
                render::method_detail(&recv_for_subst, &list_meta, add_meta, &resolver)
            };

            let mut member_ctx = SemanticContext::new(
                CursorLocation::MemberAccess {
                    receiver_semantic_type: None,
                    receiver_type: None,
                    member_prefix: "add".to_string(),
                    receiver_expr: "nums".to_string(),
                    arguments: None,
                },
                "add",
                vec![LocalVar {
                    name: Arc::from("nums"),
                    type_internal: TypeName::new("List<Box<? extends Number>>"),
                    init_expr: None,
                }],
                Some(Arc::from("ClassWithGenerics")),
                Some(Arc::from("org/cubewhy/ClassWithGenerics")),
                Some(Arc::from("org/cubewhy")),
                vec!["java.util.*".into()],
            )
            .with_extension(type_ctx);
            ContextEnricher::new(&view).enrich(&mut member_ctx);
            let add_detail_from_provider = MemberProvider
                .provide(scope, &member_ctx, &view)
                .into_iter()
                .find(|c| c.label.as_ref() == "add")
                .and_then(|c| c.detail)
                .unwrap_or_else(|| "<none>".to_string());

            format!(
                "{title}:\nlocal_nums_before={}\nreceiver_before={:?}\nselected_add_desc={}\nselected_add_generic_signature={:?}\nparam_token={}\nreceiver_for_substitution={}\nsubstitution_result={:?}\nlocal_nums_after={}\nfinal_expected={:?}\nrender_detail_direct={}\nprovider_add_detail={}\n\n",
                local_nums_before,
                recv_before.as_ref().map(TypeName::to_internal_with_generics),
                add_meta.desc(),
                add_meta.generic_signature,
                param_token,
                recv_for_subst,
                subst,
                local_nums_after,
                expected,
                detail,
                add_detail_from_provider
            )
        };

        let mut out = String::new();
        out.push_str(&run_case(
            make_index_with_list_add_generic_and_scoped_inner_box(),
            "inner_box_case",
        ));
        out.push_str(&run_case(
            make_index_with_list_add_generic_and_top_level_box(),
            "top_level_box_case",
        ));

        insta::assert_snapshot!("inner_vs_top_level_box_nums_add_provenance", out);
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
            expr_shape: None,
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
            expr_shape: None,
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

    #[test]
    fn test_functional_compat_method_ref_type_method_exact_for_tointfunction() {
        let idx = make_index_with_functional_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.util.function.*".into(), "java.lang.*".into()],
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
            vec!["java.util.function.*".into(), "java.lang.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: Some("ToIntFunction<String>".to_string()),
            method_call: None,
            expr_shape: Some(
                crate::semantic::context::FunctionalExprShape::MethodReference {
                    qualifier_expr: "String".to_string(),
                    member_name: "length".to_string(),
                    is_constructor: false,
                    qualifier_kind: crate::semantic::context::MethodRefQualifierKind::Type,
                },
            ),
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.functional_compat.as_ref())
                .map(|c| c.status),
            Some(crate::semantic::context::FunctionalCompatStatus::Exact)
        );
    }

    #[test]
    fn test_functional_compat_method_ref_method_argument_partial_for_map_trim() {
        let idx = make_index_with_functional_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec![
                "java.util.stream.*".into(),
                "java.util.function.*".into(),
                "java.lang.*".into(),
            ],
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
            vec![
                "java.util.stream.*".into(),
                "java.util.function.*".into(),
                "java.lang.*".into(),
            ],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: None,
            method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                receiver_expr: "stream".to_string(),
                method_name: "map".to_string(),
                arg_index: 0,
                arg_texts: vec!["String::trim".to_string()],
            }),
            expr_shape: Some(
                crate::semantic::context::FunctionalExprShape::MethodReference {
                    qualifier_expr: "String".to_string(),
                    member_name: "trim".to_string(),
                    is_constructor: false,
                    qualifier_kind: crate::semantic::context::MethodRefQualifierKind::Type,
                },
            ),
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.functional_compat.as_ref())
                .map(|c| c.status),
            Some(crate::semantic::context::FunctionalCompatStatus::Partial)
        );
    }

    #[test]
    fn test_functional_compat_lambda_simple_partial() {
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
            expected_type_source: Some("Function<Integer, Integer>".to_string()),
            method_call: None,
            expr_shape: Some(crate::semantic::context::FunctionalExprShape::Lambda {
                param_count: 1,
                expression_body: Some("x + 1".to_string()),
            }),
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.functional_compat.as_ref())
                .map(|c| c.status),
            Some(crate::semantic::context::FunctionalCompatStatus::Partial)
        );
    }

    #[test]
    fn test_functional_compat_lambda_arity_mismatch_incompatible() {
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
            expected_type_source: Some("Function<String, Integer>".to_string()),
            method_call: None,
            expr_shape: Some(crate::semantic::context::FunctionalExprShape::Lambda {
                param_count: 2,
                expression_body: Some("x".to_string()),
            }),
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.functional_compat.as_ref())
                .map(|c| c.status),
            Some(crate::semantic::context::FunctionalCompatStatus::Incompatible)
        );
    }

    #[test]
    fn test_snapshot_semantic_pipeline_regression_baseline() {
        let mut out = String::new();

        // 1) box.map(List::size).get() chain concretization in completion context
        {
            let idx = make_index_with_box_map_get_ambiguous_list_size();
            let scope = IndexScope {
                module: ModuleId::ROOT,
            };
            let view = idx.view(scope);
            let name_table = view.build_name_table();
            let type_ctx = Arc::new(SourceTypeCtx::new(
                None,
                vec!["java.util.*".into()],
                Some(name_table),
            ));
            let mut ctx = SemanticContext::new(
                CursorLocation::MemberAccess {
                    receiver_semantic_type: None,
                    receiver_type: None,
                    member_prefix: "ge".to_string(),
                    receiver_expr: "box.map(List::size)".to_string(),
                    arguments: Some("()".to_string()),
                },
                "ge",
                vec![LocalVar {
                    name: Arc::from("box"),
                    type_internal: TypeName::with_args(
                        "Box",
                        vec![TypeName::with_args(
                            "java/util/List",
                            vec![TypeName::new("java/lang/String")],
                        )],
                    ),
                    init_expr: None,
                }],
                Some(Arc::from("Demo")),
                Some(Arc::from("Demo")),
                None,
                vec!["java.util.*".into()],
            )
            .with_extension(type_ctx);
            ContextEnricher::new(&view).enrich(&mut ctx);

            if let CursorLocation::MemberAccess {
                receiver_semantic_type,
                receiver_type,
                ..
            } = &ctx.location
            {
                out.push_str("case1_box_map_list_size_get:\n");
                out.push_str(&format!(
                    "receiver_semantic={:?}\nreceiver_type={:?}\n\n",
                    receiver_semantic_type
                        .as_ref()
                        .map(TypeName::to_internal_with_generics),
                    receiver_type
                ));
            }
        }

        // 2) Function<String, Integer> f = String::length
        {
            let idx = make_index_with_functional_types();
            let scope = IndexScope {
                module: ModuleId::ROOT,
            };
            let view = idx.view(scope);
            let name_table = view.build_name_table();
            let type_ctx = Arc::new(SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy/a")),
                vec!["java.util.function.*".into(), "java.lang.*".into()],
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
                vec!["java.util.function.*".into(), "java.lang.*".into()],
            )
            .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
                expected_type_source: Some("Function<String, Integer>".to_string()),
                method_call: None,
                expr_shape: Some(
                    crate::semantic::context::FunctionalExprShape::MethodReference {
                        qualifier_expr: "String".to_string(),
                        member_name: "length".to_string(),
                        is_constructor: false,
                        qualifier_kind: crate::semantic::context::MethodRefQualifierKind::Type,
                    },
                ),
            }))
            .with_extension(type_ctx);
            ContextEnricher::new(&view).enrich(&mut ctx);

            out.push_str("case2_function_string_integer_string_length:\n");
            out.push_str(&format!(
                "expected_type={:?}\nexpected_sam={:?}\nfunctional_compat={:?}\n\n",
                ctx.typed_expr_ctx
                    .as_ref()
                    .and_then(|t| t.expected_type.as_ref())
                    .map(|e| (
                        e.ty.to_internal_with_generics(),
                        e.source.clone(),
                        e.confidence
                    )),
                ctx.expected_sam.as_ref().map(|s| (
                    s.method_name.clone(),
                    s.param_types.len(),
                    s.return_type.clone()
                )),
                ctx.typed_expr_ctx
                    .as_ref()
                    .and_then(|t| t.functional_compat.as_ref())
                    .map(|c| (
                        c.status,
                        c.resolved_owner
                            .as_ref()
                            .map(TypeName::to_internal_with_generics),
                        c.resolved_return
                            .as_ref()
                            .map(TypeName::to_internal_with_generics)
                    )),
            ));
        }

        // 3) stream.map(String::trim)
        {
            let idx = make_index_with_functional_types();
            let scope = IndexScope {
                module: ModuleId::ROOT,
            };
            let view = idx.view(scope);
            let name_table = view.build_name_table();
            let type_ctx = Arc::new(SourceTypeCtx::new(
                Some(Arc::from("org/cubewhy/a")),
                vec![
                    "java.util.stream.*".into(),
                    "java.util.function.*".into(),
                    "java.lang.*".into(),
                ],
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
                vec![
                    "java.util.stream.*".into(),
                    "java.util.function.*".into(),
                    "java.lang.*".into(),
                ],
            )
            .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
                expected_type_source: None,
                method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                    receiver_expr: "stream".to_string(),
                    method_name: "map".to_string(),
                    arg_index: 0,
                    arg_texts: vec!["String::trim".to_string()],
                }),
                expr_shape: Some(
                    crate::semantic::context::FunctionalExprShape::MethodReference {
                        qualifier_expr: "String".to_string(),
                        member_name: "trim".to_string(),
                        is_constructor: false,
                        qualifier_kind: crate::semantic::context::MethodRefQualifierKind::Type,
                    },
                ),
            }))
            .with_extension(type_ctx);
            ContextEnricher::new(&view).enrich(&mut ctx);

            out.push_str("case3_stream_map_string_trim:\n");
            out.push_str(&format!(
                "expected_type={:?}\nexpected_sam={:?}\nfunctional_compat={:?}\n\n",
                ctx.typed_expr_ctx
                    .as_ref()
                    .and_then(|t| t.expected_type.as_ref())
                    .map(|e| (
                        e.ty.to_internal_with_generics(),
                        e.source.clone(),
                        e.confidence
                    )),
                ctx.expected_sam.as_ref().map(|s| (
                    s.method_name.clone(),
                    s.param_types.len(),
                    s.return_type.clone()
                )),
                ctx.typed_expr_ctx
                    .as_ref()
                    .and_then(|t| t.functional_compat.as_ref())
                    .map(|c| (
                        c.status,
                        c.resolved_owner
                            .as_ref()
                            .map(TypeName::to_internal_with_generics),
                        c.resolved_return
                            .as_ref()
                            .map(TypeName::to_internal_with_generics)
                    )),
            ));
        }

        // 5) nums.add( ... ) method-argument expected type + receiver preservation
        {
            let idx = make_index_with_list_add_box_for_expected_arg();
            let scope = IndexScope {
                module: ModuleId::ROOT,
            };
            let view = idx.view(scope);
            let name_table = view.build_name_table();
            let type_ctx = Arc::new(SourceTypeCtx::new(
                None,
                vec!["java.util.*".into()],
                Some(name_table),
            ));
            let mut ctx = SemanticContext::new(
                CursorLocation::Expression {
                    prefix: "".to_string(),
                },
                "",
                vec![LocalVar {
                    name: Arc::from("nums"),
                    type_internal: TypeName::with_args(
                        "java/util/List",
                        vec![TypeName::with_args(
                            "Box",
                            vec![TypeName::with_args(
                                "+",
                                vec![TypeName::new("java/lang/Number")],
                            )],
                        )],
                    ),
                    init_expr: None,
                }],
                Some(Arc::from("Demo")),
                Some(Arc::from("Demo")),
                None,
                vec!["java.util.*".into()],
            )
            .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
                expected_type_source: None,
                method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                    receiver_expr: "nums".to_string(),
                    method_name: "add".to_string(),
                    arg_index: 0,
                    arg_texts: vec!["new Box()".to_string()],
                }),
                expr_shape: None,
            }))
            .with_extension(type_ctx);
            ContextEnricher::new(&view).enrich(&mut ctx);

            out.push_str("case5_nums_add_argument_context:\n");
            out.push_str(&format!(
                "expected_type={:?}\nreceiver_type={:?}\nreceiver_semantic={:?}\n\n",
                ctx.typed_expr_ctx
                    .as_ref()
                    .and_then(|t| t.expected_type.as_ref())
                    .map(|e| (
                        e.ty.to_internal_with_generics(),
                        e.source.clone(),
                        e.confidence
                    )),
                ctx.typed_expr_ctx
                    .as_ref()
                    .and_then(|t| t.receiver_type.as_ref())
                    .map(TypeName::to_internal_with_generics),
                match &ctx.location {
                    CursorLocation::MemberAccess {
                        receiver_semantic_type,
                        ..
                    } => receiver_semantic_type
                        .as_ref()
                        .map(TypeName::to_internal_with_generics),
                    _ => None,
                },
            ));
        }

        insta::assert_snapshot!("semantic_pipeline_regression_baseline", out);
    }

    #[test]
    fn test_snapshot_chain_receiver_concretization_trim_and_constructor_new() {
        use crate::completion::provider::CompletionProvider;
        use crate::language::java::completion::providers::member::MemberProvider;

        let idx = make_index_with_box_map_get_trim_and_constructor_chain();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into(), "java.lang.*".into()],
            Some(name_table),
        ));

        let mk_ctx = |receiver_expr: &str, prefix: &str| {
            SemanticContext::new(
                CursorLocation::MemberAccess {
                    receiver_semantic_type: None,
                    receiver_type: None,
                    member_prefix: prefix.to_string(),
                    receiver_expr: receiver_expr.to_string(),
                    arguments: None,
                },
                prefix,
                vec![
                    LocalVar {
                        name: Arc::from("strBox"),
                        type_internal: TypeName::with_args(
                            "Box",
                            vec![TypeName::new("java/lang/String")],
                        ),
                        init_expr: None,
                    },
                    LocalVar {
                        name: Arc::from("s"),
                        type_internal: TypeName::with_args(
                            "Box",
                            vec![TypeName::new("java/lang/String")],
                        ),
                        init_expr: None,
                    },
                ],
                Some(Arc::from("Demo")),
                Some(Arc::from("Demo")),
                None,
                vec!["java.util.*".into(), "java.lang.*".into()],
            )
            .with_extension(type_ctx.clone())
        };

        let resolver = TypeResolver::new(&view);
        let resolve_qualifier = |q: &str| type_ctx.resolve_type_name_strict(q);
        let mut ctx_trim = mk_ctx("strBox.map(String::trim).get()", "sub");
        let trim_chain = parse_chain_from_expr("strBox.map(String::trim).get()");
        let trim_eval = evaluate_chain(
            &trim_chain,
            &ctx_trim.local_variables,
            ctx_trim.enclosing_internal_name.as_ref(),
            &resolver,
            ctx_trim.extension::<SourceTypeCtx>().expect("type ctx"),
            &view,
        );
        let trim_canonical = canonicalize_receiver_semantic(
            trim_eval.clone(),
            ctx_trim.extension::<SourceTypeCtx>().expect("type ctx"),
        );
        let trim_direct_map = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
            "Box<Ljava/lang/String;>",
            "map",
            1,
            &[],
            &["String::trim".to_string()],
            &ctx_trim.local_variables,
            ctx_trim.enclosing_internal_name.as_ref(),
            Some(&resolve_qualifier),
        );
        let trim_direct_get = trim_direct_map.as_ref().and_then(|m| {
            resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &m.to_internal_with_generics(),
                "get",
                0,
                &[],
                &[],
                &ctx_trim.local_variables,
                ctx_trim.enclosing_internal_name.as_ref(),
                Some(&resolve_qualifier),
            )
        });
        ContextEnricher::new(&view).enrich(&mut ctx_trim);
        let mut trim_labels: Vec<String> = MemberProvider
            .provide(scope, &ctx_trim, &view)
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();
        trim_labels.sort();
        assert!(
            trim_labels.iter().any(|l| l == "substring"),
            "trim chain should expose String members"
        );
        assert_eq!(
            trim_eval.as_ref().map(TypeName::to_internal_with_generics),
            trim_direct_get
                .as_ref()
                .map(TypeName::to_internal_with_generics),
            "chain evaluation should match direct callsite concretization for String::trim"
        );

        let mut ctx_ctor = mk_ctx("s.map(ArrayList::new).get()", "ad");
        let ctor_chain = parse_chain_from_expr("s.map(ArrayList::new).get()");
        let ctor_eval = evaluate_chain(
            &ctor_chain,
            &ctx_ctor.local_variables,
            ctx_ctor.enclosing_internal_name.as_ref(),
            &resolver,
            ctx_ctor.extension::<SourceTypeCtx>().expect("type ctx"),
            &view,
        );
        let ctor_canonical = canonicalize_receiver_semantic(
            ctor_eval.clone(),
            ctx_ctor.extension::<SourceTypeCtx>().expect("type ctx"),
        );
        let ctor_direct_map = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
            "Box<Ljava/lang/String;>",
            "map",
            1,
            &[],
            &["ArrayList::new".to_string()],
            &ctx_ctor.local_variables,
            ctx_ctor.enclosing_internal_name.as_ref(),
            Some(&resolve_qualifier),
        );
        let ctor_direct_get = ctor_direct_map.as_ref().and_then(|m| {
            resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &m.to_internal_with_generics(),
                "get",
                0,
                &[],
                &[],
                &ctx_ctor.local_variables,
                ctx_ctor.enclosing_internal_name.as_ref(),
                Some(&resolve_qualifier),
            )
        });
        ContextEnricher::new(&view).enrich(&mut ctx_ctor);
        let mut ctor_labels: Vec<String> = MemberProvider
            .provide(scope, &ctx_ctor, &view)
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();
        ctor_labels.sort();
        assert!(
            ctor_labels.iter().any(|l| l == "add"),
            "constructor chain should expose ArrayList members"
        );
        assert_eq!(
            ctor_eval.as_ref().map(TypeName::to_internal_with_generics),
            ctor_direct_get
                .as_ref()
                .map(TypeName::to_internal_with_generics),
            "chain evaluation should match direct callsite concretization for ArrayList::new"
        );

        let mut out = String::new();
        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            ..
        } = &ctx_trim.location
        {
            out.push_str(&format!(
                "case_trim:\nreceiver_semantic={:?}\nreceiver_type={:?}\nlabels={:?}\n\n",
                receiver_semantic_type
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                receiver_type,
                trim_labels
            ));
            out.push_str(&format!(
                "trim_internal_trace:\ndirect_map={:?}\ndirect_get={:?}\nevaluate_chain={:?}\ncanonicalized={:?}\n\n",
                trim_direct_map
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                trim_direct_get
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                trim_eval.as_ref().map(TypeName::to_internal_with_generics),
                trim_canonical
                    .as_ref()
                    .map(TypeName::to_internal_with_generics)
            ));
        }
        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            ..
        } = &ctx_ctor.location
        {
            out.push_str(&format!(
                "case_constructor:\nreceiver_semantic={:?}\nreceiver_type={:?}\nlabels={:?}\n",
                receiver_semantic_type
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                receiver_type,
                ctor_labels
            ));
            out.push_str(&format!(
                "constructor_internal_trace:\ndirect_map={:?}\ndirect_get={:?}\nevaluate_chain={:?}\ncanonicalized={:?}\n",
                ctor_direct_map
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                ctor_direct_get
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                ctor_eval.as_ref().map(TypeName::to_internal_with_generics),
                ctor_canonical
                    .as_ref()
                    .map(TypeName::to_internal_with_generics)
            ));
        }

        insta::assert_snapshot!(
            "chain_receiver_concretization_trim_and_constructor_new",
            out
        );
    }

    #[test]
    fn test_var_rhs_inference_preserves_structured_hashmap_generics() {
        let idx = make_index_with_var_local_generic_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into(), "java.lang.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "a".to_string(),
            },
            "a",
            vec![LocalVar {
                name: Arc::from("a"),
                type_internal: TypeName::new("var"),
                init_expr: Some("new HashMap<String, String>()".to_string()),
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.util.*".into(), "java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let a = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "a")
            .expect("local a");
        assert_eq!(
            a.type_internal.to_internal_with_generics(),
            "java/util/HashMap<Ljava/lang/String;Ljava/lang/String;>"
        );
    }

    #[test]
    fn test_var_rhs_inference_propagates_receiver_generics_for_chain() {
        use crate::completion::provider::CompletionProvider;
        use crate::language::java::completion::providers::member::MemberProvider;

        let idx = make_index_with_var_local_generic_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into(), "java.lang.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "si".to_string(),
                receiver_expr: "a.get(0)".to_string(),
                arguments: None,
            },
            "si",
            vec![LocalVar {
                name: Arc::from("a"),
                type_internal: TypeName::new("var"),
                init_expr: Some("new ArrayList<List<String>>()".to_string()),
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.util.*".into(), "java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        if let CursorLocation::MemberAccess {
            receiver_semantic_type,
            ..
        } = &ctx.location
        {
            assert_eq!(
                receiver_semantic_type
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                Some("java/util/List<Ljava/lang/String;>".to_string())
            );
        } else {
            panic!("expected member access location");
        }

        let labels: Vec<String> = MemberProvider
            .provide(scope, &ctx, &view)
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();
        assert!(
            labels.iter().any(|l| l == "size"),
            "expected List members on a.get(0)"
        );
    }

    #[test]
    fn test_var_rhs_inference_fallback_keeps_var_for_unresolved_init() {
        let idx = make_index_with_var_local_generic_types();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into(), "java.lang.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "a".to_string(),
            },
            "a",
            vec![LocalVar {
                name: Arc::from("a"),
                type_internal: TypeName::new("var"),
                init_expr: Some("unknownFactory()".to_string()),
            }],
            None,
            None,
            None,
            vec!["java.util.*".into(), "java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let a = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "a")
            .expect("local a");
        assert_eq!(a.type_internal.erased_internal(), "var");
    }
}
