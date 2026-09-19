//! The Kotlin *built-in* classifiers: the language's own types, which no
//! classpath declares.
//!
//! A Kotlin classpath is not Java's. The types the *language* is defined over —
//! `Any`, `Nothing`, `Unit`, the eight primitives, `String`, `CharSequence`,
//! `Comparable`, `Number`, `Throwable`, `Enum`, `Annotation`, `Cloneable`, the
//! function types and the collection interfaces — are **built-in** classifiers:
//! the compiler knows them without any library, and on the JVM each one *is* a
//! JVM type ([KLS
//! `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)
//! names the classifiers and their mappings). `kotlin-stdlib.jar` therefore
//! carries the library declarations — the `CollectionsKt`/`StringsKt` facades,
//! `Lazy`, `Pair` — and *not* the built-in ones: `kotlin/Any.class`,
//! `kotlin/Int.class` and `kotlin/collections/List.class` are not in it at all.
//! Verified against the jar this workspace's corpus resolves against
//! (`kotlin-stdlib-2.2.0.jar`: `kotlin/Unit.class` and `kotlin/Function.class`
//! are present, `kotlin/Any.class`, `kotlin/Int.class`, `kotlin/String.class`
//! and every `kotlin/collections/{List,Map,Set,…}.class` are not).
//!
//! So a name like `Int` or `List` resolves to no classfile, and the lookups
//! that need one — member lookup and the supertype walk — answer for the *JVM
//! type* the built-in maps to ([`jvm_class`]): `kotlin.collections.List` is
//! `java.util.List`, `kotlin.Int` is `java.lang.Integer`, `kotlin.Any` is
//! `java.lang.Object`, `kotlin.Function1` is `kotlin.jvm.functions.Function1`.
//! The mapped class's members come back through [`super::ty::ty_from_java`], so
//! `List.size` is `java.util.List.size()` read as the Kotlin property `size`,
//! and its types are Kotlin's again ([`super::ty::ty_from_java`] rewrites
//! `java.util.List` to `kotlin.collections.List`) — the mapping is symmetric,
//! which is what keeps the two spellings of one type comparable.
//!
//! What the JVM type cannot answer is the members the *language* declares on
//! the built-ins: the numeric conversion functions (`Double.toInt`) are
//! compiler intrinsics with no method in any classfile, and `Array.size` is a
//! property of the array itself. Those are the table [`member_return`] holds
//! ([KLS
//! `built-in-types-and-their-semantics.html#built-in-integer-types`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)
//! declares the conversion functions of every built-in numeric type).

use super::ty::MAPPED_TYPES;

use crate::jvm::db::TyDatabase;
use crate::ty::{Ty, TyKind};
use syntax::stub::PrimitiveType;

/// The JVM class a built-in Kotlin classifier compiles to, for the built-ins
/// whose JVM type is a *class* the classpath can be asked about.
///
/// The pairs [`MAPPED_TYPES`] holds are the reference mappings of KLS
/// `built-in-types-and-their-semantics.html`; the `Mutable*` interfaces are
/// their Java mutable counterparts (Kotlin declares a mutable *view* of the
/// same JVM interface — `kotlin.collections.MutableList` and
/// `kotlin.collections.List` both erase to `java.util.List`), and the rest are
/// the compiler's own mappings (`kotlin.Nothing` is `java.lang.Void`, the type
/// no value has).
pub const BUILTIN_JVM_CLASSES: &[(&str, &str)] = &[
    // The reference mappings, read in the other direction.
    ("kotlin.Any", "java.lang.Object"),
    ("kotlin.String", "java.lang.String"),
    ("kotlin.CharSequence", "java.lang.CharSequence"),
    ("kotlin.Throwable", "java.lang.Throwable"),
    ("kotlin.Cloneable", "java.lang.Cloneable"),
    ("kotlin.Number", "java.lang.Number"),
    ("kotlin.Comparable", "java.lang.Comparable"),
    ("kotlin.Enum", "java.lang.Enum"),
    ("kotlin.Annotation", "java.lang.Annotation"),
    ("kotlin.Byte", "java.lang.Byte"),
    ("kotlin.Short", "java.lang.Short"),
    ("kotlin.Int", "java.lang.Integer"),
    ("kotlin.Long", "java.lang.Long"),
    ("kotlin.Float", "java.lang.Float"),
    ("kotlin.Double", "java.lang.Double"),
    ("kotlin.Char", "java.lang.Character"),
    ("kotlin.Boolean", "java.lang.Boolean"),
    ("kotlin.collections.Iterable", "java.lang.Iterable"),
    ("kotlin.collections.Iterator", "java.util.Iterator"),
    ("kotlin.collections.Collection", "java.util.Collection"),
    ("kotlin.collections.List", "java.util.List"),
    ("kotlin.collections.ListIterator", "java.util.ListIterator"),
    ("kotlin.collections.Set", "java.util.Set"),
    ("kotlin.collections.Map", "java.util.Map"),
    ("kotlin.collections.Map.Entry", "java.util.Map$Entry"),
    // The mutable views: one JVM interface each.
    ("kotlin.collections.MutableIterable", "java.lang.Iterable"),
    ("kotlin.collections.MutableIterator", "java.util.Iterator"),
    (
        "kotlin.collections.MutableCollection",
        "java.util.Collection",
    ),
    ("kotlin.collections.MutableList", "java.util.List"),
    (
        "kotlin.collections.MutableListIterator",
        "java.util.ListIterator",
    ),
    ("kotlin.collections.MutableSet", "java.util.Set"),
    ("kotlin.collections.MutableMap", "java.util.Map"),
    (
        "kotlin.collections.MutableMap.MutableEntry",
        "java.util.Map$Entry",
    ),
    // `Nothing` is the type no value has: the JVM type it compiles to is
    // `Void`, the class that cannot be instantiated.
    ("kotlin.Nothing", "java.lang.Void"),
];

/// Whether `name` is a *built-in* Kotlin classifier — one the language
/// declares, so a classpath need not.
///
/// The list is KLS `built-in-types-and-their-semantics.html`'s, plus the
/// function types (`kotlin.Function0`…`kotlin.Function22`, and the
/// `kotlin.Function`/`kotlin.FunctionN` aliases of the family) and `Array`,
/// which KLS `type-system.html#classifier-types` names as built-in classifier
/// types.
pub fn is_builtin(name: &str) -> bool {
    is_primitive(name)
        || primitive_array_element(name).is_some()
        || matches!(
            name,
            "kotlin.Any"
                | "kotlin.Nothing"
                | "kotlin.Unit"
                | "kotlin.String"
                | "kotlin.CharSequence"
                | "kotlin.Throwable"
                | "kotlin.Cloneable"
                | "kotlin.Number"
                | "kotlin.Comparable"
                | "kotlin.Enum"
                | "kotlin.Annotation"
                | "kotlin.Array"
                | "kotlin.Function"
                | "kotlin.collections.Iterable"
                | "kotlin.collections.Iterator"
                | "kotlin.collections.Collection"
                | "kotlin.collections.List"
                | "kotlin.collections.ListIterator"
                | "kotlin.collections.Set"
                | "kotlin.collections.Map"
                | "kotlin.collections.Map.Entry"
                | "kotlin.collections.MutableIterable"
                | "kotlin.collections.MutableIterator"
                | "kotlin.collections.MutableCollection"
                | "kotlin.collections.MutableList"
                | "kotlin.collections.MutableListIterator"
                | "kotlin.collections.MutableSet"
                | "kotlin.collections.MutableMap"
                | "kotlin.collections.MutableMap.MutableEntry"
        )
        || function_arity(name).is_some()
}

/// The arity of a `kotlin.FunctionN` name, when the name is one.
fn function_arity(name: &str) -> Option<u32> {
    name.strip_prefix("kotlin.Function")?.parse().ok()
}

/// Whether `name` is one of the eight primitive-typed built-in classifiers
/// (plus `Char` and `Boolean`, which Kotlin's numeric operators treat apart
/// but which the JVM passes as primitives just the same).
pub fn is_primitive(name: &str) -> bool {
    primitive_of(name).is_some()
}

/// The Kotlin primitive classifier `name` denotes, when it is one.
pub fn primitive_of(name: &str) -> Option<PrimitiveType> {
    Some(match name {
        "kotlin.Boolean" => PrimitiveType::Boolean,
        "kotlin.Char" => PrimitiveType::Char,
        "kotlin.Byte" => PrimitiveType::Byte,
        "kotlin.Short" => PrimitiveType::Short,
        "kotlin.Int" => PrimitiveType::Int,
        "kotlin.Long" => PrimitiveType::Long,
        "kotlin.Float" => PrimitiveType::Float,
        "kotlin.Double" => PrimitiveType::Double,
        _ => return None,
    })
}

const PRIMITIVE_ARRAYS: &[(PrimitiveType, &str)] = &[
    (PrimitiveType::Boolean, "kotlin.BooleanArray"),
    (PrimitiveType::Char, "kotlin.CharArray"),
    (PrimitiveType::Byte, "kotlin.ByteArray"),
    (PrimitiveType::Short, "kotlin.ShortArray"),
    (PrimitiveType::Int, "kotlin.IntArray"),
    (PrimitiveType::Long, "kotlin.LongArray"),
    (PrimitiveType::Float, "kotlin.FloatArray"),
    (PrimitiveType::Double, "kotlin.DoubleArray"),
];

pub(crate) fn primitive_array_element(name: &str) -> Option<PrimitiveType> {
    PRIMITIVE_ARRAYS
        .iter()
        .find_map(|&(element, array)| (array == name).then_some(element))
}

pub(crate) fn primitive_array_name(element: PrimitiveType) -> &'static str {
    PRIMITIVE_ARRAYS
        .iter()
        .find(|(primitive, _)| *primitive == element)
        .expect("all JVM primitives have specialized arrays")
        .1
}

/// The JVM class of a built-in Kotlin classifier, when a classpath can be
/// asked about it: the mapped class ([`BUILTIN_JVM_CLASSES`]), the class the
/// function types live in (`kotlin.jvm.functions.FunctionN`, where `invoke` is
/// declared), or the boxed class of a primitive. `kotlin.Array` answers `None`
/// — a Kotlin array is a JVM *array*, not a class.
pub fn jvm_class(name: &str) -> Option<&'static str> {
    if let Some((_, jvm)) = BUILTIN_JVM_CLASSES
        .iter()
        .find(|(kotlin, _)| *kotlin == name)
    {
        return Some(jvm);
    }
    // A Kotlin *spelling* of a class the classpath holds under its Java name:
    // `ArrayList` is `java.util.ArrayList` (Kotlin declares it as a type alias
    // of it), and `MutableList` is `java.util.List` — Kotlin's own type
    // declaration, whose JVM type is the Java interface.
    if let Some((jvm, _)) = MAPPED_TYPES.iter().find(|(_, kotlin)| *kotlin == name) {
        return Some(jvm);
    }
    if let Some(arity) = function_arity(name) {
        return Some(match arity {
            0 => "kotlin.jvm.functions.Function0",
            1 => "kotlin.jvm.functions.Function1",
            2 => "kotlin.jvm.functions.Function2",
            3 => "kotlin.jvm.functions.Function3",
            4 => "kotlin.jvm.functions.Function4",
            5 => "kotlin.jvm.functions.Function5",
            6 => "kotlin.jvm.functions.Function6",
            7 => "kotlin.jvm.functions.Function7",
            8 => "kotlin.jvm.functions.Function8",
            9 => "kotlin.jvm.functions.Function9",
            10 => "kotlin.jvm.functions.Function10",
            11 => "kotlin.jvm.functions.Function11",
            12 => "kotlin.jvm.functions.Function12",
            13 => "kotlin.jvm.functions.Function13",
            14 => "kotlin.jvm.functions.Function14",
            15 => "kotlin.jvm.functions.Function15",
            16 => "kotlin.jvm.functions.Function16",
            17 => "kotlin.jvm.functions.Function17",
            18 => "kotlin.jvm.functions.Function18",
            19 => "kotlin.jvm.functions.Function19",
            20 => "kotlin.jvm.functions.Function20",
            21 => "kotlin.jvm.functions.Function21",
            22 => "kotlin.jvm.functions.Function22",
            _ => return None,
        });
    }
    if let Some(primitive) = primitive_of(name) {
        // The *boxed* class: the members of `Int` a classfile knows
        // (`compareTo`, `equals`, `toString`) are `java.lang.Integer`'s.
        return Some(match primitive {
            PrimitiveType::Boolean => "java.lang.Boolean",
            PrimitiveType::Char => "java.lang.Character",
            PrimitiveType::Byte => "java.lang.Byte",
            PrimitiveType::Short => "java.lang.Short",
            PrimitiveType::Int => "java.lang.Integer",
            PrimitiveType::Long => "java.lang.Long",
            PrimitiveType::Float => "java.lang.Float",
            PrimitiveType::Double => "java.lang.Double",
        });
    }
    // `kotlin.Unit` and the library classes (`Pair`, `Lazy`, `Result`, the
    // facades) have classfiles of their own and never reach this table; the
    // mapping exists for the *language* types.
    None
}

/// The Kotlin name of a JVM class a built-in maps to, when `name` is one of
/// them: the reverse of [`BUILTIN_JVM_CLASSES`] (so a receiver spelled
/// `java.util.List` in a classfile is the `kotlin.collections.List` the
/// language named).
pub fn kotlin_name(jvm: &str) -> Option<&'static str> {
    let normalized = jvm.replace('$', ".");
    BUILTIN_JVM_CLASSES
        .iter()
        .find(|(_, class)| *class == normalized)
        .map(|(kotlin, _)| *kotlin)
        .or_else(|| {
            MAPPED_TYPES
                .iter()
                .find(|(java, _)| *java == jvm || *java == normalized)
                .map(|(_, kotlin)| *kotlin)
        })
}

/// The supertypes Kotlin's own declarations give a built-in classifier, beyond
/// what the JVM type's classfile says.
///
/// The collection interfaces are where the two disagree: `MutableList` and
/// `List` have the *same* JVM type (`java.util.List`), so the classfile's
/// supertypes cannot tell them apart, while Kotlin declares
/// `MutableList<E> : List<E>, MutableCollection<E>` — the mutable view of an
/// interface is a subtype of its read-only view, never the other way round
/// (<https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.collections/-mutable-list/>).
/// The pairs are KLS's, read off the standard-library declarations.
pub const BUILTIN_SUPERTYPES: &[(&str, &str)] = &[
    (
        "kotlin.collections.MutableIterable",
        "kotlin.collections.Iterable",
    ),
    (
        "kotlin.collections.MutableIterator",
        "kotlin.collections.Iterator",
    ),
    (
        "kotlin.collections.MutableListIterator",
        "kotlin.collections.ListIterator",
    ),
    (
        "kotlin.collections.MutableCollection",
        "kotlin.collections.Collection",
    ),
    (
        "kotlin.collections.MutableCollection",
        "kotlin.collections.MutableIterable",
    ),
    ("kotlin.collections.MutableList", "kotlin.collections.List"),
    (
        "kotlin.collections.MutableList",
        "kotlin.collections.MutableCollection",
    ),
    ("kotlin.collections.MutableSet", "kotlin.collections.Set"),
    (
        "kotlin.collections.MutableSet",
        "kotlin.collections.MutableCollection",
    ),
    ("kotlin.collections.MutableMap", "kotlin.collections.Map"),
    (
        "kotlin.collections.MutableMap.MutableEntry",
        "kotlin.collections.Map.Entry",
    ),
    // The concrete collections Kotlin declares in terms of the mutable views:
    // `ArrayList<E> : MutableList<E>`, `HashMap<K, V>` and `LinkedHashMap`
    // likewise. Their JVM types only say `java.util.` lists and maps, which the
    // mapping reads as the read-only views.
    (
        "kotlin.collections.ArrayList",
        "kotlin.collections.MutableList",
    ),
    (
        "kotlin.collections.HashMap",
        "kotlin.collections.MutableMap",
    ),
    (
        "kotlin.collections.LinkedHashMap",
        "kotlin.collections.MutableMap",
    ),
    // The read-only views of the collections are the JVM's own hierarchy,
    // which the classfile already declares (`java.util.List` is a
    // `java.util.Collection`); `Map.Entry` has no JVM supertype the mapping
    // keeps, so `kotlin.Any` is recorded here.
    ("kotlin.collections.Map.Entry", "kotlin.Any"),
];

/// The declared supertypes of a built-in Kotlin classifier that the JVM type
/// does not already cover ([`BUILTIN_SUPERTYPES`]).
pub fn declared_supertypes(name: &str) -> Vec<&'static str> {
    BUILTIN_SUPERTYPES
        .iter()
        .filter(|(kotlin, _)| *kotlin == name)
        .map(|(_, supertype)| *supertype)
        .collect()
}

/// The return type of a member the *language* declares on a built-in
/// classifier, when `name` is one: the conversion functions of the numeric
/// types, and the members of `Array` and `String` that have no JVM method
/// behind them.
///
/// KLS
/// `built-in-types-and-their-semantics.html#built-in-integer-types` declares
/// the conversion functions for every built-in numeric type; the compiler
/// implements them as conversions, so no classfile carries them.
pub fn member_return(receiver: &str, member: &str) -> Option<&'static str> {
    if member_is_property(receiver, member) {
        return Some("kotlin.Int");
    }
    let numeric = [
        "kotlin.Byte",
        "kotlin.Short",
        "kotlin.Int",
        "kotlin.Long",
        "kotlin.Float",
        "kotlin.Double",
        "kotlin.Char",
    ];
    if numeric.contains(&receiver) {
        let target = match member {
            "toByte" => "kotlin.Byte",
            "toShort" => "kotlin.Short",
            "toInt" => "kotlin.Int",
            "toLong" => "kotlin.Long",
            "toFloat" => "kotlin.Float",
            "toDouble" => "kotlin.Double",
            "toChar" => "kotlin.Char",
            _ => return None,
        };
        return Some(target);
    }
    match (receiver, member) {
        // `Array.size` is a property of the array itself ([KLS
        // `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)),
        // and `isNotEmpty`/`isEmpty` are declared on it as well.
        ("kotlin.Array", "size") => Some("kotlin.Int"),
        // `String.get`/`CharSequence.get` are the indexing operator's own
        // members; the JVM method is `charAt`.
        ("kotlin.String", "get") | ("kotlin.CharSequence", "get") => Some("kotlin.Char"),
        _ => None,
    }
}

/// The type of a property the *language* declares on a built-in *collection*,
/// built from the receiver's own arguments: `Map.entries`, `Map.keys` and
/// `Map.values` are members of the Kotlin interface, while the classfile spells
/// them `entrySet()`, `keySet()` and `values()` — with the mutable views for a
/// `MutableMap` ([KLS
/// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)
/// names the built-in classifiers; the members are the declarations of the
/// standard library's own interfaces).
pub fn collection_property(
    db: &dyn TyDatabase,
    receiver: &str,
    args: &[Ty],
    member: &str,
) -> Option<Ty> {
    let mutable = receiver == "kotlin.collections.MutableMap";
    if receiver != "kotlin.collections.Map" && !mutable {
        return None;
    }
    let key = args.first().copied();
    let value = args.get(1).copied();
    let reference = |name: &str, args: Vec<Ty>| Ty::reference(db, name, args);
    match member {
        "entries" => {
            let entry = if mutable {
                "kotlin.collections.MutableMap.MutableEntry"
            } else {
                "kotlin.collections.Map.Entry"
            };
            let entry = reference(entry, vec![key?, value?]);
            let set = if mutable {
                "kotlin.collections.MutableSet"
            } else {
                "kotlin.collections.Set"
            };
            Some(reference(set, vec![entry]))
        }
        "keys" => {
            let set = if mutable {
                "kotlin.collections.MutableSet"
            } else {
                "kotlin.collections.Set"
            };
            Some(reference(set, vec![key?]))
        }
        // Kotlin declares the iterator of a *mutable* collection as the mutable
        // one ([`MutableSet.iterator(): MutableIterator<E>`](https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.collections/-mutable-set/)),
        // while the JVM interface's says `java.util.Iterator`.
        "iterator" if mutable => {
            let _ = member;
            None
        }
        "values" => {
            let collection = if mutable {
                "kotlin.collections.MutableCollection"
            } else {
                "kotlin.collections.Collection"
            };
            Some(reference(collection, vec![value?]))
        }
        _ => None,
    }
}

/// The type of a member the *language* declares on a built-in classifier under
/// a name the classfile spells differently — the `@JvmName`-renamed accessors
/// of the standard library, whose Kotlin name lives in the library's
/// `@Metadata` (which this model does not decode) and whose JVM name is all the
/// classpath records.
///
/// `KClass<T>.java` is the one a Kotlin file uses everywhere: the
/// `kotlin.jvm.java` extension property
/// (<https://kotlinlang.org/docs/reflection.html#class-references>) compiles to
/// `JvmClassMappingKt.getJavaClass(KClass)`, so the accessor a name lookup would
/// have to try is `getJavaClass` — not the `getJava` the getter convention
/// derives. The table is what stands in for the metadata, and is a recorded
/// deviation.
pub fn renamed_member_return(
    db: &dyn TyDatabase,
    receiver: &str,
    args: &[Ty],
    member: &str,
) -> Option<Ty> {
    match (receiver, member) {
        // `val <T : Any> KClass<T>.java: Class<T>` — the classfile's
        // `getJavaClass`.
        ("kotlin.reflect.KClass", "java") => Some(Ty::reference(
            db,
            "java.lang.Class",
            vec![args.first().copied()?],
        )),
        _ => None,
    }
}

/// The `iterator()` a *mutable* collection declares, which Kotlin types as the
/// mutable iterator — the JVM interface's own method returns `java.util.Iterator`
/// ([KLS
/// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)
/// names the classifiers; the members are the standard library's).
pub fn mutable_iterator(
    db: &dyn TyDatabase,
    receiver: &str,
    args: &[Ty],
    member: &str,
) -> Option<Ty> {
    if member != "iterator"
        || !matches!(
            receiver,
            "kotlin.collections.MutableSet"
                | "kotlin.collections.MutableList"
                | "kotlin.collections.MutableCollection"
        )
    {
        return None;
    }
    Some(Ty::reference(
        db,
        "kotlin.collections.MutableIterator",
        vec![*args.first()?],
    ))
}

/// Whether `(receiver, member)` is one of the collection properties
/// [`collection_property`] answers — read with no argument list.
pub fn is_collection_property(receiver: &str, member: &str) -> bool {
    matches!(
        (receiver, member),
        ("kotlin.collections.Map", "entries" | "keys" | "values")
            | (
                "kotlin.collections.MutableMap",
                "entries" | "keys" | "values"
            )
    )
}

/// Whether the member the *language* declares for `(receiver, member)` is a
/// *property* (read with no argument list) rather than a function.
pub fn member_is_property(receiver: &str, member: &str) -> bool {
    member == "size" && (receiver == "kotlin.Array" || primitive_array_element(receiver).is_some())
}

/// The JVM type a built-in Kotlin classifier is looked up through: its mapped
/// class ([`jvm_class`]) with the receiver's own type arguments, or the JVM
/// *array* of its element for `kotlin.Array`.
///
/// The arguments are the receiver's, unrewritten: they denote the same types
/// either way ([`super::ty::ty_from_java`] is what makes a classfile spelling
/// Kotlin's again), and a classfile member's `E` unifies with the Kotlin
/// argument it is instantiated at.
pub fn jvm_ty(db: &dyn TyDatabase, ty: Ty) -> Option<Ty> {
    let TyKind::Reference { name, args, .. } = ty.kind(db) else {
        return None;
    };
    if let Some(element) = primitive_array_element(name.as_str()) {
        return Some(Ty::array(db, Ty::primitive(db, element)));
    }
    if name.as_str() == "kotlin.Array" {
        let element = args.first().copied()?;
        return Some(Ty::array(db, element));
    }
    let class = jvm_class(name.as_str())?;
    Some(Ty::reference(db, class, args.to_vec()))
}
