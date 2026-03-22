use std::sync::Arc;

use crate::index::IndexView;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::SemanticContext;
use crate::semantic::types::type_name::TypeName;

const JAVA_LANG_OBJECT_INTERNAL: &str = "java/lang/Object";

pub(crate) fn is_super_receiver_expr(expr: &str) -> bool {
    expr.trim() == "super"
}

pub(crate) fn resolve_direct_super_owner(
    ctx: &SemanticContext,
    view: &IndexView,
) -> Option<Arc<str>> {
    resolve_direct_super_owner_with_type_ctx(
        ctx.enclosing_internal_name.as_ref(),
        ctx.extension::<SourceTypeCtx>(),
        view,
    )
}

pub(crate) fn resolve_direct_super_owner_with_type_ctx(
    enclosing_internal: Option<&Arc<str>>,
    type_ctx: Option<&SourceTypeCtx>,
    view: &IndexView,
) -> Option<Arc<str>> {
    let enclosing = enclosing_internal.map(|name| name.as_ref())?;

    if let Some(type_ctx) = type_ctx
        && let Some(raw) = type_ctx.current_class_super_name()
        && let Some(owner) = resolve_super_name(raw.as_ref(), type_ctx)
    {
        return Some(owner);
    }

    if let Some(class_meta) = view.get_class(enclosing) {
        if let Some(raw) = class_meta.super_name.as_deref() {
            if let Some(type_ctx) = type_ctx
                && let Some(owner) = resolve_super_name(raw, type_ctx)
            {
                return Some(owner);
            }

            if raw.contains('/') || view.get_class(raw).is_some() {
                return Some(Arc::from(raw));
            }
        }

        if class_meta.super_name.is_none() && enclosing != JAVA_LANG_OBJECT_INTERNAL {
            return Some(Arc::from(JAVA_LANG_OBJECT_INTERNAL));
        }
    }

    if enclosing == JAVA_LANG_OBJECT_INTERNAL {
        None
    } else {
        Some(Arc::from(JAVA_LANG_OBJECT_INTERNAL))
    }
}

pub(crate) fn resolve_direct_super_type(
    ctx: &SemanticContext,
    view: &IndexView,
) -> Option<TypeName> {
    resolve_direct_super_type_with_type_ctx(
        ctx.enclosing_internal_name.as_ref(),
        ctx.extension::<SourceTypeCtx>(),
        view,
    )
}

pub(crate) fn resolve_direct_super_type_with_type_ctx(
    enclosing_internal: Option<&Arc<str>>,
    type_ctx: Option<&SourceTypeCtx>,
    view: &IndexView,
) -> Option<TypeName> {
    resolve_direct_super_owner_with_type_ctx(enclosing_internal, type_ctx, view).map(TypeName::from)
}

fn resolve_super_name(raw: &str, type_ctx: &SourceTypeCtx) -> Option<Arc<str>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.contains('/') {
        return Some(Arc::from(raw));
    }
    if let Some(resolved) = type_ctx.resolve_type_name_strict(raw) {
        return Some(Arc::from(resolved.erased_internal()));
    }
    type_ctx.resolve_simple_strict(raw).map(Arc::from)
}
