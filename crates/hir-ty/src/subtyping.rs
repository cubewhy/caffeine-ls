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
//! [§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8))
//! and capture conversion
//! ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10),
//! via the §4.5.1 contains relation) are modelled for assignment conversion
//! and parameterized-type arguments.

use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::{PrimitiveType, TypeParameter, TypeRef};

use hir_expand::{item_tree::ItemData, name::Name};

use crate::{
    db::{ScopeId, ScopeKind, TyDatabase},
    resolve::{Resolver, item_data, resolve_type_ref, scope_for_file},
    ty::{BoundKind, Ty, TyData, TyKind, WildcardBound, boxed_type, unboxed_primitive},
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
pub fn supertypes(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: &Ty) -> Vec<Ty> {
    let scope = ScopeId::new(db, ScopeKind::from_scope(scope));
    supertypes_query(db, scope, ty.id)
}

/// Memoized per (scope, type). See [`supertypes`].
#[salsa::tracked(returns(clone))]
pub(crate) fn supertypes_query(db: &dyn TyDatabase, scope: ScopeId, ty: TyData) -> Vec<Ty> {
    supertypes_impl(db, &scope.kind(db).to_scope(), &Ty { id: ty })
}

/// The non-memoized form of [`supertypes`], usable with types that carry
/// inference variables ([`TyKind::InferenceVar`], JLS §18.1.1) — such types
/// must never reach the memoized tracked query, and the invocation type
/// inference ([§18.5.2]) needs the direct supertype walk of a parameterized
/// class whose type arguments are still being solved.
pub(crate) fn supertypes_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Vec<Ty> {
    match ty.kind(db) {
        TyKind::Reference { name, args } => {
            let Some(resolved) = resolve_name(db, scope, name) else {
                return Vec::new();
            };
            match resolved {
                hir::Resolved::Library(resolved) => {
                    if let Some(info) =
                        hir::class_generic_info(db, &hir::Resolved::Library(resolved.clone()))
                    {
                        if info.type_params.is_empty() || !args.is_empty() {
                            // §4.10.2 with type-argument substitution: bind the class's
                            // declared type parameters to the actual arguments and
                            // substitute into the superclass/interfaces from the
                            // classfile `Signature` attribute. A non-generic class
                            // (e.g. `String`, whose parents are `Comparable<String>`)
                            // carries fully-ground parent types.
                            let interner = &db.hir_state().interner;
                            let binding: FxHashMap<Name, Ty> = info
                                .type_params
                                .iter()
                                .map(|tp| Name::new(interner.resolve(&tp.name)))
                                .zip(args.iter().copied())
                                .collect();
                            let instantiate = |tyref: &hir::TypeRef<hir::Symbol>| {
                                crate::resolve::ty_from_library(db, tyref).substitute(db, &binding)
                            };
                            let mut out = Vec::new();
                            if let Some(super_class) = &info.super_class {
                                out.push(instantiate(super_class));
                            }
                            out.extend(info.interfaces.iter().map(instantiate));
                            out
                        } else {
                            // Erasure-style fallback (raw types per §4.8, or tier-2 data
                            // unavailable): tier-1 classfile data carries no arguments.
                            let interner = &db.hir_state().interner;
                            hir::super_types(db, &hir::Resolved::Library(resolved))
                                .into_iter()
                                .map(|fqn| {
                                    Ty::reference(db, Name::new(interner.resolve(&fqn)), Vec::new())
                                })
                                .collect()
                        }
                    } else {
                        // Erasure-style fallback (raw types per §4.8, or tier-2 data
                        // unavailable): tier-1 classfile data carries no arguments.
                        let interner = &db.hir_state().interner;
                        hir::super_types(db, &hir::Resolved::Library(resolved))
                            .into_iter()
                            .map(|fqn| {
                                Ty::reference(db, Name::new(interner.resolve(&fqn)), Vec::new())
                            })
                            .collect()
                    }
                }
                hir::Resolved::Source(source) => source_supertypes(db, source, args),
            }
        }
        TyKind::Array(_) => vec![
            Ty::reference(db, "java.lang.Object", Vec::new()),
            Ty::reference(db, "java.lang.Cloneable", Vec::new()),
            Ty::reference(db, "java.io.Serializable", Vec::new()),
        ],
        _ => Vec::new(),
    }
}

/// The direct supertypes of a source class, resolved against its own file's
/// scope ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)).
/// When the class declares type parameters and `args` are present, the
/// parameters are bound to the arguments and substituted into the
/// superclass/interfaces; otherwise they are resolved raw. Enums, records and
/// annotations gain their implicit superclass (`java.lang.Enum`,
/// `java.lang.Record`, `java.lang.annotation.Annotation`).
fn source_supertypes(db: &dyn TyDatabase, source: hir::SourceClass, args: &[Ty]) -> Vec<Ty> {
    let tree = hir::file_item_tree(db, source.file);
    let Some(data) = item_data(&tree, source.item) else {
        return Vec::new();
    };
    let scope = scope_for_file(db, source.file);
    let type_params = crate::db::type_params_map_query(db, db.file_text(source.file));
    let resolver = Resolver::new(&tree, type_params, source.item);
    let implicit = |fqn: &str| TypeRef::Reference {
        name: Name::new(fqn),
        generic_args: Vec::new(),
    };
    let (super_class, interfaces): (Option<TypeRef<Name>>, Vec<TypeRef<Name>>) = match data {
        // A class (other than `java.lang.Object`) has exactly one direct
        // superclass; the implicit one is `java.lang.Object` (§8.1.4).
        ItemData::Class(d) => (
            Some(
                d.super_class
                    .clone()
                    .unwrap_or_else(|| implicit("java.lang.Object")),
            ),
            d.interfaces.clone(),
        ),
        ItemData::Interface(d) => (None, d.interfaces.clone()),
        ItemData::Record(d) => (Some(implicit("java.lang.Record")), d.interfaces.clone()),
        ItemData::Enum(d) => (Some(implicit("java.lang.Enum")), d.interfaces.clone()),
        ItemData::Annotation(_) => (
            Some(implicit("java.lang.annotation.Annotation")),
            Vec::new(),
        ),
        _ => return Vec::new(),
    };
    let declared: &[TypeParameter<Name>] = match data {
        ItemData::Class(d) | ItemData::Interface(d) => &d.type_params,
        ItemData::Record(d) => &d.type_params,
        _ => &[],
    };
    let instantiate = |tyref: &TypeRef<Name>| {
        let resolved = resolve_type_ref(db, &scope, &resolver, tyref);
        if args.is_empty() {
            resolved
        } else {
            let binding: FxHashMap<Name, Ty> = declared
                .iter()
                .map(|tp| tp.name.clone())
                .zip(args.iter().copied())
                .collect();
            resolved.substitute(db, &binding)
        }
    };
    let mut out = Vec::new();
    if let Some(super_class) = &super_class {
        out.push(instantiate(super_class));
    }
    out.extend(interfaces.iter().map(instantiate));
    out
}

/// Resolves `name` against the classes of `scope`, honoring classpath order:
/// a source set's own classes, then its classpath entries.
fn resolve_name(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    name: &Name,
) -> Option<hir::Resolved> {
    hir::fqn_resolve(db, scope, name.as_str())
}

/// Whether `sub` is a subtype of `sup`
/// ([JLS §4.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10)).
pub fn is_subtype(db: &dyn TyDatabase, scope: &hir::ResolutionScope, sub: &Ty, sup: &Ty) -> bool {
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
        // §4.10.2: a type variable is a subtype of its declared bounds.
        (TyKind::TypeVar { .. }, TyKind::Reference { .. })
        | (TyKind::TypeVar { .. }, TyKind::TypeVar { .. }) => type_var_subtype(db, scope, sub, sup),
        // §4.10.2 with §5.1.10: a type variable with a lower bound `L` ranges
        // over `L <: X`, so `S <: CAP` is provable from `S <: L`.
        (
            _,
            TyKind::TypeVar {
                lower: Some(lower), ..
            },
        ) => is_subtype_query(db, scope, sub.id, lower.id),
        (TyKind::Reference { .. }, TyKind::Reference { .. }) => {
            reference_subtype(db, scope, sub, sup)
        }
        // §4.10.2: `S <: A & B` iff `S <: A` and `S <: B`; `A & B <: T` iff
        // some member is a subtype of `T` (§4.9). Equal intersections are
        // already handled by the identity check above.
        (TyKind::Intersection(members), _) => members
            .iter()
            .any(|member| is_subtype_query(db, scope, member.id, sup.id)),
        (_, TyKind::Intersection(members)) => members
            .iter()
            .all(|member| is_subtype_query(db, scope, sub.id, member.id)),
        _ => false,
    }
}

/// Whether the type variable `sub` is a subtype of `sup`
/// ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)):
/// every type variable is a subtype of `Object`, and of whatever its declared
/// bounds ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
/// are subtypes of, walking the bound chain (`<T extends U, U extends Number>`
/// gives `T <: Number`).
fn type_var_subtype(db: &dyn TyDatabase, scope: ScopeId, sub: Ty, sup: Ty) -> bool {
    if sup.is_object(db) {
        return true;
    }
    let mut visited = FxHashSet::default();
    let mut stack = vec![sub];
    while let Some(current) = stack.pop() {
        let TyKind::TypeVar { name, bounds, .. } = current.kind(db) else {
            continue;
        };
        if !visited.insert(name) {
            continue;
        }
        for bound in bounds {
            if bound == &sup {
                return true;
            }
            match bound.kind(db) {
                // A reference bound reaches `sup` through its supertype closure.
                TyKind::Reference { .. } => {
                    if is_subtype_query(db, scope, bound.id, sup.id) {
                        return true;
                    }
                }
                // A type-var bound keeps unwinding the bound chain.
                TyKind::TypeVar { .. } => stack.push(*bound),
                _ => {}
            }
        }
    }
    false
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
    match (sub.kind(db), sup.kind(db)) {
        // `?` accepts any type argument (§4.5.1: `? extends T <= ?` and
        // `? super T <= ?`).
        (_, TyKind::Wildcard(None)) => true,
        // A wildcard against a bounded wildcard: the contains relation
        // (§4.5.1) drives parameterized subtyping between wildcard arguments
        // (§4.10.2).
        (TyKind::Wildcard(Some(sub_bound)), TyKind::Wildcard(Some(sup_bound))) => {
            wildcard_contains(db, scope, sub_bound, sup_bound)
        }
        // A wildcard source against a concrete parameter is never a subtype:
        // after capture conversion (§5.1.10) the captured type variable is
        // distinct from the concrete argument (§4.10.2 invariance), so
        // `List<? extends A> <: List<B>` does not hold.
        (TyKind::Wildcard(_), _) => false,
        // A concrete argument against a bounded wildcard parameter (§4.5.1):
        // `T <= ? extends S` iff `T <: S`; `T <= ? super S` iff `S <: T`.
        (_, TyKind::Wildcard(Some(bound))) => match bound.kind {
            BoundKind::Upper => is_subtype_query(db, scope, sub.id, bound.ty.id),
            BoundKind::Lower => is_subtype_query(db, scope, bound.ty.id, sub.id),
        },
        // Invariance (§4.10.2): `G<T> <: G<T'>` iff `T = T'`.
        _ => sub == sup,
    }
}

/// The wildcard "contains" relation
/// ([JLS §4.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5.1)),
/// as sets of types: `? extends A` denotes `{ X : X <: A }` and `? super A`
/// denotes `{ X : A <: X }`, and the source argument must be contained in the
/// parameter argument. The only mixed-upper/lower rule is
/// `? super T <= ? extends Object`, so `(Upper, Lower)` never holds and
/// `(Lower, Upper)` holds only when the upper bound is `Object`.
fn wildcard_contains(
    db: &dyn TyDatabase,
    scope: ScopeId,
    sub: &WildcardBound,
    sup: &WildcardBound,
) -> bool {
    match (sub.kind, sup.kind) {
        // `? extends A <= ? extends B` iff `A <: B`.
        (BoundKind::Upper, BoundKind::Upper) => is_subtype_query(db, scope, sub.ty.id, sup.ty.id),
        // `? super A <= ? super B` iff `B <: A`.
        (BoundKind::Lower, BoundKind::Lower) => is_subtype_query(db, scope, sup.ty.id, sub.ty.id),
        // `? extends A <= ? super B` is never provable (§4.5.1).
        (BoundKind::Upper, BoundKind::Lower) => false,
        // `? super A <= ? extends B` iff `B` is `Object` (§4.5.1).
        (BoundKind::Lower, BoundKind::Upper) => sup.ty.is_object(db),
    }
}

/// Whether `src` is assignable to `dst` by assignment conversion
/// ([JLS §5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2)):
/// identity, primitive widening
/// ([§5.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.2)),
/// reference widening
/// ([§5.1.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.5)),
/// boxing ([§5.1.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.7),
/// optionally followed by a widening reference conversion) and unboxing
/// ([§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8),
/// optionally followed by a widening primitive conversion).
pub fn is_assignable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    src: &Ty,
    dst: &Ty,
) -> bool {
    if src == dst {
        return true;
    }
    match (src.kind(db), dst.kind(db)) {
        (TyKind::Primitive(src), TyKind::Primitive(dst)) => widening_primitive(*src, *dst),
        (
            TyKind::Reference { .. } | TyKind::Intersection(_),
            TyKind::Reference { .. } | TyKind::Intersection(_),
        ) => is_subtype(db, scope, src, dst),
        // Boxing (§5.1.7): a primitive is assignable to its boxed type, or to
        // any reference supertype of it (a widening reference conversion §5.1.5
        // after boxing).
        (TyKind::Primitive(src), TyKind::Reference { .. }) => {
            let boxed = Ty::reference(db, boxed_type(*src), Vec::new());
            is_subtype(db, scope, &boxed, dst)
        }
        // Unboxing (§5.1.8): a boxed reference is assignable to the primitive
        // it unboxes to, or to a wider primitive (a widening primitive
        // conversion §5.1.2 after unboxing).
        (TyKind::Reference { name, .. }, TyKind::Primitive(dst)) => {
            let Some(unboxed) = unboxed_primitive(name.as_str()) else {
                return false;
            };
            unboxed == *dst || widening_primitive(unboxed, *dst)
        }
        _ => false,
    }
}

/// Whether `arg` is convertible to `param` by a strict invocation conversion
/// ([JLS §5.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.3)):
/// identity, widening primitive
/// ([§5.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.2))
/// or widening reference
/// ([§5.1.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.5)).
/// Unlike assignment conversion there is no boxing or unboxing.
pub(crate) fn strict_conversion(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    arg: &Ty,
    param: &Ty,
) -> bool {
    if arg == param {
        return true;
    }
    match (arg.kind(db), param.kind(db)) {
        (TyKind::Primitive(src), TyKind::Primitive(dst)) => widening_primitive(*src, *dst),
        _ => is_subtype(db, scope, arg, param),
    }
}

/// Primitive widening conversion, the transitions of
/// [JLS table 5.1-B](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.2).
pub(crate) fn widening_primitive(src: PrimitiveType, dst: PrimitiveType) -> bool {
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
