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
use crate::java::db::TyDatabase;
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
/// The conversion is the compiler's, in three steps:
///
/// * a primitive is the Kotlin classifier it maps onto — `int` is
///   `kotlin.Int`, `void` is `kotlin.Unit`
///   ([`MAPPED_TYPES`] names the reference mappings);
/// * a *reference* name is rewritten through the mapping table, so
///   `java.lang.String` is `kotlin.String` and `java.util.List` is
///   `kotlin.collections.List`;
/// * a reference type coming from a **classfile** declaration — one with no
///   Kotlin source, so the compiler has no nullability information about it —
///   is wrapped in the platform type `T!`, which is the flexible type
///   `T..T?` ([KLS
///   `type-system.html#flexible-types`](https://kotlinlang.org/spec/type-system.html#flexible-types)).
///   A *Java source* type is not wrapped: within a mixed source set the
///   compiler reads the Java source's annotations, and this model carries no
///   `@Nullable`-equivalent for it — a recorded deviation, and one that only
///   under-reports nullability (a Java source type stays usable from Kotlin
///   without an unsafe-call warning either way).
pub fn ty_from_java(db: &dyn TyDatabase, ty: Ty) -> Ty {
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
                PrimitiveType::Void => "Unit",
            };
            Ty::reference(db, format!("kotlin.{name}"), Vec::new())
        }
        // `void` is not a primitive of the *Kotlin* model: it is `kotlin.Unit`
        // ([KLS
        // `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)).
        TyKind::Void => Ty::reference(db, "kotlin.Unit", Vec::new()),
        TyKind::Reference { name, args, local } => {
            let args = args.iter().map(|arg| ty_from_java(db, *arg)).collect();
            // A local class keeps its declaration: it is the type's identity
            // ([JLS §6.7]), and a local class has no canonical name to map.
            let mapped = match local {
                Some(class) => Ty::local_reference(db, *class, mapped_type_name(name), args),
                None => Ty::reference(db, mapped_type_name(name), args),
            };
            if local.is_none() && !name.as_str().starts_with("kotlin.") {
                // A classfile type: the compiler knows nothing about its
                // nullability, so it is a platform type.
                let upper = Ty::nullable(db, mapped);
                Ty::flexible(db, mapped, upper)
            } else {
                mapped
            }
        }
        TyKind::Array(inner) => Ty::array(db, ty_from_java(db, **inner)),
        TyKind::TypeVar { .. } => ty,
        // Everything else is either a Kotlin-only form (which a Java type
        // cannot be) or already Kotlin's.
        _ => ty,
    }
}

/// The mapped classifier a Java reference name denotes ([KLS
/// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)
/// names the classifiers the compiler maps onto JVM types; the mapping itself
/// is spelled out in <https://kotlinlang.org/docs/java-interop.html#mapped-types>,
/// which KLS does not cover). A name not in the table is itself.
pub(crate) fn mapped_type_name(name: &Name) -> Name {
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
    ("java.lang.Iterable", "kotlin.collections.Iterable"),
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
    ("java.util.List", "kotlin.collections.List"),
    ("java.util.Set", "kotlin.collections.Set"),
    ("java.util.Map", "kotlin.collections.Map"),
    ("java.util.Map$Entry", "kotlin.collections.Map.Entry"),
    ("java.util.Collection", "kotlin.collections.Collection"),
    ("java.util.Iterator", "kotlin.collections.Iterator"),
    ("java.util.ListIterator", "kotlin.collections.ListIterator"),
    ("java.util.ArrayList", "kotlin.collections.ArrayList"),
    ("java.util.HashMap", "kotlin.collections.HashMap"),
    (
        "java.util.LinkedHashMap",
        "kotlin.collections.LinkedHashMap",
    ),
];

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
