//! Method and constructor invocation inference ([JLS §15.12]): the call
//! site's receiver, the qualified/unqualified method lookup, arity checks,
//! and the explicit constructor-call handling of [§8.8.7.1].

use hir_expand::{
    body::{ExprData, ExprId},
    name::Name,
    span::SpannedTypeRef,
};
use rowan::TextRange;
use rustc_hash::FxHashSet;
use syntax::stub::PrimitiveType;

use crate::java::{
    diagnostics::{NonStaticThisKind, TypeError},
    method::{InvocationMode, MethodData, member_set},
    resolve::resolve_type_ref,
    subtyping::supertypes_impl,
    ty::{Ty, TyKind},
};

use super::{
    InferCtx, ResolvedMember, body_in_flight, body_types,
    context::find_method_item,
    overload::CallResolution,
    poly::{ArgInfo, ArgKind, reinfer_poly_standalone},
};

impl InferCtx<'_> {
    /// treatment.
    pub(super) fn resolve_type_name_checked(&self, name: &Name) -> Option<Ty> {
        let candidates = crate::java::resolve::candidate_fqns(&self.resolver, name);
        for candidate in candidates {
            if hir::fqn_resolve(self.db, &self.scope, candidate.as_str()).is_some() {
                return Some(Ty::reference(self.db, candidate.as_str(), Vec::new()));
            }
        }
        None
    }

    /// The invocation is a statement form and has no value.
    pub(super) fn ctor_call(
        &mut self,
        expr: ExprId,
        args: &[ExprId],
        target: hir_expand::body::CtorCallTarget,
    ) -> Ty {
        let receiver_ty = match target {
            hir_expand::body::CtorCallTarget::This => self.enclosing_class,
            // `super(args)`: the candidates are the constructors of the
            // direct superclass ([§8.8.7.1]), not of the enclosing class.
            hir_expand::body::CtorCallTarget::Super => Some(self.super_ty()),
        }
        .filter(|ty| !ty.is_error(self.db));
        let Some(receiver_ty) = receiver_ty else {
            for arg in args {
                let _ = self.infer_expr(*arg);
            }
            return self.error();
        };
        // Constructors are declared under the class's simple name in source;
        // library classes keep the JVMS `<init>` name ([JVMS §4.6]), so the
        // member lookup normalizes by how the target resolves ([§8.8.7.1]).
        let (fqn, _) = receiver_ty.as_reference(self.db).expect("class reference");
        let owner = Name::new(fqn.as_str().rsplit('.').next().unwrap_or(fqn.as_str()));
        let name = match hir::fqn_resolve(self.db, &self.scope, fqn.as_str()) {
            Some(hir::Resolved::Library(_)) => Name::new("<init>"),
            _ => owner.clone(),
        };
        // §8.8.7.1: an explicit `super(...)` delegation accesses the
        // superclass's constructors through the `super` keyword, whose
        // invocation mode is `Super` ([§15.12.1]) — a *protected* superclass
        // constructor may be invoked from a subclass this way even when the
        // receiver-type rule of [§6.6.2] would reject a virtual access.
        // (`this(...)` delegation, by contrast, is a virtual invocation.)
        let mode = match target {
            hir_expand::body::CtorCallTarget::Super => InvocationMode::Super,
            hir_expand::body::CtorCallTarget::This => InvocationMode::Virtual,
        };
        let access = self.access.with_mode(mode);
        let arg_kinds = self.arg_kinds(args);
        match self.resolve_call(&receiver_ty, &name, &arg_kinds, None, &access, None) {
            CallResolution::Selected {
                candidate,
                invocation: method,
                deferred,
            } => {
                self.record_member(expr, ResolvedMember::Method(*candidate));
                self.check_release_api_method(expr, &method);
                self.check_deprecated_method(expr, &method);
                self.reinfer_deferred(&method, &deferred);
                // §11.2.1: a delegating constructor's declared exceptions add
                // to the enclosing liability.
                for thrown in &method.throws {
                    if self.is_checked(thrown) {
                        self.thrown.push((*thrown, expr));
                    }
                }
                // §8.8.7.1/[§16]: a `this(...)` delegation runs the target
                // constructor before the rest of this body, so any blank
                // `final` field the target assigns is already assigned for
                // the remainder of *this* constructor — a later write to it
                // is the already-assigned error. (A `super(...)` delegation
                // assigns only the superclass's fields, which this class
                // cannot initialize.) A recursive chain
                // (`class A { A() { this(); } }`) resolves the target to a
                // constructor whose `body_types_query` is still being computed
                // — a salsa dependency-graph cycle under parallel diagnostics
                // collection — so a delegation target that is on the current
                // in-flight body (self) or already being tracked (mutual) skips
                // the seeding conservatively: the seeding simply does not run,
                // and the outer body still seeds from prior initializer bodies.
                if target == hir_expand::body::CtorCallTarget::This
                    && let Some(owner_file) = method.owner_file
                    && let Some(method_item) = find_method_item(self.db, owner_file, &method)
                    && !body_in_flight(owner_file, method_item)
                    && let Some(types) = body_types(self.db, owner_file, method_item)
                {
                    for touched in &types.field_touched {
                        self.flow.field_touched.insert(touched.clone());
                    }
                }
            }
            // The invocation selected no constructor: the applicable ones tie,
            // or none is applicable. Either way the reference still names the
            // constructors the member set found — recorded so goto-definition
            // answers each of them — and is reported as before, the diagnosis
            // ([`Self::report_wrong_arity`]) reading the same candidates.
            outcome => {
                // The concrete arguments were already inferred (and their
                // diagnostics reported) by `arg_kinds`; only the poly
                // arguments still need their standalone types.
                reinfer_poly_standalone(self, &arg_kinds);
                let members =
                    member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
                if members.is_empty() {
                    // §8.8.7.1: no constructor of that signature exists.
                    self.report(TypeError::NoSuchConstructor {
                        expr,
                        name: name.clone(),
                    });
                } else {
                    let name = name.clone();
                    let found = args.len();
                    self.report_wrong_arity(expr, name, Some(owner), &members, &arg_kinds, found);
                }
                // §15.12.2.5/[§15.12.2]: the tied candidates when there were
                // applicable ones, every constructor of the name otherwise.
                let named = match outcome {
                    CallResolution::Ambiguous(methods) => methods,
                    _ => members,
                };
                if !named.is_empty() {
                    self.record_member(expr, ResolvedMember::Unresolved(named));
                }
            }
        }
        // §8.8.7.1: the explicit constructor invocation (and the implicit
        // one) is the point past which the object is fully usable — its own
        // arguments were inferred *inside* the before-super window above
        // (`this(...)`/`super(...)` argument expressions may not reference
        // the un-initialized instance either).
        self.before_super = false;
        self.primitive(PrimitiveType::Void)
    }

    /// the message with javac's `constructor {Owner}() cannot be applied…`.
    pub(super) fn report_wrong_arity(
        &mut self,
        expr: ExprId,
        name: Name,
        owner: Option<Name>,
        members: &[crate::java::method::MethodData],
        arg_kinds: &[ArgInfo],
        found: usize,
    ) {
        let expected = members
            .first()
            .map(|m| m.params.len())
            .unwrap_or(arg_kinds.len());
        let best = members
            .iter()
            .min_by_key(|m| m.params.len().abs_diff(found))
            .cloned();
        // [§15.12.2.4]/[§18.4]: a variable-arity invocation maps the trailing
        // actuals onto the *element* type of the last formal (or uses a lone
        // array-typed actual as the array itself), and a generic member's
        // declared formals — which still carry the member's own type
        // parameters — are not the invocation type. Report against the packed,
        // instantiated formals, or the message names a type the invocation
        // never had (`required: 'V[]'` for a `V…` formal whose element is a
        // lambda's parameter).
        let required: Vec<Ty> = best
            .as_ref()
            .map(|m| {
                let formals = reported_formals(self.db, m);
                if m.varargs {
                    pack_varargs(self.db, &formals, found)
                } else {
                    formals
                }
            })
            .unwrap_or_default();
        // The actual argument types: a concrete argument carries its own
        // type; a poly argument (or an error/void one) has no standalone type
        // and renders as `<poly>`.
        let found_tys: Vec<Option<Ty>> = arg_kinds
            .iter()
            .map(|info| match info.leaves.as_slice() {
                [ArgKind::Concrete(ty)] => Some(*ty),
                _ => self
                    .types
                    .get(&info.id)
                    .copied()
                    .filter(|ty| !ty.is_error(self.db) && !ty.is_void_like(self.db)),
            })
            .collect();
        // The source range of every actual argument ([JLS §15.12.2]): the
        // "bad arguments" the diagnostic underlines, IntelliJ-style.
        let arg_ranges: Vec<TextRange> = arg_kinds
            .iter()
            .filter_map(|info| self.tree.expr_range(info.id))
            .collect();
        // The reason lines: against a same-arity candidate, every concrete
        // argument that does not convert (loosely) to its formal ([§5.3]) is
        // a mismatch; the first renders the `reason:` line, each also surfaces
        // at its own range as `related_information`.
        //
        // The comparison is against the packed, instantiated formals, and an
        // argument is only blamed when *every* candidate as close as `best`
        // rejects it: a tied set is one the invocation never chose between, so
        // attributing the mismatch to whichever candidate happened to come
        // first would name a formal the call was never resolved against
        // (`m(A...)`/`m(B...)` against `(A, B)`: each rejects a different
        // argument, and neither was selected).
        let min_distance = members
            .iter()
            .map(|m| m.params.len().abs_diff(found))
            .min()
            .unwrap_or(0);
        let closest: Vec<&crate::java::method::MethodData> = members
            .iter()
            .filter(|m| m.params.len().abs_diff(found) == min_distance)
            .collect();
        let ambiguous = closest.len() > 1;
        let mut bad_args = Vec::new();
        'arg: for (idx, info) in arg_kinds.iter().enumerate() {
            if info.poly {
                continue;
            }
            let [ArgKind::Concrete(ty)] = info.leaves.as_slice() else {
                continue;
            };
            let mut blamed: Option<Ty> = None;
            for member in &closest {
                let formals = reported_formals(self.db, member);
                let packed = if member.varargs {
                    pack_varargs(self.db, &formals, found)
                } else {
                    formals
                };
                let Some(formal) = packed.get(idx) else {
                    continue 'arg;
                };
                // [§18.4]/[§18.5.2.2]: a formal that still carries the member's
                // own type parameters is not the invocation type — the
                // argument was checked against the instantiated formal during
                // resolution, so comparing here again would report a mismatch
                // the invocation does not have.
                if formal.contains_declared_type_var(self.db) || formal.contains_infer_var(self.db)
                {
                    continue 'arg;
                }
                if crate::java::subtyping::is_assignable(self.db, &self.scope, ty, formal) {
                    continue 'arg;
                }
                blamed.get_or_insert(*formal);
            }
            if let Some(formal) = blamed {
                bad_args.push((idx, *ty, formal));
            }
        }
        // §15.12.2: when the invocation supplies *more* arguments than the
        // closest candidate takes, the surplus arguments are the offending
        // ones — the diagnostic points at them (IntelliJ-style) instead of
        // the whole argument list. No argument is specifically at fault when
        // the closest candidate is not unique (the arity is ambiguous) or the
        // call is too short; the diagnostic then stays on the member name.
        let surplus: Vec<usize> = match &best {
            Some(best) if !ambiguous && best.params.len() < found => {
                (best.params.len()..found).collect()
            }
            _ => Vec::new(),
        };
        // §15.12.2: every incompatible argument beyond the first is reported
        // as its own diagnostic at its own range — a split so each bad
        // argument draws its own error line (IntelliJ-style) instead of riding
        // as `related_information` on the first one, which an editor renders
        // only on that first range. Collected before `bad_args` moves into the
        // primary diagnostic, reported after it so the summary comes first.
        let extra_bad: Vec<(ExprId, Ty, Ty)> = bad_args
            .iter()
            .skip(1)
            .filter_map(|(idx, found, expected)| {
                arg_kinds.get(*idx).map(|info| (info.id, *found, *expected))
            })
            .collect();
        let _ = expected;
        self.report(TypeError::WrongArity {
            expr,
            name,
            owner,
            found,
            expected,
            varargs: best.as_ref().is_some_and(|m| m.varargs),
            required,
            found_tys,
            arg_ranges,
            bad_args,
            surplus,
        });
        for (expr, found, expected) in extra_bad {
            self.report(TypeError::IncompatibleTypes {
                expr,
                found,
                expected,
            });
        }
    }

    pub(super) fn method_call(
        &mut self,
        expr: ExprId,
        receiver: Option<ExprId>,
        name: Name,
        args: &[ExprId],
        target: Option<Ty>,
        explicit_type_args: Option<Vec<Ty>>,
    ) -> Ty {
        let (receiver_ty, mode, method_name_form) = self.receiver_info(receiver, &name);
        let access = self.access.with_mode(mode);
        // §5.1.10/§15.12.2: a method invocation's concrete actual arguments
        // are capture-converted before entering the constraint table — a
        // `Box<?>` actual against a `Box<T>` formal constrains `T` by the
        // wildcard's capture, not by the bare wildcard.
        let arg_kinds = self.arg_kinds_capturing(args);
        // §15.12.1: no member of the name on the receiver is a compile-time
        // error; members of the name that are all inapplicable (§15.12.2) is a
        // wrong-argument-count error.
        let members = member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
        match self.resolve_call(
            &receiver_ty,
            &name,
            &arg_kinds,
            target,
            &access,
            explicit_type_args,
        ) {
            CallResolution::Selected {
                candidate,
                invocation: method,
                deferred,
            } => {
                self.record_member(expr, ResolvedMember::Method(*candidate));
                self.check_release_api_method(expr, &method);
                self.check_deprecated_method(expr, &method);
                // §15.12.3/[§15.8.4]: `super.m(...)` invokes the method *as
                // declared in the supertype* — the receiver's own override
                // never applies. An abstract supertype member therefore has
                // no implementation to call, so the invocation is an error
                // whatever the enclosing class implements: javac `abstract
                // method m() in Abs cannot be accessed directly`. The
                // interface-qualified `I.super.m(...)` form ([§15.11.2]) is
                // the same (it exists to reach an interface *default*; an
                // abstract interface member has no body to reach).
                if (mode == InvocationMode::Super || mode == InvocationMode::Interface)
                    && method.abstract_
                {
                    self.report(TypeError::AbstractSuperAccess {
                        expr,
                        method: name.clone(),
                        owner: method.owner.display_name(self.db),
                    });
                }
                // §5.1.9/[§15.12.2.6]: the selected member was reached through
                // a *raw* receiver ([§4.8]) and its declaration is generic, so
                // its signature was erased and the invocation cannot be
                // statically checked — javac's `unchecked call to … as a
                // member of the raw type …`.
                self.warn_unchecked_invocation(expr, &method);
                // §5.1.9/§15.12.2.2: an actual argument whose type is raw
                // converting to a parameterized formal is an unchecked method
                // invocation. Reported once, on the invocation.
                self.warn_unchecked_arguments(expr, &arg_kinds, &method);
                // §18.5.2.2/§18.5.2.4: the resolved formal parameters are the
                // target types of the poly arguments — the lambda, method
                // reference or nested invocation is re-inferred against the
                // instantiated formal ([JLS §18.5.2.4]).
                self.reinfer_deferred(&method, &deferred);
                // §15.12.3: an unqualified invocation (`MethodName` form) of an
                // instance method from a static context — where `this` is
                // unavailable ([§8.1.3]) — is a compile-time error; javac
                // rejects the selected instance method here rather than
                // excluding it from the member set, so an overload that is less
                // specific than a static candidate still resolves first.
                if self.static_context && method_name_form && !method.is_static {
                    self.report(TypeError::NonStaticMethodFromStaticContext {
                        expr,
                        name: name.clone(),
                    });
                }
                // §8.8.7.1: an unqualified invocation of an instance method of
                // the class under construction before the supertype constructor
                // has run is a compile-time error.
                if self.before_super && method_name_form && !method.is_static {
                    self.report(TypeError::CannotReferenceBeforeSuper {
                        expr,
                        name: name.clone(),
                    });
                }
                // §11.2.1: an invocation of a method that declares checked
                // exceptions adds them to the enclosing liability.
                for thrown in &method.throws {
                    if self.is_checked(thrown) {
                        self.thrown.push((*thrown, expr));
                    }
                }
                method.ret
            }
            // On total failure the poly arguments keep their standalone types
            // (a lambda or method reference without a target is the error
            // type; a nested invocation resolves in isolation), so the
            // recorded types stay those of the argument expressions as
            // independent expressions. The concrete arguments were already
            // inferred by `arg_kinds` — re-inferring them would duplicate
            // their diagnostics.
            // The invocation selected no declaration: the applicable candidates
            // tie, or none is applicable. Either way the reference still names
            // the declarations of the member set — recorded so goto-definition
            // answers each of them (navigation is not a compile check) — and is
            // reported as before, the diagnosis reading the same candidates.
            outcome => {
                reinfer_poly_standalone(self, &arg_kinds);
                // §15.27.3/[§18.5.2.2]: a receiver still carrying an
                // *unresolved inference variable* — a lambda parameter typed
                // by the enclosing invocation's type parameter
                // (`Function<Z, T>.apply(..., e -> { e.isOverride() ... })`
                // with `Z` the method's variable: the `EnvironmentAttributeMap`
                // `Either` codec chain) — cannot resolve any member yet. The
                // call is *deferred*, not an error: it reports nothing and
                // yields the error type so its result contributes no
                // constraint ([§18.5.2.2] makes the variables mentioned by
                // the function type's parameter types *input variables* of
                // the lambda's constraints). The chosen method's
                // post-resolution re-inference ([§18.5.2.4]) types the
                // parameter by the instantiated formal and resolves the
                // member access there — `e.isOverride()` against the
                // substituted `Entry<T, ?>` (§5.1.10 capture applies to the
                // wildcard-parameterized receiver before lookup).
                if receiver_ty.contains_infer_var(self.db) {
                    return self.error();
                }
                // §15.12.1: no method of the name on the receiver. A receiver
                // that itself failed to type (an unassigned local, a failed
                // call) has reported its own error — do not cascade.
                if members.is_empty() {
                    if !receiver_ty.is_error(self.db) {
                        // §6.6: methods of the name exist but are not
                        // accessible from the enclosing class.
                        if !self.report_illegal_method_access(
                            expr,
                            receiver_ty,
                            name.as_str(),
                            &access,
                        ) {
                            self.report(TypeError::NoSuchMethod {
                                expr,
                                name: name.clone(),
                            });
                        }
                    }
                } else {
                    // §15.12.2: members of the name exist but none is
                    // applicable to the actual arguments.
                    let name = name.clone();
                    let found = args.len();
                    self.report_wrong_arity(expr, name, None, &members, &arg_kinds, found);
                }
                // §15.12.2.5/[§15.12.2]: the tied candidates when there were
                // applicable ones, every declaration of the name otherwise.
                let named = match outcome {
                    CallResolution::Ambiguous(methods) => methods,
                    _ => members,
                };
                if !named.is_empty() {
                    self.record_member(expr, ResolvedMember::Unresolved(named));
                }
                self.error()
            }
        }
    }

    /// ([§8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3)).
    pub(super) fn receiver_info(
        &mut self,
        receiver: Option<ExprId>,
        name: &Name,
    ) -> (Ty, InvocationMode, bool) {
        match receiver {
            Some(receiver) => {
                // JLS §6.5.2: a simple name that denotes both a type (via
                // import) and a field in scope reclassifies as an expression —
                // `UUID.withAlternative(...)` where `UUID` is both
                // `java.util.UUID` and the static field `NbtCodec<UUID> UUID`
                // is the field's instance method, not the type's (which has no
                // such member). Check the field chain before the type name.
                if let hir_expand::body::ExprData::Var(simple) = self.tree.expr(receiver).clone()
                    && self.lookup_local(&simple).is_none()
                    && self.pick_field_of_chain(simple.as_str()).is_some()
                {
                    // Fall through to the expression path below (virtual
                    // invocation on the field's type).
                } else if let Some(ty) = self.dotted_type_name(receiver) {
                    // `Type.method(...)` — a static invocation whose receiver
                    // expression is a pure type name ([§15.12.1]): a bare name or
                    // a qualified name such as `java.util.Collections`.
                    // §9.6.4.6: the written type name is itself a reference to
                    // the (possibly deprecated) class.
                    self.check_deprecated_type_qualifier(receiver, &ty);
                    return (ty, InvocationMode::Static, false);
                }
                match self.tree.expr(receiver).clone() {
                    // `super.method(...)` — a super invocation whose receiver is
                    // the superclass of the enclosing class ([§15.12.1]).
                    ExprData::Super { qualifier: None } => {
                        // §8.1.3: `super` names the enclosing instance, which
                        // does not exist in a static context. Reported at the
                        // `super` keyword, not the invoked member.
                        if self.static_context {
                            self.report(TypeError::NonStaticThisFromStaticContext {
                                expr: receiver,
                                keyword: NonStaticThisKind::Super,
                            });
                        }
                        (self.super_ty(), InvocationMode::Super, false)
                    }
                    // §15.11.2/§15.12.1: `I.super.m(...)` — a qualified-super
                    // invocation selects the default method of the *named*
                    // interface; the receiver type is `I` itself and the mode
                    // restricts candidates to instance members.
                    ExprData::Super {
                        qualifier: Some(qualifier),
                    } => (
                        {
                            if self.static_context {
                                self.report(TypeError::NonStaticThisFromStaticContext {
                                    expr: receiver,
                                    keyword: NonStaticThisKind::Super,
                                });
                            }
                            let raw = resolve_type_ref(
                                self.db,
                                &self.scope,
                                &self.resolver,
                                &qualifier.ty,
                            );
                            // §15.11.2/[§8.1.3]: the qualifier of `I.super.m`
                            // must be a *direct superinterface* of the
                            // enclosing class (or an interface it inherits) —
                            // `J.super` for a `J` the class neither implements
                            // nor (transitively) extends as an interface is
                            // javac's `not an enclosing class: J`. Reporting
                            // here (rather than silently resolving the raw
                            // qualifier's members) matches javac, which
                            // rejects the form outright.
                            if !self.qualifier_is_superinterface(&raw) {
                                self.report(TypeError::QualifiedSuperNotEnclosing {
                                    expr: receiver,
                                    qualifier: raw,
                                });
                            }
                            // §15.11.2: the receiver of `I.super.m(...)` is the
                            // interface `I` *as inherited* — parameterized by
                            // the enclosing class's own type arguments, not a
                            // raw `I` (whose members would erase to their
                            // bounds).
                            self.qualified_super_ty(&qualifier)
                        },
                        InvocationMode::Interface,
                        false,
                    ),
                    // §15.12.1: the receiver of an invocation is inferred
                    // *standalone* — the invocation's own target type does
                    // not reach it, or a chained generic call would inherit
                    // an unrelated expectation (`x.collect(toList())` would
                    // demand `concat(...)`'s stream to be a `List`).
                    _ => (
                        self.with_target(None, |this| this.infer_expr(receiver)),
                        InvocationMode::Virtual,
                        false,
                    ),
                }
            }
            // An unqualified call is an implicit `this` invocation
            // ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)),
            // unless a static import ([§7.5.4]) names the method through its
            // declaring type. The simple `MethodName` form is subject to
            // §15.12.3's static-context restriction ([§8.1.3]).
            None => {
                if let Some(ty) = self.static_import_method_receiver(name.as_str()) {
                    return (ty, InvocationMode::Static, false);
                }
                (
                    self.unqualified_method_receiver(name.as_str()),
                    // §15.12.1/§15.12.3: the unqualified form searches the type
                    // of `T.this` — a *declaring-interface* static method is in
                    // scope there, which the virtual-invocation filter would
                    // wrongly exclude (javac rejects `expr.s()` for a static
                    // interface `s()`, but accepts the bare `s()` inside the
                    // interface that declares it).
                    InvocationMode::MethodName,
                    true,
                )
            }
        }
    }

    /// skipped ([§15.12.3]).
    pub(super) fn unqualified_method_receiver(&self, name: &str) -> Ty {
        // JLS §15.12.1 (MethodName form): the search is for `T.this`, where a
        // static interface method declared in the searched type itself IS a
        // candidate (§15.12.3).
        let probe = self.access.with_mode(InvocationMode::MethodName);
        let mut levels: Vec<Ty> = Vec::new();
        if let Some(class) = &self.enclosing_class {
            levels.push(*class);
        }
        levels.extend(self.enclosing_chain.iter().cloned());
        for class in &levels {
            if !member_set(self.db, &self.scope, class, name, &probe).is_empty() {
                return *class;
            }
        }
        self.enclosing_class.unwrap_or_else(|| self.error())
    }

    /// implicit `this` receiver.
    pub(super) fn static_import_method_receiver(&self, simple: &str) -> Option<Ty> {
        for (owner, member) in self.resolver.static_import_owners(simple) {
            let receiver = Ty::reference(self.db, owner.as_str(), Vec::new());
            let access = self.access.with_mode(InvocationMode::Static);
            let has_static = member_set(self.db, &self.scope, &receiver, &member, &access)
                .iter()
                .any(|method| method.is_static);
            if has_static {
                return Some(receiver);
            }
        }
        None
    }

    /// ([`supertypes_impl`]).
    pub(super) fn super_ty(&self) -> Ty {
        let Some(enclosing) = self.enclosing_class else {
            return self.error();
        };
        supertypes_impl(self.db, &self.scope, &enclosing)
            .first()
            .copied()
            .unwrap_or_else(|| self.error())
    }

    /// The receiver type of a *qualified super* invocation `I.super.m(...)`
    /// ([JLS §15.11.2], [§15.12.1]): the interface `I` as parameterized by the
    /// enclosing class's inheritance — `ComponentSerializer<I,O,R>` invoked as
    /// `ComponentDecoder.super.deserializeOr(...)` must resolve the default on
    /// `ComponentDecoder<R,O>`, not on a raw `ComponentDecoder` whose `O`
    /// erases to its bound. Walks the enclosing type's supertypes for the one
    /// whose erasure matches the written qualifier and keeps its type
    /// arguments; falls back to the raw qualifier type.
    pub(super) fn qualified_super_ty(&self, qualifier: &SpannedTypeRef) -> Ty {
        let raw = resolve_type_ref(self.db, &self.scope, &self.resolver, qualifier);
        let Some((raw_name, _)) = raw.erasure(self.db).as_reference(self.db) else {
            return raw;
        };
        let mut levels: Vec<Ty> = Vec::new();
        if let Some(class) = &self.enclosing_class {
            levels.push(*class);
        }
        levels.extend(self.enclosing_chain.iter().cloned());
        for class in &levels {
            let mut stack = vec![*class];
            let mut seen = FxHashSet::default();
            while let Some(current) = stack.pop() {
                if !seen.insert(current.id) {
                    continue;
                }
                for parent in supertypes_impl(self.db, &self.scope, &current) {
                    if parent.is_error(self.db) {
                        continue;
                    }
                    let TyKind::Reference { name, .. } = parent.kind(self.db) else {
                        continue;
                    };
                    if name == raw_name {
                        return parent;
                    }
                    stack.push(parent);
                }
            }
        }
        raw
    }

    /// §15.11.2/[§8.1.3]: whether the type named by a qualified-super
    /// qualifier (`I.super`) is a superinterface of one of the enclosing
    /// classes — an interface the enclosing class implements directly or
    /// inherits from its supertypes. A qualifier naming a class that is not
    /// such an interface is javac's `not an enclosing class` error (the
    /// class's own `super.m()` covers the class-supertype case, and a class
    /// *cannot* be named here: §15.11.2's qualifier must be an interface or
    /// a type variable whose bound is an interface).
    fn qualifier_is_superinterface(&self, raw: &Ty) -> bool {
        let Some((raw_name, _)) = raw.erasure(self.db).as_reference(self.db) else {
            return false;
        };
        let mut levels: Vec<Ty> = Vec::new();
        if let Some(class) = &self.enclosing_class {
            levels.push(*class);
        }
        levels.extend(self.enclosing_chain.iter().cloned());
        for class in &levels {
            let mut stack = vec![*class];
            let mut seen = FxHashSet::default();
            while let Some(current) = stack.pop() {
                if !seen.insert(current.id) {
                    continue;
                }
                for parent in supertypes_impl(self.db, &self.scope, &current) {
                    if parent.is_error(self.db) {
                        continue;
                    }
                    let TyKind::Reference { name, .. } = parent.kind(self.db) else {
                        continue;
                    };
                    if name == raw_name {
                        return true;
                    }
                    stack.push(parent);
                }
            }
        }
        false
    }
}

/// The declared formals of `method` with its own type parameters instantiated
/// as fresh inference variables ([JLS §18.5.2.2]): the *invocation* type a
/// diagnostic should name, since the declared formals still mention the
/// member's type parameters.
///
/// Only used for rendering: the variables carry their declared bounds so a
/// bound-shaped formal still renders meaningfully (`V` rather than `Object`).
fn reported_formals(db: &dyn crate::java::db::TyDatabase, method: &MethodData) -> Vec<Ty> {
    if method.type_params.is_empty() {
        return method.params.clone();
    }
    let subst: rustc_hash::FxHashMap<crate::java::ty::TypeVarScope, Ty> = method
        .type_params
        .iter()
        .map(|tp| {
            (
                tp.scope.clone(),
                Ty::type_var(db, tp.scope.clone(), tp.bounds.clone()),
            )
        })
        .collect();
    method
        .params
        .iter()
        .map(|p| p.substitute(db, &subst))
        .collect()
}

/// The parameter types a variable-arity invocation of `formals` actually
/// applies to `found` actual arguments ([JLS §15.12.2.4]): the fixed prefix,
/// then the last formal's element type repeated for every trailing actual —
/// or the array formal itself when a lone trailing actual is array-shaped.
fn pack_varargs(db: &dyn crate::java::db::TyDatabase, formals: &[Ty], found: usize) -> Vec<Ty> {
    let Some((last, fixed)) = formals.split_last() else {
        return Vec::new();
    };
    if found < fixed.len() {
        return formals.to_vec();
    }
    let Some(element) = last.element(db).copied() else {
        return formals.to_vec();
    };
    let trailing = found - fixed.len();
    if trailing == 1 && last.is_array(db) {
        // A lone trailing actual of the array type is the array itself
        // (§15.12.2.4); the caller substitutes the element type when the
        // actual turns out not to be array-shaped.
        let mut out = fixed.to_vec();
        out.push(*last);
        return out;
    }
    let mut out = fixed.to_vec();
    out.extend(std::iter::repeat_n(element, trailing));
    out
}
