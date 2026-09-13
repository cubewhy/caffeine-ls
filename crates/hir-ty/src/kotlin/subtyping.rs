//! Kotlin subtyping and assignability.
//!
//! The rules are KLS
//! `type-system.html#subtyping`](https://kotlinlang.org/spec/type-system.html#subtyping)
//! and its sections:
//!
//! * *reflexivity*: `T <: T` for every type;
//! * *nullability*: `T <: T?`, and `T? <: U?` exactly when `T <: U`
//!   ([`#subtyping-for-nullable-types`](https://kotlinlang.org/spec/type-system.html#subtyping-for-nullable-types)).
//!   `T?` is *not* a subtype of a non-null `T`, and `Nothing?` — the type of
//!   `null` — is a subtype of every nullable type only;
//! * `kotlin.Any?` is the root: every type is a subtype of it, and no type
//!   is a subtype of `kotlin.Nothing` but itself and `Nothing?`;
//! * *classifier subtyping*: a classifier type is a subtype of its supertypes
//!   with the type arguments compared by the parameter's variance — a
//!   declaration-site `out`/`in` parameter
//!   ([`#declaration-site-variance`](https://kotlinlang.org/spec/type-system.html#declaration-site-variance))
//!   and a use-site projection as an anonymous declaration
//!   ([`#use-site-variance`](https://kotlinlang.org/spec/type-system.html#use-site-variance));
//!   array types are invariant;
//! * a definitely-non-nullable type `T & Any` is a subtype of `T`
//!   ([`#definitely-non-nullable-types`](https://kotlinlang.org/spec/type-system.html#definitely-non-nullable-types)).
//!
//! # Scope
//!
//! The declaration-site variance of a **source** class comes from the item
//! tree. A *library* class's variance is not modelled: the classfile
//! `Signature` attribute encodes `out T` as `+T`, and the stub layer
//! ([`syntax::stub::TypeParameter`]) keeps bounds but not that flag, so a
//! library classifier is compared invariantly — a recorded deviation, and the
//! reason a covariance test declares its own class.

use hir::hir_def::kotlin::item_tree::{KotlinClassKind, KotlinItemData, KotlinItemTree};
use hir::hir_def::kotlin::modifiers::KotlinVariance;
use hir_expand::name::Name;
use vfs::FileId;

use crate::java::db::TyDatabase;
use crate::ty::{Ty, TyKind};

/// The direct supertypes of `ty` in `scope`.
pub fn supertypes(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: &Ty) -> Vec<Ty> {
    let Some(fqn) = reference_fqn(db, ty) else {
        return Vec::new();
    };
    let Some(resolved) = hir::fqn_resolve(db, scope, fqn.as_str()) else {
        return Vec::new();
    };
    match &resolved {
        hir::Resolved::Source(class) => {
            let tree = hir::file_item_tree(db, class.file);
            let Some(tree) = tree.as_kotlin() else {
                // A Java source class: the Java layer owns its supertypes.
                return crate::java::subtyping::supertypes(db, scope, ty);
            };
            let _ = tree;
            super::db::supertypes(db, class.file, class.item)
                .iter()
                .copied()
                .collect()
        }
        // A library classifier's supertypes come from its classfile record, as
        // interned binary names ('the Java layer's own spelling).
        hir::Resolved::Library(_) => hir::super_types(db, &resolved)
            .into_iter()
            .map(|symbol| Name::new(&db.hir_state().interner.resolve(&symbol)))
            .map(|fqn| Ty::reference(db, fqn, Vec::new()))
            .collect(),
    }
}

/// Whether `sub` is a subtype of `sup` ([KLS
/// `type-system.html#subtyping`](https://kotlinlang.org/spec/type-system.html#subtyping)).
pub fn is_subtype(db: &dyn TyDatabase, scope: &hir::ResolutionScope, sub: &Ty, sup: &Ty) -> bool {
    if sub == sup {
        return true;
    }
    let sub_kind = sub.kind(db).clone();
    let sup_kind = sup.kind(db).clone();

    // `T & Any <: T` and `T & Any <: T?`; a definitely-non-nullable type is
    // its inner type for every subtyping question.
    if let TyKind::DefinitelyNonNull(inner) = sub_kind {
        return is_subtype(db, scope, &inner, sup);
    }
    if let TyKind::DefinitelyNonNull(inner) = sup_kind {
        // `T <: U & Any` needs `T <: U` *and* `T` definitely non-null; a
        // non-null `T` satisfies both, a nullable one neither.
        return !sub.is_nullable(db) && is_subtype(db, scope, sub, &inner);
    }

    // A nullable sub- or supertype: Kotlin's `T?` lattice.
    match (&sub_kind, &sup_kind) {
        (TyKind::Nullable(sub_inner), TyKind::Nullable(sup_inner)) => {
            return is_subtype(db, scope, sub_inner, sup_inner);
        }
        (TyKind::Nullable(_), _) => {
            // `T? <: U?` only — a nullable type never matches a non-null one
            // (the `U?` case is the arm above).
            return false;
        }
        (_, TyKind::Nullable(sup_inner)) => {
            // `T <: U?` whenever `T <: U`, and `U? <: U?` is the reflexive case
            // handled above.
            return is_subtype(db, scope, sub, sup_inner);
        }
        _ => {}
    }

    // `kotlin.Any?` is the root; `kotlin.Nothing` is the bottom.
    if let TyKind::Reference { name, .. } = &sup_kind
        && name.as_str() == "kotlin.Any"
    {
        // Every non-null type is a subtype of `Any`; a nullable one only of
        // `Any?` (handled above).
        return true;
    }
    if let TyKind::Reference { name, .. } = &sub_kind
        && name.as_str() == "kotlin.Nothing"
    {
        return true;
    }

    match (&sub_kind, &sup_kind) {
        (
            TyKind::Reference {
                name: sub_name,
                args: sub_args,
                ..
            },
            TyKind::Reference {
                name: sup_name,
                args: sup_args,
                ..
            },
        ) => {
            // The same classifier: its arguments are compared with the
            // *declaration-site* variance of the class's parameters.
            if sub_name == sup_name {
                let variances = declared_variances(db, scope, sup_name.as_str());
                return arguments_are_subtypes(db, scope, variances.as_deref(), sub_args, sup_args);
            }
            let _ = sup_args;
            // A supertype of the sub-classifier that is the super classifier.
            for supertype in supertypes(db, scope, sub) {
                if !matches!(supertype.kind(db), TyKind::Reference { .. }) {
                    continue;
                }
                if is_subtype(db, scope, &supertype, sup) {
                    return true;
                }
            }
            false
        }
        (TyKind::TypeVar { scope: var, .. }, _) | (_, TyKind::TypeVar { scope: var, .. }) => {
            // A type variable is its own type and nothing else: the bounds are
            // consulted by the constraint solver, not here.
            let _ = var;
            false
        }
        (TyKind::Array(sub_inner), TyKind::Array(sup_inner)) => {
            // Kotlin's array types are invariant
            // ([KLS `built-in-types-and-their-semantics.html#built-in-array-types`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-array-types)).
            sub_inner == sup_inner
        }
        (TyKind::Primitive(sub_primitive), TyKind::Primitive(sup_primitive)) => {
            sub_primitive == sup_primitive
        }
        _ => false,
    }
}

/// Whether `sub_args` satisfy `sup_args` positionally, given the variance each
/// parameter declares (`variances`), or invariant when it is `None`.
fn arguments_are_subtypes(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    variances: Option<&[Option<KotlinVariance>]>,
    sub_args: &[Ty],
    sup_args: &[Ty],
) -> bool {
    if sub_args.len() != sup_args.len() {
        return false;
    }
    sub_args
        .iter()
        .zip(sup_args)
        .enumerate()
        .all(|(index, (sub, sup))| {
            let variance = variances.and_then(|variances| variances.get(index).copied().flatten());
            match variance {
                Some(KotlinVariance::Out) => is_subtype(db, scope, sub, sup),
                Some(KotlinVariance::In) => is_subtype(db, scope, sup, sub),
                None => match sup.kind(db).clone() {
                    // A use-site projection is an anonymous declaration: `out
                    // T` covariant, `in T` contravariant, `*` accepts anything.
                    TyKind::Wildcard(Some(bound)) => match &bound.kind {
                        crate::ty::BoundKind::Upper => is_subtype(db, scope, sub, &bound.ty),
                        crate::ty::BoundKind::Lower => is_subtype(db, scope, &bound.ty, sub),
                    },
                    TyKind::Wildcard(None) => true,
                    // An invariant parameter: the arguments must be equal.
                    _ => sub == sup,
                },
            }
        })
}

/// [`is_subtype`] for a test-visible caller, which names it `kotlin_subtype`.
pub fn kotlin_subtype(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    sub: &Ty,
    sup: &Ty,
) -> bool {
    is_subtype(db, scope, sub, sup)
}

/// Whether a value of type `src` can be assigned where `dst` is expected
/// ([KLS
/// `type-system.html#subtyping`](https://kotlinlang.org/spec/type-system.html#subtyping):
/// Kotlin's assignment compatibility is the subtype relation).
pub fn is_assignable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    src: &Ty,
    dst: &Ty,
) -> bool {
    is_subtype(db, scope, src, dst)
}

/// The canonical name of a reference type, for a classpath lookup.
fn reference_fqn(db: &dyn TyDatabase, ty: &Ty) -> Option<Name> {
    match ty.kind(db) {
        TyKind::Reference { name, .. } => Some(name.clone()),
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => reference_fqn(db, inner),
        _ => None,
    }
}

/// The declaration-site variances of the type parameters of the classifier
/// `fqn` names, in order — from the item tree for a source class, and `None`
/// for a library one (see the module docs).
pub fn declared_variances(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
) -> Option<Vec<Option<KotlinVariance>>> {
    let resolved = hir::fqn_resolve(db, scope, fqn)?;
    let hir::Resolved::Source(class) = &resolved else {
        return None;
    };
    let tree = hir::file_item_tree(db, class.file);
    let tree = tree.as_kotlin()?;
    let KotlinItemData::Class(data) = tree.data(class.item) else {
        return None;
    };
    Some(
        data.type_params
            .iter()
            .map(|param| param.variance)
            .collect(),
    )
}

/// Whether the classifier `fqn` names is an interface or an annotation class.
pub fn is_interface_like(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
) -> Option<bool> {
    let resolved = hir::fqn_resolve(db, scope, fqn)?;
    let hir::Resolved::Source(class) = &resolved else {
        return None;
    };
    let tree = hir::file_item_tree(db, class.file);
    let tree = tree.as_kotlin()?;
    match tree.data(class.item) {
        KotlinItemData::Class(data) => Some(matches!(
            data.kind,
            KotlinClassKind::Interface | KotlinClassKind::Annotation
        )),
        _ => None,
    }
}

/// The Kotlin item tree of a source class named by `fqn`, with its file.
pub fn source_class<'a>(
    db: &'a dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
) -> Option<(FileId, triomphe::Arc<KotlinItemTree>)> {
    let resolved = hir::fqn_resolve(db, scope, fqn)?;
    let hir::Resolved::Source(class) = &resolved else {
        return None;
    };
    let tree = hir::file_item_tree(db, class.file);
    let tree = tree.as_kotlin()?.clone();
    Some((class.file, tree))
}
