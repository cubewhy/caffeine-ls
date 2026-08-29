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
use syntax::stub::{PrimitiveType, TypeRef};

use hir_def::java::item_tree::{ItemData, TypeParam};
use hir_def::jvm::access::JvmAccessFlags;
use hir_expand::{name::Name, span::SpannedTypeRef};

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
            // §4.10.2: the direct supertypes of an interface type include
            // `java.lang.Object` — an interface has no superclass to carry
            // it transitively, so it is added explicitly below.
            let object = Ty::reference(db, "java.lang.Object", Vec::new());
            match resolved {
                hir::Resolved::Library(resolved) => {
                    let is_interface = hir::class_record(db, &resolved)
                        .map(|record| match &*record {
                            hir::ClassOrModuleRecord::Class(class) => {
                                JvmAccessFlags::from_bits_retain(class.flags).is_interface()
                            }
                            hir::ClassOrModuleRecord::Module(_) => false,
                        })
                        .unwrap_or(false);
                    let mut out = if let Some(info) =
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
                    };
                    if is_interface && !out.contains(&object) {
                        out.push(object);
                    }
                    out
                }
                hir::Resolved::Source(source) => {
                    let mut out = source_supertypes(db, source, args);
                    if is_source_interface(db, source) && !out.contains(&object) {
                        out.push(object);
                    }
                    out
                }
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

/// Whether the source class-like declaration is an `interface` (or
/// `@interface`): its supertype set gains `java.lang.Object` explicitly
/// ([JLS §4.10.2], [§9.1]).
fn is_source_interface(db: &dyn TyDatabase, source: hir::SourceClass) -> bool {
    let tree = hir::file_item_tree(db, source.file);
    matches!(
        item_data(&tree, source.item),
        Some(ItemData::Interface(_)) | Some(ItemData::Annotation(_))
    )
}

/// The direct supertypes of a source class, resolved against its own file's
/// scope ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)).
/// When the class declares type parameters and `args` are present, the
/// parameters are bound to the arguments and substituted into the
/// superclass/interfaces; otherwise they are resolved raw. Enums, records and
/// annotations gain their implicit superclass (`java.lang.Enum`,
/// `java.lang.Record`, `java.lang.annotation.Annotation`).
pub(crate) fn source_supertypes(
    db: &dyn TyDatabase,
    source: hir::SourceClass,
    args: &[Ty],
) -> Vec<Ty> {
    let tree = hir::file_item_tree(db, source.file);
    let Some(data) = item_data(&tree, source.item) else {
        return Vec::new();
    };
    let scope = scope_for_file(db, source.file);
    let type_params = crate::db::type_params_map_query(db, db.file_text(source.file));
    let resolver = Resolver::new(&tree, type_params, source.item);
    let implicit = |fqn: &str| {
        SpannedTypeRef::synthetic(TypeRef::Reference {
            name: Name::new(fqn),
            generic_args: Vec::new(),
        })
    };
    let (super_class, interfaces): (Option<SpannedTypeRef>, Vec<SpannedTypeRef>) = match data {
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
    let declared: &[TypeParam] = match data {
        ItemData::Class(d) | ItemData::Interface(d) => &d.type_params,
        ItemData::Record(d) => &d.type_params,
        _ => &[],
    };
    let instantiate = |tyref: &SpannedTypeRef| {
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

/// The declared constant names of the named type, when it is an `enum`
/// ([JLS §8.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9)):
/// a source enum lists its `EnumConstant` items, a library enum its
/// ACC_ENUM-flagged fields ([JVMS §4.6]). A non-enum or unresolvable type
/// yields `None`.
pub(crate) fn enum_constants(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<Vec<Name>> {
    let (name, _) = ty.as_reference(db)?;
    match resolve_name(db, scope, name)? {
        hir::Resolved::Library(library) => {
            let record = hir::class_record(db, &library)?;
            let syntax::stub::ClassOrModuleStub::Class(class) = record.as_ref() else {
                return None;
            };
            // JVMS §4.6: enum constants carry the ACC_ENUM flag.
            if !JvmAccessFlags::from_bits_retain(class.flags).is_enum() {
                return None;
            }
            let interner = &db.hir_state().interner;
            Some(
                class
                    .fields
                    .iter()
                    .filter(|field| JvmAccessFlags::from_bits_retain(field.flags).is_enum())
                    .map(|field| Name::new(interner.resolve(&field.name)))
                    .collect(),
            )
        }
        hir::Resolved::Source(source) => {
            let tree = hir::file_item_tree(db, source.file);
            let ItemData::Enum(data) = item_data(&tree, source.item)? else {
                return None;
            };
            Some(
                data.body
                    .iter()
                    .filter_map(|&item| match item_data(&tree, item) {
                        Some(ItemData::EnumConstant(constant)) => Some(constant.name.clone()),
                        _ => None,
                    })
                    .collect(),
            )
        }
    }
}

/// Whether the named reference type is a *class-like* type (`class`, `enum`
/// or `record`, as opposed to an interface or annotation) and whether it is
/// `final` ([JLS §8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1),
/// [§8.1.1.2], [§8.9], [§9.1]). Unresolvable names yield `None`; callers must
/// stay permissive there.
pub(crate) fn class_like_and_final(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<(bool, bool)> {
    let (name, _) = ty.as_reference(db)?;
    let resolved = resolve_name(db, scope, name)?;
    match resolved {
        hir::Resolved::Library(library) => {
            let record = hir::class_record(db, &library)?;
            let syntax::stub::ClassOrModuleStub::Class(class) = record.as_ref() else {
                return None;
            };
            // JVM access flags ([JVMS §4.1]): an interface carries ACC_INTERFACE, a
            // final class ACC_FINAL. A record is implicitly final ([§8.10]).
            let flags = JvmAccessFlags::from_bits_retain(class.flags);
            let interface = flags.is_interface();
            let final_ = flags.is_final() || class.is_record;
            Some((!interface, !interface && final_))
        }
        hir::Resolved::Source(source) => {
            let tree = hir::file_item_tree(db, source.file);
            match item_data(&tree, source.item)? {
                ItemData::Class(d) => Some((true, d.modifiers.is_final())),
                ItemData::Record(_) => Some((true, true)),
                // §8.9: an enum without constant bodies is implicitly final,
                // but treating every enum as final only ever tightens a cast
                // check that single inheritance already makes disjoint.
                ItemData::Enum(_) => Some((true, true)),
                _ => Some((false, false)),
            }
        }
    }
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
        // §4.10.2/§4.10.3: the null type is a subtype of every reference type
        // (class, interface, type variable, array and intersection).
        (TyKind::Null, TyKind::Reference { .. } | TyKind::Array(_) | TyKind::TypeVar { .. }) => {
            true
        }
        (TyKind::Null, TyKind::Intersection(members)) => members
            .iter()
            .all(|member| is_subtype_query(db, scope, sub.id, member.id)),
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
        ) => {
            is_subtype(db, scope, src, dst)
                // §5.1.9/§5.2: a raw type converts to any parameterization of
                // its own generic class by *unchecked conversion* — legal in
                // assignment context, reported as a warning by the type layer.
                || unchecked_conversion(db, scope, src, dst)
        }
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
        // §4.10.3/§5.2: an array converts to its class supertypes —
        // `Object`, `Cloneable`, `Serializable` — and through them to any
        // reference type by subtyping.
        (TyKind::Array(_), TyKind::Reference { .. } | TyKind::TypeVar { .. }) => {
            is_subtype(db, scope, src, dst)
        }
        // §5.1.8 with §5.1.10: an expression typed by a *captured* type
        // variable with a lower bound `L` (the capture of `? super L`) holds
        // an `L`, which unboxes to its primitive.
        (
            TyKind::TypeVar {
                lower: Some(lower), ..
            },
            TyKind::Primitive(dst),
        ) => match lower.kind(db) {
            TyKind::Reference { name, .. } => {
                let Some(unboxed) = unboxed_primitive(name.as_str()) else {
                    return false;
                };
                unboxed == *dst || widening_primitive(unboxed, *dst)
            }
            _ => false,
        },
        // §5.2/§4.10.2: a type-variable-typed expression converts by the
        // reference conversions of its declared bounds, and a target that is
        // itself a type variable admits sources below its lower bound (the
        // §5.1.10 capture of `? super L`). Both are exactly the subtyping
        // judgments, so delegate; a primitive source boxes first (§5.1.7).
        (TyKind::Primitive(src), TyKind::TypeVar { .. }) => {
            let boxed = Ty::reference(db, boxed_type(*src), Vec::new());
            is_subtype(db, scope, &boxed, dst)
        }
        (TyKind::TypeVar { .. }, _)
        | (
            TyKind::Reference { .. } | TyKind::Intersection(_) | TyKind::Null,
            TyKind::TypeVar { .. },
        ) => is_subtype(db, scope, src, dst),
        // §5.1.4/§5.1.5: the null literal is assignable to any reference type.
        (TyKind::Null, _) => is_subtype(db, scope, src, dst),
        _ => false,
    }
}

/// §5.1.9: whether `src` is a *raw type* — a parameterized class used without
/// its type arguments — whose erasure has `dst`'s erasure among its
/// supertypes. The conversion is unchecked ([§5.2]): legal, but carrying no
/// element-type guarantee.
fn unchecked_conversion(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    src: &Ty,
    dst: &Ty,
) -> bool {
    let Some((src_name, src_args)) = src.as_reference(db) else {
        return false;
    };
    if !src_args.is_empty() {
        return false;
    }
    let Some((dst_name, _)) = dst.as_reference(db) else {
        return false;
    };
    // §5.1.9: the destination may be a parameterization of the raw type's own
    // generic class — `raw List` converts to `List<String>` directly.
    if src_name == dst_name {
        return true;
    }
    let raw_src = Ty::reference(db, src_name.clone(), Vec::new());
    let scope = ScopeId::new(db, ScopeKind::from_scope(scope));
    let mut stack = vec![raw_src];
    let mut visited = FxHashSet::default();
    while let Some(current) = stack.pop() {
        for parent in supertypes_query(db, scope, current.id) {
            if !visited.insert(parent.id) {
                continue;
            }
            match parent.as_reference(db) {
                // The erased supertype matches the destination's erasure:
                // every instantiation of it is reachable from the raw source.
                Some((parent_name, _)) if parent_name == dst_name => return true,
                Some(_) => stack.push(parent),
                None => {}
            }
        }
    }
    false
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

/// Whether a constant value is representable in the target primitive
/// ([JLS §5.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.3)):
/// the narrowing-of-constants half of the assignment-context conversion
/// ([§5.2]) for `int` constants to `byte`, `short` and `char`.
pub(crate) fn fits_primitive(value: i64, dst: PrimitiveType) -> bool {
    use PrimitiveType::*;
    match dst {
        Byte => (i8::MIN as i64..=i8::MAX as i64).contains(&value),
        Short => (i16::MIN as i64..=i16::MAX as i64).contains(&value),
        Char => (0..=u16::MAX as i64).contains(&value),
        _ => false,
    }
}
