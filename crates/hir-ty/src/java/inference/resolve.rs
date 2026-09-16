//! Bound set resolution ([JLS §18.4.1]).

use rustc_hash::FxHashMap;

use super::Inference;
use super::instantiation::{bounds_compatible, pick_instantiation};
use crate::{java::ty::Ty, jvm::db::TyDatabase};

impl Inference {
    /// Bound set resolution ([JLS §18.4.1]). Returns the instantiation of
    /// every inference variable. Cyclic references between variables are
    /// approximated by estimating unresolved variables as `Object` (§18.4.2
    /// concrete instantiation is not fully modelled).
    pub(super) fn resolve(
        &self,
        db: &dyn TyDatabase,
        scope: &hir::ResolutionScope,
    ) -> Option<FxHashMap<u64, Ty>> {
        // The equalities incorporated eagerly are no longer in the bound set;
        // seed the substitution with them so the caller's instantiation sees
        // the resolved values.
        let mut subst: FxHashMap<u64, Ty> = self.applied.clone();
        // §18.3.1: the applied equalities chain (`⟨?4 = ?6⟩` with
        // `⟨?6 = Boolean⟩`), and a bound that references the chained variable
        // must see the *value*, not the next variable. Without flattening, the
        // bound still contains an inference variable when the resolution loop
        // inspects it, the variable is skipped there, and the estimate pass
        // resolves the bound's owner to `Object` — an instantiation the call
        // site then rejects (`Optional.ofNullable(readBoolean(..., identity()))
        // .orElse(false)` inferred `Optional<Object>` and failed the
        // `boolean` return).  The loop is bounded by the number of keys: a
        // genuine cycle cannot shorten and stops changing.
        let seeded: Vec<u64> = subst.keys().copied().collect();
        for _ in 0..=seeded.len() {
            let mut changed = false;
            for key in &seeded {
                let Some(value) = subst.get(key).copied() else {
                    continue;
                };
                let updated = value.substitute_infer(db, &subst);
                if subst.get(key).copied() != Some(updated) {
                    subst.insert(*key, updated);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let ids: Vec<u64> = self.bounds.keys().copied().collect();
        // The *dependency* upper bounds of every variable: those whose type
        // mentions the variable itself ([JLS §18.3.2]), which
        // [`Self::effective_bounds`] withholds from resolution and which are
        // validated against the resolved instantiation below.
        let dependency: FxHashMap<u64, Vec<Ty>> = self
            .bounds
            .iter()
            .filter_map(|(id, bounds)| {
                let deps: Vec<Ty> = bounds
                    .upper
                    .iter()
                    .copied()
                    .filter(|t| t.contains_infer_var_id(db, *id))
                    .collect();
                (!deps.is_empty()).then_some((*id, deps))
            })
            .collect();
        loop {
            let mut progress = false;
            for &id in &ids {
                if subst.contains_key(&id) {
                    continue;
                }
                let (eq, lower, upper, dependencies) = self.effective_bounds(db, id, &subst, false);
                if eq.as_ref().is_some_and(|t| t.contains_infer_var(db))
                    || lower.iter().any(|t| t.contains_infer_var(db))
                    || upper.iter().any(|t| t.contains_infer_var(db))
                    || dependencies.iter().any(|t| t.contains_infer_var(db))
                {
                    continue;
                }
                let inst = pick_instantiation(
                    db,
                    scope,
                    &lower,
                    &with_erasure_fallback(db, &upper, &dependencies),
                    eq,
                    self.throws.contains(&id),
                )?;
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
                    let (eq, lower, upper, dependencies) =
                        self.effective_bounds(db, id, &subst, true);
                    let inst = pick_instantiation(
                        db,
                        scope,
                        &lower,
                        &with_erasure_fallback(db, &upper, &dependencies),
                        eq,
                        self.throws.contains(&id),
                    )?;
                    subst.insert(id, inst);
                }
            }
        }
        // §18.3.2/§18.4.1: the *dependency* bounds dropped from the bound set
        // ([`Self::effective_bounds`]) must hold for the resolved
        // instantiation. They are only decidable here, after substitution —
        // `α <: BC<α,β>` becomes `BC<O,B>` once `α := O` and `β := B`, which
        // `O`'s own declared bound satisfies (§4.4).
        for (id, bounds) in &dependency {
            let Some(inst) = subst.get(id).copied() else {
                continue;
            };
            if inst.contains_infer_var(db) {
                continue;
            }
            for bound in bounds {
                let bound = bound.substitute_infer(db, &subst);
                if bound.contains_infer_var(db) {
                    continue;
                }
                if !bounds_compatible(db, scope, &inst, &bound) {
                    return None;
                }
            }
        }
        // The incorporated equalities can reference each other and the
        // resolved variables; substitute the values to a fixpoint so the
        // instantiation is fully resolved. The loop is bounded: a cyclic
        // equality that no substitution can settle (α maps to a type that
        // still references α) would otherwise grow the values without bound.
        let keys: Vec<u64> = subst.keys().copied().collect();
        // A dependency chain of length n unwinds within n passes (each pass
        // substitutes one level of every value), so 2n rounds bound any
        // acyclic table; a true cycle cannot converge, and 2n rounds let it
        // grow at most 2^(2n) deep before the substitution's own depth guard
        // degrades it to `error`.
        let rounds = keys.len().saturating_mul(2).max(1);
        for _ in 0..rounds {
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
    ) -> (Option<Ty>, Vec<Ty>, Vec<Ty>, Vec<Ty>) {
        let b = &self.bounds[&id];
        let eq = b.equality.map(|t| t.substitute_infer(db, subst));
        let lower: Vec<Ty> = b
            .lower
            .iter()
            .map(|t| t.substitute_infer(db, subst))
            .collect();
        // §18.3.2: a bound whose type mentions the variable it bounds is a
        // *dependency* bound, not an instantiation — `α <: BC<α,β>` (the
        // declared bound of a type parameter that appears in its own bound,
        // §4.4) cannot be used to compute `α`, and javac drops such bounds when
        // instantiating. Keeping them made the lub of the upper bounds the
        // instantiation whenever the bound's own variable had already been
        // estimated away: for `<O extends BC<O,B>, B extends CB<O,B>> O
        // pick(B b)` inside a method returning `O`, `α`'s bounds are
        // `α <: BC<α,β>` and `α <: O`, and the lub of the *erased*
        // `BC<Object,β>` with `O` is `BC<Object,β>` — which does not satisfy
        // `α <: O`, so the invocation was reported inapplicable. With the
        // dependency bound dropped, `α` instantiates from `O` and the dropped
        // bound is validated against it afterwards.
        let substituted: Vec<Ty> = b
            .upper
            .iter()
            .map(|t| t.substitute_infer(db, subst))
            .collect();
        let (dependencies, upper): (Vec<Ty>, Vec<Ty>) = substituted
            .into_iter()
            .partition(|t| t.contains_infer_var_id(db, id));
        if estimate {
            let eq = eq.map(|t| t.erase_infer_vars(db));
            let lower = lower.iter().map(|t| t.erase_infer_vars(db)).collect();
            let upper = upper.iter().map(|t| t.erase_infer_vars(db)).collect();
            let dependencies = dependencies
                .iter()
                .map(|t| t.erase_infer_vars(db))
                .collect();
            (eq, lower, upper, dependencies)
        } else {
            (eq, lower, upper, dependencies)
        }
    }
}

/// The upper bounds a resolution attempt sees: the *proper* upper bounds when
/// the variable has any, otherwise the erasures of its dependency bounds
/// ([JLS §18.3.2]).
///
/// A variable whose only bound mentions itself — the F-bounded declaration of
/// a class type parameter, `⟨α <: Enum<α⟩` for `EnumSet<E extends Enum<E>>` —
/// has nothing to instantiate from, and javac falls back to the *erasure* of
/// that bound: `EnumSet.copyOf((Collection) raw)` infers the raw `Enum`, which
/// the bound check then admits by unchecked conversion ([§5.1.9]), matching
/// javac's own unchecked-usage note for the call. Instantiating to `Object`
/// instead makes the bound unsatisfiable and rejects the invocation.
fn with_erasure_fallback(db: &dyn TyDatabase, upper: &[Ty], dependencies: &[Ty]) -> Vec<Ty> {
    if !upper.is_empty() || dependencies.is_empty() {
        return upper.to_vec();
    }
    dependencies.iter().map(|t| t.erasure(db)).collect()
}
