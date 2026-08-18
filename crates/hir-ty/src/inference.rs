//! Method invocation type inference ([JLS §18]).
//!
//! [`crate::method::pick_method`] determines the invocation type
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
//! invocation has no target type), nor is throws inference (§18.5.2.3). The
//! least upper bound of §4.10.4 is approximated by the most specific of the
//! bounds under subtyping. Inference-variable-bearing types never reach the
//! memoized subtype/supertype queries in [`crate::subtyping`]: all
//! [`Ty`]s handed to `is_subtype`/`is_assignable` here are proper.

use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    db::TyDatabase,
    subtyping::{is_assignable, is_subtype, strict_conversion, supertypes_impl},
    ty::{BoundKind, Ty, TyData, TyKind},
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
#[derive(Debug, Default)]
struct Bounds {
    upper: Vec<Ty>,
    lower: Vec<Ty>,
    equality: Option<Ty>,
}

/// An invocation type inference table ([JLS §18.5.2]).
pub(crate) struct Inference {
    bounds: FxHashMap<u64, Bounds>,
}

impl Inference {
    pub(crate) fn new() -> Self {
        Self {
            bounds: FxHashMap::default(),
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
        if !self.reduce(db, scope, phase, constraints) {
            return None;
        }
        if !self.incorporate(db, scope) {
            return None;
        }
        self.resolve(db, scope)
    }

    /// Constraint reduction ([JLS §18.2.1, §18.2.2, §18.2.3]): each constraint
    /// either adds a bound to an inference variable, spawns derived
    /// constraints, or — when both sides are proper types — is checked against
    /// the subtype/conversion relation.
    fn reduce(
        &mut self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
        phase: InvocationPhase,
        constraints: Vec<Constraint>,
    ) -> bool {
        let mut worklist: VecDeque<Constraint> = constraints.into();
        while let Some(constraint) = worklist.pop_front() {
            match constraint {
                Constraint::Sub(s, t) => {
                    if !self.reduce_sub(db, scope, phase, &s, &t, &mut worklist) {
                        return false;
                    }
                }
                Constraint::Eq(s, t) => {
                    if !self.reduce_eq(db, &s, &t, &mut worklist) {
                        return false;
                    }
                }
            }
        }
        true
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
        // §18.2.1: ⟨S → α⟩ is the lower bound `S <: α`; ⟨α → T⟩ is the upper
        // bound `α <: T`.
        if let Some(id) = t.as_infer_var(db) {
            self.bounds.entry(id).or_default().lower.push(*s);
            return true;
        }
        if let Some(id) = s.as_infer_var(db) {
            self.bounds.entry(id).or_default().upper.push(*t);
            return true;
        }
        match (s.kind(db), t.kind(db)) {
            // §18.2.1: ⟨S[] → T[]⟩ reduces to ⟨S → T⟩.
            (TyKind::Array(si), TyKind::Array(ti)) => {
                worklist.push_back(Constraint::Sub(**si, **ti));
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
                if sa.is_empty() || sa.len() != ta.len() {
                    return false;
                }
                if ta.iter().any(|arg| arg.is_wildcard(db)) {
                    for (s_arg, t_arg) in sa.iter().zip(ta.iter()) {
                        worklist.push_back(Constraint::Sub(*s_arg, *t_arg));
                    }
                } else {
                    for (s_arg, t_arg) in sa.iter().zip(ta.iter()) {
                        worklist.push_back(Constraint::Eq(*s_arg, *t_arg));
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
                // variables is reduced against the direct supertype with the
                // target's erasure (§18.2.1).
                if !s.contains_infer_var(db) && !t.contains_infer_var(db) {
                    return convertible(db, scope, &phase, s, t);
                }
                if let Some((tn, _)) = t.as_reference(db) {
                    for parent in supertypes_impl(db, scope, s) {
                        if let Some((pn, _)) = parent.as_reference(db)
                            && pn == tn
                        {
                            worklist.push_back(Constraint::Sub(parent, *t));
                            return true;
                        }
                    }
                    return false;
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
        if let Some(id) = t.as_infer_var(db) {
            self.bounds.entry(id).or_default().equality = Some(*s);
            return true;
        }
        if let Some(id) = s.as_infer_var(db) {
            self.bounds.entry(id).or_default().equality = Some(*t);
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
    /// away, and a proper lower bound `S <: α` against a proper upper bound
    /// `α <: T` must satisfy `S <: T`.
    fn incorporate(&mut self, db: &dyn TyDatabase, scope: &hir::ResolutionScope) -> bool {
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
                self.bounds.remove(&id);
                changed = true;
            }

            let ids: Vec<u64> = self.bounds.keys().copied().collect();
            for id in ids {
                let b = &self.bounds[&id];
                let lower = b.lower.clone();
                let upper = b.upper.clone();
                for l in &lower {
                    for u in &upper {
                        if !l.contains_infer_var(db)
                            && !u.contains_infer_var(db)
                            && !is_subtype(db, scope, l, u)
                        {
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

    /// Bound set resolution ([JLS §18.4.1]). Returns the instantiation of
    /// every inference variable. Cyclic references between variables are
    /// approximated by estimating unresolved variables as `Object` (§18.4.2
    /// concrete instantiation is not fully modelled).
    fn resolve(
        &self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
    ) -> Option<FxHashMap<u64, Ty>> {
        let mut subst: FxHashMap<u64, Ty> = FxHashMap::default();
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
                let inst = pick_instantiation(db, scope, &lower, &upper, eq)?;
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
                    let inst = pick_instantiation(db, scope, &lower, &upper, eq)?;
                    subst.insert(id, inst);
                }
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

/// The instantiation of one inference variable from its resolved bounds
/// ([JLS §18.4.1]): the equality bound if any, otherwise the least upper bound
/// of the lower bounds that lies within every upper bound, otherwise the least
/// upper bound of the upper bounds, otherwise `Object`.
fn pick_instantiation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    lower: &[Ty],
    upper: &[Ty],
    equality: Option<Ty>,
) -> Option<Ty> {
    if let Some(eq) = equality {
        for u in upper {
            if !eq.contains_infer_var(db)
                && !u.contains_infer_var(db)
                && !is_subtype(db, scope, &eq, u)
            {
                return None;
            }
        }
        return Some(eq);
    }
    if !lower.is_empty() {
        let inst = lub(db, scope, lower);
        for u in upper {
            if !is_subtype(db, scope, &inst, u) {
                return None;
            }
        }
        return Some(inst);
    }
    if !upper.is_empty() {
        return Some(lub(db, scope, upper));
    }
    Some(Ty::reference(db, "java.lang.Object", Vec::new()))
}

/// The least upper bound approximation ([JLS §4.10.4]): the bound that is a
/// supertype of all the others, if it exists; otherwise the most specific
/// common supertype of all the bounds; otherwise `Object`.
fn lub(db: &dyn TyDatabase, scope: &hir::ResolutionScope, tys: &[Ty]) -> Ty {
    // A bound that is already a supertype of all the others wins directly.
    for candidate in tys {
        if tys
            .iter()
            .all(|t| t == candidate || is_subtype(db, scope, t, candidate))
        {
            return *candidate;
        }
    }
    // Otherwise the most specific common supertype (§4.10.4): the
    // intersection of the transitive supertype sets, kept minimal.
    let mut common = supertypes_transitive(db, scope, &tys[0]);
    for ty in &tys[1..] {
        let supers = supertypes_transitive(db, scope, ty);
        common.retain(|c| supers.iter().any(|s| s.id == c.id));
    }
    for c in &common {
        if common
            .iter()
            .all(|o| o.id == c.id || !is_subtype(db, scope, o, c))
        {
            return *c;
        }
    }
    Ty::reference(db, "java.lang.Object", Vec::new())
}

/// `ty` and its transitive supertypes ([§4.10.2]), closure-first order.
fn supertypes_transitive(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: &Ty) -> Vec<Ty> {
    let mut out = Vec::new();
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut stack = vec![*ty];
    while let Some(t) = stack.pop() {
        if !seen.insert(t.id) {
            continue;
        }
        out.push(t);
        for parent in supertypes_impl(db, scope, &t) {
            stack.push(parent);
        }
    }
    out
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
