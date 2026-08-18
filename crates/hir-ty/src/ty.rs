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

use std::fmt;

use hir_expand::name::Name;
use syntax::stub::{PrimitiveType, TypeBound, TypeRef};

/// A Java type. See the [module docs](self) for the model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ty {
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
    pub fn void() -> Self {
        Self { kind: TyKind::Void }
    }

    pub fn primitive(p: PrimitiveType) -> Self {
        Self {
            kind: TyKind::Primitive(p),
        }
    }

    pub fn reference(name: impl Into<Name>, args: Vec<Ty>) -> Self {
        Self {
            kind: TyKind::Reference {
                name: name.into(),
                args,
            },
        }
    }

    pub fn type_var(name: impl Into<Name>) -> Self {
        Self {
            kind: TyKind::TypeVar(name.into()),
        }
    }

    pub fn array(inner: Ty) -> Self {
        Self {
            kind: TyKind::Array(Box::new(inner)),
        }
    }

    pub fn wildcard(bound: Option<Box<WildcardBound>>) -> Self {
        Self {
            kind: TyKind::Wildcard(bound),
        }
    }

    pub fn error() -> Self {
        Self {
            kind: TyKind::Error,
        }
    }

    pub fn is_void(&self) -> bool {
        matches!(self.kind, TyKind::Void)
    }

    pub fn is_primitive(&self) -> bool {
        matches!(self.kind, TyKind::Primitive(_))
    }

    pub fn is_reference(&self) -> bool {
        matches!(self.kind, TyKind::Reference { .. })
    }

    pub fn is_type_var(&self) -> bool {
        matches!(self.kind, TyKind::TypeVar(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self.kind, TyKind::Array(_))
    }

    pub fn is_wildcard(&self) -> bool {
        matches!(self.kind, TyKind::Wildcard(_))
    }

    pub fn is_error(&self) -> bool {
        matches!(self.kind, TyKind::Error)
    }

    /// Whether this is exactly the type `java.lang.Object`, the root of the
    /// reference type hierarchy ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)).
    pub fn is_object(&self) -> bool {
        matches!(
            &self.kind,
            TyKind::Reference { name, args } if name.as_str() == "java.lang.Object" && args.is_empty()
        )
    }

    /// `(name, args)` if this is a reference type.
    pub fn as_reference(&self) -> Option<(&Name, &[Ty])> {
        match &self.kind {
            TyKind::Reference { name, args } => Some((name, args)),
            _ => None,
        }
    }

    /// The element type if this is an array type.
    pub fn element(&self) -> Option<&Ty> {
        match &self.kind {
            TyKind::Array(inner) => Some(inner),
            _ => None,
        }
    }

    /// The erasure of this type ([JLS §4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6)):
    /// type arguments are dropped and a type variable erases to its leftmost
    /// bound, or `java.lang.Object` when no bound is known. [`Ty`] does not
    /// carry declared bounds, so the bound is approximated by `Object`.
    pub fn erasure(&self) -> Ty {
        match &self.kind {
            TyKind::Reference { name, .. } => Ty::reference(name.clone(), Vec::new()),
            TyKind::Array(inner) => Ty::array(inner.erasure()),
            TyKind::TypeVar(_) => Ty::reference("java.lang.Object", Vec::new()),
            other => Self {
                kind: other.clone(),
            },
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
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
                        write!(f, "{arg}")?;
                    }
                    f.write_str(">")?;
                }
                Ok(())
            }
            TyKind::TypeVar(v) => f.write_str(v.as_str()),
            TyKind::Array(inner) => write!(f, "{inner}[]"),
            TyKind::Wildcard(bound) => {
                f.write_str("?")?;
                if let Some(bound) = bound {
                    match bound.kind {
                        BoundKind::Upper => write!(f, " extends {}", bound.ty)?,
                        BoundKind::Lower => write!(f, " super {}", bound.ty)?,
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
pub fn ty_from_type_ref<N>(tyref: &TypeRef<N>, name: &mut dyn FnMut(&N) -> Name) -> Ty {
    match tyref {
        TypeRef::Primitive(p) => Ty::primitive(*p),
        TypeRef::Reference {
            name: n,
            generic_args,
        } => {
            let args = generic_args
                .iter()
                .map(|arg| ty_from_type_ref(arg, name))
                .collect();
            Ty::reference(name(n), args)
        }
        TypeRef::Wildcard { bound } => Ty::wildcard(bound.as_deref().map(|b| match b {
            TypeBound::Upper(t) => Box::new(WildcardBound {
                kind: BoundKind::Upper,
                ty: ty_from_type_ref(t, name),
            }),
            TypeBound::Lower(t) => Box::new(WildcardBound {
                kind: BoundKind::Lower,
                ty: ty_from_type_ref(t, name),
            }),
        })),
        TypeRef::TypeVariable(v) => Ty::type_var(name(v)),
        TypeRef::Array(inner) => Ty::array(ty_from_type_ref(inner, name)),
        TypeRef::Error => Ty::error(),
    }
}

/// Lowers a source [`TypeRef<Name>`] without name resolution (names kept
/// verbatim).
pub fn ty_from_source(tyref: &TypeRef<Name>) -> Ty {
    ty_from_type_ref(tyref, &mut |n| n.clone())
}
