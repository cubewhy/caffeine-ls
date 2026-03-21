use std::ops::Range;
use std::sync::Arc;

use ropey::Rope;
use tree_sitter::Node;

use crate::index::{IndexView, MethodSummary};
use crate::language::java::JavaContextExtractor;
use crate::language::java::completion_context::ContextEnricher;
use crate::language::java::expression_typing;
use crate::language::java::render;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{
    CurrentClassMember, ExpectedTypeConfidence, FunctionalMethodCallHint,
};
use crate::semantic::types::type_name::TypeName;
use crate::semantic::types::{
    CallArgs, ContextualResolver, EvalContext, OverloadInvocationMode, TypeResolver,
    parse_single_type_to_internal, singleton_descriptor_to_type,
};
use crate::semantic::{LocalVar, SemanticContext};
use crate::{
    language::java::completion_context::resolve_source_like_type_with_scope,
    semantic::types::symbol_resolver::SymbolResolver,
};

#[derive(Debug, Clone)]
pub(crate) enum JavaInvocationSite {
    Method {
        receiver_expr: String,
        method_name: String,
        arg_texts: Vec<String>,
    },
    Constructor {
        call_text: String,
        arg_texts: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedJavaCall {
    pub receiver: TypeName,
    pub method: Arc<MethodSummary>,
    pub invocation_mode: OverloadInvocationMode,
    pub arg_texts: Vec<String>,
    pub arg_types: Vec<TypeName>,
}

impl ResolvedJavaCall {
    pub fn parameter_name_for_argument(&self, arg_index: usize) -> Option<Arc<str>> {
        let params = self.method.params.param_names();
        let param_len = params.len();
        if param_len == 0 {
            return None;
        }

        let mapped_index = match self.invocation_mode {
            OverloadInvocationMode::Fixed => arg_index,
            OverloadInvocationMode::Varargs => arg_index.min(param_len.saturating_sub(1)),
        };
        params.get(mapped_index).cloned()
    }

    pub fn parameter_type_for_argument(
        &self,
        resolver: &TypeResolver,
        arg_index: usize,
        locals: &[LocalVar],
        enclosing: Option<&Arc<str>>,
    ) -> Option<(TypeName, ExpectedTypeConfidence)> {
        resolver
            .resolve_selected_param_type_with_callsite_inference(
                &self.receiver.to_internal_with_generics_for_substitution(),
                &self.method,
                arg_index,
                self.invocation_mode,
                CallArgs::new(self.arg_types.len(), &self.arg_types, &self.arg_texts),
                EvalContext::new(locals, enclosing),
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
                resolver
                    .resolve_selected_param_type_from_generic_signature(
                        &self.receiver.to_internal_with_generics_for_substitution(),
                        &self.method,
                        arg_index,
                        self.invocation_mode,
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
            })
            .or_else(|| {
                resolver
                    .resolve_selected_param_descriptor_for_call(
                        &self.method,
                        arg_index,
                        self.invocation_mode,
                    )
                    .and_then(|desc| descriptor_to_type_name(&desc))
                    .map(|ty| (ty, ExpectedTypeConfidence::Exact))
            })
    }
}

pub(crate) fn semantic_context_at_offset(
    source: &str,
    rope: &Rope,
    root: Node,
    offset: usize,
    view: &IndexView,
) -> Option<SemanticContext> {
    let extractor = JavaContextExtractor::with_rope(
        source.to_string(),
        offset.min(source.len()),
        rope.clone(),
        None,
    )
    .with_view(view.clone());
    if extractor.is_in_comment() {
        return None;
    }

    let mut ctx = extractor.extract(root, None);
    ContextEnricher::new(view).enrich(&mut ctx);
    Some(ctx)
}

pub(crate) fn materialize_local_variable_types(
    ctx: &mut SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
) {
    let type_ctx_for_resolution = Arc::new(
        type_ctx.clone().with_current_class_methods(
            ctx.current_class_members
                .iter()
                .filter_map(|(name, member)| match member {
                    CurrentClassMember::Method(method) => Some((name.clone(), method.clone())),
                    _ => None,
                })
                .collect(),
        ),
    );
    let sym = SymbolResolver::new(view);
    let new_types: Vec<TypeName> = ctx
        .local_variables
        .iter()
        .map(|lv| expand_local_type_strict(&sym, ctx, type_ctx, &lv.type_internal))
        .collect();
    for (lv, new_ty) in ctx.local_variables.iter_mut().zip(new_types) {
        lv.type_internal = new_ty;
    }

    let resolver = TypeResolver::new(view);
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
        if let Some(resolved) = expression_typing::resolve_var_init_expr(
            &init_expr,
            &ctx.local_variables,
            ctx.enclosing_internal_name.as_ref(),
            &resolver,
            &type_ctx_for_resolution,
            view,
        ) {
            ctx.local_variables[idx_in_vec].type_internal = resolved;
        }
    }

    let new_types: Vec<TypeName> = ctx
        .local_variables
        .iter()
        .map(|lv| expand_local_type_strict(&sym, ctx, type_ctx, &lv.type_internal))
        .collect();

    for (lv, new_ty) in ctx.local_variables.iter_mut().zip(new_types) {
        lv.type_internal = new_ty;
    }
}

pub(crate) fn resolve_invocation(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    site: &JavaInvocationSite,
    selection_unknown_arg_index: Option<usize>,
) -> Option<ResolvedJavaCall> {
    let resolver = TypeResolver::new(view);
    match site {
        JavaInvocationSite::Method {
            receiver_expr,
            method_name,
            arg_texts,
        } => resolve_method_invocation(
            ctx,
            view,
            type_ctx,
            &resolver,
            receiver_expr,
            method_name,
            arg_texts,
            selection_unknown_arg_index,
        ),
        JavaInvocationSite::Constructor {
            call_text,
            arg_texts,
        } => resolve_constructor_invocation(
            ctx,
            view,
            type_ctx,
            &resolver,
            call_text,
            arg_texts,
            selection_unknown_arg_index,
        ),
    }
}

pub(crate) fn resolve_method_argument_expected_type(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    hint: &FunctionalMethodCallHint,
) -> (Option<(TypeName, ExpectedTypeConfidence)>, Option<TypeName>) {
    let site = JavaInvocationSite::Method {
        receiver_expr: hint.receiver_expr.clone(),
        method_name: hint.method_name.clone(),
        arg_texts: hint.arg_texts.clone(),
    };
    let Some(call) = resolve_invocation(ctx, view, type_ctx, &site, Some(hint.arg_index)) else {
        return (None, None);
    };
    let expected = call.parameter_type_for_argument(
        &TypeResolver::new(view),
        hint.arg_index,
        &ctx.local_variables,
        ctx.enclosing_internal_name.as_ref(),
    );
    (expected, Some(call.receiver))
}

pub(crate) fn render_type_for_ui(ty: &TypeName, view: &IndexView, ctx: &SemanticContext) -> String {
    render::type_name_to_source_style(ty, &ContextualResolver::new(view, ctx))
}

#[allow(clippy::too_many_arguments)]
fn resolve_method_invocation(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    resolver: &TypeResolver,
    receiver_expr: &str,
    method_name: &str,
    arg_texts: &[String],
    selection_unknown_arg_index: Option<usize>,
) -> Option<ResolvedJavaCall> {
    let receiver = resolve_receiver_type(ctx, type_ctx, view, resolver, receiver_expr)?;
    let receiver_owner = receiver.erased_internal();
    let candidates = collect_method_candidates(ctx, view, receiver_owner, method_name);
    resolve_selected_call(
        ctx,
        view,
        type_ctx,
        resolver,
        receiver,
        candidates,
        arg_texts,
        selection_unknown_arg_index,
    )
}

fn resolve_constructor_invocation(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    resolver: &TypeResolver,
    call_text: &str,
    arg_texts: &[String],
    selection_unknown_arg_index: Option<usize>,
) -> Option<ResolvedJavaCall> {
    let receiver = expression_typing::resolve_expression_type(
        call_text,
        &ctx.local_variables,
        ctx.enclosing_internal_name.as_ref(),
        resolver,
        type_ctx,
        view,
    )?;
    let receiver_owner = receiver.erased_internal();
    let candidates = collect_constructor_candidates(ctx, view, receiver_owner);
    resolve_selected_call(
        ctx,
        view,
        type_ctx,
        resolver,
        receiver,
        candidates,
        arg_texts,
        selection_unknown_arg_index,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_selected_call(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    resolver: &TypeResolver,
    receiver: TypeName,
    candidates: Vec<Arc<MethodSummary>>,
    arg_texts: &[String],
    selection_unknown_arg_index: Option<usize>,
) -> Option<ResolvedJavaCall> {
    if candidates.is_empty() {
        return None;
    }

    let arg_types: Vec<TypeName> = arg_texts
        .iter()
        .map(|arg| {
            expression_typing::resolve_expression_type(
                arg,
                &ctx.local_variables,
                ctx.enclosing_internal_name.as_ref(),
                resolver,
                type_ctx,
                view,
            )
            .unwrap_or_else(|| TypeName::new("unknown"))
        })
        .collect();
    let mut selection_arg_types = arg_types.clone();
    if let Some(arg_index) = selection_unknown_arg_index
        && let Some(slot) = selection_arg_types.get_mut(arg_index)
    {
        *slot = TypeName::new("unknown");
    }

    let candidate_refs: Vec<&MethodSummary> = candidates.iter().map(|m| m.as_ref()).collect();
    let (selected_name, selected_desc, selected_mode) = {
        let selected = resolver.select_overload_match(
            &candidate_refs,
            arg_texts.len(),
            &selection_arg_types,
        )?;
        (
            selected.method.name.clone(),
            selected.method.desc(),
            selected.mode,
        )
    };
    let method = candidates
        .into_iter()
        .find(|candidate| candidate.name == selected_name && candidate.desc() == selected_desc)?;

    Some(ResolvedJavaCall {
        receiver,
        method,
        invocation_mode: selected_mode,
        arg_texts: arg_texts.to_vec(),
        arg_types,
    })
}

fn collect_method_candidates(
    ctx: &SemanticContext,
    view: &IndexView,
    owner_internal: &str,
    method_name: &str,
) -> Vec<Arc<MethodSummary>> {
    let (methods, _) = view.collect_inherited_members(owner_internal);
    let mut candidates: Vec<Arc<MethodSummary>> = methods
        .into_iter()
        .filter(|m| m.name.as_ref() == method_name)
        .collect();
    if ctx.enclosing_internal_name.as_deref() == Some(owner_internal)
        && let Some(CurrentClassMember::Method(method)) = ctx.current_class_members.get(method_name)
        && method.name.as_ref() == method_name
    {
        candidates.push(method.clone());
    }
    candidates
}

fn collect_constructor_candidates(
    ctx: &SemanticContext,
    view: &IndexView,
    owner_internal: &str,
) -> Vec<Arc<MethodSummary>> {
    let Some(class_meta) = view.get_class(owner_internal) else {
        return Vec::new();
    };
    let mut candidates: Vec<Arc<MethodSummary>> = class_meta
        .methods
        .iter()
        .filter(|m| m.name.as_ref() == "<init>")
        .cloned()
        .map(Arc::new)
        .collect();
    if ctx.enclosing_internal_name.as_deref() == Some(owner_internal) {
        candidates.extend(
            ctx.current_class_members
                .values()
                .filter_map(|member| match member {
                    CurrentClassMember::Method(method) if method.name.as_ref() == "<init>" => {
                        Some(method.clone())
                    }
                    _ => None,
                }),
        );
    }
    candidates
}

fn resolve_receiver_type(
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

    let sym = SymbolResolver::new(view);
    let resolved = expression_typing::resolve_expression_type(
        expr,
        &ctx.local_variables,
        ctx.enclosing_internal_name.as_ref(),
        resolver,
        type_ctx,
        view,
    );
    if let Some(canonical) = resolved
        .clone()
        .and_then(|ty| canonicalize_scoped_type(ctx, type_ctx, &sym, ty))
    {
        return Some(canonical);
    }

    resolved
}

fn canonicalize_scoped_type(
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    sym: &SymbolResolver<'_>,
    ty: TypeName,
) -> Option<TypeName> {
    if ty.contains_slash() || !ty.args.is_empty() {
        return Some(ty);
    }

    let mut scoped = resolve_source_like_type_with_scope(ctx, type_ctx, sym, ty.erased_internal())?;
    scoped.array_dims = ty.array_dims;
    Some(scoped)
}

fn descriptor_to_type_name(desc: &str) -> Option<TypeName> {
    parse_single_type_to_internal(desc)
        .or_else(|| singleton_descriptor_to_type(desc).map(TypeName::new))
}

fn expand_local_type_strict(
    sym: &SymbolResolver,
    ctx: &SemanticContext,
    type_ctx: &SourceTypeCtx,
    ty: &TypeName,
) -> TypeName {
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

pub(crate) fn intersects_range(node: Node, range: &Range<usize>) -> bool {
    node.end_byte() > range.start && node.start_byte() < range.end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{
        ClassMetadata, ClassOrigin, IndexScope, MethodParams, ModuleId, WorkspaceIndex,
    };
    use crate::language::java::make_java_parser;
    use rust_asm::constants::ACC_PUBLIC;

    fn make_view() -> crate::index::IndexView {
        let idx = Box::leak(Box::new(WorkspaceIndex::new()));
        idx.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            vec![ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("ArrayList"),
                internal_name: Arc::from("java/util/ArrayList"),
                super_name: Some(Arc::from("java/util/AbstractList")),
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
                        name: Arc::from("<init>"),
                        params: MethodParams::from([("I", "initialCapacity")]),
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
                generic_signature: Some(Arc::from(
                    "<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;",
                )),
                origin: ClassOrigin::Jar(Arc::from("rt.jar")),
            }],
        );
        idx.view(IndexScope {
            module: ModuleId::ROOT,
        })
    }

    #[test]
    fn resolved_constructor_call_exposes_parameter_names() {
        let view = make_view();
        let src = "import java.util.ArrayList; class T { void m() { new ArrayList<>(1); } }";
        let rope = Rope::from_str(src);
        let mut parser = make_java_parser();
        let tree = parser.parse(src, None).expect("tree");
        let root = tree.root_node();
        let offset = src.find("1").expect("arg offset");
        let ctx =
            semantic_context_at_offset(src, &rope, root, offset, &view).expect("semantic ctx");
        let type_ctx = ctx.extension::<SourceTypeCtx>().expect("type ctx");
        let site = JavaInvocationSite::Constructor {
            call_text: "new ArrayList<>(1)".to_string(),
            arg_texts: vec!["1".to_string()],
        };
        let call = resolve_invocation(&ctx, &view, type_ctx, &site, None).expect("resolved call");
        assert_eq!(
            call.parameter_name_for_argument(0).as_deref(),
            Some("initialCapacity")
        );
    }

    #[test]
    fn semantic_context_materializes_var_local_types() {
        let view = make_view();
        let src = "class T { void m() { var n = 1; } }";
        let rope = Rope::from_str(src);
        let mut parser = make_java_parser();
        let tree = parser.parse(src, None).expect("tree");
        let root = tree.root_node();
        let offset = src.find("n").expect("var offset") + 1;
        let ctx =
            semantic_context_at_offset(src, &rope, root, offset, &view).expect("semantic ctx");
        let local = ctx
            .local_variables
            .iter()
            .find(|local| local.name.as_ref() == "n")
            .expect("var local");
        assert_eq!(local.type_internal.erased_internal(), "int");
    }
}
