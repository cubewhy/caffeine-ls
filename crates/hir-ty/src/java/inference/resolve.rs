//! Bound set resolution ([JLS §18.4.1]).

use rustc_hash::FxHashMap;

use super::{Inference, instantiation::pick_instantiation};
use crate::{java::db::TyDatabase, java::ty::Ty};

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
