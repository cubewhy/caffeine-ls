//! The type model.
//!
//! [`Ty`] is the internal representation of a Java type, following the
//! taxonomy of [JLS §4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.1):
//! primitive types ([§4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.2)),
//! reference types ([§4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3)),
//! type variables ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
//! parameterized types ([§4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5))
//! and array types ([§10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.1)).
//!
//! Reference types carry a canonical fully qualified name ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)).
//! For source types the FQN is produced by [`crate::resolve`]; for library
//! types it comes straight out of the classfile stubs.
//!
//! [`Ty`] values are interned in the salsa database: each distinct
//! [`TyKind`] maps to one id, so a [`Ty`] is a cheap `Copy` handle with
//! `O(1)` equality that can key the memoized subtype/supertype queries in
//! [`crate::subtyping`]. Every accessor therefore takes the database.

use std::fmt;

use hir_expand::name::Name;
use syntax::stub::{PrimitiveType, TypeBound, TypeRef};

use crate::db::TyDatabase;

/// A Java type. See the [module docs](self) for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ty {
    pub id: TyData,
}

/// The interned data of a [`Ty`]: salsa maps each distinct [`TyKind`] to a
/// unique id for the database's lifetime. Uses `no_lifetime` because the
/// fields are all `'static` (children are interned [`Ty`] handles), keeping
/// the handle itself free of a database lifetime.
#[salsa::interned(unsafe(no_lifetime), debug, revisions = usize::MAX)]
pub struct TyData {
    pub kind: TyKind,
}

/// The kind of a [`Ty`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TyKind {
    /// The `void` type ([JLS §4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3)).
    Void,
    /// A primitive type ([JLS §4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.2)).
    Primitive(PrimitiveType),
    /// A reference type `name<args>` with a canonical FQN name. `args` is
    /// empty for non-generic and raw types
    /// ([§4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3),
    /// [§4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5),
    /// [§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)).
    Reference { name: Name, args: Vec<Ty> },
    /// A type variable ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)).
    TypeVar(Name),
    /// An array type ([JLS §10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.1)).
    Array(Box<Ty>),
    /// A wildcard type argument `?`, `? extends T` or `? super T`
    /// ([JLS §4.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5.1)).
    Wildcard(Option<Box<WildcardBound>>),
    /// An unresolved or malformed type (a compile-time error per
    /// [JLS §4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.1)).
    Error,
}

/// A wildcard bound: `? extends T` ([`BoundKind::Upper`]) or `? super T`
/// ([`BoundKind::Lower`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WildcardBound {
    pub kind: BoundKind,
    pub ty: Ty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoundKind {
    Upper,
    Lower,
}

impl Ty {
    fn new(db: &dyn TyDatabase, kind: TyKind) -> Self {
        Self {
            id: TyData::new(db, kind),
        }
    }

    pub fn void(db: &dyn TyDatabase) -> Self {
        Self::new(db, TyKind::Void)
    }

    pub fn primitive(db: &dyn TyDatabase, p: PrimitiveType) -> Self {
        Self::new(db, TyKind::Primitive(p))
    }

    pub fn reference(db: &dyn TyDatabase, name: impl Into<Name>, args: Vec<Ty>) -> Self {
        Self::new(
            db,
            TyKind::Reference {
                name: name.into(),
                args,
            },
        )
    }

    pub fn type_var(db: &dyn TyDatabase, name: impl Into<Name>) -> Self {
        Self::new(db, TyKind::TypeVar(name.into()))
    }

    pub fn array(db: &dyn TyDatabase, inner: Ty) -> Self {
        Self::new(db, TyKind::Array(Box::new(inner)))
    }

    pub fn wildcard(db: &dyn TyDatabase, bound: Option<Box<WildcardBound>>) -> Self {
        Self::new(db, TyKind::Wildcard(bound))
    }

    pub fn error(db: &dyn TyDatabase) -> Self {
        Self::new(db, TyKind::Error)
    }

    /// The kind of this type.
    pub fn kind<'a>(&self, db: &'a dyn TyDatabase) -> &'a TyKind {
        self.id.kind(db)
    }

    pub fn is_void(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Void)
    }

    pub fn is_primitive(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Primitive(_))
    }

    pub fn is_reference(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Reference { .. })
    }

    pub fn is_type_var(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::TypeVar(_))
    }

    pub fn is_array(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Array(_))
    }

    pub fn is_wildcard(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Wildcard(_))
    }

    pub fn is_error(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Error)
    }

    /// Whether this is exactly the type `java.lang.Object`, the root of the
    /// reference type hierarchy ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)).
    pub fn is_object(&self, db: &dyn TyDatabase) -> bool {
        matches!(
            self.kind(db),
            TyKind::Reference { name, args } if name.as_str() == "java.lang.Object" && args.is_empty()
        )
    }

    /// `(name, args)` if this is a reference type.
    pub fn as_reference<'a>(&self, db: &'a dyn TyDatabase) -> Option<(&'a Name, &'a [Ty])> {
        match self.kind(db) {
            TyKind::Reference { name, args } => Some((name, args)),
            _ => None,
        }
    }

    /// The element type if this is an array type.
    pub fn element<'a>(&self, db: &'a dyn TyDatabase) -> Option<&'a Ty> {
        match self.kind(db) {
            TyKind::Array(inner) => Some(inner),
            _ => None,
        }
    }

    /// The erasure of this type ([JLS §4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6)):
    /// type arguments are dropped and a type variable erases to its leftmost
    /// bound, or `java.lang.Object` when no bound is known. [`Ty`] does not
    /// carry declared bounds, so the bound is approximated by `Object`.
    pub fn erasure(&self, db: &dyn TyDatabase) -> Ty {
        match self.kind(db) {
            TyKind::Reference { name, .. } => Ty::reference(db, name.clone(), Vec::new()),
            TyKind::Array(inner) => Ty::array(db, inner.erasure(db)),
            TyKind::TypeVar(_) => Ty::reference(db, "java.lang.Object", Vec::new()),
            other => Self::new(db, other.clone()),
        }
    }

    /// Formats this type for display. [`fmt::Display`] cannot be implemented
    /// directly because rendering needs the database.
    pub fn display<'a>(&'a self, db: &'a dyn TyDatabase) -> TyDisplay<'a> {
        TyDisplay { ty: self, db }
    }
}

/// A displayable view of a [`Ty`], produced by [`Ty::display`].
pub struct TyDisplay<'a> {
    ty: &'a Ty,
    db: &'a dyn TyDatabase,
}

impl fmt::Display for TyDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty.kind(self.db) {
            TyKind::Void => f.write_str("void"),
            TyKind::Primitive(p) => f.write_str(primitive_name(*p)),
            TyKind::Reference { name, args } => {
                f.write_str(name.as_str())?;
                if !args.is_empty() {
                    f.write_str("<")?;
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{}", arg.display(self.db))?;
                    }
                    f.write_str(">")?;
                }
                Ok(())
            }
            TyKind::TypeVar(v) => f.write_str(v.as_str()),
            TyKind::Array(inner) => write!(f, "{}[]", inner.display(self.db)),
            TyKind::Wildcard(bound) => {
                f.write_str("?")?;
                if let Some(bound) = bound {
                    match bound.kind {
                        BoundKind::Upper => write!(f, " extends {}", bound.ty.display(self.db))?,
                        BoundKind::Lower => write!(f, " super {}", bound.ty.display(self.db))?,
                    }
                }
                Ok(())
            }
            TyKind::Error => f.write_str("<error>"),
        }
    }
}

/// The display name of a primitive type.
pub(crate) fn primitive_name(p: PrimitiveType) -> &'static str {
    match p {
        PrimitiveType::Int => "int",
        PrimitiveType::Long => "long",
        PrimitiveType::Float => "float",
        PrimitiveType::Double => "double",
        PrimitiveType::Boolean => "boolean",
        PrimitiveType::Byte => "byte",
        PrimitiveType::Char => "char",
        PrimitiveType::Short => "short",
        PrimitiveType::Void => "void",
    }
}

/// Lowers a [`TypeRef`] into a [`Ty`], mapping names with `name`. Reference
/// names are used verbatim (no resolution): use
/// [`crate::resolve::resolve_type_ref`] for source-side resolution, and this
/// for library `TypeRef<Symbol>`s whose names are already fully qualified.
pub fn ty_from_type_ref<N>(
    db: &dyn TyDatabase,
    tyref: &TypeRef<N>,
    name: &mut dyn FnMut(&N) -> Name,
) -> Ty {
    match tyref {
        TypeRef::Primitive(p) => Ty::primitive(db, *p),
        TypeRef::Reference {
            name: n,
            generic_args,
        } => {
            let args = generic_args
                .iter()
                .map(|arg| ty_from_type_ref(db, arg, name))
                .collect();
            Ty::reference(db, name(n), args)
        }
        TypeRef::Wildcard { bound } => Ty::wildcard(
            db,
            bound.as_deref().map(|b| match b {
                TypeBound::Upper(t) => Box::new(WildcardBound {
                    kind: BoundKind::Upper,
                    ty: ty_from_type_ref(db, t, name),
                }),
                TypeBound::Lower(t) => Box::new(WildcardBound {
                    kind: BoundKind::Lower,
                    ty: ty_from_type_ref(db, t, name),
                }),
            }),
        ),
        TypeRef::TypeVariable(v) => Ty::type_var(db, name(v)),
        TypeRef::Array(inner) => Ty::array(db, ty_from_type_ref(db, inner, name)),
        TypeRef::Error => Ty::error(db),
    }
}

/// Lowers a source [`TypeRef<Name>`] without name resolution (names kept
/// verbatim).
pub fn ty_from_source(db: &dyn TyDatabase, tyref: &TypeRef<Name>) -> Ty {
    ty_from_type_ref(db, tyref, &mut |n| n.clone())
}
