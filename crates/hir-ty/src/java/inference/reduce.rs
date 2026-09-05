//! Constraint reduction ([JLS §18.2]).

use std::collections::VecDeque;

use rustc_hash::FxHashSet;

use super::{Constraint, Inference, InvocationPhase};
use crate::{
    java::db::TyDatabase,
    java::subtyping::{is_assignable, strict_conversion, supertypes_impl},
    java::ty::{BoundKind, Ty, TyData, TyKind, boxed_type},
};

impl Inference {
    pub(super) fn reduce_sub(
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
        // §18.2.1/[§4.10.2]: the null type is a subtype of every reference
        // type, so `⟨null → T⟩` is a tautology for any reference `T` — in
        // particular a `Reader<α>` still carrying an inference variable.
        // Without this arm the constraint fell through to the proper-type
        // check, whose "one side carries an inference variable" rule reports
        // false and rejects the candidate (`<Z> C<Z> makeC(Reader<Z>)` called
        // with `null` and a `C<Vector3f>` target).
        if s.is_null(db) {
            return matches!(
                t.kind(db),
                TyKind::Reference { .. }
                    | TyKind::Array(_)
                    | TyKind::TypeVar { .. }
                    | TyKind::Intersection(_)
            );
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
    pub(super) fn reduce_eq(
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
