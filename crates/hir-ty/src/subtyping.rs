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
//! The per-scope subtype and supertype results are memoized as tracked
//! queries keyed on the interned scope ([`crate::db::ScopeId`]) and the
//! interned type id ([`crate::ty::TyData`]), so repeated checks of the same
//! pair — the IDE pattern — hit the query cache. The public functions
//! intern the scope and the types, then delegate.
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
    db::{ScopeId, ScopeKind, TyDatabase},
    ty::{BoundKind, Ty, TyData, TyKind},
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
    let scope = ScopeId::new(db, ScopeKind::from_scope(scope));
    supertypes_query(db, scope, ty.id)
}

/// Memoized per (scope, type). See [`supertypes`].
#[salsa::tracked(returns(clone))]
pub(crate) fn supertypes_query(db: &dyn TyDatabase, scope: ScopeId, ty: TyData) -> Vec<Ty> {
    let ty = Ty { id: ty };
    match ty.kind(db) {
        TyKind::Reference { name, .. } => {
            let Some(resolved) = resolve_name(db, scope, name) else {
                return Vec::new();
            };
            let interner = &db.hir_state().interner;
            hir::super_types(db, &resolved)
                .into_iter()
                .map(|fqn| Ty::reference(db, Name::new(interner.resolve(&fqn)), Vec::new()))
                .collect()
        }
        TyKind::Array(_) => vec![
            Ty::reference(db, "java.lang.Object", Vec::new()),
            Ty::reference(db, "java.lang.Cloneable", Vec::new()),
            Ty::reference(db, "java.io.Serializable", Vec::new()),
        ],
        _ => Vec::new(),
    }
}

/// Resolves `name` against the libraries of `scope`, honoring classpath
/// order (the first library containing the name wins).
fn resolve_name(db: &dyn TyDatabase, scope: ScopeId, name: &Name) -> Option<hir::ResolvedClass> {
    let libraries: Vec<hir::LibraryId> = match scope.kind(db) {
        ScopeKind::SourceSet(source_set) => hir::classpath_libraries(db, source_set.clone()),
        ScopeKind::Classpath(libraries) => libraries.clone(),
        ScopeKind::JdkBuiltins => hir::jdk_builtin_libraries(db),
    };
    hir::resolve_in_libraries(db, &libraries, name.as_str())
}

/// Whether `sub` is a subtype of `sup`
/// ([JLS §4.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10)).
pub fn is_subtype(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope<'_>,
    sub: &Ty,
    sup: &Ty,
) -> bool {
    let scope = ScopeId::new(db, ScopeKind::from_scope(scope));
    is_subtype_query(db, scope, sub.id, sup.id)
}

/// Memoized per (scope, sub, sup). See [`is_subtype`].
#[salsa::tracked(returns(clone))]
pub(crate) fn is_subtype_query(
    db: &dyn TyDatabase,
    scope: ScopeId,
    sub: TyData,
    sup: TyData,
) -> bool {
    let sub = Ty { id: sub };
    let sup = Ty { id: sup };
    if sub == sup {
        return true;
    }
    match (sub.kind(db), sup.kind(db)) {
        // §4.10.1: the primitive partial order is identity only (widening is
        // a conversion, not a subtyping).
        (TyKind::Primitive(_), TyKind::Primitive(_)) => false,
        // §4.10.3: S[] <: T[] iff S <: T.
        (TyKind::Array(_), TyKind::Array(_)) => match (sub.element(db), sup.element(db)) {
            (Some(s), Some(t)) => is_subtype_query(db, scope, s.id, t.id),
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
fn reference_subtype(db: &dyn TyDatabase, scope: ScopeId, sub: Ty, sup: Ty) -> bool {
    let (sub_name, sub_args) = sub.as_reference(db).expect("reference kind");
    let (sup_name, sup_args) = sup.as_reference(db).expect("reference kind");
    if sub_name == sup_name {
        // Same erasure: the parameterized types are subtypes only when the
        // type arguments are compatible (§4.10.2: invariant except wildcards).
        return params_ok(db, scope, sub_args, sup_args);
    }

    // Otherwise walk the transitive closure of `sub`'s supertypes (§4.10.2).
    let mut stack = vec![sub];
    let mut visited = FxHashSet::default();
    while let Some(current) = stack.pop() {
        for parent in supertypes_query(db, scope, current.id) {
            if !visited.insert(parent) {
                continue;
            }
            if parent == sup {
                return true;
            }
            if let Some((parent_name, parent_args)) = parent.as_reference(db)
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
fn params_ok(db: &dyn TyDatabase, scope: ScopeId, sub_args: &[Ty], sup_args: &[Ty]) -> bool {
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

fn arg_ok(db: &dyn TyDatabase, scope: ScopeId, sub: &Ty, sup: &Ty) -> bool {
    match sup.kind(db) {
        // `?` accepts any type argument.
        TyKind::Wildcard(None) => true,
        TyKind::Wildcard(Some(bound)) => match bound.kind {
            BoundKind::Upper => is_subtype_query(db, scope, sub.id, bound.ty.id),
            BoundKind::Lower => is_subtype_query(db, scope, bound.ty.id, sub.id),
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
    match (src.kind(db), dst.kind(db)) {
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
