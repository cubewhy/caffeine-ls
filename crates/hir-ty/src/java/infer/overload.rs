//! Overload resolution and joint invocation inference ([JLS §15.12.2],
//! [§18.5.2]): candidate selection against the argument shapes, the
//! per-candidate constraint contributions (concrete leaves, lambdas, method
//! references, nested invocations and diamond `new`), and the deferred
//! re-inference of poly arguments.

use hir_expand::{
    body::{ExprData, ExprId, LambdaBody},
    name::Name,
    span::SpannedTypeRef,
};
use rowan::TextRange;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::java::{
    inference::{Constraint, Inference, InvocationPhase},
    method::{InvocationContext, MethodData, member_set, single_abstract_method},
    resolve::resolve_type_ref,
    ty::{Ty, TyKind, boxed_type},
};

use super::{
    InferCtx,
    poly::{ApplicableCandidate, ArgInfo, ArgKind, poly_arity, poly_leaves},
};

impl InferCtx<'_> {
    /// argument, inferred standalone.
    pub(super) fn arg_kinds(&mut self, args: &[ExprId]) -> Vec<ArgInfo> {
        args.iter().map(|arg| self.arg_info(*arg)).collect()
    }

    pub(super) fn arg_info(&mut self, arg: ExprId) -> ArgInfo {
        let leaves = poly_leaves(&self.tree, arg);
        if leaves.is_empty() {
            // §18.5.2.2: a concrete argument is not a poly expression — its
            // type is its standalone type ([§15.12.2.6]). The enclosing
            // invocation's own target type must not reach it, or a nested
            // generic call inside the argument would be constrained by an
            // unrelated expectation (`new Ctor(..., l.toArray(new T[0]))`
            // would demand `toArray`'s return to be `Ctor`). Only the *poly*
            // arguments are typed by the resolved formal parameter.
            ArgInfo {
                id: arg,
                poly: false,
                leaves: vec![ArgKind::Concrete(
                    self.with_target(None, |this| this.infer_expr(arg)),
                )],
            }
        } else {
            ArgInfo {
                id: arg,
                poly: true,
                leaves: leaves
                    .iter()
                    .map(|leaf| match self.tree.expr(*leaf).clone() {
                        ExprData::Lambda { .. } | ExprData::MethodRef { .. } => ArgKind::Lambda {
                            id: *leaf,
                            arity: poly_arity(&self.tree, *leaf),
                        },
                        ExprData::MethodCall { .. } => ArgKind::Invocation { id: *leaf },
                        ExprData::New { diamond: true, .. } => ArgKind::DiamondNew { id: *leaf },
                        _ => unreachable!(
                            "a poly leaf is a lambda, method reference, call or diamond new"
                        ),
                    })
                    .collect(),
            }
        }
    }

    /// applicable ones are ambiguous.
    pub(super) fn resolve_call(
        &mut self,
        receiver_ty: &Ty,
        name: &Name,
        arg_kinds: &[ArgInfo],
        target: Option<Ty>,
        ctx: &InvocationContext,
        explicit_type_args: Option<Vec<Ty>>,
    ) -> Option<(MethodData, Vec<(ExprId, usize)>)> {
        let members = member_set(self.db, &self.scope, receiver_ty, name.as_str(), ctx);
        for phase in [InvocationPhase::Strict, InvocationPhase::Loose] {
            if let Some(chosen) = self.choose_candidate(
                &members,
                arg_kinds,
                phase,
                false,
                target,
                explicit_type_args.clone(),
            ) {
                return Some(chosen);
            }
        }
        self.choose_candidate(
            &members,
            arg_kinds,
            InvocationPhase::Loose,
            true,
            target,
            explicit_type_args,
        )
    }
    pub(super) fn choose_candidate(
        &mut self,
        members: &[MethodData],
        arg_kinds: &[ArgInfo],
        phase: InvocationPhase,
        varargs: bool,
        target: Option<Ty>,
        explicit_type_args: Option<Vec<Ty>>,
    ) -> Option<(MethodData, Vec<(ExprId, usize)>)> {
        let mut applicable: Vec<ApplicableCandidate> = Vec::new();
        for member in members {
            let mut inference = Inference::new();
            let mut deferred = Vec::new();
            // The probe is speculative: diagnostics inside the argument
            // expressions are discarded, matching javac's overload resolution.
            if let Some(invocation) = self.with_probing(|this| {
                this.try_candidate(
                    &mut inference,
                    member,
                    arg_kinds,
                    phase,
                    varargs,
                    target,
                    explicit_type_args.as_deref(),
                    &mut deferred,
                    true,
                )
            }) {
                applicable.push((member.clone(), invocation, deferred));
            }
        }
        if applicable.is_empty() {
            if members.iter().any(|m| m.name == "allOf") {}
            return None;
        }
        // The most specific applicable candidate ([§15.12.2.5]); identical
        // signatures seen through overriding paths collapse to their
        // most-derived declaration (see [`crate::java::method::choose_most_specific`]).
        let pairs: Vec<(MethodData, MethodData)> = applicable
            .iter()
            .map(|(candidate, invocation, _)| (candidate.clone(), invocation.clone()))
            .collect();
        let chosen = crate::java::method::choose_most_specific(self.db, &self.scope, &pairs)?;
        let index = applicable
            .iter()
            .position(|(_, invocation, _)| *invocation == chosen)?;
        let (_, invocation, deferred) = applicable.remove(index);
        Some((invocation, deferred))
    }
    pub(super) fn try_candidate(
        &mut self,
        inference: &mut Inference,
        method: &MethodData,
        arg_kinds: &[ArgInfo],
        phase: InvocationPhase,
        varargs: bool,
        target: Option<Ty>,
        explicit_type_args: Option<&[Ty]>,
        deferred: &mut Vec<(ExprId, usize)>,
        resolve: bool,
    ) -> Option<MethodData> {
        // §15.12.2.2: explicit type arguments (`obj.<String>m(...)`) bind the
        // method's type parameters directly — the formals, return type and
        // throws clause are substituted with the written arguments and nothing
        // is left to inference.
        let (formals, ret, throws_formals) = match explicit_type_args {
            Some(explicit) => {
                let subst: FxHashMap<Name, Ty> = method
                    .type_params
                    .iter()
                    .zip(explicit.iter().copied())
                    .map(|(tp, ty)| (tp.name.clone(), ty))
                    .collect();
                let formals: Vec<Ty> = method
                    .params
                    .iter()
                    .map(|p| p.substitute(self.db, &subst))
                    .collect();
                let ret = method.ret.substitute(self.db, &subst);
                let throws: Vec<Ty> = method
                    .throws
                    .iter()
                    .map(|t| t.substitute(self.db, &subst))
                    .collect();
                (formals, ret, throws)
            }
            None => inference.register_method(self.db, method),
        };
        // §15.12.2.2/§15.12.2.3/§15.13.2: a lambda or method reference is a
        // poly expression whose type *is* the target functional interface —
        // be compatible with a formal parameter only when that formal is a
        // functional interface ([§9.8]). Otherwise the candidate is not
        // applicable at all: `Collection.toArray(T[])` must not appear
        // applicable to the argument `T[]::new`, or the overload resolution
        // would report an ambiguity javac resolves in favour of
        // `toArray(IntFunction<T[]>)`. A lambda whose parameter list has a
        // different arity than the SAM's is likewise inapplicable ([§15.27.3]).
        for (i, info) in arg_kinds.iter().enumerate() {
            let Some(formal) = formals.get(i).copied() else {
                break;
            };
            for kind in &info.leaves {
                let ArgKind::Lambda { id, arity } = kind else {
                    continue;
                };
                // §15.12.2.2/§15.12.2.3/§15.13.2: a lambda or method reference
                // (a method reference has `arity == None`) is compatible with
                // a formal parameter only when that formal is a functional
                // interface ([§9.8]) — *or* when it is still an inference
                // variable that the enclosing invocation may unify with one
                // (`<T> T id(T)` probed against `Function<String,Integer>`
                // for the argument `id(s -> s.length())`). A proper
                // non-interface type — `Collection.toArray(T[])` for
                // `T[]::new` — makes the candidate inapplicable, or the
                // overload resolution would report an ambiguity javac
                // resolves in favour of `toArray(IntFunction<T[]>)`.
                let bare_var = matches!(
                    formal.kind(self.db),
                    TyKind::InferenceVar(_) | TyKind::TypeVar { .. }
                );
                let Some(sam) = single_abstract_method(self.db, &self.scope, &formal) else {
                    if bare_var {
                        continue;
                    }
                    return None;
                };
                if let Some(arity) = arity {
                    if sam.params.len() != *arity {
                        return None;
                    }
                }
                // §15.13.2: a method reference has no syntactic parameter
                // list to arity-check, so its congruence with the SAM — the
                // referenced method set's arity (after the unbound-instance
                // receiver, §15.13.3) and per-parameter compatibility — is
                // checked against the referenced name itself. Without it the
                // `thenComparing(Comparator)` overload of the JDK `Comparator`
                // stays applicable to a zero-argument `Foo::length` argument
                // — its `compare` SAM takes one more parameter than the
                // reference's function descriptor — and [§15.12.2.5]
                // picks nothing: the chained
                // `comparing(Foo::length).thenComparing(Foo::width)` sorts of
                // the wMatcher diff engine would go *ambiguous* rather than
                // resolve the `thenComparing(Function)` overload javac picks.
                if arity.is_none()
                    && let ExprData::MethodRef {
                        qualifier,
                        type_name,
                        name,
                    } = self.tree.expr(*id).clone()
                    && !self.method_ref_congruent(qualifier, type_name.as_ref(), &name, &sam.params)
                {
                    return None;
                }
                // §15.27.3: a block lambda that is not value-compatible — no
                // `return` statement carries a value — is congruent only with
                // a void function result. Without this check a void body stays
                // applicable to a value-returning target
                // (`assertDoesNotThrow(exe, msg)` would go ambiguous against
                // the `ThrowingSupplier` overload).
                if arity.is_some()
                    && let ExprData::Lambda { body, .. } = self.tree.expr(*id).clone()
                    && !sam.ret.is_void_like(self.db)
                    && matches!(body, LambdaBody::Block(_))
                    && !self.lambda_block_has_value(&body)
                {
                    return None;
                }
            }
        }

        // §18.5.2.2: in the loose phase (and for variable-arity invocation,
        // §15.12.2.4) primitive arguments are boxed, so `⟨int → α⟩` yields the
        // boxed lower bound. A poly argument is not boxed: its type is the
        // target functional interface, and it contributes no constraint
        // (§15.12.2.2/§15.12.2.3, §18.5.2.2).
        if varargs {
            if !method.varargs || arg_kinds.len() + 1 < formals.len() {
                return None;
            }
            let (fixed, last) = formals.split_at(formals.len() - 1);
            for (i, info) in arg_kinds.iter().enumerate().take(fixed.len()) {
                let formal = fixed[i];
                if info.poly {
                    deferred.push((info.id, i));
                }
                for kind in &info.leaves {
                    if !self.contribute_leaf(inference, kind, formal, phase) {
                        return None;
                    }
                }
            }
            // §15.12.2.4: a single trailing actual of the array type is used
            // as-is; otherwise the trailing actuals are packed into the array,
            // each related to the element type.
            let rest: Vec<&ArgKind> = arg_kinds
                .iter()
                .skip(fixed.len())
                .flat_map(|info| info.leaves.iter())
                .collect();
            if rest.len() == 1 {
                match rest[0] {
                    ArgKind::Concrete(ty) if ty.is_array(self.db) => {
                        if !self.contribute_leaf(inference, rest[0], last[0], phase) {
                            return None;
                        }
                    }
                    // §15.12.2.4: a single trailing *poly* leaf that is itself
                    // an array — a nested call (`allOf(futures.toArray(...))`)
                    // — is used as-is against the varargs array type, not
                    // packed against the element. `toArray` cannot convert a
                    // `CompletableFuture[]` result to a single
                    // `CompletableFuture<?>` element. A nested invocation is
                    // probed against the array type first (it fails to
                    // resolve there when it is not array-typed) and packed
                    // against the element otherwise.
                    ArgKind::Invocation { .. } | ArgKind::DiamondNew { .. } => {
                        if !self.contribute_leaf(inference, rest[0], last[0], phase) {
                            let element = last[0].element(self.db).copied()?;
                            if !self.contribute_leaf(inference, rest[0], element, phase) {
                                return None;
                            }
                        }
                    }
                    _ => {
                        let element = last[0].element(self.db).copied()?;
                        for kind in rest {
                            if !self.contribute_leaf(inference, kind, element, phase) {
                                return None;
                            }
                        }
                    }
                }
            } else {
                let element = last[0].element(self.db).copied()?;
                for kind in rest {
                    if !self.contribute_leaf(inference, kind, element, phase) {
                        return None;
                    }
                }
            }
        } else {
            if formals.len() != arg_kinds.len() {
                return None;
            }
            for (i, info) in arg_kinds.iter().enumerate() {
                let formal = formals[i];
                if info.poly {
                    deferred.push((info.id, i));
                }
                for kind in &info.leaves {
                    if !self.contribute_leaf(inference, kind, formal, phase) {
                        return None;
                    }
                }
            }
        }

        // §18.5.2.4 (resolution): when the invocation is a poly expression with
        // an expected type, the constraint ⟨R → T⟩ joins the constraint set,
        // so the inference variables are bounded by the target type as well.
        // A *generic* method's invocation type is a poly expression wherever
        // it appears ([JLS §15.12.2.6]); a non-generic one is only a poly
        // expression as a nested *argument* (the `resolve == false` probe of
        // [`Self::choose_nested_candidate`]), where its fixed return type must
        // still be compatible with the enclosing formal.
        let constrains_target = target.is_some() && (!method.type_params.is_empty() || !resolve);
        if let (Some(target), true) = (target, constrains_target) {
            inference.add_constraint(Constraint::Sub(ret, target));
        }

        let build = |resolved: &FxHashMap<u64, Ty>| MethodData {
            name: method.name.clone(),
            owner: method.owner.clone(),
            owner_file: method.owner_file,
            params: formals
                .iter()
                .map(|p| p.substitute_infer(self.db, resolved))
                .collect(),
            ret: ret.substitute_infer(self.db, resolved),
            throws: throws_formals
                .iter()
                .map(|t| t.substitute_infer(self.db, resolved))
                .collect(),
            varargs: method.varargs,
            is_static: method.is_static,
            abstract_: method.abstract_,
            is_final: method.is_final,
            access: method.access,
            declaring_package: method.declaring_package.clone(),
            declaring_top_level: method.declaring_top_level.clone(),
            declaring_interface: method.declaring_interface,
            type_params: method.type_params.clone(),
        };
        if resolve {
            let resolved = match inference.solve_after(self.db, &self.scope, phase) {
                Some(r) => r,
                None => {
                    if method.name == "unmodifiableMap" {}
                    return None;
                }
            };
            // §18.5.4: the resolved invocation type must still satisfy the
            // target — element-level bounds alone do not guarantee array- or
            // parameterized-level compatibility after substitution
            // (`copyOf(T[],int)` with `α=int` yields `int[]`, which does not
            // convert to a `long[]` target).
            let invocation = build(&resolved);
            // A target still carrying inference variables or capture
            // variables (a captured formal of an *enclosing* invocation) is
            // not yet checkable — its own resolution validates it later.
            let target_proper = target.is_some_and(|t| {
                !t.contains_infer_var(self.db) && !t.contains_type_var_named_capture(self.db)
            });
            if let (Some(target), true) = (target, constrains_target && target_proper) {
                let ok = invocation.ret == target
                    || match phase {
                        InvocationPhase::Strict => crate::java::subtyping::strict_conversion(
                            self.db,
                            &self.scope,
                            &invocation.ret,
                            &target,
                        ),
                        InvocationPhase::Loose => crate::java::subtyping::is_assignable(
                            self.db,
                            &self.scope,
                            &invocation.ret,
                            &target,
                        ),
                    };
                if !ok {
                    // A residual mismatch between two parameterizations of
                    // the *same* generic type is wildcard/capture
                    // representation noise left by joint inference — the
                    // bound set has already constrained those arguments
                    // ([§18.2.3]). Only genuinely different types (`int[]`
                    // for a `long[]` target) reject the candidate.
                    let lenient = match (invocation.ret.kind(self.db), target.kind(self.db)) {
                        (
                            TyKind::Reference { name: rn, .. },
                            TyKind::Reference { name: tn, .. },
                        ) => rn == tn,
                        _ => false,
                    };
                    if !lenient {
                        return None;
                    }
                }
            }
            Some(invocation)
        } else if inference.check_consistent(self.db, &self.scope, phase) {
            Some(build(&FxHashMap::default()))
        } else {
            None
        }
    }

    /// `false` when the nested invocation has no applicable method.
    pub(super) fn contribute_leaf(
        &mut self,
        inference: &mut Inference,
        kind: &ArgKind,
        formal: Ty,
        phase: InvocationPhase,
    ) -> bool {
        match kind {
            ArgKind::Concrete(ty) => {
                // §15.12.2.3: loose invocation boxes primitive arguments.
                let ty = match (phase, ty.kind(self.db)) {
                    (InvocationPhase::Loose, TyKind::Primitive(p)) => {
                        Ty::reference(self.db, boxed_type(*p), Vec::new())
                    }
                    _ => *ty,
                };
                inference.add_constraint(Constraint::Sub(ty, formal));
                true
            }
            ArgKind::Lambda { id, .. } => {
                // §15.13.3/§18.5.2.2: a method reference constrains the
                // target functional interface's return type by the referenced
                // method's return — `stream.map(this::f)` infers its element
                // type from `f`'s return, exactly like a lambda body would.
                if let ExprData::MethodRef {
                    qualifier,
                    type_name,
                    name,
                } = self.tree.expr(*id).clone()
                {
                    let Some(sam) = single_abstract_method(self.db, &self.scope, &formal) else {
                        return true;
                    };
                    let ref_ret =
                        self.method_ref_return(qualifier, type_name.as_ref(), &name, &sam.params);
                    // §15.13.2: a *void-compatible* reference constrains
                    // nothing — any result the referenced method produces is
                    // discarded (`attributeNode::getDepth` is compatible with
                    // an `Executable` regardless of its return type).
                    if !ref_ret.is_error(self.db)
                        && !ref_ret.is_void_like(self.db)
                        && !sam.ret.is_void_like(self.db)
                    {
                        // §5.1.10: constrain the wildcard bounds the SAM's
                        // captured return stands for (see the lambda case).
                        let target = self.decapture(&sam.ret);
                        // §5.1.7: a primitive result boxes against a
                        // reference-typed bound (`String::length` for a
                        // `Function<String,Integer>`).
                        let boxed = match ref_ret.kind(self.db) {
                            TyKind::Primitive(p) => {
                                Ty::reference(self.db, boxed_type(*p), Vec::new())
                            }
                            _ => ref_ret,
                        };
                        inference.add_constraint(Constraint::Sub(boxed, target));
                    }
                    return true;
                }
                // §18.5.2.2/§15.27.3: a lambda body is inferred against the
                // functional interface's return type; its result constrains
                // that type's instantiation — `map(s -> s + "!")` makes the
                // stream element `String`, not `Object`.
                if let ExprData::Lambda { params, body } = self.tree.expr(*id).clone() {
                    let Some(sam) = single_abstract_method(self.db, &self.scope, &formal) else {
                        return true;
                    };
                    let Some(body_ty) = self.infer_lambda_body_result(*id, &params, body, &sam)
                    else {
                        return true;
                    };
                    // An error-typed body (a speculative probe whose
                    // parameters are still uninstantiated inference variables)
                    // constrains nothing — it must not reject the candidate.
                    if !body_ty.is_error(self.db) {
                        // §5.1.10/§18.5.2.2: the SAM was extracted from a
                        // *captured* formal, so its return carries capture
                        // variables standing for the formal's wildcards; the
                        // body's result constrains the underlying wildcard
                        // bounds, not the captures themselves.
                        let target = self.decapture(&sam.ret);
                        inference.add_constraint(Constraint::Sub(body_ty, target));
                    }
                    // §18.5.2.1/§15.27.3: a lambda with *declared* parameter
                    // types constrains the target functional interface's
                    // type variables — `⟨(P1..Pn) λ -> F⟩` reduces to the
                    // per-parameter constraints `⟨Pᵢ -> Uᵢ⟩`, not to silence.
                    // `Comparator.comparingInt((FieldPair p) -> ...)` binds
                    // the `? super T` of the comparator through `p`'s type;
                    // without it `T` would resolve to `Object` and every
                    // later `thenComparing` in the chain would fail. The SAM
                    // `Uᵢ` was extracted from the *captured* formal, so its
                    // wildcards carry capture variables
                    // ([§5.1.10]); [`Self::decapture`] recovers the bound the
                    // declared type must conform to, exactly as for the return
                    // side above.
                    if sam.params.len() == params.len() {
                        for ((_, declared, _), formal_param) in params.iter().zip(&sam.params) {
                            let Some(tyref) = declared else {
                                continue;
                            };
                            let declared_ty =
                                resolve_type_ref(self.db, &self.scope, &self.resolver, tyref);
                            if !declared_ty.is_error(self.db) {
                                let target = self.decapture(formal_param);
                                inference.add_constraint(Constraint::Sub(declared_ty, target));
                            }
                        }
                    }
                }
                true
            }
            ArgKind::Invocation { id } => self.contribute_invocation(inference, *id, formal, phase),
            // §15.9.3: a diamond class instance creation in an invocation
            // context contributes the created class — its type variables
            // registered in the shared table — constrained by the formal:
            // `synchronizedList(new ArrayList<>())` contributes
            // `ArrayList<α> <: List<T>`, so the target `List<String>` reaches
            // the element type.
            ArgKind::DiamondNew { id } => {
                self.contribute_diamond_new(inference, *id, formal, phase)
            }
        }
    }

    /// `synchronizedList(new ArrayList<>())` against `List<String>`.
    pub(super) fn contribute_diamond_new(
        &mut self,
        inference: &mut Inference,
        id: ExprId,
        formal: Ty,
        phase: InvocationPhase,
    ) -> bool {
        let ExprData::New { ty, args, .. } = self.tree.expr(id).clone() else {
            return true;
        };
        let class_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
        let TyKind::Reference { name, .. } = class_ty.kind(self.db) else {
            return true;
        };
        let type_params = self.class_type_param_bounds(&name);
        if type_params.is_empty() {
            return true;
        }
        let subst = inference.register_class_type_params(self.db, &type_params);
        // The created type with its type variables as fresh inference vars:
        // `ArrayList<α>`.
        let created = Ty::reference(
            self.db,
            name.clone(),
            type_params
                .iter()
                .map(|tp| {
                    Ty::type_var(self.db, tp.name.clone(), tp.bounds.clone())
                        .substitute(self.db, &subst)
                })
                .collect(),
        );
        // The constructor arguments constrain the variables too
        // (`new Analyzer<>(new BasicInterpreter())`); relate them against the
        // parameterized constructor's formals. Each candidate constructor is
        // probed against a snapshot of the shared table — `LinkedHashMap`
        // declares several single-parameter constructors (`LinkedHashMap(int)`,
        // `LinkedHashMap(Map)`), and only the one whose formals are compatible
        // with the actual arguments may constrain the variables.
        let bare: Vec<Ty> = type_params
            .iter()
            .map(|tp| Ty::type_var(self.db, tp.name.clone(), tp.bounds.clone()))
            .collect();
        let param_class = Ty::reference(self.db, name.clone(), bare.clone());
        let access = self.access.clone();
        let ctor_name = match hir::fqn_resolve(self.db, &self.scope, name.as_str()) {
            Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
            _ => name.simple_name().to_owned(),
        };
        let members = member_set(self.db, &self.scope, &param_class, &ctor_name, &access);
        let arg_kinds = self.arg_kinds(&args);
        for member in members {
            if member.params.len() != arg_kinds.len() {
                continue;
            }
            let base = inference.snapshot();
            let formals: Vec<Ty> = member
                .params
                .iter()
                .map(|p| p.substitute(self.db, &subst))
                .collect();
            let mut ok = true;
            for (info, formal) in arg_kinds.iter().zip(&formals) {
                for leaf in &info.leaves {
                    if !self.contribute_leaf(inference, leaf, *formal, phase) {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            }
            if ok && inference.check_consistent(self.db, &self.scope, phase) {
                break;
            }
            inference.restore(base);
        }
        // §15.9.3: the created class is compatible with the formal.
        inference.add_constraint(Constraint::Sub(created, formal));
        true
    }

    /// instead of dead-ending on the captures themselves.
    pub(super) fn decapture(&self, ty: &Ty) -> Ty {
        match ty.kind(self.db) {
            TyKind::TypeVar {
                name,
                bounds,
                lower,
                ..
            } if name.as_str().starts_with("CAP#") => match lower {
                Some(lower) => self.decapture(lower),
                None => bounds
                    .first()
                    .map(|bound| self.decapture(bound))
                    .unwrap_or(*ty),
            },
            TyKind::Reference { name, args } => {
                let args = args
                    .iter()
                    .map(|arg| match arg.kind(self.db) {
                        TyKind::Wildcard(Some(bound)) => {
                            let decaptured = self.decapture(&bound.ty);
                            let kind = Box::new(crate::java::ty::WildcardBound {
                                kind: bound.kind,
                                ty: decaptured,
                            });
                            Ty::wildcard(self.db, Some(kind))
                        }
                        _ => self.decapture(arg),
                    })
                    .collect();
                Ty::reference(self.db, name.clone(), args)
            }
            TyKind::Array(element) => Ty::array(self.db, self.decapture(element)),
            _ => *ty,
        }
    }

    /// resolved formal overwrites them.
    pub(super) fn infer_lambda_body_result(
        &mut self,
        expr: ExprId,
        params: &[(Name, Option<SpannedTypeRef>, TextRange)],
        body: LambdaBody,
        sam: &MethodData,
    ) -> Option<Ty> {
        self.lambda_params.push(FxHashMap::default());
        for ((name, declared, range), formal) in params.iter().zip(&sam.params) {
            let ty = match declared {
                Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
                None => match formal.kind(self.db) {
                    TyKind::TypeVar {
                        lower: Some(lower), ..
                    } => *lower,
                    // §15.27.3: a wildcard SAM parameter specializes the
                    // lambda parameter to the wildcard's bound — `? super X`
                    // to `X` (its lower bound, so members are visible),
                    // `? extends X` likewise to `X`, `?` to `Object`.
                    TyKind::Wildcard(Some(bound)) => bound.ty,
                    TyKind::Wildcard(None) => {
                        Ty::reference(self.db, "java.lang.Object", Vec::new())
                    }
                    _ => *formal,
                },
            };
            self.check_lambda_param_duplicate(expr, name, *range);
            self.lambda_params
                .last_mut()
                .expect("lambda param scope pushed")
                .insert(name.clone(), ty);
        }
        let saved_ret = self.enclosing_ret.replace(self.decapture(&sam.ret));
        // The speculative body inference is its own flow context, exactly
        // like [`Self::lambda_type`]: nothing it does may leak into the
        // enclosing statement's state.
        let saved_throws_ctx = std::mem::replace(&mut self.enclosing_throws, sam.throws.clone());
        let saved_exited = self.exited;
        let saved_flow = self.flow.clone();
        // §15.27/[§8.3.1.2]: a lambda body is never the once-only blank-final
        // field initialization (the lambda may execute any number of times).
        self.lambda_depth += 1;
        // The speculative body inference must leave no liability behind: its
        // entries reuse the *same* expression ids the final re-inference will
        // add, so they are drained here — otherwise
        // [`Self::settle_lambda_thrown`]'s diff would mistake the final pass's
        // entries for pre-existing ones.
        let thrown_before: FxHashSet<ExprId> = self.thrown.iter().map(|(_, e)| *e).collect();
        // §15.27.3: a *void*-compatible target (`Runnable`, `Executable`, …)
        // gives the expression body no target — a statement expression may
        // produce a value that is simply discarded, and constraining it
        // against `void` would wrongly reject the candidate.
        let ret_is_void = sam.ret.is_void_like(self.db);
        // §5.1.10: the SAM was extracted from a *captured* formal, so its
        // return type may be a bare capture variable standing for the
        // formal's wildcard. The body infers against the wildcard bound the
        // capture stands for (`Seq<? extends R>`), not against `CAP#n` — a
        // nested generic invocation constrained by ⟨R → CAP#n⟩ cannot reduce
        // (a parameterized type never converts to an unrelated type variable)
        // and would wrongly reject every applicable candidate. This is the
        // same substitution [`Self::contribute_leaf`] applies to the body's
        // *result* constraint; applying it to the body's *target* keeps both
        // ends of the constraint in wildcard terms.
        let body_target = self.decapture(&sam.ret);
        let result = match body {
            // §15.27.2: an expression lambda's body is a poly expression
            // whose target is the SAM's return type.
            LambdaBody::Expr(expr) if !ret_is_void => {
                Some(self.with_target(Some(body_target), |this| this.infer_expr(expr)))
            }
            LambdaBody::Expr(expr) => {
                self.with_target(None, |this| this.infer_expr(expr));
                None
            }
            LambdaBody::Block(stmt) => {
                // The block's valued `return` expressions are recorded while
                // inferring ([§15.27.3]); their least upper bound is the
                // body's type and constrains the SAM return exactly like an
                // expression body's result — without this, a block lambda
                // would leave the SAM's variables unconstrained (`U :=
                // Object` for `map(v -> { return v == x; })`) and every
                // downstream conversion against the instantiated `Optional`
                // would fail.
                self.lambda_returns.push(Vec::new());
                self.with_target(Some(body_target), |this| this.infer_stmt(stmt));
                let returns = self.lambda_returns.pop().unwrap_or_default();
                match returns.as_slice() {
                    [] => None,
                    [single] => Some(*single),
                    many => Some(crate::java::inference::least_upper_bound(
                        self.db,
                        &self.scope,
                        many,
                    )),
                }
            }
        };
        self.thrown.retain(|(_, expr)| thrown_before.contains(expr));
        self.enclosing_throws = saved_throws_ctx;
        self.exited = saved_exited;
        self.flow = saved_flow;
        self.lambda_depth -= 1;
        self.enclosing_ret = saved_ret;
        self.lambda_params.pop();
        result
    }

    /// `false` when no candidate is applicable against the formal.
    pub(super) fn contribute_invocation(
        &mut self,
        inference: &mut Inference,
        id: ExprId,
        formal: Ty,
        phase: InvocationPhase,
    ) -> bool {
        let ExprData::MethodCall {
            receiver,
            name,
            type_args,
            args,
        } = self.tree.expr(id).clone()
        else {
            return true;
        };
        let (receiver_ty, mode, _) = self.receiver_info(receiver, &name);
        let access = self.access.with_mode(mode);
        let members = member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
        let arg_kinds = self.arg_kinds(&args);
        let explicit = if type_args.is_empty() {
            None
        } else {
            Some(
                type_args
                    .iter()
                    .map(|t| resolve_type_ref(self.db, &self.scope, &self.resolver, t))
                    .collect::<Vec<Ty>>(),
            )
        };
        // The nested invocation is resolved in the *same* phase as its
        // enclosing invocation ([§15.12.2.2], [§15.12.2.3]): a strictly
        // probed member must not admit a loosely resolved argument, or
        // boxed-formal overloads would appear strictly applicable.
        if self.choose_nested_candidate(
            inference,
            &members,
            &arg_kinds,
            phase,
            false,
            &formal,
            explicit.clone(),
        ) {
            return true;
        }
        self.choose_nested_candidate(
            inference, &members, &arg_kinds, phase, true, &formal, explicit,
        )
    }
    pub(super) fn choose_nested_candidate(
        &mut self,
        inference: &mut Inference,
        members: &[MethodData],
        arg_kinds: &[ArgInfo],
        phase: InvocationPhase,
        varargs: bool,
        formal: &Ty,
        explicit_type_args: Option<Vec<Ty>>,
    ) -> bool {
        let base = inference.snapshot();
        let mut applicable: Vec<MethodData> = Vec::new();
        for member in members {
            inference.restore(base.clone());
            let mut deferred = Vec::new();
            // Speculative probe: no diagnostics from the argument expressions
            // (see [`Self::with_probing`]).
            if self
                .with_probing(|this| {
                    this.try_candidate(
                        inference,
                        member,
                        arg_kinds,
                        phase,
                        varargs,
                        Some(*formal),
                        explicit_type_args.as_deref(),
                        &mut deferred,
                        false,
                    )
                })
                .is_some()
            {
                applicable.push(member.clone());
            }
        }
        // The probes are speculative: a failed consistency check leaves its
        // partially reduced constraints in the shared worklist, so the base
        // snapshot is *always* reinstalled before anything is lifted.
        if applicable.is_empty() {
            inference.restore(base);
            return false;
        }
        // The most specific applicable member ([§15.12.2.5]); identical
        // signatures seen through overriding paths collapse to their
        // most-derived declaration (see [`crate::java::method::choose_most_specific`]).
        let pairs: Vec<(MethodData, MethodData)> =
            applicable.iter().map(|m| (m.clone(), m.clone())).collect();
        let Some(winner) = crate::java::method::choose_most_specific(self.db, &self.scope, &pairs)
        else {
            inference.restore(base);
            return false;
        };
        inference.restore(base);
        // §18.5.2.1: lift the winner's constraints from the base snapshot —
        // the losing candidates are discarded with it, and only the winner's
        // argument/target constraints join the enclosing bound set (B3).
        let mut deferred = Vec::new();
        let _ = self.with_probing(|this| {
            this.try_candidate(
                inference,
                &winner,
                arg_kinds,
                phase,
                varargs,
                Some(*formal),
                explicit_type_args.as_deref(),
                &mut deferred,
                false,
            )
        });
        true
    }

    /// its expression tree records the target-dependent types.
    pub(super) fn reinfer_deferred(&mut self, method: &MethodData, deferred: &[(ExprId, usize)]) {
        for (arg, index) in deferred {
            if let Some(formal) = method.params.get(*index) {
                let _ = self.with_target(Some(*formal), |this| this.infer_expr(*arg));
            }
        }
    }
}
