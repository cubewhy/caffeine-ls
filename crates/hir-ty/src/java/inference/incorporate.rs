//! Bound set incorporation ([JLS §18.3.1]).

use rustc_hash::FxHashMap;

use super::{Constraint, Inference, InvocationPhase};
use crate::{
    java::db::TyDatabase,
    java::subtyping::{is_assignable, is_subtype},
    java::ty::{Ty, TyKind},
};

impl Inference {
    /// The total number of bounds in the table, used to detect whether
    /// reducing the implied bounds changed anything.
    fn bound_count(&self) -> usize {
        self.bounds
            .values()
            .map(|b| b.upper.len() + b.lower.len() + usize::from(b.equality.is_some()))
            .sum()
    }

    /// Bound set incorporation ([JLS §18.3.1]): equality bounds are substituted
    /// away, a proper lower bound `S <: α` against a proper upper bound
    /// `α <: T` must satisfy `S <: T`, and same-erasure lower/upper pairs
    /// imply equalities between their type arguments.
    pub(super) fn incorporate(
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
                // §18.3.1 complementary pairs with an instantiation: `α = S`
                // paired with a *dependency* `α <: T` (where T mentions another
                // inference variable) implies `⟨S <: T⟩`, and paired with a
                // lower-bound dependency `T <: α` implies `⟨T <: S⟩`. Without
                // this propagation a method type parameter bound that
                // references another type parameter
                // (`<T, Z extends T> EntityDataType<Z> make(...)`) loses the
                // dependency when the referenced parameter is instantiated from
                // the return type: `Z := Vector3f` from the target would remove
                // `Z`'s `Z <: T` dependency without giving `T` the `Vector3f`
                // lower bound, and `T` would degrade to `Object`, making the
                // `Writer<T>` lambda parameter useless. A *proper* upper bound
                // `U` needs no constraint push here: the equality substitution
                // below rewrites α away and resolution/validation checks
                // `S <: U` directly.
                let b = &self.bounds[&id];
                // §18.3.1 complementary pairs with an instantiation: `α = S`
                // paired with a *dependency* `α <: T` (where T mentions another
                // inference variable) implies `⟨S <: T⟩`, and paired with a
                // lower-bound dependency `T <: α` implies `⟨T <: S⟩`. Without
                // this propagation a method type parameter bound that
                // references another type parameter
                // (`<T, Z extends T> EntityDataType<Z> make(...)`) loses the
                // dependency when the referenced parameter is instantiated from
                // the return type: `Z := Vector3f` from the target would remove
                // `Z`'s `Z <: T` dependency without giving `T` the `Vector3f`
                // lower bound, and `T` would degrade to `Object`, making the
                // `Writer<T>` lambda parameter useless. A *proper* upper bound
                // `U` needs no constraint push here: the equality substitution
                // below rewrites α away and resolution/validation checks
                // `S <: U` directly.
                //
                // The bound is substituted for α *before* the dependency is
                // queued: a declared bound that references its own type
                // parameter (`<E extends Enum<E>>` lowers to `α <: Enum<α>`)
                // mentions the very variable being eliminated, and a queued
                // `⟨S <: Enum<α⟩` would later reduce against a variable
                // already removed from the table — rejecting an otherwise
                // valid invocation (`<E extends Enum<E>> E pick(Class<E>, E)`
                // called with a `Class<K>`/`K` pair whose `K` has the matching
                // recursive enum bound). After substitution the bound is
                // `Enum<K>` — a proper type, consistent by `K`'s own declared
                // bound — so no dependency remains to propagate.
                let subst: FxHashMap<u64, Ty> = FxHashMap::from_iter([(id, eq)]);
                for u in &b.upper {
                    let u = u.substitute_infer(db, &subst);
                    if u.contains_infer_var(db) {
                        // §5.1.10/§18.2.3: the equality instantiation `S` is a
                        // value type; when it is wildcard-parameterized
                        // (`CE<?>` — the element type of a `List<CE<?>>`
                        // actual), the constraint ⟨S <: U⟩ is reduced against
                        // its *capture*, not the bare wildcard: a generic
                        // referenced method `<T extends CE<?>> void wf(..., CE<T>)`
                        // whose type variable `α` is bounded above by the SAM's
                        // `CE<?>` element must instantiate `α` to the capture
                        // (`CE<CAP#n>`), which javac infers. Without it the
                        // wildcard degrades to its `Object` lower bound and the
                        // reference is reported inapplicable.
                        let eq = match eq.kind(db) {
                            TyKind::Reference { args, .. }
                                if args.iter().any(|arg| arg.is_wildcard(db)) =>
                            {
                                crate::java::ty::capture_conversion(db, scope, eq)
                            }
                            _ => eq,
                        };
                        self.worklist.push_back(Constraint::Sub(eq, u));
                    }
                }
                for l in &b.lower {
                    let l = l.substitute_infer(db, &subst);
                    if l.contains_infer_var(db) {
                        self.worklist.push_back(Constraint::Sub(l, eq));
                    }
                }
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
                        // JLS §18.3.1: every lower bound `S` and upper bound
                        // `T` of one variable imply `⟨S <: T⟩`. When both are
                        // proper it is a validation (`S` must convert to `T`);
                        // when one side is a lone inference variable and the
                        // other is proper, it propagates the dependency
                        // (`<T, Z extends T>` with `Z` lower `Byte` and upper
                        // `T` gives `Byte <: T`, so `T` picks up the lower
                        // bound). Complex bounds mentioning variables inside
                        // (e.g. `List<T>`) stay skipped — re-pushing them
                        // re-adds the same derived bound forever and the loop
                        // never ends. Duplicates already in the set are
                        // skipped for the same reason.
                        if l.contains_infer_var(db) || u.contains_infer_var(db) {
                            let simple = match (l.as_infer_var(db), u.as_infer_var(db)) {
                                (None, Some(uid)) if !l.contains_infer_var(db) => {
                                    !self.bounds.get(&uid).is_some_and(|b| b.lower.contains(l))
                                }
                                (Some(lid), None) if !u.contains_infer_var(db) => {
                                    !self.bounds.get(&lid).is_some_and(|b| b.upper.contains(u))
                                }
                                _ => false,
                            };
                            if simple {
                                self.worklist.push_back(Constraint::Sub(*l, *u));
                                changed = true;
                            }
                            continue;
                        }
                        // §18.3.1 bound validation: a proper lower bound `S`
                        // and a proper upper bound `T` must satisfy `S <: T`.
                        // Like `pick_instantiation`, this is *assignment*
                        // compatibility ([§5.2]) — a raw lower bound
                        // (`CompletableFuture` from `new CompletableFuture[0]`)
                        // converts to a parameterized upper
                        // (`CompletableFuture<?>`) by unchecked conversion
                        // ([§5.1.9]), exactly as in javac's bound check. Two
                        // primitives relate only by identity here ([§4.10.1]).
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
}
