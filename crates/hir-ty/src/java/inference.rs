//! Method invocation type inference ([JLS §18]).
//!
//! [`crate::java::method::pick_method`] determines the invocation type
//! ([§15.12.2.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.6))
//! of a generic method from the actual argument types ([§18.5.2]): the
//! method's own type parameters ([§8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4))
//! become fresh inference variables ([§18.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.1.1)),
//! the arguments are related to the formal types by constraint formulas
//! ([§18.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.1.2))
//! whose reduction ([§18.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.2))
//! collects bounds per variable, the bounds are incorporated
//! ([§18.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.3))
//! and resolved ([§18.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.4)),
//! and the resulting substitution is applied to the return type
//! ([§18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)).
//!
//! The target-type compatibility of §18.5.2.4 is not modelled (a bare
//! invocation has no target type); throws inference ([§18.5.2.3]) instantiates
//! `throws` clause type parameters from their bounds. Inference-variable-bearing
//! types never reach the memoized subtype/supertype queries in
//! [`crate::java::subtyping`]: all [`Ty`]s handed to `is_subtype`/`is_assignable`
//! here are proper.

use std::collections::VecDeque;

use hir_expand::name::Name;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    java::db::TyDatabase,
    java::method::{MethodData, MethodTypeParam},
    java::subtyping::{is_assignable, is_subtype, strict_conversion, supertypes_impl},
    java::ty::{BoundKind, Ty, TyData, TyKind, WildcardBound, boxed_type},
};

/// The invocation conversion of a phase: strict invocation
/// ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2))
/// admits only identity, widening primitive ([§5.1.2]) and widening reference
/// ([§5.1.5]) conversions; loose invocation
/// ([§15.12.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.3))
/// also admits boxing ([§5.1.7]) and unboxing ([§5.1.8]), i.e. assignment
/// conversion ([§5.2]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvocationPhase {
    Strict,
    Loose,
}

/// A constraint formula ([JLS §18.1.2]): `⟨S → T⟩` (subtyping) or `⟨S = T⟩`
/// (type equality).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Constraint {
    Sub(Ty, Ty),
    Eq(Ty, Ty),
}

/// The bounds of one inference variable ([JLS §18.3.1]): upper bounds
/// `α <: T`, lower bounds `S <: α` and the equality bound `α = T`.
#[derive(Debug, Default, Clone)]
struct Bounds {
    upper: Vec<Ty>,
    lower: Vec<Ty>,
    equality: Option<Ty>,
}

/// An invocation type inference table ([JLS §18.5.2]).
#[derive(Clone)]
pub(crate) struct Inference {
    bounds: FxHashMap<u64, Bounds>,
    /// The inference variables that appear in the method's `throws` clause
    /// ([§18.5.2.2]): the `throws` α bound of §18.1.3. Purely informational,
    /// it directs resolution to prefer an unchecked exception type
    /// ([§18.4], [§18.5.2.3]).
    throws: FxHashSet<u64>,
    /// The equalities incorporated eagerly ([§18.3.1]). `incorporate`
    /// substitutes an equality into the bound set and removes the variable, so
    /// `resolve` can no longer see it; the applied substitution is recorded
    /// here and merged into the resolved instantiation.
    applied: FxHashMap<u64, Ty>,
    /// Constraints added but not yet reduced ([JLS §18.2]).
    worklist: VecDeque<Constraint>,
}

impl Inference {
    pub(crate) fn new() -> Self {
        Self {
            bounds: FxHashMap::default(),
            throws: FxHashSet::default(),
            applied: FxHashMap::default(),
            worklist: VecDeque::new(),
        }
    }

    /// A fresh inference variable ([§18.1.1]) with no bounds yet.
    pub(crate) fn fresh_var(&mut self, db: &dyn TyDatabase) -> Ty {
        let var = Ty::infer_var(db);
        self.bounds
            .entry(var.as_infer_var(db).expect("fresh inference var"))
            .or_default();
        var
    }

    /// Adds the upper bound `var <: bound`.
    pub(crate) fn add_upper(&mut self, db: &dyn TyDatabase, var: Ty, bound: Ty) {
        let id = var.as_infer_var(db).expect("inference variable");
        self.bounds.entry(id).or_default().upper.push(bound);
    }

    /// Records the `throws` α bound ([§18.5.2.2]): `var` appears in the
    /// method's `throws` clause ([JLS §18.1.3]).
    pub(crate) fn mark_throws(&mut self, db: &dyn TyDatabase, var: Ty) {
        let id = var.as_infer_var(db).expect("inference variable");
        self.throws.insert(id);
    }

    /// Reduces the constraint set ([§18.2]), incorporates the bounds
    /// ([§18.3.1]) and resolves ([§18.4.1]). `None` when a constraint reduces
    /// to false or the bounds are contradictory — the invocation is not
    /// applicable in this phase.
    pub(crate) fn solve(
        &mut self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
        phase: InvocationPhase,
        constraints: Vec<Constraint>,
    ) -> Option<FxHashMap<u64, Ty>> {
        self.worklist.extend(constraints);
        self.solve_after(db, scope, phase)
    }

    /// Adds a constraint to the worklist, to be reduced by a later
    /// [`Self::drain_worklist`]. Used by the joint inference of §18.5.2.4,
    /// where nested poly invocations contribute their constraints to the
    /// enclosing invocation's table incrementally.
    pub(crate) fn add_constraint(&mut self, constraint: Constraint) {
        // debug disabled (needs db)
        self.worklist.push_back(constraint);
    }

    /// Reduces the pending constraints ([JLS §18.2]); `false` when a
    /// constraint reduces to false.
    pub(crate) fn drain_worklist(
        &mut self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
        phase: InvocationPhase,
    ) -> bool {
        let mut worklist = std::mem::take(&mut self.worklist);
        let result = loop {
            let Some(constraint) = worklist.pop_front() else {
                break true;
            };
            match constraint {
                Constraint::Sub(s, t) => {
                    if !self.reduce_sub(db, scope, phase, &s, &t, &mut worklist) {
                        break false;
                    }
                }
                Constraint::Eq(s, t) => {
                    if !self.reduce_eq(db, &s, &t, &mut worklist) {
                        break false;
                    }
                }
            }
        };
        self.worklist = worklist;
        result
    }

    /// The tail of a full solve: drain the worklist, incorporate the bounds
    /// ([§18.3.1]) and resolve ([§18.4.1]). `None` when the bounds are
    /// contradictory — the invocation is not applicable in this phase. Used by
    /// the joint inference of §18.5.2.4 after the argument and target
    /// constraints have been contributed incrementally.
    pub(crate) fn solve_after(
        &mut self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
        phase: InvocationPhase,
    ) -> Option<FxHashMap<u64, Ty>> {
        if !self.drain_worklist(db, scope, phase) {
            return None;
        }
        if !self.incorporate(db, scope, phase) {
            return None;
        }
        self.resolve(db, scope)
    }

    /// Whether the bound set is consistent with the constraints contributed so
    /// far: the worklist is drained and the bounds incorporated, but no
    /// variable is resolved to a concrete type. Used by the joint resolution
    /// of a nested poly invocation, which probes each candidate against the
    /// enclosing invocation's shared table and commits to the first locally
    /// consistent one.
    pub(crate) fn check_consistent(
        &mut self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
        phase: InvocationPhase,
    ) -> bool {
        if !self.drain_worklist(db, scope, phase) {
            return false;
        }
        if !self.incorporate(db, scope, phase) {
            return false;
        }
        true
    }

    /// A snapshot of the table state for speculative candidate probing: the
    /// joint resolution of a nested poly invocation probes each candidate
    /// against the shared table and restores the state of the failed probes.
    pub(crate) fn snapshot(&self) -> Self {
        self.clone()
    }

    /// Restores the table to a [`Self::snapshot`].
    pub(crate) fn restore(&mut self, snapshot: Self) {
        *self = snapshot;
    }

    /// Registers a class's type parameters as fresh inference variables with
    /// their declared bounds, returning the name-to-variable substitution.
    /// Used by diamond `new Foo<>()` inference from constructor arguments
    /// ([JLS §15.9.2.2]): the created class's type variables are constrained
    /// by the argument types exactly like a generic method's type parameters,
    /// and the resolved values instantiate the class.
    pub(crate) fn register_class_type_params(
        &mut self,
        db: &dyn TyDatabase,
        type_params: &[MethodTypeParam],
    ) -> FxHashMap<Name, Ty> {
        let mut subst: FxHashMap<Name, Ty> = FxHashMap::default();
        for tp in type_params {
            let var = self.fresh_var(db);
            subst.insert(tp.name.clone(), var);
            let bounds: Vec<Ty> = tp.bounds.iter().map(|b| b.substitute(db, &subst)).collect();
            if bounds.is_empty() {
                self.add_upper(db, var, Ty::reference(db, "java.lang.Object", Vec::new()));
            } else {
                for bound in bounds {
                    self.add_upper(db, var, bound);
                }
            }
        }
        subst
    }

    /// Instantiates `method`'s type parameters ([JLS §18.5.2.2]) as fresh
    /// inference variables with their declared bounds, returning the
    /// substituted formal parameter types, return type and throws clause
    /// types ([§18.5.2.3]). The types reference the fresh variables; the
    /// caller relates them to the actual argument types with further
    /// constraints. Unlike the self-contained [`Self::solve`], the fresh
    /// variables live in this table, so a nested poly invocation's inference
    /// is shared with its enclosing invocation ([JLS §18.5.2.4]).
    pub(crate) fn register_method(
        &mut self,
        db: &dyn TyDatabase,
        method: &MethodData,
    ) -> (Vec<Ty>, Ty, Vec<Ty>) {
        let mut subst: FxHashMap<Name, Ty> = FxHashMap::default();
        for tp in &method.type_params {
            let var = self.fresh_var(db);
            subst.insert(tp.name.clone(), var);
            let bounds: Vec<Ty> = tp.bounds.iter().map(|b| b.substitute(db, &subst)).collect();
            if bounds.is_empty() {
                self.add_upper(db, var, Ty::reference(db, "java.lang.Object", Vec::new()));
            } else {
                for bound in bounds {
                    self.add_upper(db, var, bound);
                }
            }
        }
        let formals: Vec<Ty> = method
            .params
            .iter()
            .map(|p| p.substitute(db, &subst))
            .collect();
        let ret = method.ret.substitute(db, &subst);
        let throws_formals: Vec<Ty> = method
            .throws
            .iter()
            .map(|t| t.substitute(db, &subst))
            .collect();
        for thrown in &throws_formals {
            if thrown.is_infer_var(db) {
                self.mark_throws(db, *thrown);
            }
        }
        (formals, ret, throws_formals)
    }

    /// The total number of bounds in the table, used to detect whether
    /// reducing the implied bounds changed anything.
    fn bound_count(&self) -> usize {
        self.bounds
            .values()
            .map(|b| b.upper.len() + b.lower.len() + usize::from(b.equality.is_some()))
            .sum()
    }

    fn reduce_sub(
        &mut self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
        phase: InvocationPhase,
        s: &Ty,
        t: &Ty,
        worklist: &mut VecDeque<Constraint>,
    ) -> bool {
        // §18.2.1: `⟨S → S⟩` is a tautology — most strikingly for an
        // inference variable on both sides, which would otherwise record
        // *itself* as a lower bound (`⟨U → U⟩` from the merge function
        // `(first, ignored) -> first` of `Collectors.toMap`) and poison the
        // bound set with a self-reference that no resolution order can break.
        // The identity test is *name-wise*: a self-referential type variable
        // bound (`T extends Box<K,T>`, §4.4) is resolved to differently-deep
        // interned handles per resolver context, so `Box<K,T> → Box<K,T>`
        // must be a tautology too — it never reduces to the invariant
        // equality `⟨T = T⟩`, which the recursion guard would fail.
        if s.same_shape(db, t) {
            return true;
        }
        // §5.1.10/§18.2.2: a wildcard on the *source* side stands for its
        // capture's least bound (`? extends X` at least `X`, `? super X`
        // exactly `X`, `?` at least `Object`) — containment does not apply
        // to it, and the bare wildcard must not enter the bound set.
        if let TyKind::Wildcard(bound) = s.kind(db) {
            let object = Ty::reference(db, "java.lang.Object", Vec::new());
            let minimum = match bound.as_deref().map(|b| (&b.kind, &b.ty)) {
                Some((BoundKind::Upper, ty)) | Some((BoundKind::Lower, ty)) => *ty,
                _ => object,
            };
            return self.reduce_sub(db, scope, phase, &minimum, t, worklist);
        }
        // §18.2.1: ⟨S → α⟩ is the lower bound `S <: α`; ⟨α → T⟩ is the upper
        // bound `α <: T`. An inference variable constrained by a *wildcard*
        // target is not an upper bound of the variable — §18.2.2/§18.2.3
        // containment reduces it to bounds on θ (`⟨α <: ? extends θ⟩` bounds
        // α from above by θ, `⟨α <: ? super θ⟩` from below), so it routes to
        // the wildcard reduction below.
        if let Some(id) = t.as_infer_var(db) {
            // §18.2.2: in a loose (or variable-arity) invocation a primitive
            // source *boxes* before entering the bound set — `⟨int → α⟩`
            // bounds α below by `Integer`, so the variable can only ever
            // instantiate to a reference type.
            let boxed = match (phase, s.kind(db)) {
                (InvocationPhase::Loose, TyKind::Primitive(p)) => {
                    Ty::reference(db, boxed_type(*p), Vec::new())
                }
                _ => *s,
            };
            self.bounds.entry(id).or_default().lower.push(boxed);
            return true;
        }
        if let Some(id) = s.as_infer_var(db) {
            if matches!(t.kind(db), TyKind::Wildcard(_)) {
                return self.reduce_wildcard(db, s, t, worklist);
            }
            self.bounds.entry(id).or_default().upper.push(*t);
            return true;
        }
        match (s.kind(db), t.kind(db)) {
            // §18.2.1: ⟨S[] → T[]⟩ reduces to ⟨S → T⟩ — except when both
            // components are *proper* and primitive ([§4.10.3], [§5.3]):
            // primitive-array components are invariant, so `long[]` is
            // unrelated to `double[]` even though `long` widens to `double`.
            // A component that is still an inference variable is not proper:
            // the array reduces to a component constraint regardless of the
            // other side's primitiveness (`int[] <: α[]` bounds α from below
            // by `int`, §18.2.1).
            (TyKind::Array(si), TyKind::Array(ti))
                if si.is_primitive(db) && ti.is_primitive(db) =>
            {
                si == ti
            }
            (TyKind::Array(si), TyKind::Array(ti)) => {
                worklist.push_back(Constraint::Sub(**si, **ti));
                true
            }
            // §4.10.3: an array's reference supertypes are exactly `Object`,
            // `Cloneable` and `Serializable`; the constraint holds against
            // them even when the target still carries inference variables.
            (TyKind::Array(_), TyKind::Reference { name, .. })
                if matches!(
                    name.as_str(),
                    "java.lang.Object" | "java.lang.Cloneable" | "java.io.Serializable"
                ) =>
            {
                true
            }
            (
                TyKind::Reference { name: sn, args: sa },
                TyKind::Reference { name: tn, args: ta },
            ) if sn == tn => {
                // §18.2.1: ⟨G<S..> → G<T..⟩ reduces to the per-argument
                // constraints. Against a concrete target the type arguments
                // are invariant (§4.10.2) and reduce to equalities; a target
                // with wildcard arguments reduces to the wildcard rules of
                // §18.2.2. Raw types accept any instantiation (§4.8).
                if ta.is_empty() {
                    return true;
                }
                if sa.is_empty() {
                    // §4.8/§5.1.9/§15.12.2.3: a *raw* source is compatible
                    // with any parameterization of its own class by
                    // unchecked conversion — admitted in the loose phase
                    // (`stream.flatMap(List::stream)`), rejected in strict.
                    return phase == InvocationPhase::Loose;
                }
                if sa.len() != ta.len() {
                    return false;
                }
                if ta.iter().any(|arg| arg.is_wildcard(db)) {
                    for (s_arg, t_arg) in sa.iter().zip(ta.iter()) {
                        worklist.push_back(Constraint::Sub(*s_arg, *t_arg));
                    }
                } else {
                    // §18.2.1: the arguments are invariant (§4.10.2) and
                    // reduce to equalities — including against an inference
                    // variable in the target's argument position, whose
                    // equality semantics pin the variable to the source
                    // exactly (`<T> T id(Class<T>)` called with
                    // `Class<String>` infers `T := String`). A *wildcard*
                    // source argument is no instantiation ([§4.5.1]): it
                    // reduces by containment instead (§18.2.3), so the
                    // variable picks up the capture's bound rather than the
                    // bare wildcard itself (`Class<? extends Number>` infers
                    // `Number`, `Class<?>` infers its capture's bound).
                    for (s_arg, t_arg) in sa.iter().zip(ta.iter()) {
                        if s_arg.is_wildcard(db) {
                            worklist.push_back(Constraint::Sub(*s_arg, *t_arg));
                        } else {
                            worklist.push_back(Constraint::Eq(*s_arg, *t_arg));
                        }
                    }
                }
                true
            }
            (TyKind::Wildcard(_), _) | (_, TyKind::Wildcard(_)) => {
                self.reduce_wildcard(db, s, t, worklist)
            }
            (TyKind::Reference { .. }, TyKind::Reference { .. }) => {
                // Different erasures: against a proper source the constraint
                // is checked directly; a source still carrying inference
                // variables is reduced transitively — the first parameterized
                // supertype of `s` named `tn` (through as many steps as the
                // hierarchy needs, §18.2.1) becomes ⟨Supertype → T⟩.
                if !s.contains_infer_var(db) && !t.contains_infer_var(db) {
                    return convertible(db, scope, &phase, s, t);
                }
                if let Some((tn, _)) = t.as_reference(db) {
                    let mut stack = vec![*s];
                    let mut visited: FxHashSet<TyData> = FxHashSet::default();
                    let mut found = false;
                    while let Some(current) = stack.pop() {
                        for parent in supertypes_impl(db, scope, &current) {
                            if !visited.insert(parent.id) {
                                continue;
                            }
                            let Some((pn, _)) = parent.as_reference(db) else {
                                continue;
                            };
                            if pn == tn {
                                worklist.push_back(Constraint::Sub(parent, *t));
                                found = true;
                            } else {
                                stack.push(parent);
                            }
                        }
                    }
                    return found;
                }
                false
            }
            _ => {
                // Primitive, type variable, array-vs-class, void: checked by
                // the phase conversion when both sides are proper.
                if !s.contains_infer_var(db) && !t.contains_infer_var(db) {
                    convertible(db, scope, &phase, s, t)
                } else {
                    false
                }
            }
        }
    }

    /// Wildcard type argument containment reduction
    /// ([JLS §18.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.2.3)):
    /// the `⟨S <= T⟩` rules of §4.5.1, driven by the type arguments of the
    /// parameterized target.
    fn reduce_wildcard(
        &mut self,
        db: &dyn TyDatabase,
        s: &Ty,
        t: &Ty,
        worklist: &mut VecDeque<Constraint>,
    ) -> bool {
        let object = Ty::reference(db, "java.lang.Object", Vec::new());
        match (s.kind(db), t.kind(db)) {
            // §18.2.3: a `?` target contains any type argument.
            (_, TyKind::Wildcard(None)) => true,
            // §18.2.3: `? extends T'` target.
            (s_kind, TyKind::Wildcard(Some(tb))) if tb.kind == BoundKind::Upper => {
                match s_kind {
                    // `?` → ⟨Object <: T'⟩.
                    TyKind::Wildcard(None) => {
                        worklist.push_back(Constraint::Sub(object, tb.ty));
                    }
                    // `? extends S'` → ⟨S' <: T'⟩.
                    TyKind::Wildcard(Some(sb)) if sb.kind == BoundKind::Upper => {
                        worklist.push_back(Constraint::Sub(sb.ty, tb.ty));
                    }
                    // `? super S'` → ⟨Object = T'⟩.
                    TyKind::Wildcard(Some(sb)) if sb.kind == BoundKind::Lower => {
                        worklist.push_back(Constraint::Eq(object, tb.ty));
                    }
                    // A concrete type `S` → ⟨S <: T'⟩.
                    _ => worklist.push_back(Constraint::Sub(*s, tb.ty)),
                }
                true
            }
            // §18.2.3: `? super T'` target.
            (s_kind, TyKind::Wildcard(Some(tb))) if tb.kind == BoundKind::Lower => {
                match s_kind {
                    // `? super S'` → ⟨T' <: S'⟩.
                    TyKind::Wildcard(Some(sb)) if sb.kind == BoundKind::Lower => {
                        worklist.push_back(Constraint::Sub(tb.ty, sb.ty));
                    }
                    // `?` or `? extends S'` against `? super T'` is otherwise
                    // false (§18.2.3).
                    TyKind::Wildcard(_) => return false,
                    // A concrete type `S` → ⟨T' <: S⟩.
                    _ => worklist.push_back(Constraint::Sub(tb.ty, *s)),
                }
                true
            }
            // §18.2.3: a wildcard source against a concrete target argument is
            // not contained.
            (TyKind::Wildcard(_), _) => false,
            _ => false,
        }
    }

    /// Equality constraint reduction ([JLS §18.2.1]).
    fn reduce_eq(
        &mut self,
        db: &dyn TyDatabase,
        s: &Ty,
        t: &Ty,
        worklist: &mut VecDeque<Constraint>,
    ) -> bool {
        if s == t {
            return true;
        }
        // §18.2.1: `⟨α = β⟩` equates two variables. A variable that already
        // carries an equality bound must not lose it — `⟨T = α⟩` then
        // `⟨T = String⟩` (an invariant-argument chain like
        // `synchronizedList(new ArrayList<>())` against a `List<String>`
        // target) would otherwise overwrite `α` with `String` and leave `α`
        // to resolve to `Object`. Instead the two equalities are related:
        // `α = T` and `T = String` imply `α = String`.
        if let Some(id) = t.as_infer_var(db) {
            let bound = self.bounds.entry(id).or_default();
            if let Some(existing) = bound.equality {
                worklist.push_back(Constraint::Eq(existing, *s));
                return true;
            }
            bound.equality = Some(*s);
            return true;
        }
        if let Some(id) = s.as_infer_var(db) {
            let bound = self.bounds.entry(id).or_default();
            if let Some(existing) = bound.equality {
                worklist.push_back(Constraint::Eq(existing, *t));
                return true;
            }
            bound.equality = Some(*t);
            return true;
        }
        match (s.kind(db), t.kind(db)) {
            (TyKind::Array(si), TyKind::Array(ti)) => {
                worklist.push_back(Constraint::Eq(**si, **ti));
                true
            }
            (
                TyKind::Reference { name: sn, args: sa },
                TyKind::Reference { name: tn, args: ta },
            ) if sn == tn => {
                if ta.is_empty() && sa.is_empty() {
                    return true;
                }
                if sa.is_empty() || ta.is_empty() || sa.len() != ta.len() {
                    return false;
                }
                // §18.2.1/§18.2.3: an argument position involving a wildcard
                // does not equate — it *contains* (`Collector<T,?,C>` may be
                // equal to a parameterization whose argument bounds it), so
                // those positions reduce to the containment rules instead.
                if sa.iter().any(|arg| arg.is_wildcard(db))
                    || ta.iter().any(|arg| arg.is_wildcard(db))
                {
                    for (s_arg, t_arg) in sa.iter().zip(ta.iter()) {
                        let both_wildcards = s_arg.is_wildcard(db) && t_arg.is_wildcard(db);
                        if t_arg.is_wildcard(db) || both_wildcards {
                            worklist.push_back(Constraint::Sub(*s_arg, *t_arg));
                        } else if s_arg.is_wildcard(db) {
                            return false;
                        } else {
                            worklist.push_back(Constraint::Eq(*s_arg, *t_arg));
                        }
                    }
                    return true;
                }
                for (s_arg, t_arg) in sa.iter().zip(ta.iter()) {
                    worklist.push_back(Constraint::Eq(*s_arg, *t_arg));
                }
                true
            }
            // §18.2.4: wildcard type argument equality.
            (TyKind::Wildcard(sb), TyKind::Wildcard(tb)) => match (sb, tb) {
                (None, None) => true,
                // `? = ? extends T'` → ⟨Object = T'⟩.
                (None, Some(tb)) if tb.kind == BoundKind::Upper => {
                    worklist.push_back(Constraint::Eq(
                        Ty::reference(db, "java.lang.Object", Vec::new()),
                        tb.ty,
                    ));
                    true
                }
                // `? extends S' = ?` → ⟨S' = Object⟩.
                (Some(sb), None) if sb.kind == BoundKind::Upper => {
                    worklist.push_back(Constraint::Eq(
                        sb.ty,
                        Ty::reference(db, "java.lang.Object", Vec::new()),
                    ));
                    true
                }
                // `? extends S' = ? extends T'` → ⟨S' = T'⟩.
                (Some(sb), Some(tb))
                    if sb.kind == BoundKind::Upper && tb.kind == BoundKind::Upper =>
                {
                    worklist.push_back(Constraint::Eq(sb.ty, tb.ty));
                    true
                }
                // `? super S' = ? super T'` → ⟨S' = T'⟩.
                (Some(sb), Some(tb))
                    if sb.kind == BoundKind::Lower && tb.kind == BoundKind::Lower =>
                {
                    worklist.push_back(Constraint::Eq(sb.ty, tb.ty));
                    true
                }
                _ => false,
            },
            (TyKind::Wildcard(_), _) | (_, TyKind::Wildcard(_)) => false,
            _ => {
                if !s.contains_infer_var(db) && !t.contains_infer_var(db) {
                    s == t
                } else {
                    false
                }
            }
        }
    }

    /// Bound set incorporation ([JLS §18.3.1]): equality bounds are substituted
    /// away, a proper lower bound `S <: α` against a proper upper bound
    /// `α <: T` must satisfy `S <: T`, and same-erasure lower/upper pairs
    /// imply equalities between their type arguments.
    fn incorporate(
        &mut self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
        phase: InvocationPhase,
    ) -> bool {
        loop {
            let mut changed = false;

            let equalities: Vec<(u64, Ty)> = self
                .bounds
                .iter()
                .filter_map(|(id, b)| b.equality.map(|eq| (*id, eq)))
                .collect();
            for (id, eq) in equalities {
                if !self.bounds.contains_key(&id) {
                    continue;
                }
                if eq.as_infer_var(db) == Some(id) {
                    return false;
                }
                let subst: FxHashMap<u64, Ty> = FxHashMap::from_iter([(id, eq)]);
                for bounds in self.bounds.values_mut() {
                    bounds.upper = bounds
                        .upper
                        .iter()
                        .map(|t| t.substitute_infer(db, &subst))
                        .collect();
                    bounds.lower = bounds
                        .lower
                        .iter()
                        .map(|t| t.substitute_infer(db, &subst))
                        .collect();
                    bounds.equality = bounds
                        .equality
                        .as_ref()
                        .map(|t| t.substitute_infer(db, &subst));
                }
                self.applied.insert(id, eq);
                self.bounds.remove(&id);
                changed = true;
            }

            // §18.3.1 implied bounds: a proper lower bound `S <: α` against a
            // proper upper bound `α <: T` of the same erasure constrains the
            // type arguments to be equal. The constraints are reduced
            // immediately and the loop repeats, so `take(id(emptyList()))`
            // resolves: β's lower `List<E>` and upper `List<String>` imply
            // `E = String` instead of leaving E to resolve to `Object` and
            // failing the invariance check.
            let ids: Vec<u64> = self.bounds.keys().copied().collect();
            let before = self.bound_count();
            for id in ids {
                let b = &self.bounds[&id];
                for constraint in self.implied_bounds(db, &b.lower, &b.upper) {
                    self.worklist.push_back(constraint);
                }
            }
            if !self.worklist.is_empty() && !self.drain_worklist(db, scope, phase) {
                return false;
            }
            if self.bound_count() > before {
                changed = true;
            }

            let ids: Vec<u64> = self.bounds.keys().copied().collect();
            for id in ids {
                let b = &self.bounds[&id];
                let lower = b.lower.clone();
                let upper = b.upper.clone();
                for l in &lower {
                    for u in &upper {
                        // §18.3.1 bound validation: a proper lower bound `S`
                        // and a proper upper bound `T` must satisfy `S <: T`.
                        // Like `pick_instantiation`, this is *assignment*
                        // compatibility ([§5.2]) — a raw lower bound
                        // (`CompletableFuture` from `new CompletableFuture[0]`)
                        // converts to a parameterized upper
                        // (`CompletableFuture<?>`) by unchecked conversion
                        // ([§5.1.9]), exactly as in javac's bound check. Two
                        // primitives relate only by identity here ([§4.10.1]).
                        if l.contains_infer_var(db) || u.contains_infer_var(db) {
                            continue;
                        }
                        let ok = if matches!(l.kind(db), TyKind::Primitive(_))
                            && matches!(u.kind(db), TyKind::Primitive(_))
                        {
                            l == u
                        } else {
                            is_subtype(db, scope, l, u) || is_assignable(db, scope, l, u)
                        };
                        if !ok {
                            return false;
                        }
                    }
                }
            }

            if !changed {
                return true;
            }
        }
    }

    /// The implied bounds of §18.3.1: for a pair of proper bounds `S <: α` and
    /// `α <: T` where `S` and `T` have the same erasure, the type arguments
    /// must be related — equal for invariant parameterizations ([§4.10.2]) and
    /// for array element types. Wildcard type arguments are left out: they are
    /// constrained by the wildcard rules of §18.2.3 instead.
    fn implied_bounds(&self, db: &dyn TyDatabase, lower: &[Ty], upper: &[Ty]) -> Vec<Constraint> {
        let mut out = Vec::new();
        for l in lower {
            for u in upper {
                match (l.kind(db), u.kind(db)) {
                    (TyKind::Array(li), TyKind::Array(ui)) => {
                        if l != u {
                            out.push(Constraint::Eq(**li, **ui));
                        }
                    }
                    (
                        TyKind::Reference { name: ln, args: la },
                        TyKind::Reference { name: un, args: ua },
                    ) if ln == un && !la.is_empty() && la.len() == ua.len() && l != u => {
                        for (a, b) in la.iter().zip(ua.iter()) {
                            if a != b && !a.is_wildcard(db) && !b.is_wildcard(db) {
                                out.push(Constraint::Eq(*a, *b));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// Bound set resolution ([JLS §18.4.1]). Returns the instantiation of
    /// every inference variable. Cyclic references between variables are
    /// approximated by estimating unresolved variables as `Object` (§18.4.2
    /// concrete instantiation is not fully modelled).
    fn resolve(
        &self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
    ) -> Option<FxHashMap<u64, Ty>> {
        // The equalities incorporated eagerly are no longer in the bound set;
        // seed the substitution with them so the caller's instantiation sees
        // the resolved values.
        let mut subst: FxHashMap<u64, Ty> = self.applied.clone();
        let ids: Vec<u64> = self.bounds.keys().copied().collect();
        loop {
            let mut progress = false;
            for &id in &ids {
                if subst.contains_key(&id) {
                    continue;
                }
                let (eq, lower, upper) = self.effective_bounds(db, id, &subst, false);
                if eq.as_ref().is_some_and(|t| t.contains_infer_var(db))
                    || lower.iter().any(|t| t.contains_infer_var(db))
                    || upper.iter().any(|t| t.contains_infer_var(db))
                {
                    continue;
                }
                let inst =
                    pick_instantiation(db, scope, &lower, &upper, eq, self.throws.contains(&id))?;
                subst.insert(id, inst);
                progress = true;
            }
            if !progress {
                // Variables that reference each other: estimate the remaining
                // references as `Object` and resolve, then re-run so the
                // resolved values propagate.
                let mut rest: Vec<u64> = ids
                    .iter()
                    .copied()
                    .filter(|id| !subst.contains_key(id))
                    .collect();
                if rest.is_empty() {
                    break;
                }
                rest.sort_unstable();
                for id in rest {
                    let (eq, lower, upper) = self.effective_bounds(db, id, &subst, true);
                    let inst = pick_instantiation(
                        db,
                        scope,
                        &lower,
                        &upper,
                        eq,
                        self.throws.contains(&id),
                    )?;
                    subst.insert(id, inst);
                }
            }
        }
        // The incorporated equalities can reference each other and the
        // resolved variables; substitute the values to a fixpoint so the
        // instantiation is fully resolved.
        let keys: Vec<u64> = subst.keys().copied().collect();
        loop {
            let mut changed = false;
            for key in &keys {
                if let Some(value) = subst.get(key).copied() {
                    let updated = value.substitute_infer(db, &subst);
                    if subst.get(key).copied() != Some(updated) {
                        subst.insert(*key, updated);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        Some(subst)
    }

    fn effective_bounds(
        &self,
        db: &dyn TyDatabase,
        id: u64,
        subst: &FxHashMap<u64, Ty>,
        estimate: bool,
    ) -> (Option<Ty>, Vec<Ty>, Vec<Ty>) {
        let b = &self.bounds[&id];
        let eq = b.equality.map(|t| t.substitute_infer(db, subst));
        let lower: Vec<Ty> = b
            .lower
            .iter()
            .map(|t| t.substitute_infer(db, subst))
            .collect();
        let upper: Vec<Ty> = b
            .upper
            .iter()
            .map(|t| t.substitute_infer(db, subst))
            .collect();
        if estimate {
            let eq = eq.map(|t| t.erase_infer_vars(db));
            let lower = lower.iter().map(|t| t.erase_infer_vars(db)).collect();
            let upper = upper.iter().map(|t| t.erase_infer_vars(db)).collect();
            (eq, lower, upper)
        } else {
            (eq, lower, upper)
        }
    }
}

/// The least upper bound of a set of types
/// ([JLS §4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)):
/// primitive types are boxed (§5.1.7), the sets of *erased* supertypes are
/// intersected into the erased candidate set `EC`, the minimal candidates
/// `MEC` are kept, the type arguments of generic candidates are recovered with
/// the least containing parameterization `lcp` / least containing type
/// argument `lcta`, and the best candidates join with `&` into an
/// intersection type ([§4.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.9)).
///
/// The specification permits `lub` to *not terminate* — "it is possible that
/// the lub() function yields an infinite type". A recursive call on an
/// identical argument set is such a cycle; it is broken by degrading to
/// `java.lang.Object`, keeping the result finite and well-formed (of the many
/// admissible choices, `Object` is the one the erased candidate set yields
/// for an unknown candidate).
pub fn least_upper_bound(db: &dyn TyDatabase, scope: &hir::ResolutionScope, types: &[Ty]) -> Ty {
    let object = Ty::reference(db, "java.lang.Object", Vec::new());
    if types.is_empty() {
        return object;
    }
    if types.len() == 1 {
        return types[0];
    }
    // §4.10.4 step 1: box primitive bounds.
    let types: Vec<Ty> = if types
        .iter()
        .any(|t| matches!(t.kind(db), TyKind::Primitive(_)))
    {
        types
            .iter()
            .map(|t| match t.kind(db) {
                TyKind::Primitive(p) => Ty::reference(db, boxed_type(*p), Vec::new()),
                _ => *t,
            })
            .collect()
    } else {
        types.to_vec()
    };
    let mut ctx = Lub {
        db,
        scope,
        guard: Vec::new(),
    };
    ctx.set(&types)
}

/// The state of one [`least_upper_bound`] computation: the scope and the
/// cycle guard of argument-type-set keys.
struct Lub<'a> {
    db: &'a dyn TyDatabase,
    scope: &'a hir::ResolutionScope,
    guard: Vec<FxHashSet<TyData>>,
}

impl Lub<'_> {
    fn object(&self) -> Ty {
        Ty::reference(self.db, "java.lang.Object", Vec::new())
    }

    /// `lub(U1, ..., Uk) = lct(U1, ..., Uk)` fold of [`Self::set_impl`]. The
    /// guard breaks the specification's admitted non-termination: a repeating
    /// argument set, or recursion past a depth cap, degrades to `Object`.
    fn set(&mut self, types: &[Ty]) -> Ty {
        let key: FxHashSet<TyData> = types.iter().map(|t| t.id).collect();
        if self.guard.contains(&key) || self.guard.len() > 64 {
            return self.object();
        }
        self.guard.push(key);
        let result = self.set_impl(types);
        self.guard.pop();
        result
    }

    /// The §4.10.4 computation on the (cycle-guarded) argument set.
    fn set_impl(&mut self, types: &[Ty]) -> Ty {
        let st: Vec<FxHashSet<TyData>> = types.iter().map(|t| self.st(*t)).collect();
        // EST(Ui) = { |W| : W in ST(Ui) }, and EC is their intersection.
        let est: Vec<FxHashSet<TyData>> = st
            .iter()
            .map(|set| {
                set.iter()
                    .map(|id| Ty { id: *id }.erasure(self.db).id)
                    .collect()
            })
            .collect();
        let mut ec: FxHashSet<TyData> = est[0].clone();
        for set in &est[1..] {
            ec.retain(|id| set.contains(id));
        }
        if ec.is_empty() {
            return self.object();
        }
        // MEC: drop candidates that are supertypes of another candidate.
        let ids: Vec<TyData> = ec.iter().copied().collect();
        let mut mec: Vec<Ty> = Vec::new();
        for id in ids {
            let candidate = Ty { id };
            let minimal = ec.iter().all(|other| {
                *other == id || !is_subtype(self.db, self.scope, &Ty { id: *other }, &candidate)
            });
            if minimal {
                mec.push(candidate);
            }
        }
        let mut all: FxHashSet<TyData> = FxHashSet::default();
        for set in &st {
            all.extend(set.iter().copied());
        }
        // Best(X) = Candidate(X) if X is generic, X otherwise.
        let best: Vec<Ty> = mec
            .iter()
            .map(|candidate| {
                if !self.is_generic(*candidate) {
                    return *candidate;
                }
                let TyKind::Reference { name, .. } = candidate.kind(self.db) else {
                    return *candidate;
                };
                let name = name.clone();
                let relevant: Vec<Ty> = all
                    .iter()
                    .map(|id| Ty { id: *id })
                    .filter(|w| {
                        matches!(w.kind(self.db), TyKind::Reference { name: wname, args } if wname == &name && !args.is_empty())
                    })
                    .collect();
                if relevant.is_empty() {
                    *candidate
                } else {
                    self.candidate(&relevant)
                }
            })
            .collect();
        Ty::intersection(self.db, best)
    }

    /// ST(Ui): the transitive supertype set of `ty` ([§4.10.2]), including
    /// itself. A type variable ranges over its declared bounds ([§4.10.2]).
    fn st(&self, ty: Ty) -> FxHashSet<TyData> {
        let mut out = FxHashSet::default();
        self.st_impl(ty, &mut out);
        out
    }

    fn st_impl(&self, ty: Ty, out: &mut FxHashSet<TyData>) {
        if !out.insert(ty.id) {
            return;
        }
        if let TyKind::TypeVar { bounds, .. } = ty.kind(self.db) {
            for bound in bounds {
                self.st_impl(*bound, out);
                for parent in supertypes_impl(self.db, self.scope, bound) {
                    self.st_impl(parent, out);
                }
            }
        } else {
            for parent in supertypes_impl(self.db, self.scope, &ty) {
                self.st_impl(parent, out);
            }
        }
    }

    /// Whether the erased candidate `g` names a class or interface that
    /// declares type parameters ([§4.5]).
    fn is_generic(&self, g: Ty) -> bool {
        let TyKind::Reference { name, .. } = g.kind(self.db) else {
            return false;
        };
        let Some(resolved) = hir::fqn_resolve(self.db, self.scope, name.as_str()) else {
            return false;
        };
        match resolved {
            hir::Resolved::Library(_) => hir::class_generic_info(self.db, &resolved)
                .is_some_and(|info| !info.type_params.is_empty()),
            hir::Resolved::Source(source) => {
                let tree = hir::file_item_tree(self.db, source.file);
                match tree.data(source.item) {
                    hir_def::java::item_tree::ItemData::Class(data)
                    | hir_def::java::item_tree::ItemData::Interface(data) => {
                        !data.type_params.is_empty()
                    }
                    hir_def::java::item_tree::ItemData::Record(data) => {
                        !data.type_params.is_empty()
                    }
                    _ => false,
                }
            }
        }
    }

    /// The candidate parameterization of a generic candidate from its
    /// relevant (possibly several) parameterizations (§4.10.4): `lcp`.
    fn candidate(&mut self, relevant: &[Ty]) -> Ty {
        if relevant.len() == 1 {
            // §4.10.4: when every input shares the *same* parameterization of
            // the candidate — `lub(Modern, Legacy)` where both ST sets contain
            // `FreecamController<Freecam>` — the least containing
            // parameterization is that parameterization itself. Mapping the
            // single (deduplicated) argument through `lcta_unary` would widen
            // the concrete `Freecam` to `? extends Object` and lose the exact
            // type the assignment `FreecamController<Freecam> y = b ? new
            // Modern() : new Legacy()` requires (§4.10.4: `lcp(G<X>) =
            // G<lcta(X)>`, and `lcta(X) = X` when `X` is proper).
            return relevant[0];
        }
        let mut acc = relevant[0];
        for w in &relevant[1..] {
            acc = self.lcp_pair(acc, *w);
        }
        acc
    }

    fn lcp_pair(&mut self, a: Ty, b: Ty) -> Ty {
        let (name, xs) = a.as_reference(self.db).expect("parameterized type");
        let (_, ys) = b.as_reference(self.db).expect("parameterized type");
        let args = xs
            .iter()
            .zip(ys)
            .map(|(x, y)| self.lcta_pair(*x, *y))
            .collect();
        Ty::reference(self.db, name.clone(), args)
    }

    /// lcta(U, V), the least containing type argument for a pair
    /// ([JLS §4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)).
    fn lcta_pair(&mut self, x: Ty, y: Ty) -> Ty {
        match (x.kind(self.db), y.kind(self.db)) {
            // `?` contains every type argument ([§4.5.1]).
            (TyKind::Wildcard(None), _) | (_, TyKind::Wildcard(None)) => {
                Ty::wildcard(self.db, None)
            }
            (TyKind::Wildcard(Some(bx)), TyKind::Wildcard(Some(by))) => match (bx.kind, by.kind) {
                (BoundKind::Upper, BoundKind::Upper) => {
                    let merged = self.set(&[bx.ty, by.ty]);
                    self.upper_wildcard(merged)
                }
                (BoundKind::Upper, BoundKind::Lower) => Ty::wildcard(self.db, None),
                (BoundKind::Lower, BoundKind::Upper) => Ty::wildcard(self.db, None),
                (BoundKind::Lower, BoundKind::Lower) => {
                    let merged = self.glb(bx.ty, by.ty);
                    self.lower_wildcard(merged)
                }
            },
            (TyKind::Wildcard(Some(bx)), _) => match bx.kind {
                BoundKind::Upper => {
                    let merged = self.set(&[bx.ty, y]);
                    self.upper_wildcard(merged)
                }
                BoundKind::Lower => {
                    let merged = self.glb(bx.ty, y);
                    self.lower_wildcard(merged)
                }
            },
            (_, TyKind::Wildcard(Some(by))) => match by.kind {
                BoundKind::Upper => {
                    let merged = self.set(&[x, by.ty]);
                    self.upper_wildcard(merged)
                }
                BoundKind::Lower => {
                    let merged = self.glb(x, by.ty);
                    self.lower_wildcard(merged)
                }
            },
            (_, _) => {
                if x == y {
                    x
                } else {
                    let merged = self.set(&[x, y]);
                    self.upper_wildcard(merged)
                }
            }
        }
    }

    fn upper_wildcard(&self, upper: Ty) -> Ty {
        Ty::wildcard(
            self.db,
            Some(Box::new(WildcardBound {
                kind: BoundKind::Upper,
                ty: upper,
            })),
        )
    }

    fn lower_wildcard(&self, lower: Ty) -> Ty {
        Ty::wildcard(
            self.db,
            Some(Box::new(WildcardBound {
                kind: BoundKind::Lower,
                ty: lower,
            })),
        )
    }

    /// The greatest lower bound ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// `u` when `u <: v`, else `v` when `v <: u`, else the intersection `u & v`.
    fn glb(&self, u: Ty, v: Ty) -> Ty {
        if is_subtype(self.db, self.scope, &u, &v) {
            return u;
        }
        if is_subtype(self.db, self.scope, &v, &u) {
            return v;
        }
        Ty::intersection(self.db, vec![u, v])
    }
}

/// The instantiation of one inference variable from its resolved bounds
/// ([JLS §18.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.4.1)): the equality bound if any, otherwise the least upper bound
/// of the lower bounds that lies within every upper bound, otherwise — when
/// the variable appears in a `throws` clause ([§18.5.2.3]) and every proper
/// upper bound is a supertype of `RuntimeException` — `RuntimeException`
/// itself, otherwise the least upper bound of the upper bounds, otherwise
/// `Object`.
fn pick_instantiation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    lower: &[Ty],
    upper: &[Ty],
    equality: Option<Ty>,
    throws: bool,
) -> Option<Ty> {
    // Bound validation uses *assignment* compatibility ([§5.2]), not strict
    // subtyping: a raw lower bound (`ArrayDeque` from `ArrayDeque::new`) is
    // compatible with a parameterized upper (`Collection<String>`) by
    // unchecked conversion ([§5.1.9]), exactly as in javac's bound check.
    let compatible = |inst: &Ty, upper: &Ty| {
        // Two primitive types relate only by identity here ([§4.10.1]): the
        // widening order (`int` → `long`) is a *conversion*, not subtyping,
        // so `⟨int ≤ α ≤ long⟩` is contradictory even though `int` widens.
        if matches!(inst.kind(db), TyKind::Primitive(_))
            && matches!(upper.kind(db), TyKind::Primitive(_))
        {
            return inst == upper;
        }
        is_subtype(db, scope, inst, upper)
            // A raw lower bound converts to a parameterized upper by
            // unchecked conversion ([§5.1.9], §18.4 bound validation).
            || crate::java::subtyping::is_assignable(db, scope, inst, upper)
    };
    if let Some(eq) = equality {
        for u in upper {
            if !eq.contains_infer_var(db) && !u.contains_infer_var(db) && !compatible(&eq, u) {
                return None;
            }
        }
        return Some(eq);
    }
    if !lower.is_empty() {
        let mut inst = least_upper_bound(db, scope, lower);
        // §18.4: a variable bounded below by a primitive but above by a
        // reference type instantiates to its *boxed* class — the bound set
        // arose from a loose invocation ([§18.2.2]), so `⟨boolean → α⟩`,
        // `α <: Object` yields `Boolean`, never the invalid
        // `Optional<boolean>`.
        if matches!(inst.kind(db), TyKind::Primitive(_))
            && upper.iter().any(|u| {
                matches!(
                    u.kind(db),
                    TyKind::Reference { .. } | TyKind::Intersection(_)
                )
            })
        {
            if let TyKind::Primitive(p) = inst.kind(db) {
                inst = Ty::reference(db, boxed_type(*p), Vec::new());
            }
        }
        for u in upper {
            if !compatible(&inst, u) {
                return None;
            }
        }
        return Some(inst);
    }
    // §18.4: `throws` αi with only unchecked upper bounds instantiates to the
    // least unchecked exception type.
    if throws && upper.iter().all(|u| is_exception_supertype(db, scope, u)) {
        return Some(Ty::reference(db, "java.lang.RuntimeException", Vec::new()));
    }
    if !upper.is_empty() {
        // §18.4: a variable with only upper bounds instantiates to their least
        // upper bound. The implicit `Object` bound of an unbounded type
        // parameter ([§8.4.4]) is redundant next to a more specific bound:
        // `⟨T → Function<String,Integer>⟩` with `T <: Object` must instantiate
        // to `Function<String,Integer>`, not `Object`. Supertype bounds are
        // dropped before the lub, matching javac's least-upper-bound.
        let bounds: Vec<Ty> = upper
            .iter()
            .copied()
            .filter(|u| {
                !upper
                    .iter()
                    .any(|v| *v != *u && !v.contains_infer_var(db) && is_subtype(db, scope, v, u))
            })
            .collect();
        if bounds.is_empty() {
            return Some(least_upper_bound(db, scope, upper));
        }
        return Some(least_upper_bound(db, scope, &bounds));
    }
    Some(Ty::reference(db, "java.lang.Object", Vec::new()))
}

/// Whether `ty` is a supertype of `java.lang.RuntimeException`
/// ([JLS §18.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.4),
/// [§18.5.2.3]): a type the invocation may declare a checked exception as.
fn is_exception_supertype(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: &Ty) -> bool {
    let runtime_exception = Ty::reference(db, "java.lang.RuntimeException", Vec::new());
    is_subtype(db, scope, &runtime_exception, ty)
}

/// The phase conversion of a proper type pair.
fn convertible(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    phase: &InvocationPhase,
    s: &Ty,
    t: &Ty,
) -> bool {
    match phase {
        InvocationPhase::Strict => strict_conversion(db, scope, s, t),
        InvocationPhase::Loose => is_assignable(db, scope, s, t),
    }
}
