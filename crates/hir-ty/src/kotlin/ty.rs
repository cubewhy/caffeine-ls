//! Building a [`Ty`] from a Kotlin declaration's written types.
//!
//! The item tree stores each type as a range-free `TypeRef<Name>`
//! ([`hir_def::kotlin::item_tree::ItemTypeRef`]), whose variants already carry
//! the two pieces of Kotlin syntax the Java model has no room for: the
//! nullability wrappers `T?` / `T & Any` ([`TypeRef::Nullable`],
//! [`TypeRef::DefinitelyNonNull`], lowered from the `NULLABLE_TYPE` /
//! `DEFINITELY_NON_NULLABLE_TYPE` nodes) and the function-type sugar, which the
//! lowering resolves to its `FunctionN` classifier
//! ([KLS `type-system.html#function-types`](https://kotlinlang.org/spec/type-system.html#function-types)).
//!
//! Names are resolved by [`KotlinResolver`]; this module only walks the
//! structure.
//!
//! # Reference
//!
//! KLS `type-system.html`: `#classifier-types`, `#type-parameters`,
//! `#function-types`, `#nullable-types`, `#type-containment`,
//! `#definitely-non-nullable-types`.

use syntax::stub::{TypeBound, TypeRef};

use hir_expand::name::Name;

use syntax::stub::PrimitiveType;

use hir_def::kotlin::modifiers::KotlinVariance as Variance;

use super::resolve::KotlinResolver;
use crate::jvm::db::TyDatabase;
use crate::ty::{BoundKind, Ty, TyKind, WildcardBound};

/// The [`Ty`] a written type reference denotes.
///
/// A reference whose name resolves to nothing is [`Ty::error`] — the unresolved
/// reference the diagnostics report ([KLS
/// `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers)).
pub fn ty_from_type_ref(
    db: &dyn TyDatabase,
    resolver: &KotlinResolver<'_>,
    ty: &TypeRef<Name>,
) -> Ty {
    match ty {
        TypeRef::Reference { name, generic_args } => {
            let args = generic_args
                .iter()
                .map(|arg| ty_from_type_ref(db, resolver, arg))
                .collect();
            resolver.resolve_reference(name, args)
        }
        // A Kotlin source type is never primitive (`Int` is the classifier
        // `kotlin.Int`); a primitive reference reaches here only from a
        // classfile-typed position.
        TypeRef::Primitive(primitive) => Ty::primitive(db, *primitive),
        // The classfile `V` ([JVMS §4.3.2]): Kotlin's `Unit` is *not* a
        // primitive of the Kotlin model either, so it lowers to the void kind
        // [`ty_from_java`] maps onto `kotlin.Unit`.
        TypeRef::Void => Ty::void(db),
        TypeRef::Wildcard { bound } => Ty::wildcard(
            db,
            bound.as_deref().map(|bound| match bound {
                TypeBound::Upper(inner) => Box::new(WildcardBound {
                    kind: BoundKind::Upper,
                    ty: ty_from_type_ref(db, resolver, inner),
                }),
                TypeBound::Lower(inner) => Box::new(WildcardBound {
                    kind: BoundKind::Lower,
                    ty: ty_from_type_ref(db, resolver, inner),
                }),
            }),
        ),
        // A type-parameter reference: the lowering writes a `USER_TYPE` for it
        // (so this arm is the classfile path), and the resolver looks it up in
        // the same scope as any other name.
        TypeRef::TypeVariable(name) => resolver.resolve_reference(name, Vec::new()),
        TypeRef::Array(inner) => Ty::array(db, ty_from_type_ref(db, resolver, inner)),
        TypeRef::Error => Ty::error(db),
        TypeRef::Nullable(inner) => Ty::nullable(db, ty_from_type_ref(db, resolver, inner)),
        TypeRef::DefinitelyNonNull(inner) => {
            Ty::definitely_non_null(db, ty_from_type_ref(db, resolver, inner))
        }
    }
}

/// The Kotlin type a Java or classfile type denotes ([KLS
/// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)
/// for the mapped classifiers,
/// [KLS `type-system.html#platform-types`](https://kotlinlang.org/spec/type-system.html#platform-types)
/// for the `T!` wrapping).
///
/// The conversion is the compiler\'s, in three steps:
///
/// * a primitive is the Kotlin classifier it maps onto — `int` is
///   `kotlin.Int`, `void` is `kotlin.Unit`
///   ([`MAPPED_TYPES`] names the reference mappings);
/// * a *reference* name is rewritten through the mapping table, so
///   `java.lang.String` is `kotlin.String` and `java.util.List` is
///   `kotlin.collections.List`;
/// * a reference type whose name is not Kotlin\'s own is wrapped in the
///   platform type `T!`, which is the flexible type `T..T?` ([KLS
///   `type-system.html#flexible-types`](https://kotlinlang.org/spec/type-system.html#flexible-types)).
///   A *Java source* type is wrapped like a classfile\'s, because this model
///   carries no `@Nullable`-equivalent for the Java source\'s annotations — a
///   recorded deviation, and one that only under-reports nullability (a Java
///   source type stays usable from Kotlin without an unsafe-call warning
///   either way).
pub fn ty_from_java(db: &dyn TyDatabase, ty: Ty) -> Ty {
    read_jvm_type(db, ty, JvmReading::Java)
}

/// The Kotlin type a **Kotlin declaration\'s** JVM-view type denotes — the shape
/// [`crate::kotlin::jvm_view`] writes for a Kotlin declaration, read back by a
/// Kotlin caller.
///
/// It is [`ty_from_java`], for a shape a Kotlin declaration wrote: the name
/// mapping is the same, and the platform type is not applied, because the
/// shape came from the declaration itself rather than from a declaration whose
/// nullability the compiler does not know
/// (<https://kotlinlang.org/docs/java-interop.html#null-safety-and-platform-types>).
///
/// The JVM view is an *erased* signature, so two things do not survive the
/// round trip: a `T?` parameter is the shape of a `T` one — the reading is the
/// non-nullable name, under-reporting nullability only — and the read-only and
/// mutable collection interfaces are one JVM type, for which the read-only
/// Kotlin interface is the reading of a Kotlin declaration (where
/// [`ty_from_java`] takes the mutable one, which a Java *class*\'s supertype
/// edge means).
pub fn ty_from_jvm_view(db: &dyn TyDatabase, ty: Ty) -> Ty {
    read_jvm_type(db, ty, JvmReading::KotlinDeclaration)
}

/// Which declaration a JVM shape came from — the one difference the Kotlin
/// reading of it makes ([`ty_from_java`] / [`ty_from_jvm_view`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum JvmReading {
    /// A Java source\'s or a classfile\'s shape: its reference types are
    /// *platform* types in Kotlin (`T!`).
    Java,
    /// A Kotlin declaration\'s JVM view: its reference types are the Kotlin
    /// types the declaration writes.
    KotlinDeclaration,
}

/// The one mapping behind [`ty_from_java`] and [`ty_from_jvm_view`]: the Kotlin
/// type the JVM shape `ty` denotes, read for the declaration it came from.
fn read_jvm_type(db: &dyn TyDatabase, ty: Ty, reading: JvmReading) -> Ty {
    match ty.kind(db) {
        TyKind::Primitive(primitive) => {
            let name = match primitive {
                PrimitiveType::Boolean => "Boolean",
                PrimitiveType::Char => "Char",
                PrimitiveType::Byte => "Byte",
                PrimitiveType::Short => "Short",
                PrimitiveType::Int => "Int",
                PrimitiveType::Long => "Long",
                PrimitiveType::Float => "Float",
                PrimitiveType::Double => "Double",
            };
            Ty::reference(db, format!("kotlin.{name}"), Vec::new())
        }
        // `void` is not a primitive of the *Kotlin* model: it is `kotlin.Unit`
        // ([KLS
        // `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)).
        TyKind::Void => Ty::reference(db, "kotlin.Unit", Vec::new()),
        TyKind::Reference { name, args, local } => {
            let args: Vec<Ty> = args
                .iter()
                .map(|arg| read_jvm_type(db, *arg, reading))
                .collect();
            // A local class keeps its declaration: it is the type\'s identity
            // ([JLS §6.7]), and a local class has no canonical name to map.
            let mapped = |name: &Name| match local {
                Some(class) => Ty::local_reference(db, *class, name.clone(), args.clone()),
                None => Ty::reference(db, name.clone(), args.clone()),
            };
            let kotlin = mapped(&mapped_type_name(name));
            let JvmReading::Java = reading else {
                return kotlin;
            };
            if local.is_none() && !name.as_str().starts_with("kotlin.") {
                // A classfile or Java declaration: the compiler knows nothing
                // about its nullability, so it is a platform type. A *collection
                // interface* is the JVM type of two Kotlin classifiers — the
                // read-only view and the mutable one, which [`MAPPED_TYPES`]
                // names as two entries — and the platform type a Java value has
                // is `(Mutable)List<T>!`, usable as either. The mutable half is
                // what a Java *class*\'s supertype edge means (a class that
                // implements `java.util.List` implements the mutable
                // interface): it is what makes `val list: MutableList<Component>
                // = LinkedList()` legal, and it is how kotlinc 2.4.20 reads the
                // same source.
                let lower = mapped_mutable_name(name)
                    .map(|mutable| mapped(&mutable))
                    .unwrap_or(kotlin);
                let upper = Ty::nullable(db, kotlin);
                Ty::flexible(db, lower, upper)
            } else {
                kotlin
            }
        }
        TyKind::Array(inner) => Ty::array(db, read_jvm_type(db, **inner, reading)),
        TyKind::TypeVar { .. } => ty,
        // Everything else is either a Kotlin-only form (which a Java type
        // cannot be) or already Kotlin\'s.
        _ => ty,
    }
}

/// Where a Kotlin type sits in the classfile the compiler emits for it — the
/// two positions the JVM cannot represent with one shape: a *type argument*
/// (and an array element) carries a primitive's box, and a function's
/// **return** carries `void` for `Unit` where a value position carries the
/// `kotlin.Unit` class (kotlinc 2.4.20, observed with `javap -p -s`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum JvmPosition {
    /// A parameter, a field, a supertype: `Int` is `int`, `Unit` is
    /// `kotlin.Unit`.
    Value,
    /// A type argument or an array element: `Int` is `java.lang.Integer`.
    Argument,
    /// A function's return type: `Unit` is `void`, `Int` is `int`.
    Return,
}

/// The Java or JVM type a Kotlin type denotes, for the Java layer that
/// consumes it: the inverse of [`ty_from_java`], and erased — the classfile a
/// Java caller reads carries no Kotlin type arguments.
///
/// Four rules decide the shape ([KLS
/// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)
/// names the classifiers the compiler maps onto JVM types; the mapping itself
/// is spelled out in <https://kotlinlang.org/docs/java-interop.html#mapped-types>,
/// which KLS does not cover), all observed with kotlinc 2.4.20 and
/// `javap -p -s`:
///
/// * a **nullable** primitive is the JVM *box*, wherever it is written:
///   `val x: Int?` is `java.lang.Integer getX()`, `fun f(a: Int?): Int?` is
///   `java.lang.Integer f(java.lang.Integer)`, and a type argument is boxed
///   the same way (`List<Int?>` is `List<java.lang.Integer>`);
/// * a primitive in a **type argument** or an **array element** is likewise
///   the box, while one in a *value* position — a parameter, a return, a
///   field — stays the primitive the signature unboxes: `List<Int>` is
///   `java.util.List<java.lang.Integer>`, `Array<Int>` is
///   `java.lang.Integer[]`, and `fun f(a: Int): Int` is `int f(int)`;
/// * `kotlin.Unit` is `void` in a **function's return type** only, and only
///   when it is written *bare*; everywhere else — a property's own type, a
///   parameter, a type argument, an array element, and a *nullable* return —
///   it is the `kotlin.Unit` class: `fun g(): Unit` is `void g()` while
///   `val u: Unit` is `kotlin.Unit getU()`, `fun takeUnit(x: Unit)` is
///   `void takeUnit(kotlin.Unit)`, `Array<Unit>` is `kotlin.Unit[]`,
///   `List<Unit>` is `List<kotlin.Unit>` and `fun r(): Unit?` is
///   `kotlin.Unit r()`;
/// * Kotlin's `Array<T>` *is* the JVM array `T[]`, whatever `T` is —
///   `Array<String>` is `java.lang.String[]`, `Array<Unit>`
///   `kotlin.Unit[]` — which is how the builtins layer already reads a
///   lookup for the same classifier ([`crate::kotlin::builtins::jvm_ty`]).
///
/// Every other mapped classifier is the JVM class [`MAPPED_TYPES`] pairs it
/// with, and a Kotlin classifier the table does not name keeps its own name
/// (the classfile the compiler emits for `class Wrapper` *is* `Wrapper`) —
/// with `$` joining nested segments, which is how the JVM spells a nested
/// class ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)).
/// A definitely-non-nullable `T & Any` reads as `T`, a flexible type as its
/// `lower` half — the type the value has when it is used — and a type variable
/// as its *erasure* ([JLS §4.6]): its first bound, or `java.lang.Object` when
/// it has none, because a generic Kotlin declaration compiles to the erased
/// signature.
///
/// A recorded deviation: a primitive *array* classifier (`kotlin.IntArray`,
/// `kotlin.LongArray`, …) is projected as its own name rather than as the JVM
/// primitive array it compiles to, and the Java→Kotlin reading of `int[]` is
/// `Array<Int>` for the same reason — the classifier's member surface (`size`,
/// indexing) is the builtins layer's and is keyed on the array kind
/// (<https://kotlinlang.org/docs/java-interop.html#mapped-types>: the arrays
/// are `int[]` ↔ `IntArray`, which this projection leaves to that layer).
pub fn ty_from_kotlin(db: &dyn TyDatabase, ty: Ty) -> Ty {
    ty_from_kotlin_in(db, ty, JvmPosition::Value)
}

/// [`ty_from_kotlin`] at a **function's return type** — the one position where
/// `kotlin.Unit` is `void` (see its rules). An accessor's return type is *not*
/// one: a getter carries the property's own type, so `val u: Unit` compiles to
/// `kotlin.Unit getU()` (kotlinc 2.4.20).
pub fn ty_from_kotlin_return(db: &dyn TyDatabase, ty: Ty) -> Ty {
    ty_from_kotlin_in(db, ty, JvmPosition::Return)
}

fn ty_from_kotlin_in(db: &dyn TyDatabase, ty: Ty, position: JvmPosition) -> Ty {
    match ty.kind(db) {
        TyKind::Reference { name, args, local } => {
            // Kotlin's `Array<T>` is the JVM array `T[]`, and its element
            // carries the box a type argument does. An `Array` written with no
            // argument is an error the declaration checker already reports, and
            // keeps the fallback below.
            if name.as_str() == "kotlin.Array"
                && let [element] = args.as_slice()
            {
                return Ty::array(db, ty_from_kotlin_in(db, *element, JvmPosition::Argument));
            }
            if let Some((primitive, boxed)) = primitive_of_mapped(name) {
                return match position {
                    JvmPosition::Argument => Ty::reference(db, boxed, Vec::new()),
                    JvmPosition::Value | JvmPosition::Return => Ty::primitive(db, primitive),
                };
            }
            if name.as_str() == "kotlin.Unit" {
                return match position {
                    JvmPosition::Return => Ty::void(db),
                    JvmPosition::Value | JvmPosition::Argument => {
                        Ty::reference(db, "kotlin.Unit", Vec::new())
                    }
                };
            }
            // The JVM name of a nested classifier joins with `$`
            // ([JVMS §4.2]).
            let jvm = java_name(name);
            let args = args
                .iter()
                .map(|arg| ty_from_kotlin_in(db, *arg, JvmPosition::Argument))
                .collect();
            match local {
                Some(class) => Ty::local_reference(db, *class, jvm, args),
                None => Ty::reference(db, jvm, args),
            }
        }
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => {
            // A nullability wrapper over a primitive is the JVM **box**: a Java
            // type has no nullability, and only the box can hold the null. The
            // wrapper is a nullability form rather than a written primitive, so
            // both spellings (`Int?` and the flexible type's `T & Any` upper
            // half) take it.
            if let TyKind::Reference { name, .. } = inner.kind(db) {
                if let Some((_, boxed)) = primitive_of_mapped(name) {
                    return Ty::reference(db, boxed, Vec::new());
                }
                // A *nullable* `Unit` is the `kotlin.Unit` class even in a
                // return position, where the bare `Unit` is `void`: the wrapper
                // is what the value carries a null in, and `void` cannot
                // (kotlinc 2.4.20 compiles `fun ret(): Unit?` to
                // `public final kotlin.Unit ret();` while `fun g(): Unit` is
                // `public final void g();`).
                if name.as_str() == "kotlin.Unit" {
                    return Ty::reference(db, "kotlin.Unit", Vec::new());
                }
            }
            ty_from_kotlin_in(db, *inner, position)
        }
        TyKind::Flexible { lower, .. } => ty_from_kotlin_in(db, *lower, position),
        TyKind::TypeVar { bounds, .. } => match bounds.first() {
            Some(bound) => ty_from_kotlin_in(db, *bound, position),
            None => Ty::reference(db, "java.lang.Object", Vec::new()),
        },
        TyKind::Array(inner) => {
            // The JVM/classfile spelling, which a Kotlin *source* type does not
            // produce: its component is already whatever the compiler erased to,
            // so it is read as an argument.
            let inner = ty_from_kotlin_in(db, **inner, JvmPosition::Argument);
            Ty::array(db, inner)
        }
        // Primitives, `void` and the Java-only shapes are already JVM types.
        _ => ty,
    }
}

/// The JVM primitive a mapped Kotlin classifier is, when it is one, paired with
/// the **box** the same classifier takes where the compiler cannot unbox it —
/// a type argument, an array element, a nullable position: `kotlin.Int` is an
/// `int` in a signature, and the `java.lang.Integer` the same classifier
/// compiles to when the value cannot stay one ([KLS
/// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html);
/// <https://kotlinlang.org/docs/java-interop.html#mapped-types> pairs each with
/// its wrapper).
fn primitive_of_mapped(name: &Name) -> Option<(PrimitiveType, &'static str)> {
    Some(match name.as_str() {
        "kotlin.Int" => (PrimitiveType::Int, "java.lang.Integer"),
        "kotlin.Long" => (PrimitiveType::Long, "java.lang.Long"),
        "kotlin.Float" => (PrimitiveType::Float, "java.lang.Float"),
        "kotlin.Double" => (PrimitiveType::Double, "java.lang.Double"),
        "kotlin.Boolean" => (PrimitiveType::Boolean, "java.lang.Boolean"),
        "kotlin.Byte" => (PrimitiveType::Byte, "java.lang.Byte"),
        "kotlin.Char" => (PrimitiveType::Char, "java.lang.Character"),
        "kotlin.Short" => (PrimitiveType::Short, "java.lang.Short"),
        _ => return None,
    })
}

/// The JVM name of a Kotlin classifier: the JVM class [`MAPPED_TYPES`] pairs it
/// with, else the Kotlin name itself.
///
/// A Kotlin classifier the table does not name keeps its Kotlin spelling, which
/// is the name the classfile carries for a top-level class (`kotlin.ranges
/// .IntRange` *is* `kotlin/ranges/IntRange`) and the name the Java layer keys a
/// *source* declaration by ([`hir::source_class_fqn`]); the compiler spells a
/// nested segment `$` ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2))
/// and this conversion keeps the dotted form, so a nested library classifier the
/// table does not name is one name away from its binary spelling — a recorded
/// deviation, since telling the package segments from the type segments needs a
/// resolution this function has no scope for.
fn java_name(name: &Name) -> Name {
    if let Some((java, _)) = MAPPED_TYPES
        .iter()
        .find(|(_, kotlin)| name.as_str() == *kotlin)
    {
        return Name::new(*java);
    }
    name.clone()
}

/// The mapped classifier a Java reference name denotes ([KLS
/// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)
/// names the classifiers the compiler maps onto JVM types; the mapping itself
/// is spelled out in <https://kotlinlang.org/docs/java-interop.html#mapped-types>,
/// which KLS does not cover). A name not in the table is itself.
pub(crate) fn mapped_type_name(name: &Name) -> Name {
    // The classfile's spelling of a function type is the Kotlin one:
    // `kotlin.jvm.functions.FunctionN` *is* `kotlin.FunctionN`
    // (<https://kotlinlang.org/docs/java-interop.html#mapped-types> names the
    // mapping for the collections; a function type is the same relation, and the
    // compiler reads the classfile spelling in every facade signature).
    if let Some(rest) = name.as_str().strip_prefix("kotlin.jvm.functions.")
        && let Some(arity) = rest.strip_prefix("Function")
        && arity.parse::<usize>().is_ok()
    {
        return Name::new(&format!("kotlin.{rest}"));
    }
    MAPPED_TYPES
        .iter()
        .find(|(java, _)| name.as_str() == *java)
        .map(|(_, kotlin)| Name::new(*kotlin))
        .unwrap_or_else(|| name.clone())
}

/// The Java classes the compiler maps onto Kotlin classifiers
/// (<https://kotlinlang.org/docs/java-interop.html#mapped-types>; KLS
/// *Kotlin/Core* has no Java-interop section, so the compiler's documentation
/// is the reference). The arrays (`int[]` ↔ `IntArray`), the primitives — which
/// map by kind, not by name ([`ty_from_java`]) — and `java.lang.Object`'s
/// removal of the `Number` hierarchy from the mapping are not listed.
pub const MAPPED_TYPES: &[(&str, &str)] = &[
    ("java.lang.Object", "kotlin.Any"),
    ("java.lang.String", "kotlin.String"),
    ("java.lang.CharSequence", "kotlin.CharSequence"),
    ("java.lang.Throwable", "kotlin.Throwable"),
    ("java.lang.Cloneable", "kotlin.Cloneable"),
    ("java.lang.Number", "kotlin.Number"),
    ("java.lang.Comparable", "kotlin.Comparable"),
    ("java.lang.Enum", "kotlin.Enum"),
    ("java.lang.Annotation", "kotlin.Annotation"),
    ("java.lang.Integer", "kotlin.Int"),
    ("java.lang.Boolean", "kotlin.Boolean"),
    ("java.lang.Character", "kotlin.Char"),
    ("java.lang.Long", "kotlin.Long"),
    ("java.lang.Float", "kotlin.Float"),
    ("java.lang.Double", "kotlin.Double"),
    ("java.lang.Short", "kotlin.Short"),
    ("java.lang.Byte", "kotlin.Byte"),
    ("java.lang.Void", "kotlin.Unit"),
    // The collection interfaces have **two** Kotlin classifiers each — the
    // read-only view and the mutable one, which share one JVM interface
    // (<https://kotlinlang.org/docs/java-interop.html#mapped-types> lists both
    // for every entry). A Java type is read as the read-only view here: the
    // compiler reads it as the *platform* type `(Mutable)List<T>!`, whose upper
    // bound is this one, and the mutable entry stays in the table because it is
    // the JVM view's answer for the classifier the Kotlin side writes
    // ([`super::builtins::declared_supertypes`] is what makes `MutableList` a
    // `List` and an `ArrayList` a `MutableList`).
    ("java.util.List", "kotlin.collections.List"),
    ("java.util.List", "kotlin.collections.MutableList"),
    ("java.util.Set", "kotlin.collections.Set"),
    ("java.util.Set", "kotlin.collections.MutableSet"),
    ("java.util.Map", "kotlin.collections.Map"),
    ("java.util.Map", "kotlin.collections.MutableMap"),
    ("java.util.Map$Entry", "kotlin.collections.Map.Entry"),
    (
        "java.util.Map$Entry",
        "kotlin.collections.MutableMap.MutableEntry",
    ),
    ("java.util.Collection", "kotlin.collections.Collection"),
    (
        "java.util.Collection",
        "kotlin.collections.MutableCollection",
    ),
    ("java.util.Iterator", "kotlin.collections.Iterator"),
    ("java.util.Iterator", "kotlin.collections.MutableIterator"),
    ("java.util.ListIterator", "kotlin.collections.ListIterator"),
    (
        "java.util.ListIterator",
        "kotlin.collections.MutableListIterator",
    ),
    ("java.lang.Iterable", "kotlin.collections.Iterable"),
    ("java.lang.Iterable", "kotlin.collections.MutableIterable"),
    ("java.util.ArrayList", "kotlin.collections.ArrayList"),
    ("java.util.HashMap", "kotlin.collections.HashMap"),
    (
        "java.util.LinkedHashMap",
        "kotlin.collections.LinkedHashMap",
    ),
];

/// The *mutable* Kotlin classifier a classfile collection interface is the JVM
/// type of, when the compiler maps that JVM type onto two — the read-only view
/// and the mutable one ([`MAPPED_TYPES`] lists both for every collection entry,
/// <https://kotlinlang.org/docs/java-interop.html#mapped-types>).
///
/// `None` for a JVM type the table names once: `java.util.ArrayList` is a
/// *class*, and an `ArrayList` is a mutable list by its own declaration rather
/// than by the interface it implements.
pub fn mapped_mutable_name(name: &Name) -> Option<Name> {
    MAPPED_TYPES
        .iter()
        .filter(|(java, _)| name.as_str() == *java)
        .nth(1)
        .map(|(_, kotlin)| Name::new(*kotlin))
}

/// The declaration-site variance of the mapped standard-library classifiers
/// ([KLS
/// `type-system.html#declaration-site-variance`](https://kotlinlang.org/spec/type-system.html#declaration-site-variance)),
/// one entry per type parameter, in declaration order.
///
/// The classfile cannot carry it: the compiler reads `List<out E>` from the
/// `@Metadata` annotation, which this model does not decode, and
/// `kotlin.collections.List` has no classfile of its own — it *is*
/// `java.util.List`. The table is therefore a recorded deviation, checked
/// against kotlinc 2.4.20's own reports of the standard library's declarations
/// (`List<out E>`, `Map<K, out V>`, `Map.Entry<out K, out V>`,
/// `Iterable<out T>`); a standard-library classifier the table does not name is
/// invariant.
pub const MAPPED_VARIANCE: &[(&str, &[Option<Variance>])] = &[
    ("kotlin.collections.List", &[Some(Variance::Out)]),
    ("kotlin.collections.Set", &[Some(Variance::Out)]),
    ("kotlin.collections.Map", &[None, Some(Variance::Out)]),
    (
        "kotlin.collections.Map.Entry",
        &[Some(Variance::Out), Some(Variance::Out)],
    ),
    ("kotlin.collections.Iterable", &[Some(Variance::Out)]),
    ("kotlin.collections.Iterator", &[Some(Variance::Out)]),
    ("kotlin.collections.Sequence", &[Some(Variance::Out)]),
    // The mutable views and the two interfaces above them. The *covariant* ones
    // are the interfaces Kotlin declares `out` because nothing writes through
    // them ([`MutableIterator`] only removes, `Collection` only reads): kotlinc
    // 2.4.20 accepts `val any: MutableIterator<Any> = iteratorOfInts` and
    // `val any: Collection<Any> = ints`. The rest are invariant, which is what
    // the absence of an entry means as well — they are named here so that their
    // *arity* is readable too ([`super::resolve`] reads it to tell one
    // star-imported candidate from another).
    ("kotlin.collections.Collection", &[Some(Variance::Out)]),
    ("kotlin.collections.ListIterator", &[Some(Variance::Out)]),
    ("kotlin.collections.MutableIterable", &[Some(Variance::Out)]),
    ("kotlin.collections.MutableIterator", &[Some(Variance::Out)]),
    ("kotlin.collections.MutableCollection", &[None]),
    ("kotlin.collections.MutableList", &[None]),
    ("kotlin.collections.MutableListIterator", &[None]),
    ("kotlin.collections.MutableSet", &[None]),
    ("kotlin.collections.MutableMap", &[None, None]),
    ("kotlin.collections.MutableMap.MutableEntry", &[None, None]),
];

/// The declaration-site variance of a classifier named by `fqn`, if the table
/// above records it ([`MAPPED_VARIANCE`]) — one entry per type parameter, in
/// declaration order, `None` for an invariant parameter.
pub fn mapped_variances(fqn: &Name) -> Option<&'static [Option<Variance>]> {
    MAPPED_VARIANCE
        .iter()
        .find(|(name, _)| fqn.as_str() == *name)
        .map(|(_, variances)| *variances)
}

/// The Kotlin spelling of a type (KLS
/// `type-system.html#type-kinds`](https://kotlinlang.org/spec/type-system.html#type-kinds)):
/// `String?`, `List<out Number>`, `List<*>`, `(Int) -> String`.
///
/// The notation *is* language-specific — Kotlin writes a use-site projection
/// `out T` where Java writes `? extends T`, and a star projection `*` where
/// Java writes `?` — so this renderer is Kotlin's entry point into the neutral
/// display ([`Ty::display_simple`] / [`Ty::display`]) rather than a second type
/// model. Every kind that has no language-specific notation delegates.
///
/// The spellings are the ones kotlinc 2.4.20 reports through the
/// `val probe: String = <expr>` probe: `actual 'Int?'`,
/// `actual '(Int) -> String'`, `actual 'List<out Number>'`,
/// `actual 'Map<Int, Int>'`.
pub struct KotlinTyDisplay<'a> {
    ty: Ty,
    db: &'a dyn TyDatabase,
}

impl std::fmt::Display for KotlinTyDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ty.kind(self.db) {
            TyKind::Nullable(inner) => write!(
                f,
                "{}?",
                KotlinTyDisplay {
                    ty: *inner,
                    db: self.db
                }
            ),
            TyKind::DefinitelyNonNull(inner) => {
                write!(
                    f,
                    "{} & Any",
                    KotlinTyDisplay {
                        ty: *inner,
                        db: self.db
                    }
                )
            }
            // A flexible type renders the way kotlinc's own type printer
            // renders it, `L..U` (KLS
            // `type-system.html#flexible-types`); a platform type spelled `T!`
            // is `T..T?`.
            TyKind::Flexible { lower, upper } => write!(
                f,
                "{}..{}",
                KotlinTyDisplay {
                    ty: *lower,
                    db: self.db
                },
                KotlinTyDisplay {
                    ty: *upper,
                    db: self.db
                }
            ),
            TyKind::Wildcard(bound) => match bound {
                None => f.write_str("*"),
                Some(bound) => {
                    let keyword = match bound.kind {
                        BoundKind::Upper => "out",
                        BoundKind::Lower => "in",
                    };
                    write!(
                        f,
                        "{keyword} {}",
                        KotlinTyDisplay {
                            ty: bound.ty,
                            db: self.db
                        }
                    )
                }
            },
            // `kotlin.FunctionN<P1, …, PN, R>` is spelled `(P1, …, PN) -> R`.
            TyKind::Reference { name, args, .. } if is_function_classifier(name) => {
                let (params, ret) = args.split_at(args.len().saturating_sub(1));
                f.write_str("(")?;
                for (index, param) in params.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(
                        f,
                        "{}",
                        KotlinTyDisplay {
                            ty: *param,
                            db: self.db
                        }
                    )?;
                }
                f.write_str(") -> ")?;
                match ret.first() {
                    Some(ret) => write!(
                        f,
                        "{}",
                        KotlinTyDisplay {
                            ty: *ret,
                            db: self.db
                        }
                    ),
                    None => f.write_str("Unit"),
                }
            }
            // A reference renders its *simple* name and its arguments in the
            // Kotlin notation (kotlinc reports `List<out Number>`, not the
            // fully qualified form).
            TyKind::Reference { name, args, .. } => {
                f.write_str(name.simple_name())?;
                if !args.is_empty() {
                    f.write_str("<")?;
                    for (index, arg) in args.iter().enumerate() {
                        if index > 0 {
                            f.write_str(", ")?;
                        }
                        write!(
                            f,
                            "{}",
                            KotlinTyDisplay {
                                ty: *arg,
                                db: self.db
                            }
                        )?;
                    }
                    f.write_str(">")?;
                }
                Ok(())
            }
            // Every other kind renders as the simple language-neutral spelling.
            _ => write!(f, "{}", self.ty.display_simple(self.db)),
        }
    }
}

/// Whether a reference name is a `kotlin.FunctionN` classifier.
fn is_function_classifier(name: &Name) -> bool {
    let simple = name.as_str().rsplit('.').next().unwrap_or(name.as_str());
    match simple.strip_prefix("Function") {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// The Kotlin spelling of a type, for the client-facing surfaces (the hover,
/// the outline, the inlay hints).
pub fn display_kotlin<'a>(db: &'a dyn TyDatabase, ty: Ty) -> KotlinTyDisplay<'a> {
    KotlinTyDisplay { ty, db }
}
