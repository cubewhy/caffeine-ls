use std::collections::HashSet;
use std::sync::Arc;

use crate::index::IndexView;
use crate::index::MethodSummary;
use crate::language::java::editor_semantics::{
    materialize_local_variable_types, resolve_method_argument_expected_type,
};
use crate::language::java::expression_typing;
use crate::language::java::intrinsics::access::classify_intrinsic_access;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{
    ExpectedType, ExpectedTypeConfidence, ExpectedTypeSource, FunctionalCompat,
    FunctionalCompatStatus, FunctionalExprShape, FunctionalMethodCallHint, MethodRefQualifierKind,
    SamSignature, TypedChainConfidence, TypedChainReceiver, TypedChainReceiverMode,
    TypedExpressionContext,
};
use crate::semantic::types::generics::{
    JvmType, parse_class_type_parameters, parse_method_signature_types,
};
use crate::semantic::types::symbol_resolver::SymbolResolver;
use crate::semantic::types::type_name::TypeName;
use crate::semantic::types::{
    TypeResolver, is_concrete_type_name, parse_single_type_to_internal,
    singleton_descriptor_to_type,
};
use crate::semantic::{CursorLocation, SemanticContext};
use rust_asm::constants::{ACC_ABSTRACT, ACC_STATIC, ACC_VARARGS};

pub struct ContextEnricher<'a> {
    view: &'a IndexView,
}

impl<'a> ContextEnricher<'a> {
    pub fn new(view: &'a IndexView) -> Self {
        Self { view }
    }

    pub fn enrich(&self, ctx: &mut SemanticContext) {
        ctx.typed_chain_receiver = None;
        ctx.java_intrinsic_access = None;
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
        materialize_local_variable_types(ctx, self.view, &type_ctx);

        enrich_expected_type_context(ctx, self.view, &type_ctx);
        bind_active_lambda_param_types(ctx);

        let resolved_member_receiver = if let CursorLocation::MemberAccess {
            receiver_type,
            receiver_expr,
            ..
        } = &ctx.location
            && receiver_type.is_none()
            && !receiver_expr.is_empty()
        {
            let resolver = TypeResolver::new(self.view);
            let resolved = resolve_member_receiver_with_flow(
                &ctx.local_variables,
                &ctx.flow_type_overrides,
                ctx.enclosing_internal_name.as_ref(),
                &type_ctx,
                self.view,
                &resolver,
                receiver_expr,
            );
            tracing::debug!(
                ?resolved,
                receiver_expr,
                "enrich_context: resolved receiver expression"
            );
            tracing::debug!(?resolved, "enrich_context: resolved before final match");
            canonicalize_receiver_semantic(resolved, &type_ctx)
        } else {
            None
        };
        if let Some(resolved_semantic) = resolved_member_receiver
            && let CursorLocation::MemberAccess {
                receiver_semantic_type,
                receiver_type,
                ..
            } = &mut ctx.location
        {
            ctx.typed_chain_receiver = Some(build_typed_chain_receiver(&resolved_semantic));

            if receiver_semantic_type.is_none() {
                *receiver_semantic_type = Some(resolved_semantic.clone());
            }

            *receiver_type = Some(Arc::from(resolved_semantic.erased_internal()));
        }

        // C3a: if receiver fields were pre-filled and skipped the main branch above,
        // still compute and commit functional-chain concretization into typed chain state.
        let resolved_chain_receiver = if let CursorLocation::MemberAccess { receiver_expr, .. } =
            &ctx.location
            && ctx.typed_chain_receiver.is_none()
            && !receiver_expr.is_empty()
            && (receiver_expr.contains('(') || receiver_expr.contains("::"))
        {
            let resolver = TypeResolver::new(self.view);
            let resolved = resolve_member_receiver_with_flow(
                &ctx.local_variables,
                &ctx.flow_type_overrides,
                ctx.enclosing_internal_name.as_ref(),
                &type_ctx,
                self.view,
                &resolver,
                receiver_expr,
            );
            canonicalize_receiver_semantic(resolved, &type_ctx)
        } else {
            None
        };
        if let Some(ty) = resolved_chain_receiver
            && let CursorLocation::MemberAccess {
                receiver_semantic_type,
                receiver_type,
                ..
            } = &mut ctx.location
        {
            ctx.typed_chain_receiver = Some(build_typed_chain_receiver(&ty));
            if receiver_semantic_type.is_none() {
                *receiver_semantic_type = Some(ty.clone());
            }
            if receiver_type.is_none() {
                *receiver_type = Some(Arc::from(ty.erased_internal()));
            }
        }

        // If receiver expression is a class/type qualifier (not a value expression),
        // normalize MemberAccess into StaticAccess so static members and nested types
        // come from the same authoritative path.
        let static_access_location = if let CursorLocation::MemberAccess {
            receiver_expr,
            member_prefix,
            ..
        } = &ctx.location
        {
            resolve_type_qualifier_internal(ctx, self.view, receiver_expr).map(|owner| {
                (
                    CursorLocation::StaticAccess {
                        class_internal_name: owner,
                        member_prefix: member_prefix.clone(),
                    },
                    member_prefix.clone(),
                )
            })
        } else {
            None
        };
        if let Some((location, query)) = static_access_location {
            ctx.location = location;
            ctx.query = query;
        }

        ctx.java_intrinsic_access = classify_intrinsic_access(ctx, self.view, &type_ctx);

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
    }
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
        Some(ty) if matches!(ty.base_internal.as_ref(), "+" | "-" | "?" | "*" | "capture") => {
            // Wildcard/capture receivers are not strict-resolvable class names.
            // Keep structured type so upper-bound lifting can pick an effective owner.
            Some(ty)
        }
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

fn resolve_type_qualifier_internal(
    ctx: &SemanticContext,
    view: &IndexView,
    receiver_expr: &str,
) -> Option<Arc<str>> {
    let expr = receiver_expr.trim();
    if expr.is_empty() || expr == "this" {
        return None;
    }

    // Type-qualifier resolution only applies to dotted/simple identifiers.
    if !expr
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.')
    {
        return None;
    }

    let parts: Vec<&str> = expr.split('.').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    // Value symbols shadow type qualifiers.
    if parts[0] == "this"
        || ctx
            .local_variables
            .iter()
            .any(|lv| lv.name.as_ref() == parts[0])
    {
        return None;
    }

    let resolver = SymbolResolver::new(view);
    resolver.resolve_type_name(ctx, &parts.join("."))
}

fn build_typed_chain_receiver(receiver_ty: &TypeName) -> TypedChainReceiver {
    if let Some(upper) = receiver_ty.wildcard_upper_bound() {
        return TypedChainReceiver {
            receiver_ty: upper.clone(),
            confidence: TypedChainConfidence::Partial,
            receiver_mode: TypedChainReceiverMode::WildcardUpperBound,
        };
    }

    TypedChainReceiver {
        receiver_ty: receiver_ty.clone(),
        confidence: if receiver_ty.contains_slash() {
            TypedChainConfidence::Exact
        } else {
            TypedChainConfidence::Partial
        },
        receiver_mode: TypedChainReceiverMode::Concrete,
    }
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

    let mut expected: Option<ExpectedType> = None;
    let mut receiver_type: Option<TypeName> = None;

    if let Some(call_hint) = hint.method_call.as_ref() {
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

    if expected.is_none() {
        expected = hint
            .expected_type_source
            .as_deref()
            .and_then(|src| resolve_source_type_hint(type_ctx, src))
            .map(|(ty, confidence)| ExpectedType {
                ty,
                source: hint
                    .expected_type_context
                    .clone()
                    .unwrap_or(ExpectedTypeSource::AssignmentRhs),
                confidence,
            });
    }

    if expected.is_none()
        && let Some(lhs_expr) = hint.assignment_lhs_expr.as_deref()
    {
        let resolver = TypeResolver::new(view);
        expected = expression_typing::resolve_expression_type(
            lhs_expr,
            &ctx.local_variables,
            ctx.enclosing_internal_name.as_ref(),
            &resolver,
            type_ctx,
            view,
        )
        .map(|ty| ExpectedType {
            ty,
            source: ExpectedTypeSource::AssignmentRhs,
            confidence: ExpectedTypeConfidence::Exact,
        });
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

fn bind_active_lambda_param_types(ctx: &mut SemanticContext) {
    let Some(sam) = ctx.expected_sam.as_ref() else {
        return;
    };
    if ctx.active_lambda_param_names.is_empty()
        || ctx.active_lambda_param_names.len() != sam.param_types.len()
    {
        return;
    }

    for (name, ty) in ctx
        .active_lambda_param_names
        .iter()
        .zip(sam.param_types.iter())
    {
        let Some(bindable_ty) = normalize_lambda_param_binding_type(ty) else {
            continue;
        };
        if let Some(local) = ctx
            .local_variables
            .iter_mut()
            .find(|lv| lv.name == *name && lv.type_internal.erased_internal() == "unknown")
        {
            local.type_internal = bindable_ty;
        }
    }
}

fn normalize_lambda_param_binding_type(ty: &TypeName) -> Option<TypeName> {
    let normalized = normalize_lambda_param_binding_jvm_type(&type_name_to_jvm_type(ty)?)?;
    let bindable = jvm_type_to_type_name(&normalized)?;
    is_concrete_type_name(&bindable).then_some(bindable)
}

fn normalize_lambda_param_binding_jvm_type(ty: &JvmType) -> Option<JvmType> {
    match ty {
        JvmType::Wildcard => None,
        JvmType::TypeVar(_) => None,
        JvmType::WildcardBound(_, inner) => normalize_lambda_param_binding_jvm_type(inner),
        JvmType::Primitive(_) => Some(ty.clone()),
        JvmType::Array(inner) => Some(JvmType::Array(Box::new(
            normalize_lambda_param_binding_jvm_type(inner)?,
        ))),
        JvmType::Object(name, args) => Some(JvmType::Object(
            name.clone(),
            args.iter()
                .map(normalize_lambda_param_binding_jvm_type)
                .collect::<Option<Vec<_>>>()?,
        )),
    }
}

fn resolve_source_type_hint(
    type_ctx: &SourceTypeCtx,
    src: &str,
) -> Option<(TypeName, ExpectedTypeConfidence)> {
    let primitive = match src.trim() {
        "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char" | "void" => {
            Some(TypeName::new(src.trim()))
        }
        _ => None,
    };
    if let Some(primitive) = primitive {
        return Some((primitive, ExpectedTypeConfidence::Exact));
    }

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

pub(crate) fn normalize_top_level_generic_base(raw: &str) -> &str {
    let trimmed = raw.trim();
    let mut angle_depth = 0i32;
    for (i, c) in trimmed.char_indices() {
        match c {
            '<' if angle_depth == 0 => return trimmed[..i].trim_end(),
            '<' => angle_depth += 1,
            '>' => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                }
            }
            _ => {}
        }
    }
    trimmed
}

pub(crate) fn resolve_expected_type_from_method_argument(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    hint: &FunctionalMethodCallHint,
) -> (Option<(TypeName, ExpectedTypeConfidence)>, Option<TypeName>) {
    resolve_method_argument_expected_type(ctx, view, type_ctx, hint)
}

fn resolve_hint_receiver_type(
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolver: &TypeResolver,
    expr: &str,
) -> Option<TypeName> {
    if expr.trim() == "this" {
        return ctx
            .enclosing_internal_name
            .as_ref()
            .map(|name| TypeName::new(name.as_ref()));
    }
    let resolved = expression_typing::resolve_expression_type(
        expr,
        &ctx.local_variables,
        ctx.enclosing_internal_name.as_ref(),
        resolver,
        type_ctx,
        view,
    );

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

fn resolve_member_receiver_with_flow(
    local_variables: &[crate::semantic::LocalVar],
    flow_type_overrides: &std::collections::HashMap<Arc<str>, TypeName>,
    enclosing_internal: Option<&Arc<str>>,
    type_ctx: &SourceTypeCtx,
    view: &IndexView,
    resolver: &TypeResolver,
    receiver_expr: &str,
) -> Option<TypeName> {
    let receiver_expr = receiver_expr.trim();
    if is_simple_identifier(receiver_expr)
        && let Some(narrowed) = flow_type_overrides.get(receiver_expr)
    {
        return Some(narrowed.clone());
    }
    expression_typing::resolve_expression_type(
        receiver_expr,
        local_variables,
        enclosing_internal,
        resolver,
        type_ctx,
        view,
    )
}

fn is_simple_identifier(expr: &str) -> bool {
    let mut chars = expr.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
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

pub(crate) fn resolve_source_like_type_with_scope(
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

#[allow(clippy::too_many_arguments)]
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
        if !method_accepts_arity(method, sam.param_types.len()) {
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
        let static_form = (method.access_flags & ACC_STATIC) != 0
            && method_accepts_arity(&method, sam.param_types.len());
        let unbound_form = (method.access_flags & ACC_STATIC) == 0
            && !sam.param_types.is_empty()
            && method_accepts_arity(&method, sam.param_types.len() - 1);
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
        if !method_accepts_arity(&method, sam.param_types.len()) {
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

fn is_varargs_method_summary(method: &MethodSummary) -> bool {
    (method.access_flags & ACC_VARARGS) != 0
        && method
            .params
            .items
            .last()
            .is_some_and(|p| p.descriptor.starts_with('['))
}

fn method_accepts_arity(method: &MethodSummary, arg_count: usize) -> bool {
    let param_len = method.params.len();
    if !is_varargs_method_summary(method) {
        return param_len == arg_count;
    }
    if param_len == 0 {
        return false;
    }
    let fixed_prefix = param_len - 1;
    arg_count >= fixed_prefix
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
    let class_meta = view.get_class(owner)?;
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
    let class_type_params = class_meta
        .generic_signature
        .as_deref()
        .map(parse_class_type_parameters)
        .unwrap_or_default();
    let interface_type_args = interface_ty
        .args
        .iter()
        .filter_map(type_name_to_jvm_type)
        .collect::<Vec<_>>();

    let (param_jvm, ret_jvm) = sam
        .generic_signature
        .as_deref()
        .and_then(parse_method_signature_types)
        .or_else(|| {
            let desc = sam.desc();
            let params = sam
                .params
                .items
                .iter()
                .map(|p| JvmType::parse(&p.descriptor).map(|(j, _)| j))
                .collect::<Option<Vec<_>>>()?;
            let ret_token = desc.split(')').nth(1)?;
            let (ret, _) = JvmType::parse(ret_token)?;
            Some((params, ret))
        })?;

    let substituted_params = if class_type_params.len() == interface_type_args.len() {
        param_jvm
            .iter()
            .map(|p| p.substitute(&class_type_params, &interface_type_args))
            .collect::<Vec<_>>()
    } else {
        param_jvm
    };
    let substituted_ret = if class_type_params.len() == interface_type_args.len() {
        ret_jvm.substitute(&class_type_params, &interface_type_args)
    } else {
        ret_jvm
    };

    let param_types = substituted_params
        .iter()
        .map(jvm_type_to_type_name)
        .collect::<Option<Vec<_>>>()?;
    let return_type = if substituted_ret == JvmType::Primitive('V') {
        None
    } else {
        jvm_type_to_type_name(&substituted_ret)
    };

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

fn jvm_type_to_type_name(ty: &JvmType) -> Option<TypeName> {
    let sig = ty.to_signature_string();
    parse_single_type_to_internal(&sig)
        .or_else(|| singleton_descriptor_to_type(&sig).map(TypeName::new))
}

fn type_name_to_jvm_type(ty: &TypeName) -> Option<JvmType> {
    let sig = ty.to_jvm_signature();
    let (parsed, rest) = JvmType::parse(&sig)?;
    if rest.is_empty() { Some(parsed) } else { None }
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
    use crate::completion::provider::CompletionProvider;
    use crate::index::ModuleId;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, WorkspaceIndex,
    };
    use crate::semantic::LocalVar;
    use crate::semantic::types::{CallArgs, EvalContext, OverloadInvocationMode};
    use rust_asm::constants::{ACC_ABSTRACT, ACC_PUBLIC, ACC_VARARGS};

    fn seg_names(expr: &str) -> Vec<(String, Option<usize>)> {
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

    fn make_index_with_demo_getint_method() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: None,
                    name: Arc::from("Demo"),
                    internal_name: Arc::from("Demo"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("getInt"),
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
                    generic_signature: None,
                    origin: ClassOrigin::Unknown,
                },
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
            ],
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

    fn make_index_with_scoped_inner_box_get_and_list_add() -> WorkspaceIndex {
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
                        params: MethodParams::from([("Ljava/lang/Object;", "e")]),
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
                    generic_signature: Some(Arc::from("<B:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/ClassWithGenerics$Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("get"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("()TT;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: Some(Arc::from("ClassWithGenerics")),
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
                    methods: vec![],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: None,
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

    fn make_index_with_list_box_wildcard_chain() -> WorkspaceIndex {
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
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/Box"),
                    super_name: None,
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("get"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("()TT;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    }],
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
                    methods: vec![MethodSummary {
                        name: Arc::from("doubleValue"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("D")),
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

    fn make_index_with_packaged_box_map_fixture() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![
                ClassMetadata {
                    package: Some(Arc::from("org/cubewhy")),
                    name: Arc::from("Box"),
                    internal_name: Arc::from("org/cubewhy/Box"),
                    super_name: Some(Arc::from("java/lang/Object")),
                    interfaces: vec![],
                    annotations: vec![],
                    methods: vec![MethodSummary {
                        name: Arc::from("map"),
                        params: MethodParams::from([("Ljava/util/function/Function;", "fn")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from(
                            "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)Lorg/cubewhy/Box<TR;>;",
                        )),
                        return_type: Some(Arc::from("Lorg/cubewhy/Box;")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                    origin: ClassOrigin::Unknown,
                },
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
                    methods: vec![MethodSummary {
                        name: Arc::from("trim"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/lang/String;")),
                    }],
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
                    methods: vec![MethodSummary {
                        name: Arc::from("apply"),
                        params: MethodParams::from([("Ljava/lang/Object;", "t")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("(TT;)TR;")),
                        return_type: Some(Arc::from("Ljava/lang/Object;")),
                    }],
                    fields: vec![],
                    access_flags: ACC_PUBLIC,
                    inner_class_of: None,
                    generic_signature: Some(Arc::from(
                        "<T:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
                    )),
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
            Some(Arc::clone(&name_table)),
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
    fn test_enrich_context_classifies_intrinsic_access_kinds() {
        use crate::semantic::context::JavaIntrinsicAccessKind;

        let idx = make_index_with_random_class();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.lang.*".into(), "org.cubewhy.*".into()],
            Some(name_table),
        ));

        let mut class_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "cl".to_string(),
                receiver_expr: "int".to_string(),
                arguments: None,
            },
            "cl",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx.clone());
        ContextEnricher::new(&view).enrich(&mut class_ctx);
        assert_eq!(
            class_ctx.java_intrinsic_access.as_ref().map(|a| a.kind),
            Some(JavaIntrinsicAccessKind::ClassLiteral)
        );

        let mut length_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::new("java/lang/String").with_array_dims(1)),
                receiver_type: Some(Arc::from("java/lang/String")),
                member_prefix: "length".to_string(),
                receiver_expr: "arr".to_string(),
                arguments: None,
            },
            "length",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx.clone());
        ContextEnricher::new(&view).enrich(&mut length_ctx);
        assert_eq!(
            length_ctx.java_intrinsic_access.as_ref().map(|a| a.kind),
            Some(JavaIntrinsicAccessKind::ArrayLength)
        );

        let mut get_class_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::new("java/lang/String")),
                receiver_type: Some(Arc::from("java/lang/String")),
                member_prefix: "getClass".to_string(),
                receiver_expr: "obj".to_string(),
                arguments: Some("()".to_string()),
            },
            "getClass",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);
        ContextEnricher::new(&view).enrich(&mut get_class_ctx);
        assert_eq!(
            get_class_ctx.java_intrinsic_access.as_ref().map(|a| a.kind),
            Some(JavaIntrinsicAccessKind::ObjectGetClass)
        );
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
    fn test_enrich_context_resolves_expected_type_from_assignment_lhs_expression() {
        let idx = make_index_with_random_class();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec![],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "fo".to_string(),
            },
            "fo",
            vec![LocalVar {
                name: Arc::from("x"),
                type_internal: TypeName::new("double"),
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: None,
            expected_type_context: Some(ExpectedTypeSource::AssignmentRhs),
            assignment_lhs_expr: Some("x".to_string()),
            method_call: None,
            expr_shape: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        let expected = ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.expected_type.as_ref())
            .expect("expected type");
        assert_eq!(expected.ty.erased_internal(), "double");
        assert_eq!(expected.source, ExpectedTypeSource::AssignmentRhs);
        assert_eq!(expected.confidence, ExpectedTypeConfidence::Exact);
    }

    #[test]
    fn test_enrich_context_preserves_return_expected_type_source_kind() {
        let idx = make_index_with_random_class();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy/a")),
            vec![],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "fo".to_string(),
            },
            "fo",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: Some("int".to_string()),
            expected_type_context: Some(ExpectedTypeSource::ReturnExpr),
            assignment_lhs_expr: None,
            method_call: None,
            expr_shape: None,
        }))
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let expected = ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.expected_type.as_ref())
            .expect("expected type");
        assert_eq!(expected.ty.erased_internal(), "int");
        assert_eq!(expected.source, ExpectedTypeSource::ReturnExpr);
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            Some(Arc::clone(&name_table)),
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            Some(Arc::clone(&name_table)),
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
            .provide(scope, &ctx, &view, None)
            .candidates
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
        let results = provider.provide(scope, &ctx, &view, None).candidates;
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
        use crate::semantic::types::generics::{
            JvmType, parse_method_signature_types, substitute_type,
        };

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

        let old_sub = substitute_type(
            &receiver_old,
            list_meta.generic_signature.as_deref(),
            &param_token,
        )
        .map(|t| t.to_internal_with_generics());
        let new_sub = substitute_type(
            &receiver_new,
            list_meta.generic_signature.as_deref(),
            &param_token,
        )
        .map(|t| t.to_internal_with_generics());

        let old_resolved = resolver
            .resolve_selected_param_type_from_generic_signature(
                &receiver_old,
                add,
                0,
                OverloadInvocationMode::Fixed,
            )
            .map(|(t, exact)| (t.to_internal_with_generics(), exact));
        let new_resolved = resolver
            .resolve_selected_param_type_from_generic_signature(
                &receiver_new,
                add,
                0,
                OverloadInvocationMode::Fixed,
            )
            .map(|(t, exact)| (t.to_internal_with_generics(), exact));

        let unresolved_old = "List<Box<? extends Number>>".to_string();
        let unresolved_new = unresolved_old.clone();
        let unresolved_old_resolved = resolver
            .resolve_selected_param_type_from_generic_signature(
                &unresolved_old,
                add,
                0,
                OverloadInvocationMode::Fixed,
            )
            .map(|(t, exact)| (t.to_internal_with_generics(), exact));
        let unresolved_new_resolved = resolver
            .resolve_selected_param_type_from_generic_signature(
                &unresolved_new,
                add,
                0,
                OverloadInvocationMode::Fixed,
            )
            .map(|(t, exact)| (t.to_internal_with_generics(), exact));

        let unresolved_old_sub = substitute_type(
            &unresolved_old,
            list_meta.generic_signature.as_deref(),
            &param_token,
        )
        .map(|t| t.to_internal_with_generics());
        let unresolved_new_sub = substitute_type(
            &unresolved_new,
            list_meta.generic_signature.as_deref(),
            &param_token,
        )
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
                expected_type_context: None,
                assignment_lhs_expr: None,
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
            let recv_before = resolve_hint_receiver_type(&ctx, &type_ctx, &view, &resolver, "nums");
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
                .provide(scope, &member_ctx, &view, None)
                .candidates
                .into_iter()
                .find(|c| c.label.as_ref() == "add")
                .and_then(|c| c.detail)
                .unwrap_or_else(|| "<none>".to_string());

            format!(
                "{title}:\nlocal_nums_before={}\nreceiver_before={:?}\nselected_add_desc={}\nselected_add_generic_signature={:?}\nparam_token={}\nreceiver_for_substitution={}\nsubstitution_result={:?}\nlocal_nums_after={}\nfinal_expected={:?}\nrender_detail_direct={}\nprovider_add_detail={}\n\n",
                local_nums_before,
                recv_before
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
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
    fn test_snapshot_m1_single_get_and_nums_add_provenance() {
        use crate::completion::provider::CompletionProvider;
        use crate::language::java::completion::providers::member::MemberProvider;
        use crate::language::java::locals::extract_locals_with_type_ctx;
        use tree_sitter::Parser;

        let idx = make_index_with_scoped_inner_box_get_and_list_add();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
            Some(Arc::clone(&name_table)),
        ));

        let src = indoc::indoc! {r#"
            package org.cubewhy;
            import java.util.*;
            class ClassWithGenerics<B> {
                class Box<T> {}
                void f() {
                    Box<String> single = new Box<>();
                    List<Box<? extends Number>> nums = List.of();
                    single.get().subs
                    nums.add(
                }
            }
        "#};
        let offset = src.find("nums.add(").expect("offset");
        let extractor =
            crate::language::java::JavaContextExtractor::new(src, offset, Some(name_table));
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("java grammar");
        let tree = parser.parse(src, None).expect("parsed");
        let root = tree.root_node();
        let cursor_node = extractor.find_cursor_node(root);
        let locals = extract_locals_with_type_ctx(&extractor, root, cursor_node, Some(&type_ctx));

        let single_local = locals
            .iter()
            .find(|lv| lv.name.as_ref() == "single")
            .map(|lv| lv.type_internal.to_internal_with_generics());
        let nums_local = locals
            .iter()
            .find(|lv| lv.name.as_ref() == "nums")
            .map(|lv| lv.type_internal.to_internal_with_generics());

        let resolver = TypeResolver::new(&view);
        let single_chain = parse_chain_from_expr("single.get()");
        let single_eval = expression_typing::evaluate_chain(
            &single_chain,
            &locals,
            Some(&Arc::from("org/cubewhy/ClassWithGenerics")),
            &resolver,
            &type_ctx,
            &view,
        );

        let mut single_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "subs".to_string(),
                receiver_expr: "single.get()".to_string(),
                arguments: None,
            },
            "subs",
            locals.clone(),
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
        )
        .with_extension(Arc::clone(&type_ctx) as Arc<dyn std::any::Any + Send + Sync>);
        ContextEnricher::new(&view).enrich(&mut single_ctx);
        let single_receiver = single_ctx.location.member_access_receiver_semantic_type();
        let single_local_after = single_ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "single")
            .map(|lv| lv.type_internal.to_internal_with_generics());

        let mut nums_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "add".to_string(),
                receiver_expr: "nums".to_string(),
                arguments: Some("(".to_string()),
            },
            "add",
            locals.clone(),
            Some(Arc::from("ClassWithGenerics")),
            Some(Arc::from("org/cubewhy/ClassWithGenerics")),
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
        )
        .with_functional_target_hint(Some(crate::semantic::context::FunctionalTargetHint {
            expected_type_source: None,
            expected_type_context: None,
            assignment_lhs_expr: None,
            method_call: Some(crate::semantic::context::FunctionalMethodCallHint {
                receiver_expr: "nums".to_string(),
                method_name: "add".to_string(),
                arg_index: 0,
                arg_texts: vec!["".to_string()],
            }),
            expr_shape: None,
        }))
        .with_extension(type_ctx);
        ContextEnricher::new(&view).enrich(&mut nums_ctx);
        let nums_receiver = nums_ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.receiver_type.clone());
        let nums_expected = nums_ctx
            .typed_expr_ctx
            .as_ref()
            .and_then(|t| t.expected_type.as_ref())
            .map(|e| (e.ty.to_internal_with_generics(), e.confidence));
        let nums_local_after = nums_ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "nums")
            .map(|lv| lv.type_internal.to_internal_with_generics());
        let nums_add_detail = MemberProvider
            .provide(scope, &nums_ctx, &view, None)
            .candidates
            .into_iter()
            .find(|c| c.label.as_ref() == "add")
            .and_then(|c| c.detail)
            .unwrap_or_else(|| "<none>".to_string());

        insta::assert_snapshot!(
            "m1_single_get_and_nums_add_provenance",
            format!(
                "single_local={:?}\nsingle_local_after_enrich={:?}\nnums_local={:?}\nnums_local_after_enrich={:?}\nsingle_eval_chain={:?}\nsingle_receiver_semantic={:?}\nnums_receiver_type={:?}\nnums_expected={:?}\nnums_add_detail={}\n",
                single_local,
                single_local_after,
                nums_local,
                nums_local_after,
                single_eval
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                single_receiver.map(|t| t.to_internal_with_generics()),
                nums_receiver.map(|t| t.to_internal_with_generics()),
                nums_expected,
                nums_add_detail
            )
        );
    }

    #[test]
    fn test_snapshot_wildcard_upper_bound_chain_lifting() {
        use crate::language::java::completion::providers::member::MemberProvider;

        let idx = make_index_with_list_box_wildcard_chain();
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

        let nums_ty = TypeName::with_args(
            "java/util/List",
            vec![TypeName::with_args(
                "org/cubewhy/Box",
                vec![TypeName::with_args(
                    "+",
                    vec![TypeName::new("java/lang/Number")],
                )],
            )],
        );
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "doubleV".to_string(),
                receiver_expr: "nums.get(0).get()".to_string(),
                arguments: None,
            },
            "doubleV",
            vec![LocalVar {
                name: Arc::from("nums"),
                type_internal: nums_ty,
                init_expr: None,
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
        )
        .with_extension(type_ctx);
        ContextEnricher::new(&view).enrich(&mut ctx);

        let chain_mode = ctx
            .typed_chain_receiver
            .as_ref()
            .map(|r| (r.receiver_mode, r.confidence));
        let chain_receiver = ctx
            .typed_chain_receiver
            .as_ref()
            .map(|r| r.receiver_ty.to_internal_with_generics());
        let semantic_receiver = ctx
            .location
            .member_access_receiver_semantic_type()
            .map(TypeName::to_internal_with_generics);
        let effective_owner = ctx
            .typed_chain_receiver
            .as_ref()
            .map(|r| r.receiver_ty.erased_internal().to_string())
            .or_else(|| {
                ctx.location
                    .member_access_receiver_owner_internal()
                    .map(|s| s.to_string())
            });
        let member_labels: Vec<String> = MemberProvider
            .provide(scope, &ctx, &view, None)
            .candidates
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();

        insta::assert_snapshot!(
            "wildcard_upper_bound_chain_lifting",
            format!(
                "semantic_receiver={:?}\nchain_receiver={:?}\nchain_mode={:?}\neffective_owner={:?}\nmember_labels={:?}\n",
                semantic_receiver, chain_receiver, chain_mode, effective_owner, member_labels
            )
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
            expected_type_context: None,
            assignment_lhs_expr: None,
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
                expected_type_context: None,
                assignment_lhs_expr: None,
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
                expected_type_context: None,
                assignment_lhs_expr: None,
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
                expected_type_context: None,
                assignment_lhs_expr: None,
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
        let trim_eval = expression_typing::evaluate_chain(
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
        let trim_segments: Vec<String> = (0..trim_chain.len())
            .map(|i| {
                let part = &trim_chain[..=i];
                expression_typing::evaluate_chain(
                    part,
                    &ctx_trim.local_variables,
                    ctx_trim.enclosing_internal_name.as_ref(),
                    &resolver,
                    ctx_trim.extension::<SourceTypeCtx>().expect("type ctx"),
                    &view,
                )
                .as_ref()
                .map(TypeName::to_internal_with_generics)
                .unwrap_or_else(|| "<none>".to_string())
            })
            .collect();
        let trim_direct_map = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
            "Box<Ljava/lang/String;>",
            "map",
            CallArgs::new(1, &[], &["String::trim".to_string()]),
            EvalContext::new(
                &ctx_trim.local_variables,
                ctx_trim.enclosing_internal_name.as_ref(),
            )
            .with_qualifier(Some(&resolve_qualifier)),
        );
        let trim_direct_get = trim_direct_map.as_ref().and_then(|m| {
            resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &m.to_internal_with_generics(),
                "get",
                CallArgs::new(0, &[], &[]),
                EvalContext::new(
                    &ctx_trim.local_variables,
                    ctx_trim.enclosing_internal_name.as_ref(),
                )
                .with_qualifier(Some(&resolve_qualifier)),
            )
        });
        ContextEnricher::new(&view).enrich(&mut ctx_trim);
        let mut trim_labels: Vec<String> = MemberProvider
            .provide(scope, &ctx_trim, &view, None)
            .candidates
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
        let ctor_eval = expression_typing::evaluate_chain(
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
        let ctor_segments: Vec<String> = (0..ctor_chain.len())
            .map(|i| {
                let part = &ctor_chain[..=i];
                expression_typing::evaluate_chain(
                    part,
                    &ctx_ctor.local_variables,
                    ctx_ctor.enclosing_internal_name.as_ref(),
                    &resolver,
                    ctx_ctor.extension::<SourceTypeCtx>().expect("type ctx"),
                    &view,
                )
                .as_ref()
                .map(TypeName::to_internal_with_generics)
                .unwrap_or_else(|| "<none>".to_string())
            })
            .collect();
        let ctor_direct_map = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
            "Box<Ljava/lang/String;>",
            "map",
            CallArgs::new(1, &[], &["ArrayList::new".to_string()]),
            EvalContext::new(
                &ctx_ctor.local_variables,
                ctx_ctor.enclosing_internal_name.as_ref(),
            )
            .with_qualifier(Some(&resolve_qualifier)),
        );
        let ctor_direct_get = ctor_direct_map.as_ref().and_then(|m| {
            resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &m.to_internal_with_generics(),
                "get",
                CallArgs::default(),
                EvalContext::new(
                    &ctx_ctor.local_variables,
                    ctx_ctor.enclosing_internal_name.as_ref(),
                )
                .with_qualifier(Some(&resolve_qualifier)),
            )
        });
        ContextEnricher::new(&view).enrich(&mut ctx_ctor);
        let mut ctor_labels: Vec<String> = MemberProvider
            .provide(scope, &ctx_ctor, &view, None)
            .candidates
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
                "trim_internal_trace:\ndirect_map={:?}\ndirect_get={:?}\nevaluate_chain={:?}\ncanonicalized={:?}\nsegments={:?}\ntyped_chain={:?}\neffective_owner={:?}\n\n",
                trim_direct_map
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                trim_direct_get
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                trim_eval.as_ref().map(TypeName::to_internal_with_generics),
                trim_canonical
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                trim_segments,
                ctx_trim
                    .typed_chain_receiver
                    .as_ref()
                    .map(|r| (
                        r.receiver_ty.to_internal_with_generics(),
                        r.confidence,
                        r.receiver_mode
                    )),
                ctx_trim
                    .typed_chain_receiver
                    .as_ref()
                    .map(|r| r.receiver_ty.erased_internal().to_string())
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
                "constructor_internal_trace:\ndirect_map={:?}\ndirect_get={:?}\nevaluate_chain={:?}\ncanonicalized={:?}\nsegments={:?}\ntyped_chain={:?}\neffective_owner={:?}\n",
                ctor_direct_map
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                ctor_direct_get
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                ctor_eval.as_ref().map(TypeName::to_internal_with_generics),
                ctor_canonical
                    .as_ref()
                    .map(TypeName::to_internal_with_generics),
                ctor_segments,
                ctx_ctor
                    .typed_chain_receiver
                    .as_ref()
                    .map(|r| (
                        r.receiver_ty.to_internal_with_generics(),
                        r.confidence,
                        r.receiver_mode
                    )),
                ctx_ctor
                    .typed_chain_receiver
                    .as_ref()
                    .map(|r| r.receiver_ty.erased_internal().to_string())
            ));
        }

        insta::assert_snapshot!(
            "chain_receiver_concretization_trim_and_constructor_new",
            out
        );
    }

    #[test]
    fn test_functional_chain_commits_typed_receiver_even_with_prefilled_receiver_fields() {
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

        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::new("Box")),
                receiver_type: Some(Arc::from("Box")),
                member_prefix: "sub".to_string(),
                receiver_expr: "strBox.map(String::trim).get()".to_string(),
                arguments: None,
            },
            "sub",
            vec![LocalVar {
                name: Arc::from("strBox"),
                type_internal: TypeName::with_args("Box", vec![TypeName::new("java/lang/String")]),
                init_expr: None,
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.util.*".into(), "java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let committed = ctx
            .typed_chain_receiver
            .as_ref()
            .map(|r| r.receiver_ty.to_internal_with_generics());
        assert_eq!(
            committed.as_deref(),
            Some("java/lang/String"),
            "functional chain should commit concretized receiver into typed chain state"
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
    fn test_var_rhs_inference_materializes_int_for_binary_literal_plus() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "a".to_string(),
            },
            "a",
            vec![LocalVar {
                name: Arc::from("a"),
                type_internal: TypeName::new("var"),
                init_expr: Some("1 + 1".to_string()),
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let a = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "a")
            .expect("local a");
        assert_eq!(a.type_internal.erased_internal(), "int");
    }

    #[test]
    fn test_var_rhs_inference_materializes_int_for_binary_identifier_plus() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "b".to_string(),
            },
            "b",
            vec![
                LocalVar {
                    name: Arc::from("i"),
                    type_internal: TypeName::new("int"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("b"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i + 1".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let b = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "b")
            .expect("local b");
        assert_eq!(b.type_internal.erased_internal(), "int");
    }

    #[test]
    fn test_var_rhs_inference_still_materializes_plain_literal_and_identifier() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "b".to_string(),
            },
            "b",
            vec![
                LocalVar {
                    name: Arc::from("i"),
                    type_internal: TypeName::new("int"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("1".to_string()),
                },
                LocalVar {
                    name: Arc::from("b"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        let a = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "a")
            .expect("local a");
        let b = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "b")
            .expect("local b");
        assert_eq!(a.type_internal.erased_internal(), "int");
        assert_eq!(b.type_internal.erased_internal(), "int");
    }

    #[test]
    fn test_var_rhs_inference_integer_arithmetic_with_precedence_materializes_int() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "a".to_string(),
            },
            "a",
            vec![
                LocalVar {
                    name: Arc::from("i"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("1".to_string()),
                },
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i + 1 + 1 * 100".to_string()),
                },
                LocalVar {
                    name: Arc::from("b"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("1 + 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("c"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("2 * 3".to_string()),
                },
                LocalVar {
                    name: Arc::from("d"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("10 / 2".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        for name in ["i", "a", "b", "c", "d"] {
            let local = ctx
                .local_variables
                .iter()
                .find(|lv| lv.name.as_ref() == name)
                .unwrap_or_else(|| panic!("expected local {name}"));
            assert_eq!(
                local.type_internal.erased_internal(),
                "int",
                "local {name} should materialize to int"
            );
        }
    }

    #[test]
    fn test_var_rhs_inference_numeric_promotion_materializes_double() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "a".to_string(),
            },
            "a",
            vec![
                LocalVar {
                    name: Arc::from("i"),
                    type_internal: TypeName::new("double"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("j"),
                    type_internal: TypeName::new("int"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i + 1.0".to_string()),
                },
                LocalVar {
                    name: Arc::from("b"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i * 2".to_string()),
                },
                LocalVar {
                    name: Arc::from("c"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("j + 1.0".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        for name in ["a", "b", "c"] {
            let local = ctx
                .local_variables
                .iter()
                .find(|lv| lv.name.as_ref() == name)
                .unwrap_or_else(|| panic!("expected local {name}"));
            assert_eq!(
                local.type_internal.erased_internal(),
                "double",
                "local {name} should materialize to double"
            );
        }
    }

    #[test]
    fn test_var_rhs_inference_string_concatenation_materializes_string() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "b".to_string(),
            },
            "b",
            vec![
                LocalVar {
                    name: Arc::from("i"),
                    type_internal: TypeName::new("double"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("b"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i + \"random str\"".to_string()),
                },
                LocalVar {
                    name: Arc::from("c"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("\"x\" + 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("d"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("1 + \"x\"".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        for name in ["b", "c", "d"] {
            let local = ctx
                .local_variables
                .iter()
                .find(|lv| lv.name.as_ref() == name)
                .unwrap_or_else(|| panic!("expected local {name}"));
            assert_eq!(
                local.type_internal.erased_internal(),
                "java/lang/String",
                "local {name} should materialize to String"
            );
        }
    }

    #[test]
    fn test_var_rhs_inference_wrapper_arithmetic_unboxing_and_promotion() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "a".to_string(),
            },
            "a",
            vec![
                LocalVar {
                    name: Arc::from("i"),
                    type_internal: TypeName::new("java/lang/Integer"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("d"),
                    type_internal: TypeName::new("java/lang/Double"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i + 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("b"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i + 1.0".to_string()),
                },
                LocalVar {
                    name: Arc::from("c"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i + 1 + 1 * 100d".to_string()),
                },
                LocalVar {
                    name: Arc::from("e"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("d + 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("f"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("2 * 3".to_string()),
                },
                LocalVar {
                    name: Arc::from("g"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i + \"x\"".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        let a = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "a")
            .expect("local a");
        assert_eq!(a.type_internal.erased_internal(), "int");

        let b = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "b")
            .expect("local b");
        assert_eq!(b.type_internal.erased_internal(), "double");

        let c = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "c")
            .expect("local c");
        assert_eq!(c.type_internal.erased_internal(), "double");

        let e = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "e")
            .expect("local e");
        assert_eq!(e.type_internal.erased_internal(), "double");

        let f = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "f")
            .expect("local f");
        assert_eq!(f.type_internal.erased_internal(), "int");

        let g = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "g")
            .expect("local g");
        assert_eq!(g.type_internal.erased_internal(), "java/lang/String");
    }

    #[test]
    fn test_var_rhs_inference_method_call_numeric_expression_materializes_types() {
        let idx = make_index_with_demo_getint_method();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "b".to_string(),
            },
            "b",
            vec![
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("getInt() + 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("b"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("getInt() + 1 + 1 * 100d".to_string()),
                },
                LocalVar {
                    name: Arc::from("c"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("getInt() + \"x\"".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);

        let a = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "a")
            .expect("local a");
        assert_eq!(a.type_internal.erased_internal(), "int");

        let b = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "b")
            .expect("local b");
        assert_eq!(b.type_internal.erased_internal(), "double");

        let c = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "c")
            .expect("local c");
        assert_eq!(c.type_internal.erased_internal(), "java/lang/String");
    }

    #[test]
    fn test_var_rhs_inference_bitwise_shift_and_unary_not_materialize_integral() {
        let idx = make_index_with_demo_getint_method();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "g".to_string(),
            },
            "g",
            vec![
                LocalVar {
                    name: Arc::from("i"),
                    type_internal: TypeName::new("int"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("w"),
                    type_internal: TypeName::new("java/lang/Integer"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i & 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("b"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i | 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("c"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("i ^ 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("d"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("w << 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("e"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("w >> 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("f"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("w >>> 1".to_string()),
                },
                LocalVar {
                    name: Arc::from("g"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("~w".to_string()),
                },
                LocalVar {
                    name: Arc::from("h"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("getInt() + getInt() ^ 1".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.lang.*".into()],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        for name in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            let local = ctx
                .local_variables
                .iter()
                .find(|lv| lv.name.as_ref() == name)
                .unwrap_or_else(|| panic!("expected local {name}"));
            assert_eq!(
                local.type_internal.erased_internal(),
                "int",
                "local {name} should materialize to int"
            );
        }
    }

    #[test]
    fn test_var_rhs_inference_propagates_receiver_generics_for_chain() {
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
            .provide(scope, &ctx, &view, None)
            .candidates
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

    #[test]
    fn test_var_rhs_inference_uses_canonicalized_locals_for_functional_chain() {
        let idx = make_index_with_packaged_box_map_fixture();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec!["java.lang.*".into(), "java.util.function.*".into()],
            Some(name_table),
        ));

        let mut ctx = SemanticContext::new(
            CursorLocation::Expression {
                prefix: "a".to_string(),
            },
            "a",
            vec![
                LocalVar {
                    name: Arc::from("strBox"),
                    type_internal: TypeName::new("Box<String>"),
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("a"),
                    type_internal: TypeName::new("var"),
                    init_expr: Some("strBox.map(String::trim)".to_string()),
                },
            ],
            Some(Arc::from("Demo")),
            Some(Arc::from("org/cubewhy/Demo")),
            Some(Arc::from("org/cubewhy")),
            vec!["java.lang.*".into(), "java.util.function.*".into()],
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
            "org/cubewhy/Box<Ljava/lang/String;>"
        );
    }

    fn make_index_with_nested_chaincheck() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
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
                generic_signature: None,
                inner_class_of: None,
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
                access_flags: ACC_PUBLIC | ACC_STATIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("ChainCheck")),
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
                access_flags: ACC_PUBLIC | ACC_STATIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("Box")),
                origin: ClassOrigin::Unknown,
            },
        ]);
        idx
    }

    #[test]
    fn test_member_access_type_qualifier_is_normalized_to_static_access() {
        let idx = make_index_with_nested_chaincheck();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        ));
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "ChainCheck".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        match &ctx.location {
            CursorLocation::StaticAccess {
                class_internal_name,
                member_prefix,
            } => {
                assert_eq!(class_internal_name.as_ref(), "org/cubewhy/ChainCheck");
                assert!(member_prefix.is_empty());
            }
            other => panic!("expected StaticAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_member_access_local_shadow_stays_member_access() {
        let idx = make_index_with_nested_chaincheck();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let view = idx.view(scope);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        ));
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "ChainCheck".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("ChainCheck"),
                type_internal: TypeName::new("org/cubewhy/ChainCheck"),
                init_expr: None,
            }],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        )
        .with_extension(type_ctx);

        ContextEnricher::new(&view).enrich(&mut ctx);
        assert!(
            matches!(ctx.location, CursorLocation::MemberAccess { .. }),
            "local variable shadowing should keep member access"
        );
    }

    #[test]
    fn test_method_argument_expected_type_maps_varargs_trailing_to_element_type() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("Owner"),
            internal_name: Arc::from("Owner"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("addAll"),
                params: MethodParams::from_method_descriptor("([Ljava/lang/String;)V"),
                annotations: vec![],
                access_flags: ACC_PUBLIC | ACC_VARARGS,
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
        let view = idx.view(scope);
        let type_ctx = SourceTypeCtx::new(None, vec![], Some(view.build_name_table()));
        let ctx = SemanticContext::new(
            CursorLocation::MethodArgument {
                prefix: "\"c\"".to_string(),
            },
            "\"c\"",
            vec![LocalVar {
                name: Arc::from("o"),
                type_internal: TypeName::new("Owner"),
                init_expr: None,
            }],
            Some(Arc::from("Test")),
            Some(Arc::from("Test")),
            None,
            vec![],
        );
        let hint = FunctionalMethodCallHint {
            receiver_expr: "o".to_string(),
            method_name: "addAll".to_string(),
            arg_index: 2,
            arg_texts: vec![
                "\"a\"".to_string(),
                "\"b\"".to_string(),
                "\"c\"".to_string(),
            ],
        };

        let (expected, _) =
            resolve_expected_type_from_method_argument(&ctx, &view, &type_ctx, &hint);
        let (ty, confidence) = expected.expect("expected type");
        assert_eq!(ty.erased_internal(), "java/lang/String");
        assert_eq!(confidence, ExpectedTypeConfidence::Exact);
    }
}
