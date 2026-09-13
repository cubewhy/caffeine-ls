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
