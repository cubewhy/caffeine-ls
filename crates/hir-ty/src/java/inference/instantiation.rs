//! The instantiation of a single inference variable ([JLS §18.4.1]).

use super::lub::least_upper_bound;
use crate::{
    java::db::TyDatabase,
    java::subtyping::is_subtype,
    java::ty::{Ty, TyKind, boxed_type},
};

/// The instantiation of one inference variable from its resolved bounds
/// ([JLS §18.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.4.1)): the equality bound if any, otherwise the least upper bound
/// of the lower bounds that lies within every upper bound, otherwise — when
/// the variable appears in a `throws` clause ([§18.5.2.3]) and every proper
/// upper bound is a supertype of `RuntimeException` — `RuntimeException`
/// itself, otherwise the least upper bound of the upper bounds, otherwise
/// `Object`.
pub(super) fn pick_instantiation(
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
            && let TyKind::Primitive(p) = inst.kind(db)
        {
            inst = Ty::reference(db, boxed_type(*p), Vec::new());
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
        // JLS §18.1.1/§4.4 with §5.1.7: an upper bound that is a primitive
        // (`⟨U → long〉` from a `long` assignment target) boxes for the
        // comparison — `Long <: Object`, so `Object` drops and `U := Long`,
        // not `Object`. Without boxing a primitive upper never drops its
        // `Object` sibling and every primitive-targeted generic resolves to
        // `Object`.
        let boxed_upper: Vec<Ty> = upper
            .iter()
            .map(|u| match u.kind(db) {
                TyKind::Primitive(p) => Ty::reference(db, boxed_type(*p), Vec::new()),
                _ => *u,
            })
            .collect();
        let bounds: Vec<Ty> = boxed_upper
            .iter()
            .copied()
            .filter(|u| {
                !boxed_upper
                    .iter()
                    .any(|v| *v != *u && !v.contains_infer_var(db) && is_subtype(db, scope, v, u))
            })
            .collect();
        if bounds.is_empty() {
            return Some(least_upper_bound(db, scope, &boxed_upper));
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
