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
        // A built-in Kotlin classifier has no classfile: its supertypes are the
        // mapped JVM type's ([`super::builtins`]), converted back to the Kotlin
        // names the mapping table pairs them with — `kotlin.collections.List` is
        // a `kotlin.collections.Collection` because `java.util.List` is a
        // `java.util.Collection`.
        if let Some(jvm) = super::builtins::jvm_ty(db, *ty) {
            // The JVM type's own hierarchy, plus the supertypes Kotlin's
            // declaration adds where the JVM cannot tell two classifiers apart
            // ([`super::builtins::declared_supertypes`]: `MutableList` and
            // `List` share `java.util.List`).
            let mut supertypes = supertypes_of_another_language(db, scope, &jvm);
            // The declared supertype takes the receiver's own arguments: every
            // pair in the table maps its arguments one-to-one (`ArrayList<E>`
            // is a `MutableList<E>`, `HashMap<K, V>` a `MutableMap<K, V>`).
            let args: Vec<Ty> = match ty.kind(db) {
                TyKind::Reference { args, .. } => args.to_vec(),
                _ => Vec::new(),
            };
            supertypes.extend(
                super::builtins::declared_supertypes(fqn.as_str())
                    .into_iter()
                    .map(|name| Ty::reference(db, name, args.clone())),
            );
            return supertypes;
        }
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

/// [`class_binding`] for a caller that has the classifier's file and item rather
/// than a [`hir::SourceClass`] — the member walk, which substitutes the
/// receiver's arguments into the members it is about to answer with. Public to
/// this crate so the two stay one rule.
pub(crate) fn class_binding_of(
    db: &dyn TyDatabase,
    file: FileId,
    item: hir_expand::ids::ItemId,
    ty: &Ty,
) -> rustc_hash::FxHashMap<crate::ty::TypeVarScope, Ty> {
    let TyKind::Reference { args, .. } = ty.kind(db) else {
        return rustc_hash::FxHashMap::default();
    };
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return rustc_hash::FxHashMap::default();
    };
    let KotlinItemData::Class(data) = tree.data(item) else {
        return rustc_hash::FxHashMap::default();
    };
    data.type_params
        .iter()
        .zip(args.iter().copied())
        .map(|(param, arg)| {
            (
                crate::ty::TypeVarScope::Class {
                    file,
                    item,
                    name: param.name.clone(),
                },
                arg,
            )
        })
        .collect()
}

/// The least upper bound of `a` and `b` — the type a branch join has
/// ([KLS
/// `type-system.html#subtyping`](https://kotlinlang.org/spec/type-system.html#subtyping)
/// makes the join the least common supertype of the branches, which kotlinc
/// words as `if`/`when`/`try` "common supertype"):
///
/// * an identical pair is itself;
/// * `kotlin.Nothing` — the type of `throw` and of a branch that never returns —
///   is the bottom, so the join is the other side;
/// * the error type absorbs, exactly as it does in [`is_subtype`];
/// * a subtype relation answers the supertype, and [`supertypes`]'s transitive
///   closure answers it when neither side is one;
/// * `kotlin.Any` remains as the last resort.
///
/// Nullability joins like any other attribute — `lub(Int?, Int)` is `Int?`
/// ([KLS
/// `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types)) —
/// so the join is taken over the nullability-stripped operands and the result
/// made nullable when either side was.
pub fn lub(db: &dyn TyDatabase, scope: &hir::ResolutionScope, a: &Ty, b: &Ty) -> Ty {
    // `null`'s type is `Nothing?`, the bottom of the nullable half of the lattice
    // ([KLS
    // `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types)):
    // the join with it is the other side, *nullable* — which is what
    // `if (c) x else null` is, and the type a `T?` return accepts.
    if matches!(a.kind(db), TyKind::Null) {
        return Ty::nullable(db, *b);
    }
    if matches!(b.kind(db), TyKind::Null) {
        return Ty::nullable(db, *a);
    }
    let nullable = a.is_nullable(db) || b.is_nullable(db);
    let plain = lub_non_nullable(
        db,
        scope,
        &a.strip_nullability(db),
        &b.strip_nullability(db),
    );
    match nullable {
        true => Ty::nullable(db, plain),
        false => plain,
    }
}

/// [`lub`] over two non-null types: the join's nullability is its caller's.
fn lub_non_nullable(db: &dyn TyDatabase, scope: &hir::ResolutionScope, a: &Ty, b: &Ty) -> Ty {
    if a == b {
        return *a;
    }
    if matches!(a.kind(db), TyKind::Error) {
        return *b;
    }
    if matches!(b.kind(db), TyKind::Error) {
        return *a;
    }
    let nothing = |ty: &Ty| matches!(ty.kind(db), TyKind::Reference { name, .. } if name.as_str() == "kotlin.Nothing");
    if nothing(a) {
        return *b;
    }
    if nothing(b) {
        return *a;
    }
    if is_subtype(db, scope, a, b) {
        return *b;
    }
    if is_subtype(db, scope, b, a) {
        return *a;
    }
    // Two instantiations of one classifier join to that classifier with the
    // arguments joined: the common supertype of `Class<Boolean>` and `Class<T>`
    // is a `Class<…>`, and the supertype walk below would answer the first
    // *interface* both satisfy (`Constable`) instead, losing the argument every
    // later member lookup needs — `when (value) { is Boolean ->
    // Boolean::class.java; else -> T::class.java }` is what
    // `getDeclaredMethod(name, clazz)` is called with.
    if let (
        TyKind::Reference {
            name: a_name,
            args: a_args,
            local: a_local,
        },
        TyKind::Reference {
            name: b_name,
            args: b_args,
            local: b_local,
        },
    ) = (a.kind(db), b.kind(db))
        && a_local == b_local
        && !a_args.is_empty()
        && a_args.len() == b_args.len()
        && super::ty::mapped_type_name(a_name) == super::ty::mapped_type_name(b_name)
    {
        let args = a_args
            .iter()
            .zip(b_args)
            .map(|(a, b)| lub_non_nullable(db, scope, a, b))
            .collect();
        return match a_local {
            Some(class) => Ty::local_reference(db, class.clone(), a_name.clone(), args),
            None => Ty::reference(db, a_name.clone(), args),
        };
    }
    // The first supertype of `a`'s closure that `b` satisfies — the least common
    // supertype of the two when the relation can answer it.
    let mut frontier = supertypes(db, scope, a);
    let mut seen = rustc_hash::FxHashSet::default();
    while let Some(supertype) = frontier.pop() {
        if !seen.insert(supertype) {
            continue;
        }
        if is_subtype(db, scope, b, &supertype) {
            return supertype;
        }
        frontier.extend(supertypes(db, scope, &supertype));
    }
    Ty::reference(db, "kotlin.Any", Vec::new())
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

    // A *type variable* on the subtype side — or inside one of its type
    // arguments — is one the model leaves unbound: Kotlin infers a call's type
    // arguments from the call and from the *expected type*
    // ([KLS
    // `type-inference.html#call-with-an-expected-type`](https://kotlinlang.org/spec/type-inference.html#call-with-an-expected-type)),
    // so `mutableListOf<File>()` written with no argument, `emptyMap()`'s `K`
    // and `V`, `withContext(…)`'s `T` and `let { … }`'s `R` are the *callee's*
    // variables. The caller cannot violate a type nobody determined, so the
    // variable compares as unknown — the same permissiveness the error type
    // above gets, and never a false `type mismatch` for an inference this model
    // does not perform.
    if mentions_unbound_var(db, *sub) {
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
        // A written `Array<X>` is the classifier the JVM spells `X[]`
        // (<https://kotlinlang.org/docs/java-interop.html#mapped-types>): the two
        // spellings are one type, and either side stands for the other. The
        // comparison is the array-to-array one, so the invariance of a Kotlin
        // array is what a mixed pair keeps as well.
        (TyKind::Array(sub_inner), TyKind::Reference { name, args, .. })
            if name.as_str() == "kotlin.Array" && args.len() == 1 =>
        {
            is_subtype(db, scope, &sub_inner, &args[0])
                && is_subtype(db, scope, &args[0], &sub_inner)
        }
        (TyKind::Reference { name, args, .. }, TyKind::Array(sup_inner))
            if name.as_str() == "kotlin.Array" && args.len() == 1 =>
        {
            is_subtype(db, scope, &args[0], &sup_inner)
                && is_subtype(db, scope, &sup_inner, &args[0])
        }
        (TyKind::Array(sub_inner), TyKind::Array(sup_inner)) => {
            // Kotlin's array types are invariant
            // ([KLS `built-in-types-and-their-semantics.html#built-in-array-types`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-array-types)),
            // so the two element types have to be the same — the relation is
            // asked in both directions rather than compared for equality, so
            // that a *platform* element type still counts as its own non-null
            // half: the classfile `ActionListener[]` a Java getter declares
            // arrives as `Array<ActionListener!>`
            // ([`ty_from_java`], element by element), and `Array<ActionListener>`
            // is what the library's `Array<T>.forEach` binds `T` to. The two
            // directions agree on a concrete pair (`String` and `CharSequence`
            // are subtypes one way only, which is the invariance this keeps).
            is_subtype(db, scope, sub_inner, sup_inner)
                && is_subtype(db, scope, sup_inner, sub_inner)
        }
        (TyKind::Primitive(sub_primitive), TyKind::Primitive(sup_primitive)) => {
            sub_primitive == sup_primitive
        }
        _ => false,
    }
}

/// Whether `sub_args` satisfy `sup_args` positionally, given the variance each
/// parameter declares (`variances`), or invariant when it is `None`.
/// Whether `ty` is, or mentions, a type variable the model leaves unbound: the
/// variables of a declaration whose call site determines them
/// ([`is_subtype`] treats them as unknown), and no others — a *class's* own
/// parameters are bound by the receiver's arguments before any comparison.
fn mentions_unbound_var(db: &dyn TyDatabase, ty: Ty) -> bool {
    match ty.kind(db) {
        // A *classfile* declaration's parameter: it is the library's metadata
        // that a call determines it from, and this model reads the erased
        // signature alone.
        TyKind::TypeVar {
            scope:
                crate::ty::TypeVarScope::LibraryClass { .. }
                | crate::ty::TypeVarScope::LibraryMethod { .. },
            ..
        } => true,
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => {
            mentions_unbound_var(db, *inner)
        }
        TyKind::Array(inner) => mentions_unbound_var(db, **inner),
        TyKind::Flexible { lower, .. } => mentions_unbound_var(db, *lower),
        TyKind::Reference { args, .. } => args.iter().any(|arg| mentions_unbound_var(db, *arg)),
        _ => false,
    }
}

fn arguments_are_subtypes(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    variances: Option<&[Option<KotlinVariance>]>,
    sub_args: &[Ty],
    sup_args: &[Ty],
) -> bool {
    // A reference the model could not instantiate — a constructor call whose
    // type arguments no source wrote and nothing inferred, so the receiver is
    // the *class* rather than an instantiation of it — is compatible with every
    // instantiation: Kotlin has no raw types, so an empty argument list here is
    // never a written type, and rejecting it would report a mismatch the
    // compiler does not ([KLS
    // `type-inference.html#call-with-an-expected-type`](https://kotlinlang.org/spec/type-inference.html#call-with-an-expected-type)
    // infers a constructor call's type arguments from the expected type).
    if sub_args.is_empty() && !sup_args.is_empty() {
        return true;
    }
    // A *raw* supertype — an argument list nothing wrote, which is what a
    // receiver the model could not instantiate produces
    // ([`Ty::reference`] with no arguments, usually a `class_receiver` that
    // nothing parameterized) — accepts every parameterization, exactly as it
    // does in Java ([JLS
    // §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)
    // makes the raw type's members erased, and an unchecked conversion lets a
    // `List<String>` stand where a raw `List` is expected). kotlinc 2.4.20
    // accepts `JList(DefaultListModel<String>())` the same way — inferring the
    // constructor's type argument from the argument, which this model leaves
    // uninstantiated instead.
    if sup_args.is_empty() {
        return true;
    }
    if sub_args.len() != sup_args.len() {
        return false;
    }
    sub_args
        .iter()
        .zip(sup_args)
        .enumerate()
        .all(|(index, (sub, sup))| {
            // A use-site projection is an anonymous declaration: `out T`
            // covariant, `in T` contravariant, `*` accepts anything
            // ([KLS
            // `type-system.html#use-site-variance`](https://kotlinlang.org/spec/type-system.html#use-site-variance)).
            // It is read before the classifier's own variance, because a
            // classfile writes the *use* of a parameter as a projection even
            // where the Kotlin declaration is covariant: a library
            // `List<T>.dropLastWhile` carries `java.util.List<? extends T>`,
            // and `List<String>` satisfies it — the projection is what the
            // comparison has to follow, not the `out` the Kotlin declaration
            // declares.
            if let TyKind::Wildcard(bound) = sup.kind(db).clone() {
                return match bound {
                    Some(bound) => match &bound.kind {
                        crate::ty::BoundKind::Upper => is_subtype(db, scope, sub, &bound.ty),
                        crate::ty::BoundKind::Lower => is_subtype(db, scope, &bound.ty, sub),
                    },
                    None => true,
                };
            }
            let variance = variances.and_then(|variances| variances.get(index).copied().flatten());
            match variance {
                Some(KotlinVariance::Out) => is_subtype(db, scope, sub, sup),
                Some(KotlinVariance::In) => is_subtype(db, scope, sup, sub),
                // An invariant parameter: the arguments are the same class
                // type, and the relation is asked in both directions rather
                // than compared for equality, so a *platform* argument counts
                // as its own non-null half — a Java `Component` written as a
                // type argument arrives as `Component!`
                // ([`ty_from_java`]), and `List<Component!>` is a
                // `List<Component>` in Kotlin exactly as `Component!` is a
                // `Component` ([`is_subtype`]'s flexible rules). A concrete
                // pair still only passes one way: `String` is a
                // `CharSequence` and not the other way round.
                None => is_subtype(db, scope, sub, sup) && is_subtype(db, scope, sup, sub),
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
