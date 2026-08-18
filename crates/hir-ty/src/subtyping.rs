//! Subtyping and assignability.
//!
//! [`is_subtype`] implements the subtype relation of
//! [JLS §4.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10):
//! identity, the primitive partial order
//! ([§4.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.1),
//! which is identity only), the class and interface transitive closure
//! ([§4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2))
//! and array covariance
//! ([§4.10.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.3)).
//! [`is_assignable`] implements assignment conversion
//! ([JLS §5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2))
//! via identity, primitive widening
//! ([§5.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.2))
//! and reference widening
//! ([§5.1.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.5)).
//!
//! Boxing/unboxing
//! ([§5.1.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.7),
//! [§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8)),
//! capture conversion
//! ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
//! and full parameterized-type subtyping
//! ([§4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)
//! with type-argument substitution) are not modelled yet.

use rustc_hash::FxHashSet;
use syntax::stub::PrimitiveType;

use hir_expand::name::Name;

use crate::{
    db::TyDatabase,
    ty::{BoundKind, Ty, TyKind},
};

/// The direct supertypes of `ty`
/// ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2),
/// [§4.10.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.3)):
///
/// * for a class/interface, its direct superclass and interfaces as declared
///   in the classfile (erasure-style: tier-1 classfile data does not carry
///   type arguments);
/// * for an array type, `Object`, `Cloneable` and `Serializable`.
///
/// Types outside the resolvable hierarchy (source classes, type variables,
/// primitives) yield no supertypes.
pub fn supertypes(db: &dyn TyDatabase, scope: &hir::ResolutionScope<'_>, ty: &Ty) -> Vec<Ty> {
    match &ty.kind {
        TyKind::Reference { name, .. } => {
            let Some(resolved) = hir::fqn_resolve(db, scope, name.as_str()) else {
                return Vec::new();
            };
            let interner = &db.hir_state().interner;
            hir::super_types(db, &resolved)
                .into_iter()
                .map(|fqn| Ty::reference(Name::new(interner.resolve(&fqn)), Vec::new()))
                .collect()
        }
        TyKind::Array(_) => vec![
            Ty::reference("java.lang.Object", Vec::new()),
            Ty::reference("java.lang.Cloneable", Vec::new()),
            Ty::reference("java.io.Serializable", Vec::new()),
        ],
        _ => Vec::new(),
    }
}

/// Whether `sub` is a subtype of `sup`
/// ([JLS §4.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10)).
pub fn is_subtype(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope<'_>,
    sub: &Ty,
    sup: &Ty,
) -> bool {
    if sub == sup {
        return true;
    }
    match (&sub.kind, &sup.kind) {
        // §4.10.1: the primitive partial order is identity only (widening is
        // a conversion, not a subtyping).
        (TyKind::Primitive(_), TyKind::Primitive(_)) => false,
        // §4.10.3: S[] <: T[] iff S <: T.
        (TyKind::Array(_), TyKind::Array(_)) => match (sub.element(), sup.element()) {
            (Some(s), Some(t)) => is_subtype(db, scope, s, t),
            _ => false,
        },
        // §4.10.3: T[] <: Object, Cloneable, Serializable.
        (TyKind::Array(_), TyKind::Reference { name, .. }) => matches!(
            name.as_str(),
            "java.lang.Object" | "java.lang.Cloneable" | "java.io.Serializable"
        ),
        (TyKind::Reference { .. }, TyKind::Reference { .. }) => {
            reference_subtype(db, scope, sub, sup)
        }
        _ => false,
    }
}

/// Reference subtyping: identical erasure (§4.10.2) or membership in the
/// declared supertype closure.
fn reference_subtype(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope<'_>,
    sub: &Ty,
    sup: &Ty,
) -> bool {
    let (sub_name, sub_args) = sub.as_reference().expect("reference kind");
    let (sup_name, sup_args) = sup.as_reference().expect("reference kind");
    if sub_name == sup_name {
        // Same erasure: the parameterized types are subtypes only when the
        // type arguments are compatible (§4.10.2: invariant except wildcards).
        return params_ok(db, scope, sub_args, sup_args);
    }

    // Otherwise walk the transitive closure of `sub`'s supertypes (§4.10.2).
    let mut stack = vec![sub.clone()];
    let mut visited = FxHashSet::default();
    while let Some(current) = stack.pop() {
        for parent in supertypes(db, scope, &current) {
            if !visited.insert(parent.clone()) {
                continue;
            }
            if parent == *sup {
                return true;
            }
            if let Some((parent_name, parent_args)) = parent.as_reference()
                && parent_name == sup_name
                && params_ok(db, scope, parent_args, sup_args)
            {
                return true;
            }
            stack.push(parent);
        }
    }
    false
}

/// Whether `sub_args` are subtype-compatible with `sup_args` (§4.10.2).
fn params_ok(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope<'_>,
    sub_args: &[Ty],
    sup_args: &[Ty],
) -> bool {
    if sup_args.is_empty() {
        // A raw or non-generic super accepts any instantiation (§4.8).
        return true;
    }
    if sub_args.is_empty() {
        // A raw type is not a subtype of a parameterized type (§4.8).
        return false;
    }
    sub_args.len() == sup_args.len()
        && sub_args
            .iter()
            .zip(sup_args)
            .all(|(sub, sup)| arg_ok(db, scope, sub, sup))
}

fn arg_ok(db: &dyn TyDatabase, scope: &hir::ResolutionScope<'_>, sub: &Ty, sup: &Ty) -> bool {
    match &sup.kind {
        // `?` accepts any type argument.
        TyKind::Wildcard(None) => true,
        TyKind::Wildcard(Some(bound)) => match bound.kind {
            BoundKind::Upper => is_subtype(db, scope, sub, &bound.ty),
            BoundKind::Lower => is_subtype(db, scope, &bound.ty, sub),
        },
        _ => sub == sup,
    }
}

/// Whether `src` is assignable to `dst` by assignment conversion
/// ([JLS §5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2)):
/// identity, primitive widening
/// ([§5.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.2))
/// and reference widening
/// ([§5.1.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.5)).
pub fn is_assignable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope<'_>,
    src: &Ty,
    dst: &Ty,
) -> bool {
    if src == dst {
        return true;
    }
    match (&src.kind, &dst.kind) {
        (TyKind::Primitive(src), TyKind::Primitive(dst)) => widening_primitive(*src, *dst),
        (TyKind::Reference { .. }, TyKind::Reference { .. }) => is_subtype(db, scope, src, dst),
        _ => false,
    }
}

/// Primitive widening conversion, the transitions of
/// [JLS table 5.1-B](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.2).
fn widening_primitive(src: PrimitiveType, dst: PrimitiveType) -> bool {
    use PrimitiveType::*;
    matches!(
        (src, dst),
        (Byte, Short | Int | Long | Float | Double)
            | (Short, Int | Long | Float | Double)
            | (Char, Int | Long | Float | Double)
            | (Int, Long | Float | Double)
            | (Long, Float | Double)
            | (Float, Double)
    )
}
