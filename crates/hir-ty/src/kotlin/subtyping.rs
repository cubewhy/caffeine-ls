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

use crate::jvm::db::TyDatabase;
use crate::kotlin::ty::ty_from_java;
use crate::ty::{Ty, TyKind};

/// The direct supertypes of `ty` in `scope`.
pub fn supertypes(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: &Ty) -> Vec<Ty> {
    // A local classifier declares its supertypes in its own item, and the
    // receiver *is* that declaration ([KLS
    // `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)):
    // `object : Runnable { … }` is a `Runnable`, which is what makes it usable
    // in a SAM position.
    if let Some(class) = super::method::local_class_of(db, ty) {
        if hir_def::kotlin::plugin::model(&hir::file_item_tree(db, class.file)).is_some() {
            let binding = class_binding(db, &class, ty);
            return super::db::supertypes(db, class.file, class.item)
                .iter()
                .map(|supertype| supertype.substitute(db, &binding))
                .collect();
        }
        return supertypes_of_another_language(db, scope, ty);
    }
    let Some(fqn) = reference_fqn(db, ty) else {
        return Vec::new();
    };
    let Some(resolved) = hir::fqn_resolve(db, scope, fqn.as_str()) else {
        return Vec::new();
    };
    match &resolved {
        // A Kotlin file's facade is a Java class with no declared supertype but
        // `Object` — `kotlin.Any` from a Kotlin receiver.
        hir::Resolved::Facade { .. } => {
            return vec![Ty::reference(db, "kotlin.Any", Vec::new())];
        }
        hir::Resolved::Source(class) => {
            let tree = hir::file_item_tree(db, class.file);
            let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
                // Another language declares the class: its own type layer owns
                // the supertypes, and [`ty_from_java`] is what makes its types
                // Kotlin's (a classfile receiver becomes a platform type).
                return supertypes_of_another_language(db, scope, ty);
            };
            let _ = tree;
            // A source supertype list is declared over the *class's* type
            // parameters: `class C<T> : List<T>` gives `C<Int> <: List<Int>`
            // ([KLS
            // `type-system.html#type-containment`](https://kotlinlang.org/spec/type-system.html#type-containment)),
            // so the receiver's arguments are substituted into each supertype.
            let binding = class_binding(db, class, ty);
            super::db::supertypes(db, class.file, class.item)
                .iter()
                .map(|supertype| supertype.substitute(db, &binding))
                .collect()
        }
        // A library classifier's supertypes come from its classfile record, as
        // interned binary names, and each is converted to the Kotlin type it
        // denotes.
        hir::Resolved::Library(_) => supertypes_of_another_language(db, scope, ty),
    }
}

/// The supertypes of a type of *another* language (a Java source class, or a
/// classfile), read through the registry: the declaring layer answers in its
/// own types, which Kotlin reads through its JVM codec ([`ty_from_java`]).
fn supertypes_of_another_language(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Vec<Ty> {
    let language = match super::method::local_class_of(db, ty) {
        // A local class of another language: the receiver carries exactly the
        // identity that layer answers for ([JLS §6.7]), so the lookup is the
        // file's own language.
        Some(class) => match crate::lang::for_file(db, class.file) {
            Some(language) => language,
            None => return Vec::new(),
        },
        None => {
            let Some(fqn) = reference_fqn(db, ty) else {
                return Vec::new();
            };
            let Some(resolved) = hir::fqn_resolve(db, scope, fqn.as_str()) else {
                return Vec::new();
            };
            match &resolved {
                // A classfile declares no language: the entry that reads
                // classfiles answers for it.
                hir::Resolved::Library(_) => crate::lang::classfile(),
                class => match crate::lang::for_class(db, class) {
                    Some(language) => language,
                    None => return Vec::new(),
                },
            }
        }
    };
    language
        .supertypes(db, scope, *ty)
        .into_iter()
        .map(|supertype| ty_from_java(db, supertype))
        .collect()
}

/// The binding of a *source* class's declared type parameters to the receiver's
/// arguments ([KLS
/// `type-system.html#type-containment`](https://kotlinlang.org/spec/type-system.html#type-containment)):
/// `class C<T>` used at `C<Int>` substitutes `T → Int` in every supertype it
/// declares. A receiver with no arguments — a raw or non-generic use — binds
/// nothing, exactly as [`Ty::substitute`] leaves an unbound variable alone.
fn class_binding(
    db: &dyn TyDatabase,
    class: &hir::SourceClass,
    ty: &Ty,
) -> rustc_hash::FxHashMap<crate::ty::TypeVarScope, Ty> {
    let TyKind::Reference { args, .. } = ty.kind(db) else {
        return rustc_hash::FxHashMap::default();
    };
    let tree = hir::file_item_tree(db, class.file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return rustc_hash::FxHashMap::default();
    };
    let KotlinItemData::Class(data) = tree.data(class.item) else {
        return rustc_hash::FxHashMap::default();
    };
    data.type_params
        .iter()
        .zip(args.iter().copied())
        .map(|(param, arg)| {
            (
                crate::ty::TypeVarScope::Class {
                    file: class.file,
                    item: class.item,
                    name: param.name.clone(),
                },
                arg,
            )
        })
        .collect()
}

/// Whether `sub` is a subtype of `sup` ([KLS
/// `type-system.html#subtyping`](https://kotlinlang.org/spec/type-system.html#subtyping)).
pub fn is_subtype(db: &dyn TyDatabase, scope: &hir::ResolutionScope, sub: &Ty, sup: &Ty) -> bool {
    if sub == sup {
        return true;
    }
    let sub_kind = sub.kind(db).clone();
    let sup_kind = sup.kind(db).clone();

    // An unresolved name is compatible with everything in both positions: the
    // error type is the analyzer's permissiveness, so a name that could not be
    // resolved does not *also* report a mismatch downstream ([KLS
    // `type-system.html`](https://kotlinlang.org/spec/type-system.html) has no
    // error type — a recorded deviation).
    if matches!(sub_kind, TyKind::Error) || matches!(sup_kind, TyKind::Error) {
        return true;
    }

    // A flexible type `L..U` is a subtype of `S` when its *lower* bound is, on
    // the sub side, and `S` is a subtype of it when `S <: U` on the sup side
    // ([KLS
    // `type-system.html#subtyping-for-flexible-types`](https://kotlinlang.org/spec/type-system.html#subtyping-for-flexible-types)).
    if let TyKind::Flexible { lower, .. } = sub_kind {
        return is_subtype(db, scope, &lower, sup);
    }
    if let TyKind::Flexible { upper, .. } = sup_kind {
        return is_subtype(db, scope, sub, &upper);
    }

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

    // The null type is a subtype of every nullable type and of nothing else
    // ([KLS
    // `type-system.html#subtyping-for-nullable-types`](https://kotlinlang.org/spec/type-system.html#subtyping-for-nullable-types)):
    // `val x: String? = null` is legal, `val x: String = null` is not. The
    // `Nothing?` case follows from the same rule, since `Nothing?` *is*
    // `Nullable(Nothing)`.
    if matches!(sub_kind, TyKind::Null) {
        return sup.is_nullable(db);
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
                local: sub_local,
            },
            TyKind::Reference {
                name: sup_name,
                args: sup_args,
                local: sup_local,
            },
        ) => {
            // The same classifier: its arguments are compared with the
            // *declaration-site* variance of the class's parameters.
            //
            // "The same" is the *mapped* name ([`MAPPED_TYPES`]): the compiler
            // maps the JVM classes onto Kotlin classifiers
            // (<https://kotlinlang.org/docs/java-interop.html#mapped-types>),
            // so `java.util.List` and `kotlin.collections.List` *are* one
            // classifier — a Kotlin file may write either name, and a Java
            // declaration's type carries the Java one. A *local* class is
            // identified by its declaration, not by its name ([JLS §6.7]), so
            // the two must agree on that too.
            let sup_name = super::ty::mapped_type_name(sup_name);
            if sub_local == sup_local && super::ty::mapped_type_name(sub_name) == sup_name {
                let variances = declared_variances(db, scope, sup_name.as_str());
                return arguments_are_subtypes(db, scope, variances.as_deref(), sub_args, sup_args);
            }
            let _ = sup_args;
            // A supertype of the sub-classifier that is the super classifier.
            // Every supertype is walked, whatever shape it has: a *library* or
            // Java supertype arrives as the platform type `T..T?`
            // ([`ty_from_java`]), and the flexible rules above compare it by
            // its halves — a filter for `Reference` here would skip exactly
            // those and make every classpath supertype unreachable.
            supertypes(db, scope, sub)
                .iter()
                .any(|supertype| is_subtype(db, scope, supertype, sup))
        }
        // A type variable is a subtype of each of its upper bounds ([KLS
        // `type-system.html#type-parameters`](https://kotlinlang.org/spec/type-system.html#type-parameters):
        // `T : U` makes `T` a subtype of `U`). An unbounded variable is bounded
        // by `Any?`, which every type satisfies — the `Any?` case is handled
        // before this match.
        (TyKind::TypeVar { bounds, .. }, _) => {
            bounds.iter().any(|bound| is_subtype(db, scope, bound, sup))
        }
        (_, TyKind::TypeVar { bounds, .. }) => {
            bounds.iter().any(|bound| is_subtype(db, scope, sub, bound))
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
/// `fqn` names, in order — from the item tree for a source class, from
/// [`MAPPED_VARIANCE`] for a standard-library classifier the compiler maps onto
/// a JVM type (whose variance lives in `@Metadata`, which the stub layer does
/// not decode), and `None` for any other library class.
pub fn declared_variances(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
) -> Option<Vec<Option<KotlinVariance>>> {
    // The mapped classifiers first: `kotlin.collections.List` has no classfile
    // of its own, so `fqn_resolve` finds nothing for it.
    if let Some(variances) = super::ty::mapped_variances(&Name::new(fqn)) {
        return Some(variances.to_vec());
    }
    let resolved = hir::fqn_resolve(db, scope, fqn)?;
    let hir::Resolved::Source(class) = &resolved else {
        return None;
    };
    let tree = hir::file_item_tree(db, class.file);
    let tree = hir_def::kotlin::plugin::model(&tree)?;
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
    let tree = hir_def::kotlin::plugin::model(&tree)?;
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
    let tree = hir_def::kotlin::plugin::tree(db, class.file)?;
    Some((class.file, tree))
}
