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

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    java::db::TyDatabase,
    java::method::{MethodData, MethodTypeParam},
    java::ty::Ty,
    java::ty::TypeVarScope,
};

mod incorporate;
mod instantiation;
mod lub;
mod reduce;
mod resolve;

pub use self::lub::least_upper_bound;

/// The invocation conversion of a phase: strict invocation
/// ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2))
/// admits identity, widening primitive ([§5.1.2]), widening reference
/// ([§5.1.5]) and unchecked ([§5.1.9]) conversions
/// ([§5.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.3));
/// loose invocation
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
    /// The upper bounds a variable carried when its equality was applied
    /// ([§18.4.1] bound validation). The variable's entry is removed from
    /// `bounds` with the substitution, so the bounds it must satisfy are kept
    /// alongside it — a constraint that arrives *after* the equality would
    /// otherwise re-create the entry and hide the pair.
    applied_upper: FxHashMap<u64, Vec<Ty>>,
    /// Constraints added but not yet reduced ([JLS §18.2]).
    worklist: VecDeque<Constraint>,
}

impl Inference {
    pub(crate) fn new() -> Self {
        Self {
            bounds: FxHashMap::default(),
            throws: FxHashSet::default(),
            applied: FxHashMap::default(),
            applied_upper: FxHashMap::default(),
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
        self.validate_applied(db, scope)
    }

    /// §18.4.1 bound validation of the instantiated variables: every equality
    /// `α = S` collapses `α` to `S`, so `S` must satisfy the bounds `α`
    /// carried — `S <: U` for every upper bound `U` ([§18.4.1] "the bound set
    /// must be satisfiable"). Resolution validates this when it picks the
    /// instantiation, but the constraint that *creates* the conflict can
    /// arrive after the equality was applied and the variable removed from
    /// the table, so a probe that never resolves
    /// (`resolve == false`, the nested-invocation target round of
    /// [§18.5.2.4]) would otherwise accept it:
    /// `⟨Builder <: α_B⟩`, `⟨α_B <: AbstractBuilder<α_R>⟩` instantiate
    /// `α_R = Config`, and the enclosing formal then bounds `α_R` by
    /// `Consumer<Config.Builder>` — an unsatisfiable pair the probe must
    /// reject.
    ///
    /// A bound or value that still mentions a variable — an inference
    /// variable (validated once it instantiates) or a *declared* type
    /// variable whose own bounds belong to its declaration ([§4.4]) — is not
    /// decidable here and is left to resolution.
    fn validate_applied(&self, db: &dyn TyDatabase, scope: &hir::ResolutionScope) -> bool {
        // §18.3.1: the applied equalities chain (`⟨?R = ?T⟩` with
        // `⟨?T = String⟩`), and the value an instantiated variable must satisfy
        // is the *end* of the chain — the raw entry still holds an inference
        // variable and would be skipped, letting a bound that contradicts the
        // value through: with `?R = ?T = String`, `⟨?R <: URL⟩` arrived later
        // and the nested call was accepted against a `URL` formal.
        let mut flat: FxHashMap<u64, Ty> = self.applied.clone();
        let keys: Vec<u64> = flat.keys().copied().collect();
        for _ in 0..=keys.len() {
            let mut changed = false;
            for key in &keys {
                let Some(value) = flat.get(key).copied() else {
                    continue;
                };
                let updated = value.substitute_infer(db, &flat);
                if flat.get(key).copied() != Some(updated) {
                    flat.insert(*key, updated);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (id, eq) in &flat {
            if eq.contains_infer_var(db) || eq.contains_declared_type_var(db) {
                continue;
            }
            let recorded = self.applied_upper.get(id).map(Vec::as_slice).unwrap_or(&[]);
            let later = self
                .bounds
                .get(id)
                .map(|b| b.upper.as_slice())
                .unwrap_or(&[]);
            for upper in recorded.iter().chain(later) {
                if upper.contains_infer_var(db) || upper.contains_declared_type_var(db) {
                    continue;
                }
                if !instantiation::bounds_compatible(db, scope, eq, upper) {
                    return false;
                }
            }
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
    ) -> FxHashMap<TypeVarScope, Ty> {
        let mut subst: FxHashMap<TypeVarScope, Ty> = FxHashMap::default();
        for tp in type_params {
            let var = self.fresh_var(db);
            subst.insert(tp.scope.clone(), var);
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
        let mut subst: FxHashMap<TypeVarScope, Ty> = FxHashMap::default();
        for tp in &method.type_params {
            let var = self.fresh_var(db);
            subst.insert(tp.scope.clone(), var);
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
}
