//! Lambda and method-reference inference ([JLS §15.27], [§15.13]): the type
//! of a lambda is the target functional interface's single abstract method,
//! and a method reference resolves its target and arity against the SAM.

use hir_expand::{
    body::{ExprData, ExprId, LambdaBody},
    name::Name,
    span::SpannedTypeRef,
};
use rowan::TextRange;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::java::{
    diagnostics::TypeError,
    method::{InvocationContext, InvocationMode, MethodData, member_set, single_abstract_method},
    resolve::resolve_type_ref,
    ty::{Ty, TyKind},
};

use super::{InferCtx, poly::MethodRefKind};

impl InferCtx<'_> {
    /// The type of a lambda expression ([JLS §15.27.2]): the target
    /// functional interface ([§15.27.3], [JLS §18.5.2.4]). The lambda's
    /// parameters are typed from the single abstract method of the target
    /// ([JLS §9.8]) and its body is inferred against the SAM's return type —
    /// a return statement inside a lambda body returns from the lambda, not
    /// from the enclosing method.
    pub(super) fn lambda_type(
        &mut self,
        expr: ExprId,
        params: &[(Name, Option<SpannedTypeRef>, TextRange)],
        body: LambdaBody,
    ) -> Ty {
        let Some(target) = self.target else {
            return self.error();
        };
        // §9.8/§15.27.3: a lambda expression's target must be a functional
        // interface — one with exactly one abstract method.
        let Some(sam) = single_abstract_method(self.db, &self.scope, &target) else {
            if !target.is_error(self.db) {
                self.report(TypeError::NotAFunctionalInterface {
                    expr,
                    target: target,
                });
            }
            return self.error();
        };
        if sam.params.len() != params.len() {
            return self.error();
        }
        self.lambda_params.push(FxHashMap::default());
        for ((name, declared, range), formal) in params.iter().zip(&sam.params) {
            let ty = match declared {
                Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
                // An inferred parameter takes the SAM formal's type
                // ([§15.27.3]); a captured super wildcard (`Consumer<?
                // super Element>`) contributes its *lower* bound, whose
                // members the parameter actually has.
                None => {
                    let ty = *formal;
                    match ty.kind(self.db) {
                        TyKind::TypeVar {
                            lower: Some(lower), ..
                        } => *lower,
                        _ => ty,
                    }
                }
            };
            self.check_lambda_param_duplicate(expr, name, *range);
            self.lambda_params
                .last_mut()
                .expect("lambda param scope pushed")
                .insert(name.clone(), ty);
        }
        let saved_ret = self.enclosing_ret.replace(self.decapture(&sam.ret));
        // The lambda body is its own flow context ([§15.27.2]): whether it
        // completes normally and which locals it definitely assigns say
        // nothing about the statement that contains the lambda. Its
        // checked-exception liability is likewise settled against the
        // *functional interface's* throws clause ([§15.27.3], [§11.2.1]) —
        // neither discharged by, nor propagated to, the enclosing method.
        let saved_throws = std::mem::replace(&mut self.enclosing_throws, sam.throws.clone());
        let saved_exited = self.exited;
        let saved_flow = self.flow.clone();
        // §15.27/[§8.3.1.2]: a lambda body is never the once-only blank-final
        // field initialization — the lambda may execute any number of times —
        // so a blank-final write inside it is always an error. Track the
        // depth so the write check rejects them.
        self.lambda_depth += 1;
        let thrown_before: FxHashSet<ExprId> = self.thrown.iter().map(|(_, e)| *e).collect();
        // §15.27.3: a *void*-compatible target (`Runnable`, `Consumer`, …)
        // gives an expression body no target — a statement expression may
        // produce a value that is simply discarded, and constraining it
        // against `void` would wrongly reject every value-returning generic
        // invocation in the body (`Consumer<State>` with
        // `s -> s.value(B(), true)` where `value` returns `State`). This
        // mirrors the speculative probe's `ret_is_void` handling in
        // [`super::overload::InferCtx::infer_lambda_body_result`]; the two
        // must agree or the final re-inference diverges from the probe.
        match body {
            // §15.27.2: an expression lambda's body is a poly expression
            // whose target is the SAM's return type — decaptured ([§5.1.10])
            // like [`Self::infer_lambda_body_result`]'s target, so a nested
            // generic invocation constrains the wildcard bound instead of
            // dead-ending on the capture variable. When the SAM is
            // void-compatible the body is *not* a poly expression: it is a
            // statement expression whose value is discarded, so it infers
            // standalone ([§15.2], [§15.27.3]).
            LambdaBody::Expr(expr) if !sam.ret.is_void_like(self.db) => {
                let _ =
                    self.with_target(Some(self.decapture(&sam.ret)), |this| this.infer_expr(expr));
            }
            LambdaBody::Expr(expr) => {
                let _ = self.with_target(None, |this| this.infer_expr(expr));
            }
            // §15.27.2: a block lambda's statements are inferred *standalone*
            // — the enclosing context's target type (the type of the variable
            // the lambda is being assigned to, or of the parameter it is the
            // argument of) is not a target for the statements inside the
            // body. Without the reset a value-returning generic invocation
            // used as a statement expression inside the body
            // (`builder.value(BOOL, true);` where `value` returns `State`)
            // would be constrained against the outer `Consumer<State>` target
            // and rejected.
            LambdaBody::Block(stmt) => {
                let _ = self.with_target(None, |this| this.infer_stmt(stmt));
            }
        }
        self.lambda_depth -= 1;
        self.settle_lambda_thrown(&sam.throws, &thrown_before);
        self.enclosing_throws = saved_throws;
        self.exited = saved_exited;
        self.flow = saved_flow;
        self.enclosing_ret = saved_ret;
        self.lambda_params.pop();
        target
    }

    /// Settles the checked-exception liability accumulated while inferring a
    /// lambda body ([§15.27.3]): every entry added by the body — entries whose
    /// throwing expression was not seen before the body — is checked against
    /// the functional interface's throws clause, reported there when no
    /// declared exception covers it, and always drained so that none of the
    /// body's liability leaks into the enclosing method's.
    pub(super) fn settle_lambda_thrown(&mut self, throws: &[Ty], before: &FxHashSet<ExprId>) {
        let new_entries: Vec<(Ty, ExprId)> = self
            .thrown
            .iter()
            .filter(|(_, expr)| !before.contains(expr))
            .cloned()
            .collect();
        for (ty, expr) in &new_entries {
            if !throws.iter().any(|declared| {
                crate::java::subtyping::is_assignable(self.db, &self.scope, ty, declared)
            }) {
                self.report(TypeError::UnreportedException {
                    expr: *expr,
                    thrown: *ty,
                });
            }
        }
        if !new_entries.is_empty() {
            self.thrown.retain(|(_, expr)| before.contains(expr));
        }
    }

    /// The type of a method reference ([JLS §15.13.2]): the target functional
    /// interface. The referenced method is resolved against the SAM's
    /// parameters ([§15.13.3]) so the qualifier is inferred.
    pub(super) fn method_ref_type(
        &mut self,
        expr: ExprId,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
    ) -> Ty {
        let Some(target) = self.target else {
            return self.error();
        };
        // §9.8/§15.13: a method reference's target must be a functional
        // interface too.
        let Some(sam) = single_abstract_method(self.db, &self.scope, &target) else {
            if !target.is_error(self.db) {
                self.report(TypeError::NotAFunctionalInterface {
                    expr,
                    target: target,
                });
            }
            return self.error();
        };
        self.resolve_method_ref(qualifier, type_name, name, &sam.params);
        target
    }

    /// The reference type, member-lookup context and *form* of a method
    /// reference ([JLS §15.13.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13.1)):
    /// a *type qualifier* — a bare name, `pkg.Type` or a nested `Outer.Inner`
    /// — resolves a type and yields a type-qualified (`Type::m`) reference; an
    /// instance qualifier is inferred as an expression and yields a bound
    /// (`expr::m`) reference. A type-qualified reference (`Path::of`,
    /// `List::stream`) may resolve a static method of an interface as well as
    /// of a class, so the member lookup uses `TypeQualified` mode: the
    /// virtual-invocation member filter of §15.12.3 must not swallow those
    /// static members, or `map`'s `<R>` stays unbound and the chain types
    /// `List<Object>`. `None` when neither qualifier form applies.
    pub(super) fn method_ref_target(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
    ) -> Option<(Ty, InvocationContext, bool)> {
        let (ref_ty, type_qualified) = match (type_name, qualifier) {
            (Some(tyref), _) => (
                resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
                true,
            ),
            (None, Some(expr)) => {
                if let Some(ty) = self.dotted_type_name(expr) {
                    (ty, true)
                } else if let ExprData::Var(name) = self.tree.expr(expr).clone()
                    && let Some(ty) = self.type_name_ty(&name)
                {
                    (ty, true)
                } else {
                    (self.infer_expr(expr), false)
                }
            }
            _ => return None,
        };
        // §15.13.1: a *type-qualified* reference (`Path::of`, `List::stream`)
        // may resolve a static method of an interface as well as of a class,
        // so the virtual-invocation member filter of §15.12.3 must not swallow
        // it — the static members of `Path` (`of(String, String…)`,
        // `of(URI)`) are invisible to `Virtual` mode, which left `map`'s `<R>`
        // unbound and typed the chain `List<Object>`.
        let ctx = if type_qualified {
            self.access.with_mode(InvocationMode::TypeQualified)
        } else {
            self.access.clone()
        };
        Some((ref_ty, ctx, type_qualified))
    }

    /// The member set the referenced method `name` resolves to against the
    /// single abstract method's parameters ([JLS §15.13.3]) and the kind of
    /// reference — how the SAM's parameters map onto the method's. `List::stream`
    /// as a `Function<List<MatchDecision>, ? extends Stream<…>>` calls
    /// `stream()` on `List<MatchDecision>`, so its result is
    /// `Stream<MatchDecision>` rather than a raw `Stream` that would erase the
    /// element type and leave `R` unconstrained. `None` when the reference does
    /// not name a reference type.
    pub(super) fn method_ref_members(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) -> Option<(Vec<MethodData>, MethodRefKind)> {
        let (ref_ty, ctx, type_qualified) = self.method_ref_target(qualifier, type_name)?;
        if !matches!(ref_ty.kind(self.db), TyKind::Reference { .. }) {
            return None;
        }
        let methods_on_type = member_set(self.db, &self.scope, &ref_ty, name.as_str(), &ctx);
        // §15.13.1/§15.13.3: a *static* reference (`Type::m` naming a static
        // member) and a *bound* reference (`expr::m` — the qualifier value is
        // the receiver) take the SAM's parameters as the method's own; only an
        // *unbound* instance reference (`Type::m` naming an instance member)
        // takes the SAM's first parameter as the receiver, which carries the
        // type arguments the bare type name lacks.
        let kind = if type_qualified && methods_on_type.iter().any(|m| m.is_static) {
            MethodRefKind::Static
        } else if type_qualified {
            MethodRefKind::Unbound
        } else {
            MethodRefKind::Bound
        };
        let receiver = match kind {
            MethodRefKind::Static | MethodRefKind::Bound => ref_ty,
            MethodRefKind::Unbound => {
                // Use the SAM receiver only when it is a *parameterized*
                // reference once inference has made it concrete — an
                // uninstantiated inference variable (`EntityId::externalName`
                // probed against `Function<? super α, …>`) carries no members
                // yet, and a plain or raw receiver (`? super EntityId`,
                // `Object`) adds no type arguments, so the bare type-name
                // lookup keeps the existing resolution (`EntityId::kind`,
                // `String::length`) untouched.
                match sam_params.first() {
                    Some(first) => {
                        let first = self.decapture(first);
                        if matches!(
                            first.kind(self.db),
                            TyKind::Reference { args, .. } if !args.is_empty()
                        ) {
                            first
                        } else {
                            ref_ty
                        }
                    }
                    None => ref_ty,
                }
            }
        };
        let methods = member_set(self.db, &self.scope, &receiver, name.as_str(), &ctx);
        Some((methods, kind))
    }

    /// The method a method reference names that is *potentially applicable*
    /// against the single abstract method's parameters ([JLS §15.13.1] inexact
    /// references): the applicable overload of the member set
    /// [`Self::method_ref_members`] resolves against, chosen by the SAM's
    /// arity and per-parameter compatibility. `None` when the reference names
    /// no reference type or no overload is congruent.
    pub(super) fn method_ref_candidate(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) -> Option<MethodData> {
        let (methods, kind) = self.method_ref_members(qualifier, type_name, name, sam_params)?;
        self.pick_method_ref(&methods, sam_params, kind)
    }

    /// Whether the method reference is *congruent* with the functional
    /// interface it targets ([JLS §15.13.2]): the referenced member set
    /// contains an overload applicable to the SAM — the check
    /// [`Self::method_ref_candidate`] performs. A reference that names no
    /// member at all reports `cannot find symbol` itself ([§15.12.1]) and a
    /// constructor or array-creation reference is congruent by arity alone, so
    /// neither turns an overload inapplicable before its own diagnostic fires.
    /// `false` means the reference cannot be compatible with this SAM at all —
    /// the hint that lets overload resolution reject the wrong candidate (a
    /// `thenComparing(Comparator)` overload probed with a `Foo::length`
    /// argument, whose `compare` SAM takes one more parameter than the
    /// reference's function descriptor) instead of leaving it applicable and
    /// ambiguous.
    pub(super) fn method_ref_congruent(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) -> bool {
        let Some((ref_ty, _, _)) = self.method_ref_target(qualifier, type_name) else {
            return true;
        };
        // §15.13.1/§15.13.4: `Type::new` is a constructor reference — also an
        // *array-creation* reference `T[]::new`; its congruence with a SAM is
        // a matter of constructor arity, resolved against the qualifier type.
        if matches!(
            ref_ty.kind(self.db),
            TyKind::Reference { .. } | TyKind::Array(_)
        ) && (name.as_str() == "new" || name.as_str() == "<missing>")
        {
            return true;
        }
        let Some((methods, kind)) = self.method_ref_members(qualifier, type_name, name, sam_params)
        else {
            return true;
        };
        if methods.is_empty() {
            // §15.12.1: an unknown name is a `cannot find symbol` on the
            // reference itself, not a reason to prefer another overload.
            return true;
        }
        self.pick_method_ref(&methods, sam_params, kind).is_some()
    }

    /// The return type a method reference contributes to inference
    /// ([JLS §15.13.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13.3)):
    /// the referenced method's declared return type instantiated with the
    /// qualifier's type arguments, or — for a constructor reference — the
    /// qualifier type itself. `<error>` when the reference does not resolve;
    /// the caller then simply skips the constraint.
    pub(super) fn method_ref_return(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) -> Ty {
        let Some((ref_ty, _, _)) = self.method_ref_target(qualifier, type_name) else {
            return self.error();
        };
        // §15.13.1/§15.13.4: `Type::new` is a constructor reference whose
        // "return" is the class itself — also an *array-creation* reference
        // `T[]::new`, whose "return" is the array type (the bounds are the
        // parameters, §15.13.4). The lowering records the `new` token of a
        // constructor reference as `<missing>`, so a missing member name on a
        // type qualifier is a constructor reference too.
        if matches!(
            ref_ty.kind(self.db),
            TyKind::Reference { .. } | TyKind::Array(_)
        ) && (name.as_str() == "new" || name.as_str() == "<missing>")
        {
            return ref_ty;
        }
        if !matches!(ref_ty.kind(self.db), TyKind::Reference { .. }) {
            return self.error();
        }
        // §15.13.1: an *inexact* reference to an overloaded method selects the
        // applicable candidate by the target functional interface — the SAM's
        // arity and per-parameter compatibility — not the first declared
        // overload, whose result would otherwise steer the enclosing
        // inference (`list.stream().map(P::of)` binding `map`'s `<R>`) to
        // `Object` and reject a `List<P>` constructor argument.
        self.method_ref_candidate(qualifier, type_name, name, sam_params)
            .map(|method| method.ret)
            .unwrap_or_else(|| self.error())
    }

    /// The method of a method reference's member set that is a *potentially
    /// applicable* candidate against the single abstract method's parameter
    /// types ([JLS §15.13.1] inexact references): arity — `sam_params.len()`
    /// value parameters for a static or bound reference, one fewer for an
    /// unbound instance reference whose receiver is the SAM's first parameter
    /// ([§15.13.3]) — and each corresponding SAM parameter assignable
    /// to the method's parameter in a loose invocation context
    /// ([§15.12.2.3]). The most specific applicable candidate
    /// ([§15.12.2.5]) wins; `None` when no overload is congruent, so the
    /// reference does not steer inference with a result.
    pub(super) fn pick_method_ref(
        &self,
        methods: &[MethodData],
        sam_params: &[Ty],
        kind: MethodRefKind,
    ) -> Option<MethodData> {
        let sam_count = sam_params.len();
        // The value parameters the referenced method must accept: all the
        // SAM's for a static or bound reference, all but the leading receiver
        // for an unbound instance reference.
        let base = match kind {
            MethodRefKind::Static | MethodRefKind::Bound => sam_count,
            MethodRefKind::Unbound => sam_count.saturating_sub(1),
        };
        // The SAM index of the referenced method's first value parameter: the
        // receiver is the SAM's first parameter for an unbound instance
        // reference, so the method's parameters start one slot in.
        let offset = match kind {
            MethodRefKind::Unbound => 1,
            MethodRefKind::Static | MethodRefKind::Bound => 0,
        };
        let mut applicable: Vec<MethodData> = Vec::new();
        for method in methods {
            let params = &method.params;
            // §15.12.2.4: a variable-arity method is applicable when the SAM
            // supplies at least its fixed parameters; the remainder pack into
            // the trailing array element.
            let (fixed, tail) = if method.varargs {
                if params.is_empty() || base < params.len() - 1 {
                    continue;
                }
                let split = params.len() - 1;
                match params[split].element(self.db) {
                    Some(element) => (&params[..split], Some((*element, base - split))),
                    None => continue,
                }
            } else {
                if params.len() != base {
                    continue;
                }
                (params.as_slice(), None)
            };
            let mut ok = true;
            for (i, method_param) in fixed.iter().enumerate() {
                if !self.method_ref_param_compatible(sam_params.get(i + offset), method_param) {
                    ok = false;
                    break;
                }
            }
            if ok && let Some((element, tail_count)) = &tail {
                for k in 0..*tail_count {
                    if !self.method_ref_param_compatible(
                        sam_params.get(fixed.len() + k + offset),
                        element,
                    ) {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                applicable.push(method.clone());
            }
        }
        if applicable.is_empty() {
            return None;
        }
        let pairs: Vec<(MethodData, MethodData)> =
            applicable.iter().map(|m| (m.clone(), m.clone())).collect();
        crate::java::method::choose_most_specific(self.db, &self.scope, &pairs)
            .or_else(|| applicable.into_iter().next())
    }

    /// Whether the referenced method's parameter accepts the SAM's
    /// corresponding parameter ([JLS §15.13.2]): the SAM type converts to the
    /// method's type in a loose invocation context ([§15.12.2.3]). A SAM
    /// parameter still carrying unresolved inference variables cannot be
    /// decided — the candidate stays applicable rather than steering inference
    /// away from a valid overload.
    pub(super) fn method_ref_param_compatible(
        &self,
        sam_param: Option<&Ty>,
        method_param: &Ty,
    ) -> bool {
        let Some(decaptured) = sam_param.map(|sam_param| self.decapture(sam_param)) else {
            return false;
        };
        if decaptured.contains_infer_var(self.db) || method_param.contains_infer_var(self.db) {
            return true;
        }
        crate::java::subtyping::is_assignable(self.db, &self.scope, &decaptured, method_param)
    }

    pub(super) fn resolve_method_ref(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) {
        // §15.13.1: resolve the applicable overload against the SAM — the same
        // selection [`Self::method_ref_return`] feeds the inference constraints
        // with — so a reference to a name that resolves only to inapplicable
        // overloads does not silently type against the first declaration.
        let _ = self.method_ref_candidate(qualifier, type_name, name, sam_params);
    }
}
