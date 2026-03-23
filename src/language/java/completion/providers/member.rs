use rust_asm::constants::ACC_STATIC;
use std::time::Instant;

use crate::completion::provider::{CompletionProvider, ProviderCompletionResult};
use crate::completion::scorer::AccessFilter;
use crate::completion::{CandidateKind, CompletionCandidate, fuzzy};
use crate::language::java::expression_typing;
use crate::language::java::render;
use crate::language::java::super_support::{is_super_receiver_expr, resolve_direct_super_type};
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{CursorLocation, SemanticContext};
use crate::{
    index::{IndexScope, IndexView},
    semantic::types::{ContextualResolver, TypeResolver, type_name::TypeName},
};
use std::sync::Arc;

const JAVA_LANG_OBJECT_INTERNAL: &str = "java/lang/Object";
const ARRAY_INTRINSIC_OWNER: &str = "[array]";

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

    fn is_applicable(&self, ctx: &SemanticContext) -> bool {
        matches!(
            ctx.location,
            CursorLocation::MemberAccess { .. } | CursorLocation::StaticAccess { .. }
        )
    }

    fn provide(
        &self,
        scope: IndexScope,
        ctx: &SemanticContext,
        index: &IndexView,
        _request: Option<&crate::lsp::request_context::RequestContext>,
        _limit: Option<usize>,
    ) -> crate::lsp::request_cancellation::RequestResult<ProviderCompletionResult> {
        if let Some(results) = self.provide_static_access(ctx, index) {
            return Ok(results.into());
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

        let is_this_receiver = receiver_expr == "this";
        let is_super_receiver = is_super_receiver_expr(receiver_expr);
        let is_implicit_receiver = receiver_expr.is_empty();

        if (is_this_receiver || is_super_receiver) && ctx.is_in_static_context() {
            return Ok(ProviderCompletionResult::default());
        }

        let trace_timing = tracing::enabled!(tracing::Level::DEBUG);
        let t_total = trace_timing.then(Instant::now);
        let t_resolve = trace_timing.then(Instant::now);

        let resolved_semantic = receiver_semantic_type
            .cloned()
            .or_else(|| receiver_owner_internal.map(TypeName::new))
            .or_else(|| {
                if is_this_receiver || is_super_receiver {
                    None
                } else {
                    ctx.typed_chain_receiver
                        .as_ref()
                        .map(|t| t.receiver_ty.clone())
                }
            })
            .or_else(|| resolve_receiver_type(receiver_expr, ctx, index, scope));

        let resolved_original = match resolved_semantic {
            Some(t) => t,
            None => {
                tracing::debug!(
                    receiver_expr,
                    "resolve_receiver_type returned None, returning empty"
                );
                return Ok(ProviderCompletionResult::default());
            }
        };

        let resolve_elapsed_ms = t_resolve
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or_default();

        let resolved_effective = if is_this_receiver || is_super_receiver {
            resolved_original.clone()
        } else {
            ctx.typed_chain_receiver
                .as_ref()
                .map(|t| t.receiver_ty.clone())
                .unwrap_or_else(|| resolved_original.clone())
        };

        let resolved_effective =
            normalize_receiver_owner_for_members(resolved_effective, ctx, index, scope);

        let base_class_internal = resolved_effective.erased_internal();

        if resolved_effective.is_array() {
            let class_internal_for_substitution =
                resolved_effective.to_internal_with_generics_for_substitution();
            return Ok(self
                .provide_array_members(
                    ctx,
                    index,
                    member_prefix,
                    &resolved_effective,
                    &class_internal_for_substitution,
                )
                .into());
        }

        tracing::debug!(base_class_internal, "looking up class in index");

        let is_same_class = (is_this_receiver || is_implicit_receiver)
            && is_same_enclosing_class_internal(base_class_internal, ctx);
        let allow_static_members = is_this_receiver || is_implicit_receiver;
        let only_static_members = is_implicit_receiver && ctx.is_in_static_context();

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

        let has_paren_after_cursor = ctx.is_followed_by_opener();
        let resolver = ContextualResolver::new(index, ctx);

        let mut results = Vec::new();
        let mut seen_methods: std::collections::HashSet<(Arc<str>, Arc<str>)> =
            std::collections::HashSet::new();
        let mut seen_fields: std::collections::HashSet<Arc<str>> = std::collections::HashSet::new();

        if (is_this_receiver || is_implicit_receiver) && !ctx.current_class_members.is_empty() {
            for candidate in self.provide_from_source_members(
                ctx,
                member_prefix,
                is_implicit_receiver,
                &resolver,
            ) {
                match &candidate.kind {
                    CandidateKind::Method { descriptor, .. }
                    | CandidateKind::StaticMethod { descriptor, .. } => {
                        seen_methods.insert((Arc::clone(&candidate.label), Arc::clone(descriptor)));
                    }
                    CandidateKind::Field { .. } | CandidateKind::StaticField { .. } => {
                        seen_fields.insert(Arc::clone(&candidate.label));
                    }
                    _ => {}
                }
                results.push(candidate);
            }
        }

        let flow_receiver_cast_plan = if is_this_receiver || is_super_receiver {
            None
        } else {
            build_flow_receiver_cast_plan(ctx, index, receiver_expr, &resolved_effective)
        };

        let t_mro = trace_timing.then(Instant::now);
        let mro_elapsed_ms = t_mro
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or_default();

        let t_collect = trace_timing.then(Instant::now);
        let mut total_mro_len = 0usize;
        for bound in resolved_effective.bounds_for_lookup() {
            let class_internal = bound.to_internal_with_generics();
            let class_internal_for_substitution =
                bound.to_internal_with_generics_for_substitution();
            let mro = index.mro(bound.erased_internal());
            total_mro_len += mro.len();
            for class_meta in &mro {
                for method in &class_meta.methods {
                    self.push_member_method_candidate(
                        &mut results,
                        index,
                        class_meta,
                        method,
                        &resolver,
                        &filter,
                        prefix_lower.as_deref(),
                        &mut seen_methods,
                        class_internal.as_str(),
                        &class_internal_for_substitution,
                        has_paren_after_cursor,
                        flow_receiver_cast_plan.as_ref(),
                        allow_static_members,
                        only_static_members,
                    );
                }
                for field in &class_meta.fields {
                    self.push_member_field_candidate(
                        &mut results,
                        index,
                        class_meta,
                        field,
                        &resolver,
                        &filter,
                        prefix_lower.as_deref(),
                        &mut seen_fields,
                        class_internal.as_str(),
                        &class_internal_for_substitution,
                        flow_receiver_cast_plan.as_ref(),
                        allow_static_members,
                        only_static_members,
                    );
                }
            }
        }

        if let (Some(t_collect), Some(t_total)) = (t_collect, t_total) {
            let collect_elapsed_ms = t_collect.elapsed().as_secs_f64() * 1000.0;
            tracing::debug!(
                resolve_ms = resolve_elapsed_ms,
                mro_ms = mro_elapsed_ms,
                collect_ms = collect_elapsed_ms,
                total_ms = t_total.elapsed().as_secs_f64() * 1000.0,
                mro_len = total_mro_len,
                candidates = results.len(),
                "MemberProvider.phase_timing"
            );
        }

        Ok(results.into())
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
    fn provide_static_access(
        &self,
        ctx: &SemanticContext,
        index: &IndexView,
    ) -> Option<Vec<CompletionCandidate>> {
        let (class_name_raw, member_prefix) = match &ctx.location {
            CursorLocation::StaticAccess {
                class_internal_name,
                member_prefix,
            } => (class_internal_name.as_ref(), member_prefix.as_str()),

            CursorLocation::MemberAccess {
                receiver_expr,
                member_prefix,
                receiver_semantic_type: None,
                receiver_type: None,
                arguments: None,
            } if is_likely_static_receiver(receiver_expr, ctx) => {
                (receiver_expr.as_str(), member_prefix.as_str())
            }

            _ => return None,
        };

        let class_meta = if let Some(meta) = index.get_class(class_name_raw) {
            meta
        } else {
            let mut candidates = index.get_classes_by_simple_name(class_name_raw).to_vec();
            if candidates.is_empty() {
                if is_self_class_by_simple_name(class_name_raw, ctx) {
                    let resolver = ContextualResolver::new(index, ctx);
                    return Some(self.provide_static_source_members(ctx, member_prefix, &resolver));
                }
                return Some(Vec::new());
            }
            if let Some(pkg) = ctx.effective_package()
                && let Some(pos) = candidates
                    .iter()
                    .position(|c| c.package.as_deref() == Some(pkg))
            {
                candidates.swap(0, pos);
            }
            candidates.into_iter().next().unwrap()
        };

        let is_same_class =
            is_same_enclosing_class_internal(class_meta.internal_name.as_ref(), ctx);
        let resolver = ContextualResolver::new(index, ctx);

        if is_same_class && !ctx.current_class_members.is_empty() {
            return Some(self.provide_static_source_members(ctx, member_prefix, &resolver));
        }

        let filter = if is_same_class {
            AccessFilter::same_class()
        } else {
            AccessFilter::member_completion()
        };

        let mut results = Vec::new();

        for method in &class_meta.methods {
            if method.name.as_ref() == "<init>" || method.name.as_ref() == "<clinit>" {
                continue;
            }
            if method.access_flags & ACC_STATIC == 0 {
                continue;
            }
            if !filter.is_method_accessible(method.access_flags, method.is_synthetic) {
                continue;
            }
            let Some(match_score) = fuzzy::fuzzy_match(member_prefix, method.name.as_ref()) else {
                continue;
            };
            results.push(
                CompletionCandidate::new(
                    Arc::clone(&method.name),
                    method.name.to_string(),
                    CandidateKind::StaticMethod {
                        descriptor: method.desc(),
                        defining_class: Arc::clone(&class_meta.internal_name),
                    },
                    self.name(),
                )
                .with_callable_insert(
                    method.name.as_ref(),
                    &method.params.param_names(),
                    ctx.is_followed_by_opener(),
                )
                .with_detail(render::method_detail(
                    class_meta.internal_name.as_ref(),
                    &class_meta,
                    method,
                    &resolver,
                ))
                .with_score(50.0 + match_score as f32 * 0.1),
            );
        }

        for field in &class_meta.fields {
            if field.access_flags & ACC_STATIC == 0 {
                continue;
            }
            if !filter.is_field_accessible(field.access_flags, field.is_synthetic) {
                continue;
            }
            let Some(match_score) = fuzzy::fuzzy_match(member_prefix, field.name.as_ref()) else {
                continue;
            };
            results.push(
                CompletionCandidate::new(
                    Arc::clone(&field.name),
                    field.name.to_string(),
                    CandidateKind::StaticField {
                        descriptor: Arc::clone(&field.descriptor),
                        defining_class: Arc::clone(&class_meta.internal_name),
                    },
                    self.name(),
                )
                .with_detail(render::field_detail(
                    class_meta.internal_name.as_ref(),
                    &class_meta,
                    field,
                    &resolver,
                ))
                .with_score(50.0 + match_score as f32 * 0.1),
            );
        }

        for inner in index.direct_inner_classes_of(class_meta.internal_name.as_ref()) {
            let Some(match_score) = fuzzy::fuzzy_match(member_prefix, inner.name.as_ref()) else {
                continue;
            };
            results.push(
                CompletionCandidate::new(
                    Arc::clone(&inner.name),
                    inner.name.to_string(),
                    CandidateKind::ClassName,
                    self.name(),
                )
                .with_replacement_mode(crate::completion::candidate::ReplacementMode::MemberSegment)
                .with_filter_text(inner.name.to_string())
                .with_detail(inner.source_name())
                .with_score(62.0 + match_score as f32 * 0.1),
            );
        }

        Some(results)
    }

    fn provide_array_members(
        &self,
        ctx: &SemanticContext,
        index: &IndexView,
        member_prefix: &str,
        receiver: &TypeName,
        class_internal_for_substitution: &str,
    ) -> Vec<CompletionCandidate> {
        let prefix_lower = if member_prefix.is_empty() {
            None
        } else {
            Some(member_prefix.to_lowercase())
        };
        let resolver = ContextualResolver::new(index, ctx);
        let has_paren_after_cursor = ctx.is_followed_by_opener();
        let mut results = Vec::new();
        let mut seen_methods = std::collections::HashSet::new();

        if name_matches_member_prefix("length", prefix_lower.as_deref()) {
            results.push(
                CompletionCandidate::new(
                    Arc::from("length"),
                    "length".to_string(),
                    CandidateKind::Field {
                        descriptor: Arc::from("I"),
                        defining_class: Arc::from(ARRAY_INTRINSIC_OWNER),
                    },
                    self.name(),
                )
                .with_detail("array intrinsic field — int length")
                .with_score(90.0),
            );
        }

        // resolve inherit member from Object for arrays
        for class_meta in index.mro(JAVA_LANG_OBJECT_INTERNAL) {
            for method in &class_meta.methods {
                self.push_member_method_candidate(
                    &mut results,
                    index,
                    &class_meta,
                    method,
                    &resolver,
                    &AccessFilter::member_completion(),
                    prefix_lower.as_deref(),
                    &mut seen_methods,
                    JAVA_LANG_OBJECT_INTERNAL,
                    class_internal_for_substitution,
                    has_paren_after_cursor,
                    None,
                    false,
                    false,
                );
            }
        }

        tracing::debug!(
            receiver_type = %receiver.to_internal_with_generics(),
            candidates = results.len(),
            "MemberProvider.array_members"
        );

        results
    }

    #[allow(clippy::too_many_arguments)]
    fn push_member_method_candidate(
        &self,
        results: &mut Vec<CompletionCandidate>,
        index: &IndexView,
        class_meta: &crate::index::ClassMetadata,
        method: &crate::index::MethodSummary,
        resolver: &ContextualResolver<'_>,
        filter: &AccessFilter,
        prefix_lower: Option<&str>,
        seen_methods: &mut std::collections::HashSet<(Arc<str>, Arc<str>)>,
        class_internal: &str,
        class_internal_for_substitution: &str,
        has_paren_after_cursor: bool,
        flow_receiver_cast_plan: Option<&FlowReceiverCastPlan>,
        allow_static_members: bool,
        only_static_members: bool,
    ) {
        if method.name.as_ref() == "<init>" || method.name.as_ref() == "<clinit>" {
            return;
        }
        let key = (Arc::clone(&method.name), Arc::clone(&method.desc()));
        if !seen_methods.insert(key) {
            return;
        }
        if !filter.is_method_accessible(method.access_flags, method.is_synthetic) {
            return;
        }
        if !name_matches_member_prefix(method.name.as_ref(), prefix_lower) {
            return;
        }

        let is_static = method.access_flags & ACC_STATIC != 0;
        if only_static_members && !is_static {
            return;
        }
        if is_static && !allow_static_members {
            return;
        }

        let mut candidate = CompletionCandidate::new(
            Arc::clone(&method.name),
            method.name.to_string(),
            if is_static {
                CandidateKind::StaticMethod {
                    descriptor: method.desc(),
                    defining_class: Arc::from(class_internal),
                }
            } else {
                CandidateKind::Method {
                    descriptor: method.desc(),
                    defining_class: Arc::from(class_internal),
                }
            },
            self.name(),
        )
        .with_callable_insert(
            method.name.as_ref(),
            &method.params.param_names(),
            has_paren_after_cursor,
        )
        .with_detail({
            render::method_detail(
                class_internal_for_substitution,
                class_meta,
                method,
                resolver,
            )
        });

        if let Some(plan) = flow_receiver_cast_plan
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

    #[allow(clippy::too_many_arguments)]
    fn push_member_field_candidate(
        &self,
        results: &mut Vec<CompletionCandidate>,
        index: &IndexView,
        class_meta: &crate::index::ClassMetadata,
        field: &crate::index::FieldSummary,
        resolver: &ContextualResolver<'_>,
        filter: &AccessFilter,
        prefix_lower: Option<&str>,
        seen_fields: &mut std::collections::HashSet<Arc<str>>,
        class_internal: &str,
        class_internal_for_substitution: &str,
        flow_receiver_cast_plan: Option<&FlowReceiverCastPlan>,
        allow_static_members: bool,
        only_static_members: bool,
    ) {
        if !seen_fields.insert(Arc::clone(&field.name)) {
            return;
        }
        if !filter.is_field_accessible(field.access_flags, field.is_synthetic) {
            return;
        }
        if !name_matches_member_prefix(field.name.as_ref(), prefix_lower) {
            return;
        }
        let is_static = field.access_flags & ACC_STATIC != 0;
        if only_static_members && !is_static {
            return;
        }
        if is_static && !allow_static_members {
            return;
        }

        let mut candidate = CompletionCandidate::new(
            Arc::clone(&field.name),
            field.name.to_string(),
            if is_static {
                CandidateKind::StaticField {
                    descriptor: Arc::clone(&field.descriptor),
                    defining_class: Arc::from(class_internal),
                }
            } else {
                CandidateKind::Field {
                    descriptor: Arc::clone(&field.descriptor),
                    defining_class: Arc::from(class_internal),
                }
            },
            self.name(),
        )
        .with_detail(render::field_detail(
            class_internal_for_substitution,
            class_meta,
            field,
            resolver,
        ));

        if let Some(plan) = flow_receiver_cast_plan
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

    fn provide_from_source_members(
        &self,
        ctx: &SemanticContext,
        member_prefix: &str,
        implicit_receiver: bool,
        resolver: &ContextualResolver<'_>,
    ) -> Vec<CompletionCandidate> {
        let enclosing = ctx.enclosing_internal_name.as_deref().unwrap_or("");
        let in_static = ctx.is_in_static_context();

        fuzzy::fuzzy_filter_sort(
            member_prefix,
            ctx.current_class_members
                .values()
                .filter(|member| !member.is_constructor_like())
                .filter(|member| !(implicit_receiver && in_static && !member.is_static())),
            |m| m.name(),
        )
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
            let detail = render::source_member_detail(enclosing, m, resolver);
            let candidate = CompletionCandidate::new(m.name(), insert_text, kind, self.name())
                .with_detail(detail)
                .with_score(70.0 + score as f32 * 0.1);

            if let crate::semantic::context::CurrentClassMember::Method(md) = m {
                candidate.with_callable_insert(
                    md.name.as_ref(),
                    &md.params.param_names(),
                    ctx.is_followed_by_opener(),
                )
            } else {
                candidate
            }
        })
        .collect()
    }

    fn provide_static_source_members(
        &self,
        ctx: &SemanticContext,
        member_prefix: &str,
        resolver: &ContextualResolver<'_>,
    ) -> Vec<CompletionCandidate> {
        let enclosing = ctx.enclosing_internal_name.as_deref().unwrap_or("");

        fuzzy::fuzzy_filter_sort(
            member_prefix,
            ctx.current_class_members
                .values()
                .filter(|member| !member.is_constructor_like() && member.is_static()),
            |m| m.name(),
        )
        .into_iter()
        .map(|(m, score)| {
            let kind = if m.is_method() {
                CandidateKind::StaticMethod {
                    descriptor: m.descriptor(),
                    defining_class: Arc::from(enclosing),
                }
            } else {
                CandidateKind::StaticField {
                    descriptor: m.descriptor(),
                    defining_class: Arc::from(enclosing),
                }
            };
            let insert_text = m.name().to_string();
            let detail = render::source_member_detail(enclosing, m, resolver);
            let candidate = CompletionCandidate::new(m.name(), insert_text, kind, self.name())
                .with_detail(detail)
                .with_score(70.0 + score as f32 * 0.1);

            if let crate::semantic::context::CurrentClassMember::Method(md) = m {
                candidate.with_callable_insert(
                    md.name.as_ref(),
                    &md.params.param_names(),
                    ctx.is_followed_by_opener(),
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

    if is_super_receiver_expr(expr) {
        let r = resolve_direct_super_type(ctx, index);
        tracing::debug!(?r, "super -> direct superclass");
        return r;
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
    if receiver.is_intersection() {
        return TypeName::intersection(
            receiver
                .args
                .iter()
                .map(|bound| normalize_receiver_owner_for_members(bound.clone(), ctx, index, scope))
                .collect(),
        )
        .with_array_dims(receiver.array_dims);
    }

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

fn is_self_class_by_simple_name(class_name_raw: &str, ctx: &SemanticContext) -> bool {
    ctx.enclosing_class
        .as_deref()
        .is_some_and(|enc| enc == class_name_raw)
}

fn is_same_enclosing_class_internal(class_internal: &str, ctx: &SemanticContext) -> bool {
    if ctx.enclosing_internal_name.as_deref() == Some(class_internal) {
        return true;
    }

    let Some(enclosing_simple) = ctx.enclosing_class.as_deref() else {
        return false;
    };

    let internal_simple = class_internal
        .rsplit('/')
        .next()
        .unwrap_or(class_internal)
        .rsplit('$')
        .next()
        .unwrap_or(class_internal);
    if internal_simple != enclosing_simple {
        return false;
    }

    let Some(pkg) = ctx.effective_package() else {
        return true;
    };

    match class_internal.rfind('/') {
        Some(last_slash) => &class_internal[..last_slash] == pkg,
        None => pkg.is_empty(),
    }
}

fn is_likely_static_receiver(expr: &str, ctx: &SemanticContext) -> bool {
    if matches!(expr, "this" | "super") {
        return false;
    }
    if expr.contains('(') || expr.contains('.') {
        return false;
    }
    if ctx
        .local_variables
        .iter()
        .any(|lv| lv.name.as_ref() == expr)
    {
        return false;
    }
    true
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
        0
    } else {
        args_text.split(',').count()
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
    if let Some(pkg) = ctx.effective_package() {
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
/// Search order: imports -> same package
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

    if let Some(pkg) = ctx.effective_package() {
        let classes = index.classes_in_package(pkg);
        tracing::debug!(pkg, count = classes.len(), "same package classes");
        if let Some(m) = classes.iter().find(|m| m.name.as_ref() == simple) {
            return Some(Arc::clone(&m.internal_name));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use crate::index::WorkspaceIndex;
    use crate::language::test_helpers::completion_context_from_source;
    use rust_asm::constants::{ACC_PRIVATE, ACC_PUBLIC, ACC_STATIC};
    use std::sync::Arc;
    use std::time::Instant;
    use tracing_subscriber::{EnvFilter, fmt};

    use crate::completion::{CandidateKind, provider::CompletionProvider};
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

    fn at(src: &str, line: u32, col: u32) -> SemanticContext {
        at_with_trigger(src, line, col, None)
    }

    fn at_with_trigger(src: &str, line: u32, col: u32, trigger: Option<char>) -> SemanticContext {
        completion_context_from_source("java", src, line, col, trigger)
    }

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

    fn static_ctx(class_raw: &str, prefix: &str, pkg: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from(class_raw),
                member_prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from("Main")),
            None,
            Some(Arc::from(pkg)),
            vec![],
        )
    }

    fn make_index_with_main() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("func"),
                annotations: vec![],
                params: MethodParams::empty(),
                access_flags: ACC_PUBLIC | ACC_STATIC,
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
        idx
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

    fn make_array_member_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    make_method("getClass", "()Ljava/lang/Class;", ACC_PUBLIC, false),
                    make_method("hashCode", "()I", ACC_PUBLIC, false),
                    make_method("equals", "(Ljava/lang/Object;)Z", ACC_PUBLIC, false),
                    make_method("toString", "()Ljava/lang/String;", ACC_PUBLIC, false),
                    make_method("notify", "()V", ACC_PUBLIC, false),
                    make_method("notifyAll", "()V", ACC_PUBLIC, false),
                    make_method("wait", "()V", ACC_PUBLIC, false),
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
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
                    make_method("substring", "(I)Ljava/lang/String;", ACC_PUBLIC, false),
                    make_method("charAt", "(I)C", ACC_PUBLIC, false),
                    make_method("isBlank", "()Z", ACC_PUBLIC, false),
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
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

    fn ctx_super(
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
                receiver_expr: "super".to_string(),
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

    fn ctx_implicit_with_inferred_package(
        enclosing_simple: &str,
        enclosing_internal: &str,
        inferred_pkg: &str,
        prefix: &str,
        static_context: bool,
    ) -> SemanticContext {
        let mut ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: prefix.to_string(),
                receiver_expr: String::new(),
                arguments: Some("(duck)".to_string()),
            },
            prefix,
            vec![],
            Some(Arc::from(enclosing_simple)),
            Some(Arc::from(enclosing_internal)),
            None,
            vec![],
        )
        .with_inferred_package(Arc::from(inferred_pkg));

        if static_context {
            ctx =
                ctx.with_enclosing_member(Some(CurrentClassMember::Method(Arc::new(make_method(
                    "main",
                    "([Ljava/lang/String;)V",
                    ACC_PUBLIC | ACC_STATIC,
                    false,
                )))));
        }

        ctx
    }

    fn ctx_static_access(class_internal: &str, prefix: &str) -> SemanticContext {
        SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from(class_internal),
                member_prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            None,
            None,
            None,
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
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.iter().any(|c| c.label.as_ref() == "getValue"));
    }

    #[test]
    fn test_implicit_static_call_uses_inferred_package_and_returns_static_members() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/example")),
            name: Arc::from("IntersectionDemo"),
            internal_name: Arc::from("org/example/IntersectionDemo"),
            super_name: Some(Arc::from("java/lang/Object")),
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                make_method(
                    "act",
                    "(Lorg/example/Duck;)V",
                    ACC_PUBLIC | ACC_STATIC,
                    false,
                ),
                make_method("actPrivate", "()V", ACC_PRIVATE | ACC_STATIC, false),
                make_method("actInstance", "()V", ACC_PUBLIC, false),
            ],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = ctx_implicit_with_inferred_package(
            "IntersectionDemo",
            "IntersectionDemo",
            "org/example",
            "ac",
            true,
        );
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(
            results.iter().any(|c| c.label.as_ref() == "act"),
            "implicit same-class static call should resolve through inferred package: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        assert!(matches!(
            results
                .iter()
                .find(|c| c.label.as_ref() == "act")
                .unwrap()
                .kind,
            CandidateKind::StaticMethod { .. }
        ));
        assert!(
            results.iter().any(|c| c.label.as_ref() == "actPrivate"),
            "implicit same-class static call should preserve same-class private access: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            !results.iter().any(|c| c.label.as_ref() == "actInstance"),
            "static context must not suggest instance members for implicit receiver: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_member_provider_handles_static_access_location() {
        let idx = make_index(
            vec![make_method(
                "create",
                "()Lcom/example/Foo;",
                ACC_PUBLIC | ACC_STATIC,
                false,
            )],
            vec![],
        );
        let ctx = ctx_static_access("com/example/Foo", "cre");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(results.iter().any(|c| c.label.as_ref() == "create"));
        assert!(matches!(
            results
                .iter()
                .find(|c| c.label.as_ref() == "create")
                .unwrap()
                .kind,
            CandidateKind::StaticMethod { .. }
        ));
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
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates;
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates;
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
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates
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
    fn test_member_completion_unions_intersection_receiver_bounds() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
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
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/io")),
                name: Arc::from("Closeable"),
                internal_name: Arc::from("java/io/Closeable"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("close", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Runnable"),
                internal_name: Arc::from("java/lang/Runnable"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("run", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = ctx_with_semantic_and_erased(
            TypeName::intersection(vec![
                TypeName::new("java/io/Closeable"),
                TypeName::new("java/lang/Runnable"),
            ]),
            "java/io/Closeable",
            "",
        );

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<&str> = results
            .iter()
            .map(|candidate| candidate.label.as_ref())
            .collect();
        assert!(labels.contains(&"close"), "labels={labels:?}");
        assert!(labels.contains(&"run"), "labels={labels:?}");
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
                make_method("pri", "()V", ACC_PRIVATE, false),
                make_method("func", "()V", ACC_PUBLIC, false),
            ],
            fields: vec![make_field("secret", "I", ACC_PRIVATE, false)],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = ctx_this("Main", "org/cubewhy/a/Main", "org/cubewhy/a", "pr");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.is_empty());
    }

    #[test]
    fn test_super_completion_uses_direct_super_from_source_hint() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
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
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Base"),
                internal_name: Arc::from("org/cubewhy/Base"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    make_method("baseWork", "()V", ACC_PUBLIC, false),
                    make_method("baseSecret", "()V", ACC_PRIVATE, false),
                ],
                fields: vec![make_field("baseValue", "I", ACC_PUBLIC, false)],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(root_scope());
        let type_ctx = Arc::new(
            SourceTypeCtx::from_view(Some(Arc::from("org/cubewhy")), vec![], view.clone())
                .with_current_class_super_name(Some(Arc::from("Base"))),
        );
        let ctx =
            ctx_super("Child", "org/cubewhy/Child", "org/cubewhy", "base").with_extension(type_ctx);

        let labels: Vec<String> = MemberProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates
            .into_iter()
            .map(|candidate| candidate.label.to_string())
            .collect();

        assert!(labels.iter().any(|label| label == "baseWork"));
        assert!(labels.iter().any(|label| label == "baseValue"));
        assert!(
            !labels.iter().any(|label| label == "baseSecret"),
            "private superclass members must stay hidden for super access"
        );
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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
    fn test_member_provider_array_receiver_uses_array_members_only() {
        let idx = make_array_member_index();
        let ctx = ctx_with_semantic_and_erased(
            TypeName::new("java/lang/String").with_array_dims(1),
            "java/lang/String",
            "",
        );

        let labels: Vec<String> = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates
            .into_iter()
            .map(|candidate| candidate.label.to_string())
            .collect();

        assert!(labels.iter().any(|label| label == "length"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "getClass"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "hashCode"), "{labels:?}");
        assert!(
            !labels.iter().any(|label| label == "substring"),
            "{labels:?}"
        );
        assert!(!labels.iter().any(|label| label == "charAt"), "{labels:?}");
        assert!(!labels.iter().any(|label| label == "isBlank"), "{labels:?}");
        assert!(!labels.iter().any(|label| label == "stream"), "{labels:?}");
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

        let warmup = MemberProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates;
        assert!(!warmup.is_empty(), "warmup should return candidates");

        let iters = 1500usize;
        let t0 = Instant::now();
        let mut total_candidates = 0usize;
        for _ in 0..iters {
            total_candidates += MemberProvider
                .provide_test(root_scope(), &ctx, &view, None)
                .candidates
                .len();
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
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
        idx.add_classes(
            crate::language::java::class_parser::parse_java_source_via_tree_for_test(
                "record Point(int x, int y) {}",
                ClassOrigin::Unknown,
                None,
            ),
        );

        let ctx = ctx_with_type("Point", "");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<&str> = results
            .iter()
            .map(|candidate| candidate.label.as_ref())
            .collect();
        assert!(labels.contains(&"x"), "labels={labels:?}");
        assert!(labels.contains(&"y"), "labels={labels:?}");
    }

    #[test]
    fn test_enum_constants_do_not_leak_into_instance_member_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(
            crate::language::java::class_parser::parse_java_source_via_tree_for_test(
                "enum Color { RED, GREEN }",
                ClassOrigin::Unknown,
                None,
            ),
        );

        let ctx = ctx_with_type("Color", "");
        let labels: Vec<String> = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates
            .into_iter()
            .map(|candidate| candidate.label.to_string())
            .collect();

        assert!(
            !labels
                .iter()
                .any(|label| label == "RED" || label == "GREEN"),
            "enum constants are static and must not appear in instance member completion: {labels:?}"
        );
    }

    #[test]
    fn test_this_branch_deduplicates_source_and_index_members() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![make_method("work", "()V", ACC_PUBLIC, false)],
            fields: vec![make_field("value", "I", ACC_PUBLIC, false)],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = ctx_this("Main", "org/cubewhy/Main", "org/cubewhy", "").with_class_members(vec![
            m("work", ACC_PUBLIC, false),
            f("value", ACC_PUBLIC, false),
        ]);

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert_eq!(
            results
                .iter()
                .filter(|candidate| candidate.label.as_ref() == "work")
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|candidate| candidate.label.as_ref() == "value")
                .count(),
            1
        );
    }

    #[test]
    fn test_inherited_duplicates_are_suppressed_for_member_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Base"),
                internal_name: Arc::from("org/cubewhy/Base"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("work", "()V", ACC_PUBLIC, false)],
                fields: vec![make_field("value", "I", ACC_PUBLIC, false)],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Child"),
                internal_name: Arc::from("org/cubewhy/Child"),
                super_name: Some(Arc::from("org/cubewhy/Base")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("work", "()V", ACC_PUBLIC, false)],
                fields: vec![make_field("value", "I", ACC_PUBLIC, false)],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
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
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = ctx_with_type("org/cubewhy/Child", "");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert_eq!(
            results
                .iter()
                .filter(|candidate| candidate.label.as_ref() == "work")
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|candidate| candidate.label.as_ref() == "value")
                .count(),
            1
        );
    }

    #[test]
    fn test_no_super_completion_in_static_method() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy")),
            name: Arc::from("Base"),
            internal_name: Arc::from("org/cubewhy/Base"),
            super_name: Some(Arc::from("java/lang/Object")),
            interfaces: vec![],
            annotations: vec![],
            methods: vec![make_method("work", "()V", ACC_PUBLIC, false)],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let enclosing_method = Arc::new(make_method(
            "main",
            "([Ljava/lang/String;)V",
            ACC_STATIC,
            false,
        ));

        let view = idx.view(root_scope());
        let type_ctx = Arc::new(
            SourceTypeCtx::from_view(Some(Arc::from("org/cubewhy")), vec![], view.clone())
                .with_current_class_super_name(Some(Arc::from("Base"))),
        );

        let ctx = ctx_super("Child", "org/cubewhy/Child", "org/cubewhy", "")
            .with_extension(type_ctx)
            .with_enclosing_member(Some(CurrentClassMember::Method(enclosing_method)));

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates;

        assert!(results.is_empty());
    }

    #[test]
    fn test_super_completion_prefers_super_mro_without_current_class_members() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Base"),
                internal_name: Arc::from("org/cubewhy/Base"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("work", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Child"),
                internal_name: Arc::from("org/cubewhy/Child"),
                super_name: Some(Arc::from("org/cubewhy/Base")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("work", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
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
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(root_scope());
        let type_ctx = Arc::new(
            SourceTypeCtx::from_view(Some(Arc::from("org/cubewhy")), vec![], view.clone())
                .with_current_class_super_name(Some(Arc::from("Base"))),
        );

        let ctx =
            ctx_super("Child", "org/cubewhy/Child", "org/cubewhy", "wo").with_extension(type_ctx);

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates;

        assert_eq!(
            results
                .iter()
                .filter(|candidate| candidate.label.as_ref() == "work")
                .count(),
            1
        );
    }

    #[test]
    fn test_this_completion_includes_inherited_members() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Base"),
                internal_name: Arc::from("org/cubewhy/Base"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("baseWork", "()V", ACC_PUBLIC, false)],
                fields: vec![make_field("baseValue", "I", ACC_PUBLIC, false)],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Child"),
                internal_name: Arc::from("org/cubewhy/Child"),
                super_name: Some(Arc::from("org/cubewhy/Base")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("childWork", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
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
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let ctx = ctx_this("Child", "org/cubewhy/Child", "org/cubewhy", "base");
        let labels: Vec<String> = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates
            .into_iter()
            .map(|candidate| candidate.label.to_string())
            .collect();

        assert!(labels.iter().any(|label| label == "baseWork"));
        assert!(labels.iter().any(|label| label == "baseValue"));
    }

    #[test]
    fn test_super_completion_includes_inherited_super_members() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("GrandBase"),
                internal_name: Arc::from("org/cubewhy/GrandBase"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("grandWork", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Base"),
                internal_name: Arc::from("org/cubewhy/Base"),
                super_name: Some(Arc::from("org/cubewhy/GrandBase")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![make_method("baseWork", "()V", ACC_PUBLIC, false)],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
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
                methods: vec![],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(root_scope());
        let type_ctx = Arc::new(
            SourceTypeCtx::from_view(Some(Arc::from("org/cubewhy")), vec![], view.clone())
                .with_current_class_super_name(Some(Arc::from("Base"))),
        );
        let ctx =
            ctx_super("Child", "org/cubewhy/Child", "org/cubewhy", "").with_extension(type_ctx);

        let labels: Vec<String> = MemberProvider
            .provide_test(root_scope(), &ctx, &view, None)
            .candidates
            .into_iter()
            .map(|candidate| candidate.label.to_string())
            .collect();

        assert!(labels.iter().any(|label| label == "baseWork"));
        assert!(labels.iter().any(|label| label == "grandWork"));
    }

    #[test]
    fn test_static_access_by_simple_name() {
        let index = make_index_with_main();
        let ctx = static_ctx("Main", "fun", "org/cubewhy");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "func"),
            "should find func via simple name lookup: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_static_access_by_internal_name() {
        let index = make_index_with_main();
        let ctx = static_ctx("org/cubewhy/Main", "fun", "org/cubewhy");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        assert!(results.iter().any(|c| c.label.as_ref() == "func"));
    }

    #[test]
    fn test_static_access_empty_prefix_returns_all_static() {
        let index = make_index_with_main();
        let ctx = static_ctx("Main", "", "org/cubewhy");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &index.view(root_scope()), None)
            .candidates;
        assert!(!results.is_empty());
        assert!(results.iter().any(|c| c.label.as_ref() == "func"));
    }

    // ── new tests for self-class static access ────────────────────────────

    /// Build an index that contains Main with a private static field and a
    /// public static method, located in org/cubewhy/a.
    fn make_index_with_self_class() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/a/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("main"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC | ACC_STATIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: None,
            }],
            fields: vec![
                FieldSummary {
                    name: Arc::from("randomField"),
                    descriptor: Arc::from("Lorg/cubewhy/Inst;"),
                    annotations: vec![],
                    access_flags: ACC_PRIVATE | ACC_STATIC,
                    is_synthetic: false,
                    generic_signature: None,
                },
                FieldSummary {
                    name: Arc::from("publicField"),
                    descriptor: Arc::from("I"),
                    access_flags: ACC_PUBLIC | ACC_STATIC,
                    annotations: vec![],
                    is_synthetic: false,
                    generic_signature: None,
                },
            ],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        idx
    }

    fn self_static_ctx(prefix: &str) -> SemanticContext {
        // Simulates: inside org.cubewhy.a.Main, typing "Main.|"
        SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("Main"), // simple name from parser
                member_prefix: prefix.to_string(),
            },
            prefix,
            vec![],
            Some(Arc::from("Main")),               // enclosing_class (simple)
            Some(Arc::from("org/cubewhy/a/Main")), // enclosing_internal_name
            Some(Arc::from("org/cubewhy/a")),      // enclosing_package
            vec![],
        )
    }

    #[test]
    fn test_self_class_static_private_field_visible() {
        // Main.| from inside Main — private static field must appear
        let idx = make_index_with_self_class();
        let ctx = self_static_ctx("");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "randomField"),
            "private static field should be visible when accessing own class: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_self_class_static_public_field_visible() {
        let idx = make_index_with_self_class();
        let ctx = self_static_ctx("");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.iter().any(|c| c.label.as_ref() == "publicField"));
    }

    #[test]
    fn test_self_class_only_static_members_no_instance() {
        // Even for same-class access, Cls.xxx must only show STATIC members
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/a/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![
                FieldSummary {
                    name: Arc::from("staticF"),
                    descriptor: Arc::from("I"),
                    access_flags: ACC_PUBLIC | ACC_STATIC,
                    annotations: vec![],
                    is_synthetic: false,
                    generic_signature: None,
                },
                FieldSummary {
                    name: Arc::from("instanceF"),
                    descriptor: Arc::from("I"),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC, // NOT static
                    is_synthetic: false,
                    generic_signature: None,
                },
            ],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);
        let ctx = self_static_ctx("");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "staticF"),
            "static field must appear"
        );
        assert!(
            results.iter().all(|c| c.label.as_ref() != "instanceF"),
            "instance field must NOT appear for Cls.xxx access: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_self_class_prefix_filter() {
        let idx = make_index_with_self_class();
        let ctx = self_static_ctx("rand");
        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "randomField"),
            "prefix 'rand' should match 'randomField': {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            results.iter().all(|c| c.label.as_ref() != "publicField"),
            "'rand' should not match 'publicField'"
        );
    }

    #[test]
    fn test_self_class_via_source_members_when_not_in_index() {
        // The current file is not compiled yet → class is absent from the index.
        // MemberProvider must fall back to current_class_members.
        let idx = WorkspaceIndex::new(); // empty — class not indexed

        let members = vec![
            // randomField: static + private
            CurrentClassMember::Field(Arc::new(make_field(
                "randomField",
                "Lorg/cubewhy/Inst;",
                ACC_STATIC | ACC_PRIVATE,
                false,
            ))),
            // instanceField: instance + public
            CurrentClassMember::Field(Arc::new(make_field(
                "instanceField",
                "I",
                ACC_PUBLIC,
                false,
            ))),
            // staticHelper: static + public
            CurrentClassMember::Method(Arc::new(make_method(
                "staticHelper",
                "()V",
                ACC_STATIC | ACC_PUBLIC,
                false,
            ))),
        ];

        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("Main"),
                member_prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")),
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        )
        .with_class_members(members);

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;

        assert!(
            results.iter().any(|c| c.label.as_ref() == "randomField"),
            "private static field from source members should appear: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
        assert!(
            results.iter().any(|c| c.label.as_ref() == "staticHelper"),
            "static method from source members should appear"
        );
        assert!(
            results.iter().all(|c| c.label.as_ref() != "instanceField"),
            "instance field must NOT appear even from source members: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_other_class_private_not_visible() {
        // Accessing a DIFFERENT class's static members → private must be hidden
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Other"),
            internal_name: Arc::from("org/cubewhy/a/Other"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![FieldSummary {
                name: Arc::from("secret"),
                descriptor: Arc::from("I"),
                annotations: vec![],
                access_flags: ACC_PRIVATE | ACC_STATIC,
                is_synthetic: false,
                generic_signature: None,
            }],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        // We are inside Main, accessing Other.secret
        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("Other"),
                member_prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("Main")),
            Some(Arc::from("org/cubewhy/a/Main")), // enclosing is Main, not Other
            Some(Arc::from("org/cubewhy/a")),
            vec![],
        );

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().all(|c| c.label.as_ref() != "secret"),
            "private field of another class must NOT be visible: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_locals_in_static_method_no_semicolon() {
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                String aVar = "test";
                String str = "a";
                s
            }
        }
    "#};
        let line = 4u32;
        let col = src.lines().nth(4).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "aVar"),
            "aVar should be extracted even without semicolon on current line: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(
            ctx.local_variables.iter().any(|v| v.name.as_ref() == "str"),
            "str should be extracted: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_locals_in_method_argument_no_semicolon() {
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                String aVar = "test";
                System.out.println(aVar)
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.find("aVar").unwrap() as u32 + 4;
        let ctx = at(src, line, col);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "aVar"),
            "aVar should be visible inside method argument: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_char_after_cursor_paren() {
        let src = indoc::indoc! {r#"
        class A {
            void fun() {
                this.priFunc()
            }
            private void priFunc() {}
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("priFunc").unwrap() as u32 + "priFunc".len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.is_followed_by_opener(),
            "char after cursor should be '(', got {:?}",
            ctx.char_after_cursor
        );
    }

    #[test]
    fn test_char_after_cursor_no_paren() {
        let src = indoc::indoc! {r#"
        class A {
            void fun() {
                this.priFunc
            }
            private void priFunc() {}
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("priFunc").unwrap() as u32 + "priFunc".len() as u32;
        let ctx = at(src, line, col);
        assert!(
            !ctx.is_followed_by_opener(),
            "no paren after cursor, got {:?}",
            ctx.char_after_cursor
        );
    }

    #[test]
    fn test_lowercase_class_name_static_access_via_provider() {
        use crate::index::{ClassMetadata, ClassOrigin, FieldSummary};
        use rust_asm::constants::{ACC_PUBLIC, ACC_STATIC};

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: None,
            name: Arc::from("myClass"),
            internal_name: Arc::from("myClass"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![FieldSummary {
                name: Arc::from("FIELD"),
                descriptor: Arc::from("I"),
                annotations: vec![],
                access_flags: ACC_PUBLIC | ACC_STATIC,
                is_synthetic: false,
                generic_signature: None,
            }],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        // Parser 产生 MemberAccess，enrich 后 receiver_type 仍为 None（不是局部变量）
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "FIELD".to_string(),
                receiver_expr: "myClass".to_string(),
                arguments: None,
            },
            "FIELD",
            vec![], // no locals named myClass
            None,
            None,
            None,
            vec![],
        );

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results.iter().any(|c| c.label.as_ref() == "FIELD"),
            "lowercase class name static field should be found via provider, got: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_static_member_fuzzy_match() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Main"),
            internal_name: Arc::from("org/cubewhy/a/Main"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![
                make_method("main", "()V", ACC_PUBLIC | ACC_STATIC, false),
                make_method("veryLongStaticName", "()V", ACC_PUBLIC | ACC_STATIC, false),
            ],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("org/cubewhy/a/Main"),
                member_prefix: "ma".to_string(),
            },
            "ma",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(results.iter().any(|c| c.label.as_ref() == "main"));
        assert!(
            results
                .iter()
                .all(|c| c.label.as_ref() != "veryLongStaticName")
        );
    }

    #[test]
    fn test_static_member_fuzzy_subsequence_match() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy/a")),
            name: Arc::from("Util"),
            internal_name: Arc::from("org/cubewhy/a/Util"),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![FieldSummary {
                name: Arc::from("veryLongStaticName"),
                descriptor: Arc::from("I"),
                annotations: vec![],
                access_flags: ACC_PUBLIC | ACC_STATIC,
                is_synthetic: false,
                generic_signature: None,
            }],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin: ClassOrigin::Unknown,
        }]);

        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("org/cubewhy/a/Util"),
                member_prefix: "vlsn".to_string(),
            },
            "vlsn",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        assert!(
            results
                .iter()
                .any(|c| c.label.as_ref() == "veryLongStaticName"),
            "fuzzy subsequence should match static member, got: {:?}",
            results.iter().map(|c| c.label.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_static_access_exposes_direct_nested_types() {
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
        ]);

        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("org/cubewhy/ChainCheck"),
                member_prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"Box"), "{:?}", labels);
    }

    #[test]
    fn test_static_access_nested_owner_exposes_its_nested_types() {
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

        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                member_prefix: "".to_string(),
            },
            "",
            vec![],
            Some(Arc::from("ChainCheck")),
            Some(Arc::from("org/cubewhy/ChainCheck")),
            Some(Arc::from("org/cubewhy")),
            vec![],
        );

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"BoxV"), "{:?}", labels);
    }

    #[test]
    fn test_enum_constants_appear_in_static_member_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(
            crate::language::java::class_parser::parse_java_source_via_tree_for_test(
                "enum Color { RED, GREEN, BLUE }",
                ClassOrigin::Unknown,
                None,
            ),
        );

        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("Color"),
                member_prefix: "".to_string(),
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        let results = MemberProvider
            .provide_test(root_scope(), &ctx, &idx.view(root_scope()), None)
            .candidates;
        let labels: Vec<&str> = results
            .iter()
            .map(|candidate| candidate.label.as_ref())
            .collect();
        assert!(labels.contains(&"RED"), "labels={labels:?}");
        assert!(labels.contains(&"GREEN"), "labels={labels:?}");
        assert!(labels.contains(&"BLUE"), "labels={labels:?}");
    }
}
