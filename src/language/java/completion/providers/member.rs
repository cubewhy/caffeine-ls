use rust_asm::constants::ACC_STATIC;
use std::time::Instant;

use crate::completion::provider::CompletionProvider;
use crate::completion::scorer::AccessFilter;
use crate::completion::{CandidateKind, CompletionCandidate, fuzzy};
use crate::language::java::expression_typing;
use crate::language::java::render;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{CursorLocation, SemanticContext};
use crate::{
    index::{IndexScope, IndexView},
    semantic::types::{ContextualResolver, TypeResolver, type_name::TypeName},
};
use std::sync::Arc;

#[derive(Debug, Clone)]
struct FlowReceiverCastPlan {
    receiver_expr: String,
    cast_type: String,
    declared_type: TypeName,
}

pub struct MemberProvider;

impl CompletionProvider for MemberProvider {
    fn name(&self) -> &'static str {
        "member"
    }

    fn provide(
        &self,
        scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Vec<CompletionCandidate> {
        if !matches!(&ctx.location, CursorLocation::MemberAccess { .. }) {
            return vec![];
        }

        let receiver_semantic_type = ctx.location.member_access_receiver_semantic_type();
        let receiver_owner_internal = ctx.location.member_access_receiver_owner_internal();
        let member_prefix = ctx.location.member_access_prefix().unwrap_or("");
        let receiver_expr = ctx.location.member_access_expr().unwrap_or("");

        tracing::debug!(
            receiver_expr,
            member_prefix,
            receiver_semantic_type = ?receiver_semantic_type
                .map(TypeName::to_internal_with_generics),
            receiver_owner_internal = ?receiver_owner_internal,
            typed_chain_receiver = ?ctx.typed_chain_receiver
                .as_ref()
                .map(|t| t.receiver_ty.to_internal_with_generics()),
            locals = ?ctx.local_variables.iter().map(|lv| format!("{}:{}", lv.name, lv.type_internal)).collect::<Vec<_>>(),
            imports = ?ctx.existing_imports,
            "MemberProvider.provide"
        );

        if receiver_expr == "this" {
            let trace_timing = tracing::enabled!(tracing::Level::DEBUG);
            let t_total = trace_timing.then(Instant::now);
            if ctx.is_in_static_context() {
                return vec![];
            }
            let has_paren_after_cursor = ctx.has_paren_after_cursor();

            // source members (including private members, directly parsed from the AST)
            let mut results = if !ctx.current_class_members.is_empty() {
                self.provide_from_source_members(ctx, member_prefix)
            } else {
                vec![]
            };
            let resolver = ContextualResolver::new(index, ctx);

            if let Some(enclosing) = ctx.enclosing_internal_name.as_deref() {
                let source_names: std::collections::HashSet<Arc<str>> =
                    ctx.current_class_members.keys().map(Arc::clone).collect();
                let prefix_lower = if member_prefix.is_empty() {
                    None
                } else {
                    Some(member_prefix.to_lowercase())
                };

                // index MRO: skip(0) means starting from the current class, using the same_class filter
                //
                // When source members have already covered the current class, the index entries of the current class will be skipped due to
                // source_names deduplication, and will not be repeated.
                let mro = index.mro(enclosing);

                for (i, class_meta) in mro.iter().enumerate() {
                    let filter = if i == 0 {
                        AccessFilter::same_class()
                    } else {
                        AccessFilter::member_completion()
                    };

                    for method in &class_meta.methods {
                        if method.name.as_ref() == "<init>" || method.name.as_ref() == "<clinit>" {
                            continue;
                        }
                        if !filter.is_method_accessible(method.access_flags, method.is_synthetic) {
                            continue;
                        }
                        // The current class itself: source members are already included, skipping to avoid duplication.
                        if i == 0 && source_names.contains(&method.name) {
                            continue;
                        }
                        if !name_matches_member_prefix(
                            method.name.as_ref(),
                            prefix_lower.as_deref(),
                        ) {
                            continue;
                        }
                        use rust_asm::constants::ACC_STATIC;
                        let is_static = method.access_flags & ACC_STATIC != 0;
                        let kind = if is_static {
                            CandidateKind::StaticMethod {
                                descriptor: method.desc(),
                                defining_class: Arc::clone(&class_meta.internal_name),
                            }
                        } else {
                            CandidateKind::Method {
                                descriptor: method.desc(),
                                defining_class: Arc::clone(&class_meta.internal_name),
                            }
                        };
                        let detail = if i == 0 {
                            render::method_detail(
                                ctx.enclosing_internal_name.as_deref().unwrap_or(""),
                                class_meta,
                                method,
                                &resolver,
                            )
                        } else {
                            format!("inherited from {}", class_meta.name)
                        };
                        results.push(
                            CompletionCandidate::new(
                                Arc::clone(&method.name),
                                method.name.to_string(),
                                kind,
                                self.name(),
                            )
                            .with_callable_insert(
                                method.name.as_ref(),
                                &method.params.param_names(),
                                has_paren_after_cursor,
                            )
                            .with_detail(detail)
                            .with_score(if i == 0 {
                                60.0
                            } else {
                                55.0
                            }),
                        );
                    }

                    for field in &class_meta.fields {
                        let filter = if i == 0 {
                            AccessFilter::same_class()
                        } else {
                            AccessFilter::member_completion()
                        };
                        if !filter.is_field_accessible(field.access_flags, field.is_synthetic) {
                            continue;
                        }
                        if i == 0 && source_names.contains(&field.name) {
                            continue;
                        }
                        if !name_matches_member_prefix(field.name.as_ref(), prefix_lower.as_deref())
                        {
                            continue;
                        }

                        let is_static = field.access_flags & ACC_STATIC != 0;
                        let kind = if is_static {
                            CandidateKind::StaticField {
                                descriptor: Arc::clone(&field.descriptor),
                                defining_class: Arc::clone(&class_meta.internal_name),
                            }
                        } else {
                            CandidateKind::Field {
                                descriptor: Arc::clone(&field.descriptor),
                                defining_class: Arc::clone(&class_meta.internal_name),
                            }
                        };
                        let detail = if i == 0 {
                            render::field_detail(
                                ctx.enclosing_internal_name.as_deref().unwrap_or(""),
                                class_meta,
                                field,
                                &resolver,
                            )
                        } else {
                            format!("inherited from {}", class_meta.name)
                        };
                        results.push(
                            CompletionCandidate::new(
                                Arc::clone(&field.name),
                                field.name.to_string(),
                                kind,
                                self.name(),
                            )
                            .with_detail(detail)
                            .with_score(if i == 0 {
                                60.0
                            } else {
                                55.0
                            }),
                        );
                    }
                }
            }
            if let Some(t_total) = t_total {
                tracing::debug!(
                    elapsed_ms = t_total.elapsed().as_secs_f64() * 1000.0,
                    candidates = results.len(),
                    "MemberProvider.this_branch_timing"
                );
            }
            return results;
        }

        let trace_timing = tracing::enabled!(tracing::Level::DEBUG);
        let t_total = trace_timing.then(Instant::now);
        let t_resolve = trace_timing.then(Instant::now);
        let resolved_semantic = receiver_semantic_type
            .cloned()
            .or_else(|| receiver_owner_internal.map(TypeName::new))
            .or_else(|| {
                ctx.typed_chain_receiver
                    .as_ref()
                    .map(|t| t.receiver_ty.clone())
            })
            .or_else(|| resolve_receiver_type(receiver_expr, ctx, index, scope));

        let resolved_original = match resolved_semantic {
            Some(t) => t,
            None => {
                tracing::debug!(
                    receiver_expr,
                    "resolve_receiver_type returned None, returning empty"
                );
                return vec![];
            }
        };
        let resolve_elapsed_ms = t_resolve
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or_default();

        let resolved_effective = ctx
            .typed_chain_receiver
            .as_ref()
            .map(|t| t.receiver_ty.clone())
            .unwrap_or_else(|| resolved_original.clone());
        let resolved_effective =
            normalize_receiver_owner_for_members(resolved_effective, ctx, index, scope);

        let class_internal = resolved_effective.to_internal_with_generics();
        let class_internal_for_substitution =
            resolved_effective.to_internal_with_generics_for_substitution();
        let base_class_internal = resolved_effective.erased_internal();

        tracing::debug!(
            base_class_internal,
            class_internal,
            "looking up class in index"
        );

        // Check if it's a similar access (allow private/protected access)
        let is_same_class = ctx.enclosing_internal_name.as_deref() == Some(base_class_internal);

        let filter = if is_same_class {
            AccessFilter::same_class()
        } else {
            AccessFilter::member_completion()
        };

        let prefix_lower = if member_prefix.is_empty() {
            None
        } else {
            Some(member_prefix.to_lowercase())
        };
        let has_paren_after_cursor = ctx.has_paren_after_cursor();
        let mut results = Vec::new();
        let flow_receiver_cast_plan =
            build_flow_receiver_cast_plan(ctx, index, receiver_expr, &resolved_effective);

        let t_mro = trace_timing.then(Instant::now);
        let mro = index.mro(base_class_internal);
        let mro_elapsed_ms = t_mro
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or_default();
        let mut seen_methods = std::collections::HashSet::new();
        let mut seen_fields = std::collections::HashSet::new();

        let resolver = ContextualResolver::new(index, ctx);
        let trace_arraylist =
            class_internal.contains("ArrayList") || base_class_internal.contains("ArrayList");
        let t_collect = trace_timing.then(Instant::now);

        for class_meta in &mro {
            for method in &class_meta.methods {
                // skip <init>, <clinit>
                if method.name.as_ref() == "<init>" || method.name.as_ref() == "<clinit>" {
                    continue;
                }
                // shadowing: subclass method hides superclass method with same name+descriptor
                let key = (Arc::clone(&method.name), Arc::clone(&method.desc()));
                if !seen_methods.insert(key) {
                    continue;
                }
                if !filter.is_method_accessible(method.access_flags, method.is_synthetic) {
                    continue;
                }
                if !name_matches_member_prefix(method.name.as_ref(), prefix_lower.as_deref()) {
                    continue;
                }
                let trace_add = trace_arraylist
                    && method.name.as_ref() == "add"
                    && tracing::enabled!(tracing::Level::DEBUG);
                if trace_add {
                    tracing::debug!(
                        receiver_expr,
                        member_prefix,
                        class_internal,
                        base_class_internal,
                        class_meta_internal = %class_meta.internal_name,
                        class_meta_origin = ?class_meta.origin,
                        class_meta_generic_signature = ?class_meta.generic_signature,
                        method_name = %method.name,
                        method_desc = %method.desc(),
                        method_generic_signature = ?method.generic_signature,
                        method_return_type = ?method.return_type,
                        param_descriptors = ?method.params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
                        param_names = ?method.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                        "member_provider: add overload candidate before detail rendering"
                    );
                }
                let is_static = method.access_flags & ACC_STATIC != 0;
                let kind = if is_static {
                    CandidateKind::StaticMethod {
                        descriptor: method.desc(),
                        defining_class: Arc::from(class_internal.as_str()),
                    }
                } else {
                    CandidateKind::Method {
                        descriptor: method.desc(),
                        defining_class: Arc::from(class_internal.as_str()),
                    }
                };
                let mut candidate = CompletionCandidate::new(
                    Arc::clone(&method.name),
                    method.name.to_string(),
                    kind,
                    self.name(),
                )
                .with_callable_insert(
                    method.name.as_ref(),
                    &method.params.param_names(),
                    has_paren_after_cursor,
                )
                .with_detail({
                    let detail = render::method_detail(
                        &class_internal_for_substitution,
                        class_meta,
                        method,
                        &resolver,
                    );
                    if trace_add {
                        tracing::debug!(
                            method_name = %method.name,
                            method_desc = %method.desc(),
                            detail,
                            "member_provider: add overload candidate after detail rendering"
                        );
                    }
                    detail
                });
                if !is_static
                    && let Some(plan) = flow_receiver_cast_plan.as_ref()
                    && needs_cast_for_method(
                        index,
                        &plan.declared_type,
                        method.name.as_ref(),
                        method.desc().as_ref(),
                    )
                {
                    candidate = candidate.with_member_access_cast_rewrite(
                        plan.receiver_expr.clone(),
                        plan.cast_type.clone(),
                    );
                }
                results.push(candidate);
            }

            for field in &class_meta.fields {
                if !seen_fields.insert(Arc::clone(&field.name)) {
                    continue;
                }
                if !filter.is_field_accessible(field.access_flags, field.is_synthetic) {
                    continue;
                }
                if !name_matches_member_prefix(field.name.as_ref(), prefix_lower.as_deref()) {
                    continue;
                }
                let is_static = field.access_flags & ACC_STATIC != 0;
                let kind = if is_static {
                    CandidateKind::StaticField {
                        descriptor: Arc::clone(&field.descriptor),
                        defining_class: Arc::from(class_internal.as_str()),
                    }
                } else {
                    CandidateKind::Field {
                        descriptor: Arc::clone(&field.descriptor),
                        defining_class: Arc::from(class_internal.as_str()),
                    }
                };
                let mut candidate = CompletionCandidate::new(
                    Arc::clone(&field.name),
                    field.name.to_string(),
                    kind,
                    self.name(),
                )
                .with_detail(render::field_detail(
                    &class_internal_for_substitution,
                    class_meta,
                    field,
                    &resolver,
                ));
                if !is_static
                    && let Some(plan) = flow_receiver_cast_plan.as_ref()
                    && needs_cast_for_field(
                        index,
                        &plan.declared_type,
                        field.name.as_ref(),
                        field.descriptor.as_ref(),
                    )
                {
                    candidate = candidate.with_member_access_cast_rewrite(
                        plan.receiver_expr.clone(),
                        plan.cast_type.clone(),
                    );
                }
                results.push(candidate);
            }
        }
        if let (Some(t_collect), Some(t_total)) = (t_collect, t_total) {
            let collect_elapsed_ms = t_collect.elapsed().as_secs_f64() * 1000.0;
            tracing::debug!(
                resolve_ms = resolve_elapsed_ms,
                mro_ms = mro_elapsed_ms,
                collect_ms = collect_elapsed_ms,
                total_ms = t_total.elapsed().as_secs_f64() * 1000.0,
                mro_len = mro.len(),
                candidates = results.len(),
                "MemberProvider.phase_timing"
            );
        }
        results
    }
}

fn build_flow_receiver_cast_plan(
    ctx: &SemanticContext,
    index: &IndexView,
    receiver_expr: &str,
    resolved_effective: &TypeName,
) -> Option<FlowReceiverCastPlan> {
    let receiver_expr = receiver_expr.trim();
    if !is_simple_identifier(receiver_expr) {
        return None;
    }

    let narrowed = ctx.flow_override_for_local(receiver_expr)?;
    if narrowed.erased_internal() != resolved_effective.erased_internal() {
        return None;
    }

    let declared = ctx
        .local_variables
        .iter()
        .find(|lv| lv.name.as_ref() == receiver_expr)
        .map(|lv| lv.type_internal.clone())?;
    if declared.erased_internal() == narrowed.erased_internal() {
        return None;
    }

    // Keep cast rewrites for concrete known owner types only.
    if index.get_class(declared.erased_internal()).is_none()
        || index.get_class(narrowed.erased_internal()).is_none()
    {
        return None;
    }

    Some(FlowReceiverCastPlan {
        receiver_expr: receiver_expr.to_string(),
        cast_type: render_cast_type(narrowed),
        declared_type: declared,
    })
}

fn needs_cast_for_method(
    index: &IndexView,
    declared_type: &TypeName,
    method_name: &str,
    method_desc: &str,
) -> bool {
    let (methods, _) = index.collect_inherited_members(declared_type.erased_internal());
    !methods
        .iter()
        .any(|m| m.name.as_ref() == method_name && m.desc().as_ref() == method_desc)
}

fn needs_cast_for_field(
    index: &IndexView,
    declared_type: &TypeName,
    field_name: &str,
    field_desc: &str,
) -> bool {
    let (_, fields) = index.collect_inherited_members(declared_type.erased_internal());
    !fields
        .iter()
        .any(|f| f.name.as_ref() == field_name && f.descriptor.as_ref() == field_desc)
}

fn render_cast_type(ty: &TypeName) -> String {
    let mut base = ty.erased_internal().replace(['/', '$'], ".");
    if ty.array_dims > 0 {
        base.push_str(&"[]".repeat(ty.array_dims));
    }
    base
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

fn name_matches_member_prefix(name: &str, prefix_lower: Option<&str>) -> bool {
    let Some(prefix_lower) = prefix_lower else {
        return true;
    };
    if name.is_ascii() && prefix_lower.is_ascii() {
        let n = name.as_bytes();
        let p = prefix_lower.as_bytes();
        if p.len() > n.len() {
            return false;
        }
        return n.windows(p.len()).any(|w| w.eq_ignore_ascii_case(p));
    }
    name.to_lowercase().contains(prefix_lower)
}

impl MemberProvider {
    fn provide_from_source_members(
        &self,
        ctx: &SemanticContext,
        member_prefix: &str,
    ) -> Vec<CompletionCandidate> {
        let enclosing = ctx.enclosing_internal_name.as_deref().unwrap_or("");

        let scored = fuzzy::fuzzy_filter_sort(
            member_prefix,
            ctx.current_class_members
                .values()
                .filter(|member| !member.is_constructor_like()),
            |m| m.name(),
        );

        scored
            .into_iter()
            .map(|(m, score)| {
                let kind = match (m.is_method(), m.is_static()) {
                    (true, true) => CandidateKind::StaticMethod {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (true, false) => CandidateKind::Method {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (false, true) => CandidateKind::StaticField {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                    (false, false) => CandidateKind::Field {
                        descriptor: m.descriptor(),
                        defining_class: Arc::from(enclosing),
                    },
                };
                let insert_text = m.name().to_string();
                let detail = format!(
                    "{} {} {}",
                    if m.is_private() { "private" } else { "public" },
                    if m.is_static() { "static" } else { "" },
                    m.name()
                );
                let candidate = CompletionCandidate::new(m.name(), insert_text, kind, self.name())
                    .with_detail(detail)
                    .with_score(70.0 + score as f32 * 0.1);

                if let crate::semantic::context::CurrentClassMember::Method(md) = m {
                    candidate.with_callable_insert(
                        md.name.as_ref(),
                        &md.params.param_names(),
                        ctx.has_paren_after_cursor(),
                    )
                } else {
                    candidate
                }
            })
            .collect()
    }
}

/// Parses the receiver expression into an inner class name
fn resolve_receiver_type(
    expr: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    scope: IndexScope,
) -> Option<TypeName> {
    tracing::debug!(
        expr,
        locals_count = ctx.local_variables.len(),
        "resolve_receiver_type"
    );

    if expr == "this" {
        let r = ctx.enclosing_internal_name.clone();
        tracing::debug!(?r, "this -> enclosing");
        return r.map(TypeName::from);
    }

    if expr.trim().is_empty() {
        let r = ctx.enclosing_internal_name.clone();
        tracing::debug!(?r, "empty receiver -> implicit enclosing");
        return r.map(TypeName::from);
    }

    if let Some(type_ctx) = ctx.extension::<SourceTypeCtx>() {
        let resolver = TypeResolver::new(index);
        let resolved = expression_typing::resolve_expression_type(
            expr,
            &ctx.local_variables,
            ctx.enclosing_internal_name.as_ref(),
            &resolver,
            type_ctx,
            index,
        );
        if resolved.is_some() {
            return resolved;
        }
    } else {
        if let Some(class_name) = extract_constructor_class(expr) {
            return resolve_simple_name_to_internal(class_name, ctx, index, scope)
                .map(TypeName::from);
        }
        if let Some(internal) = resolve_method_call_receiver(expr, ctx, index, scope) {
            return Some(internal);
        }
        if let Some(resolved) = TypeResolver::new(index).resolve(
            expr,
            &ctx.local_variables,
            ctx.enclosing_internal_name.as_ref(),
        ) {
            return Some(resolved);
        }
    }

    if let Some(internal_class) = resolve_strict_class_name(expr, ctx, index, scope) {
        return Some(TypeName::from(internal_class));
    }

    None
}

fn normalize_receiver_owner_for_members(
    receiver: TypeName,
    ctx: &SemanticContext,
    index: &IndexView,
    scope: IndexScope,
) -> TypeName {
    if matches!(
        receiver.base_internal.as_ref(),
        "+" | "-" | "?" | "*" | "capture"
    ) {
        return receiver;
    }

    if let Some(type_ctx) = ctx.extension::<SourceTypeCtx>() {
        let source = receiver.erased_internal_with_arrays();
        if let Some(mut resolved) = type_ctx.resolve_type_name_relaxed(&source).map(|r| r.ty) {
            if !receiver.args.is_empty() {
                resolved.args = receiver.args.clone();
            }
            resolved.array_dims = receiver.array_dims;
            if resolved.contains_slash() {
                return resolved;
            }
        }
    }

    if receiver.contains_slash() {
        return receiver;
    }

    if let Some(internal) =
        resolve_simple_name_to_internal(receiver.erased_internal(), ctx, index, scope)
    {
        return TypeName {
            base_internal: internal,
            args: receiver.args,
            array_dims: receiver.array_dims,
        };
    }

    receiver
}

/// Extract "Foo" from "new Foo()" / "new Foo(a, b)".
fn extract_constructor_class(expr: &str) -> Option<&str> {
    let rest = expr.trim().strip_prefix("new ")?;
    let class_part = rest.split('(').next()?.trim();
    if class_part.is_empty() {
        None
    } else {
        Some(class_part)
    }
}

/// Parse receiver expressions of the form "someMethod()" / "someMethod(args)" against enclosing.
fn resolve_method_call_receiver(
    expr: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    _scope: IndexScope,
) -> Option<TypeName> {
    let paren = expr.find('(')?;
    if !expr.ends_with(')') {
        return None;
    }
    let method_name = expr[..paren].trim();
    if method_name.is_empty() || method_name.contains('.') || method_name.contains(' ') {
        return None;
    }
    let args_text = &expr[paren + 1..expr.len() - 1];
    let arg_count = if args_text.trim().is_empty() {
        0i32
    } else {
        args_text.split(',').count() as i32
    };
    let enclosing = ctx.enclosing_internal_name.as_deref()?;
    TypeResolver::new(index).resolve_method_return(enclosing, method_name, arg_count, &[])
}

fn resolve_strict_class_name(
    simple_name: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    _scope: IndexScope,
) -> Option<Arc<str>> {
    // Enclosing class
    if let Some(enclosing) = &ctx.enclosing_internal_name {
        let enclosing_simple = enclosing
            .rsplit('/')
            .next()
            .unwrap_or(enclosing)
            .rsplit('$')
            .next()
            .unwrap_or(enclosing);

        if simple_name == enclosing_simple {
            return Some(Arc::clone(enclosing));
        }
    }

    // Explicit Imports
    for imp in &ctx.existing_imports {
        let explicit_suffix = format!("/{}", simple_name);
        if imp.ends_with(&explicit_suffix) {
            return Some(Arc::clone(imp));
        }

        // wildcard imports (*)
        if let Some(pkg) = imp.strip_suffix("/*") {
            let candidate = format!("{}/{}", pkg, simple_name);

            if index.get_class(&candidate).is_some() {
                return Some(Arc::from(candidate));
            }
        }
    }

    // Same Package
    if let Some(enclosing) = &ctx.enclosing_internal_name
        && let Some(last_slash) = enclosing.rfind('/')
    {
        let pkg = &enclosing[..last_slash];
        let candidate = format!("{}/{}", pkg, simple_name);

        if index.get_class(&candidate).is_some() {
            return Some(Arc::from(candidate));
        }
    }

    // java.lang.*
    let java_lang_candidate = format!("java/lang/{}", simple_name);
    if index.get_class(&java_lang_candidate).is_some() {
        return Some(Arc::from(java_lang_candidate));
    }

    None
}

/// Resolves simple class names to internal names
/// Search order: imports → same package → global
fn resolve_simple_name_to_internal(
    simple: &str,
    ctx: &SemanticContext,
    index: &IndexView,
    _scope: IndexScope,
) -> Option<Arc<str>> {
    tracing::debug!(simple, "resolve_simple_name_to_internal called");

    let imported = index.resolve_imports(&ctx.existing_imports);
    tracing::debug!(
        simple,
        imported = ?imported.iter().map(|m| format!("name={} internal={}", m.name, m.internal_name)).collect::<Vec<_>>(),
        "resolve_simple_name: imports"
    );

    if let Some(m) = imported.iter().find(|m| m.name.as_ref() == simple) {
        let exists = index.get_class(m.internal_name.as_ref()).is_some();
        tracing::debug!(
            internal = m.internal_name.as_ref(),
            exists,
            "found in imports"
        );
        return Some(Arc::clone(&m.internal_name));
    }

    if let Some(pkg) = ctx.enclosing_package.as_deref() {
        let classes = index.classes_in_package(pkg);
        tracing::debug!(pkg, count = classes.len(), "same package classes");
        if let Some(m) = classes.iter().find(|m| m.name.as_ref() == simple) {
            return Some(Arc::clone(&m.internal_name));
        }
    }

    let candidates = index.get_classes_by_simple_name(simple);
    tracing::debug!(simple, count = candidates.len(), internals = ?candidates.iter().map(|c| c.internal_name.as_ref()).collect::<Vec<_>>(), "global lookup");

    if !candidates.is_empty() {
        return Some(Arc::clone(&candidates[0].internal_name));
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::index::WorkspaceIndex;
    use rust_asm::constants::{ACC_PRIVATE, ACC_PUBLIC, ACC_STATIC};
    use std::sync::Arc;
    use std::time::Instant;
    use tracing_subscriber::{EnvFilter, fmt};

    use crate::completion::provider::CompletionProvider;
    use crate::index::{
        ClassMetadata, ClassOrigin, FieldSummary, IndexScope, MethodParams, MethodSummary, ModuleId,
    };
    use crate::language::java::completion::providers::member::MemberProvider;
    use crate::language::java::type_ctx::SourceTypeCtx;
    use crate::semantic::LocalVar;
    use crate::semantic::context::{CurrentClassMember, CursorLocation, SemanticContext};
    use crate::semantic::types::{
        generics::substitute_type, parse_return_type_from_descriptor, type_name::TypeName,
    };

    fn init_test_tracing() {
        let _ = fmt()
            .with_test_writer()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")),
            )
            .try_init();
    }

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_method(name: &str, descriptor: &str, flags: u16, is_synthetic: bool) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            access_flags: flags,
            is_synthetic,
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    fn make_field(name: &str, descriptor: &str, flags: u16, is_synthetic: bool) -> FieldSummary {
        FieldSummary {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            annotations: vec![],
            access_flags: flags,
            is_synthetic,
            generic_signature: None,
        }
    }

    /// Small helper to build method members in tests.
    fn m(name: &str, flags: u16, is_private: bool) -> CurrentClassMember {
        let mut f = flags;
        if is_private {
            f |= ACC_PRIVATE;
        }
        CurrentClassMember::Method(Arc::new(make_method(name, "()V", f, false)))
    }

    /// Small helper to build field members in tests.
    fn f(name: &str, flags: u16, is_private: bool) -> CurrentClassMember {
        let mut f = flags;
        if is_private {
            f |= ACC_PRIVATE;
        }
        CurrentClassMember::Field(Arc::new(make_field(name, "I", f, false)))
    }

    fn make_index(methods: Vec<MethodSummary>, fields: Vec<FieldSummary>) -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("com/example")),
            name: Arc::from("Foo"),
            internal_name: Arc::from("com/example/Foo"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods,
            fields,
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        idx
    }

    fn ctx_with_type(receiver_internal: &str, prefix: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: Some(Arc::from(receiver_internal)),
                member_prefix: prefix.to_string(),
                receiver_expr: "someObj".to_string(),
                arguments: None,
            },
            prefix,
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    fn ctx_with_semantic_and_erased(
        semantic_receiver: TypeName,
        erased_receiver: &str,
        prefix: &str,
    ) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(semantic_receiver),
                receiver_type: Some(Arc::from(erased_receiver)),
                member_prefix: prefix.to_string(),
                receiver_expr: "someObj".to_string(),
                arguments: None,
            },
            prefix,
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    fn ctx_this(
        enclosing_simple: &str,
        enclosing_internal: &str,
        enclosing_pkg: &str,
        prefix: &str,
    ) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: prefix.to_string(),
                receiver_expr: "this".to_string(),
                arguments: None,
            },
            prefix,
            vec![],
            Some(Arc::from(enclosing_simple)),
            Some(Arc::from(enclosing_internal)),
            Some(Arc::from(enclosing_pkg)),
            vec![],
        )
    }

    #[test]
    fn test_instance_method_found() {
        let idx = make_index(
            vec![make_method(
                "getValue",
                "()Ljava/lang/String;",
                ACC_PUBLIC,
                false,
            )],
            vec![],
        );
        let ctx = ctx_with_type("com/example/Foo", "get");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "getValue"));
    }

    #[test]
    fn test_empty_prefix_returns_all_public() {
        let idx = make_index(
            vec![
                make_method("getName", "()Ljava/lang/String;", ACC_PUBLIC, false),
                make_method("setName", "(Ljava/lang/String;)V", ACC_PUBLIC, false),
                make_method("secret", "()V", ACC_PRIVATE, false),
            ],
            vec![],
        );
        let ctx = ctx_with_type("com/example/Foo", "");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_local_receiver_with_complex_wildcard_generics_preserves_outer_base() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/util")),
            name: Arc::from("List"),
            internal_name: Arc::from("java/util/List"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![make_method("size", "()I", ACC_PUBLIC, false)],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let view = idx.view(root_scope());
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into()],
            Some(name_table),
        ));

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "si".to_string(),
                receiver_expr: "nums".to_string(),
                arguments: None,
            },
            "si",
            vec![LocalVar {
                name: Arc::from("nums"),
                type_internal: TypeName::new("java/util/List<Box<? extends Number>>"),
                init_expr: None,
            }],
            None,
            None,
            None,
            vec!["java.util.*".into()],
        )
        .with_extension(type_ctx);

        let results = MemberProvider.provide(root_scope(), &ctx, &view);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "size"),
            "member completion should keep outer List base even when nested generic args are partial"
        );
    }

    #[test]
    fn test_source_style_box_extends_number_resolves_member_owner() {
        init_test_tracing();
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("size", "()I", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
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
                generic_signature: None,
                inner_class_of: None,
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
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(root_scope());
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into()],
            Some(name_table),
        ));

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "nums".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("nums"),
                type_internal: TypeName::new("List<Box<? extends Number>>"),
                init_expr: None,
            }],
            None,
            None,
            None,
            vec!["java.util.*".into()],
        )
        .with_extension(type_ctx);

        let results = MemberProvider.provide(root_scope(), &ctx, &view);
        assert!(
            results.iter().any(|c| c.label.as_ref() == "size"),
            "source-style generic receiver should still resolve outer List owner for member lookup"
        );
    }

    #[test]
    fn test_snapshot_member_completion_on_wildcard_generic_receiver() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    make_method("size", "()I", ACC_PUBLIC, false),
                    make_method("add", "(Ljava/lang/Object;)Z", ACC_PUBLIC, false),
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
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
                generic_signature: None,
                inner_class_of: None,
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
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(root_scope());
        let name_table = view.build_name_table();
        let type_ctx = Arc::new(SourceTypeCtx::new(
            None,
            vec!["java.util.*".into()],
            Some(name_table),
        ));
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "nums".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("nums"),
                type_internal: TypeName::new("List<Box<? extends Number>>"),
                init_expr: None,
            }],
            None,
            None,
            None,
            vec!["java.util.*".into()],
        )
        .with_extension(type_ctx);

        let mut labels: Vec<String> = MemberProvider
            .provide(root_scope(), &ctx, &view)
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();
        labels.sort();
        insta::assert_snapshot!(
            "member_completion_wildcard_generic_receiver_labels",
            labels.join("\n")
        );
    }

    #[test]
    fn test_same_class_private_visible_via_this() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/a/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                make_method("pri", "()V", ACC_PRIVATE | ACC_STATIC, false),
                make_method("func", "()V", ACC_PUBLIC, false),
            ],
            fields: vec![make_field("secret", "I", ACC_PRIVATE, false)],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = ctx_this("Main", "org/cubewhy/a/Main", "org/cubewhy/a", "pr");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "pri"));
    }

    #[test]
    fn test_this_dot_uses_source_members_including_private() {
        let idx = WorkspaceIndex::new();
        let members = vec![
            m("priFunc", ACC_PUBLIC, true), // is_private = true
            m("fun", ACC_PUBLIC, false),
            m("func", ACC_PUBLIC, false),
        ];

        let ctx =
            ctx_this("Main", "org/cubewhy/a/Main", "org/cubewhy/a", "").with_class_members(members);

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "priFunc"));
        assert!(results.iter().any(|c| c.label.as_ref() == "fun"));
    }

    #[test]
    fn test_no_this_completion_in_static_method() {
        let idx = WorkspaceIndex::new();
        let members = vec![
            f("staticField", ACC_STATIC, false),
            f("instanceField", ACC_PUBLIC, false),
        ];

        let enclosing_method = Arc::new(make_method(
            "main",
            "([Ljava/lang/String;)V",
            ACC_STATIC,
            false,
        ));

        let ctx = ctx_this("Main", "org/cubewhy/Main", "org/cubewhy", "")
            .with_class_members(members)
            .with_enclosing_member(Some(CurrentClassMember::Method(enclosing_method)));

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.is_empty());
    }

    #[test]
    fn test_bare_method_call_receiver_resolved() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Main"),
                internal_name: Arc::from("Main"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("getMain2", "()LMain2;", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: None,
                name: Arc::from("Main2"),
                internal_name: Arc::from("Main2"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("func", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "getMain2()".to_string(),
                arguments: None,
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "func"));
    }

    #[test]
    fn test_member_provider_prefers_semantic_receiver_type() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("com/example")),
                name: Arc::from("Legacy"),
                internal_name: Arc::from("com/example/Legacy"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("legacyOnly", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("com/example")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("listOnly", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = ctx_with_semantic_and_erased(
            TypeName::with_args("java/util/List", vec![TypeName::new("java/lang/String")]),
            "com/example/Legacy",
            "",
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "listOnly"));
        assert!(!results.iter().any(|c| c.label.as_ref() == "legacyOnly"));
    }

    #[test]
    fn test_member_provider_falls_back_to_legacy_receiver_type() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("com/example")),
            name: Arc::from("Legacy"),
            internal_name: Arc::from("com/example/Legacy"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![make_method("legacyOnly", "()V", ACC_PUBLIC, false)],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = ctx_with_type("com/example/Legacy", "");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(results.iter().any(|c| c.label.as_ref() == "legacyOnly"));
    }

    #[test]
    fn test_member_provider_arraylist_add_overloads_for_trace() {
        init_test_tracing();

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/util")),
            name: Arc::from("ArrayList"),
            internal_name: Arc::from("java/util/ArrayList"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                MethodSummary {
                    name: Arc::from("add"),
                    params: MethodParams::from([("TE;", "e")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TE;)Z")),
                    return_type: parse_return_type_from_descriptor("(TE;)Z"),
                },
                MethodSummary {
                    name: Arc::from("add"),
                    params: MethodParams::from([("I", "index"), ("TE;", "element")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(ITE;)V")),
                    return_type: parse_return_type_from_descriptor("(ITE;)V"),
                },
            ],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::with_args(
                    "java/util/ArrayList",
                    vec![TypeName::new("java/lang/String")],
                )),
                receiver_type: Some(Arc::from("java/util/ArrayList")),
                member_prefix: "add".to_string(),
                receiver_expr: "list".to_string(),
                arguments: None,
            },
            "add",
            vec![LocalVar {
                name: Arc::from("list"),
                type_internal: TypeName::with_args(
                    "java/util/ArrayList",
                    vec![TypeName::new("java/lang/String")],
                ),
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("Main")),
            None,
            vec![],
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().filter(|c| c.label.as_ref() == "add").count() >= 2,
            "expected at least 2 add overloads, got {:?}",
            results
                .iter()
                .filter(|c| c.label.as_ref() == "add")
                .map(|c| c.detail.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_member_provider_hot_path_timing_baseline() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("Collection"),
                internal_name: Arc::from("java/util/Collection"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    make_method("size", "()I", ACC_PUBLIC, false),
                    make_method("isEmpty", "()Z", ACC_PUBLIC, false),
                    make_method("add", "(Ljava/lang/Object;)Z", ACC_PUBLIC, false),
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: Some(Arc::from("java/util/Collection")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    make_method("get", "(I)Ljava/lang/Object;", ACC_PUBLIC, false),
                    make_method("add", "(ILjava/lang/Object;)V", ACC_PUBLIC, false),
                    make_method("add", "(Ljava/lang/Object;)Z", ACC_PUBLIC, false),
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("ArrayList"),
                internal_name: Arc::from("java/util/ArrayList"),
                super_name: Some(Arc::from("java/util/List")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    make_method("<init>", "()V", ACC_PUBLIC, false),
                    make_method("add", "(Ljava/lang/Object;)Z", ACC_PUBLIC, false),
                    make_method("add", "(ILjava/lang/Object;)V", ACC_PUBLIC, false),
                    make_method("get", "(I)Ljava/lang/Object;", ACC_PUBLIC, false),
                    make_method("size", "()I", ACC_PUBLIC, false),
                    make_method("trimToSize", "()V", ACC_PUBLIC, false),
                    make_method("ensureCapacity", "(I)V", ACC_PUBLIC, false),
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method(
                    "toString",
                    "()Ljava/lang/String;",
                    ACC_PUBLIC,
                    false,
                )],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::with_args(
                    "java/util/ArrayList",
                    vec![TypeName::new("java/lang/String")],
                )),
                receiver_type: Some(Arc::from("java/util/ArrayList")),
                member_prefix: "a".to_string(),
                receiver_expr: "list".to_string(),
                arguments: None,
            },
            "a",
            vec![LocalVar {
                name: Arc::from("list"),
                type_internal: TypeName::with_args(
                    "java/util/ArrayList",
                    vec![TypeName::new("java/lang/String")],
                ),
                init_expr: None,
            }],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/Main")),
            Some(Arc::from("org/cubewhy")),
            vec!["java.util.*".into()],
        );
        let view = idx.view(root_scope());

        let warmup = MemberProvider.provide(root_scope(), &ctx, &view);
        assert!(!warmup.is_empty(), "warmup should return candidates");

        let iters = 1500usize;
        let t0 = Instant::now();
        let mut total_candidates = 0usize;
        for _ in 0..iters {
            total_candidates += MemberProvider.provide(root_scope(), &ctx, &view).len();
        }
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let avg_us = elapsed_ms * 1000.0 / iters as f64;
        eprintln!(
            "member_hot_path_baseline: total_ms={elapsed_ms:.3} avg_us={avg_us:.3} total_candidates={total_candidates}"
        );
    }

    #[test]
    fn test_member_prefix_match_ascii_case_insensitive() {
        assert!(super::name_matches_member_prefix("getValue", Some("GET")));
        assert!(super::name_matches_member_prefix("getValue", Some("val")));
        assert!(!super::name_matches_member_prefix("getValue", Some("xyz")));
    }

    #[test]
    fn test_member_prefix_match_unicode_fallback() {
        assert!(super::name_matches_member_prefix("测试Value", Some("测试")));
        assert!(!super::name_matches_member_prefix("测试Value", Some("abc")));
    }

    #[test]
    fn test_member_provider_exact_prefix_regression() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy")),
            name: Arc::from("Sample"),
            internal_name: Arc::from("org/cubewhy/Sample"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                MethodSummary {
                    name: Arc::from("size"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("I")),
                },
                MethodSummary {
                    name: Arc::from("other"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("I")),
                },
            ],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::new("org/cubewhy/Sample")),
                receiver_type: Some(Arc::from("org/cubewhy/Sample")),
                member_prefix: "si".to_string(),
                receiver_expr: "sample".to_string(),
                arguments: None,
            },
            "si",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "size"),
            "exact prefix should still work for member provider: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_member_provider_uses_erased_owner_when_semantic_receiver_has_args() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: None,
                name: Arc::from("Box"),
                internal_name: Arc::from("Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method(
                    "get",
                    "()Ljava/lang/Object;",
                    ACC_PUBLIC,
                    false,
                )],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: None,
                name: Arc::from("Legacy"),
                internal_name: Arc::from("Legacy"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("legacyOnly", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::with_args("Box", vec![TypeName::new("R")])),
                receiver_type: Some(Arc::from("Legacy")),
                member_prefix: "ge".to_string(),
                receiver_expr: "x".to_string(),
                arguments: None,
            },
            "ge",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "get"),
            "erased owner lookup should still target Box and find get()"
        );
        assert!(
            !results.iter().any(|c| c.label.as_ref() == "legacyOnly"),
            "legacy receiver_type should not override semantic owner"
        );
    }

    #[test]
    fn test_snapshot_add_detail_wildcard_receiver_provenance() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
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
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
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
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
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
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

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
        let receiver_class_internal =
            receiver_semantic.to_internal_with_generics_for_substitution();

        let ctx = SemanticContext::new(
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
                type_internal: receiver_semantic.clone(),
                init_expr: None,
            }],
            Some(Arc::from("Demo")),
            Some(Arc::from("Demo")),
            None,
            vec!["java.util.*".into()],
        );

        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let add = results
            .iter()
            .find(|c| c.label.as_ref() == "add")
            .expect("expected add candidate");
        let detail = add.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("Box<? extends"),
            "detail should preserve wildcard bound structure, got: {}",
            detail
        );
        let list_meta = idx
            .view(root_scope())
            .get_class("java/util/List")
            .expect("List class should exist");
        let add_meta = list_meta
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "add")
            .expect("add method should exist");
        let param_token = add_meta
            .generic_signature
            .as_deref()
            .and_then(|sig| {
                sig.find('(')
                    .zip(sig.find(')'))
                    .map(|(s, e)| &sig[s + 1..e])
            })
            .unwrap_or("?");
        let substituted_param = substitute_type(
            &receiver_class_internal,
            list_meta.generic_signature.as_deref(),
            param_token,
        )
        .map(|t| t.to_internal_with_generics())
        .unwrap_or_else(|| "None".to_string());

        insta::assert_snapshot!(
            "member_add_wildcard_detail_provenance",
            format!(
                "receiver_semantic={}\nreceiver_class_internal={}\nmethod_desc={}\nmethod_generic_signature={:?}\nparam_token={}\nsubstituted_param={}\nadd_detail={}\n",
                receiver_semantic.to_internal_with_generics(),
                receiver_class_internal,
                add_meta.desc(),
                add_meta.generic_signature,
                param_token,
                substituted_param,
                detail
            )
        );
    }

    #[test]
    fn test_record_accessors_appear_in_member_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(crate::language::java::class_parser::parse_java_source(
            "record Point(int x, int y) {}",
            ClassOrigin::Unknown,
            None,
        ));

        let ctx = ctx_with_type("Point", "");
        let results = MemberProvider.provide(root_scope(), &ctx, &idx.view(root_scope()));
        let labels: Vec<&str> = results
            .iter()
            .map(|candidate| candidate.label.as_ref())
            .collect();
        assert!(labels.contains(&"x"), "labels={labels:?}");
        assert!(labels.contains(&"y"), "labels={labels:?}");
    }
}
