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
use rustc_hash::FxHashMap;
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
    /// The null type ([JLS §4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.1),
    /// [§3.10.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.10.8)):
    /// the type of the null literal, a subtype of every reference and array
    /// type ([§4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2),
    /// [§4.10.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.3)).
    Null,
    /// A primitive type ([JLS §4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.2)).
    Primitive(PrimitiveType),
    /// A reference type `name<args>` with a canonical FQN name. `args` is
    /// empty for non-generic and raw types
    /// ([§4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3),
    /// [§4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5),
    /// [§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)).
    Reference { name: Name, args: Vec<Ty> },
    /// A type variable ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
    /// with its declared bounds ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)).
    /// `bounds` is empty for unbounded type variables and for re-entrant
    /// (recursive) references — the cycle guard in [`crate::resolve`] erases
    /// bounds on re-entry so interning terminates. `lower` is set only for the
    /// fresh type variables of capture conversion
    /// ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// `? super T` captures to a variable with the `Object` upper bound and
    /// the `T` lower bound.
    TypeVar {
        name: Name,
        bounds: Vec<Ty>,
        lower: Option<Ty>,
    },
    /// An array type ([JLS §10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.1)).
    Array(Box<Ty>),
    /// A wildcard type argument `?`, `? extends T` or `? super T`
    /// ([JLS §4.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5.1)).
    Wildcard(Option<Box<WildcardBound>>),
    /// An intersection type `A & B` ([JLS §4.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.9)),
    /// produced by the least upper bound computation
    /// ([§4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)) —
    /// `lub(U1, ..., Uk) = Best(W1) & ... & Best(Wr)` — and by the greatest
    /// lower bound used in capture conversion ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)).
    /// Java has no intersection type literal; the type is a compiler-internal
    /// projection.
    Intersection(Vec<Ty>),
    /// An inference variable ([JLS §18.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.1.1)),
    /// created fresh per method invocation type inference ([JLS §18.5.2]) from
    /// the session-wide id counter ([`HirState::next_infer_var`]). Ids are
    /// unique for the database's lifetime, so no two invocations ever share an
    /// inference variable. Such types exist only inside a single `pick_method`
    /// call and must never reach the memoized subtype/supertype queries.
    InferenceVar(u64),
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

    pub fn null(db: &dyn TyDatabase) -> Self {
        Self::new(db, TyKind::Null)
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

    pub fn type_var(db: &dyn TyDatabase, name: impl Into<Name>, bounds: Vec<Ty>) -> Self {
        Self::new(
            db,
            TyKind::TypeVar {
                name: name.into(),
                bounds,
                lower: None,
            },
        )
    }

    /// A capture variable ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// a fresh type variable with the `Object` upper bound and the `lower`
    /// bound (the `? super T` capture).
    pub(crate) fn captured_var(db: &dyn TyDatabase, lower: Ty) -> Self {
        let name = format!(
            "CAP#{}",
            NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        Self::new(
            db,
            TyKind::TypeVar {
                name: Name::new(&name),
                bounds: vec![Ty::reference(db, "java.lang.Object", Vec::new())],
                lower: Some(lower),
            },
        )
    }

    pub fn array(db: &dyn TyDatabase, inner: Ty) -> Self {
        Self::new(db, TyKind::Array(Box::new(inner)))
    }

    pub fn wildcard(db: &dyn TyDatabase, bound: Option<Box<WildcardBound>>) -> Self {
        // §4.5.1: `? extends Object` is equivalent to the unbounded wildcard `?`.
        let bound = match bound {
            Some(b) if b.kind == BoundKind::Upper && b.ty.is_object(db) => None,
            other => other,
        };
        Self::new(db, TyKind::Wildcard(bound))
    }

    /// An intersection type `A & B` ([JLS §4.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.9)),
    /// produced by [`crate::least_upper_bound`]
    /// ([§4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)).
    pub fn intersection(db: &dyn TyDatabase, members: Vec<Ty>) -> Self {
        match members.len() {
            0 => Self::error(db),
            1 => members[0],
            _ => Self::new(db, TyKind::Intersection(members)),
        }
    }

    /// A fresh inference variable ([JLS §18.1.1]), unique for the session.
    pub fn infer_var(db: &dyn TyDatabase) -> Self {
        let mut next = db.hir_state().next_infer_var.lock().unwrap();
        let id = *next;
        *next += 1;
        Self::new(db, TyKind::InferenceVar(id))
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

    pub fn is_null(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Null)
    }

    pub fn is_primitive(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Primitive(_))
    }

    pub fn is_reference(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Reference { .. })
    }

    pub fn is_type_var(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::TypeVar { .. })
    }

    pub fn is_array(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Array(_))
    }

    pub fn is_wildcard(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Wildcard(_))
    }

    /// Whether this is exactly an inference variable ([JLS §18.1.1]).
    pub fn is_infer_var(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::InferenceVar(_))
    }

    /// The id of this inference variable, if it is one.
    pub fn as_infer_var(&self, db: &dyn TyDatabase) -> Option<u64> {
        match self.kind(db) {
            TyKind::InferenceVar(id) => Some(*id),
            _ => None,
        }
    }

    /// Whether any nested type argument is an inference variable.
    pub fn contains_infer_var(&self, db: &dyn TyDatabase) -> bool {
        match self.kind(db) {
            TyKind::InferenceVar(_) => true,
            TyKind::Reference { args, .. } => args.iter().any(|arg| arg.contains_infer_var(db)),
            TyKind::Array(inner) => inner.contains_infer_var(db),
            TyKind::Wildcard(bound) => bound
                .as_deref()
                .is_some_and(|b| b.ty.contains_infer_var(db)),
            TyKind::Intersection(members) => members.iter().any(|m| m.contains_infer_var(db)),
            _ => false,
        }
    }

    /// Whether any nested component is a type variable.
    pub fn contains_type_var(&self, db: &dyn TyDatabase) -> bool {
        match self.kind(db) {
            TyKind::TypeVar { .. } => true,
            TyKind::Reference { args, .. } => args.iter().any(|arg| arg.contains_type_var(db)),
            TyKind::Array(inner) => inner.contains_type_var(db),
            TyKind::Wildcard(bound) => bound.as_deref().is_some_and(|b| b.ty.contains_type_var(db)),
            TyKind::Intersection(members) => members.iter().any(|m| m.contains_type_var(db)),
            _ => false,
        }
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

    /// The declared bounds of this type variable
    /// ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
    /// or an empty slice for non-type-variable types.
    pub fn bounds<'a>(&self, db: &'a dyn TyDatabase) -> &'a [Ty] {
        match self.kind(db) {
            TyKind::TypeVar { bounds, .. } => bounds,
            _ => &[],
        }
    }

    /// The lower bound of this (capture) type variable
    /// ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)),
    /// or `None` for ordinary type variables.
    pub fn lower<'a>(&self, db: &'a dyn TyDatabase) -> Option<Ty> {
        match self.kind(db) {
            TyKind::TypeVar { lower, .. } => *lower,
            _ => None,
        }
    }

    /// Replaces every type variable named in `binding` with its type argument.
    /// Used to instantiate the supertypes of a parameterized type
    /// ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)):
    /// the classfile signature of `ArrayList<E>` declares `extends AbstractList<E>`,
    /// and substituting `E → String` gives `AbstractList<String>`.
    pub fn substitute(&self, db: &dyn TyDatabase, binding: &FxHashMap<Name, Ty>) -> Ty {
        match self.kind(db) {
            TyKind::Void | TyKind::Null | TyKind::Primitive(_) | TyKind::Error => *self,
            TyKind::TypeVar { name, .. } => binding.get(name).copied().unwrap_or(*self),
            TyKind::Reference { name, args } => {
                let args: Vec<Ty> = args.iter().map(|arg| arg.substitute(db, binding)).collect();
                Ty::reference(db, name.clone(), args)
            }
            TyKind::Array(inner) => Ty::array(db, inner.substitute(db, binding)),
            TyKind::Wildcard(bound) => Ty::wildcard(
                db,
                bound.as_deref().map(|b| {
                    Box::new(WildcardBound {
                        kind: b.kind,
                        ty: b.ty.substitute(db, binding),
                    })
                }),
            ),
            TyKind::Intersection(members) => Ty::intersection(
                db,
                members.iter().map(|m| m.substitute(db, binding)).collect(),
            ),
            TyKind::InferenceVar(_) => *self,
        }
    }

    /// Replaces every inference variable ([`TyKind::InferenceVar`]) whose id is
    /// in `subst` with its instantiation. Used to apply the resolved
    /// substitution of invocation type inference
    /// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4))
    /// to the formal and return types of a generic method.
    pub fn substitute_infer(&self, db: &dyn TyDatabase, subst: &FxHashMap<u64, Ty>) -> Ty {
        match self.kind(db) {
            TyKind::InferenceVar(id) => subst.get(id).copied().unwrap_or(*self),
            TyKind::Reference { name, args } => Ty::reference(
                db,
                name.clone(),
                args.iter()
                    .map(|arg| arg.substitute_infer(db, subst))
                    .collect(),
            ),
            TyKind::Array(inner) => Ty::array(db, inner.substitute_infer(db, subst)),
            TyKind::Wildcard(bound) => Ty::wildcard(
                db,
                bound.as_deref().map(|b| {
                    Box::new(WildcardBound {
                        kind: b.kind,
                        ty: b.ty.substitute_infer(db, subst),
                    })
                }),
            ),
            TyKind::Intersection(members) => Ty::intersection(
                db,
                members
                    .iter()
                    .map(|m| m.substitute_infer(db, subst))
                    .collect(),
            ),
            _ => *self,
        }
    }

    /// Replaces every inference variable ([`TyKind::InferenceVar`]) with
    /// `java.lang.Object`, erasing the still-unresolved unknowns of an
    /// inference table. Used by the estimate pass of bound set resolution
    /// ([JLS §18.4]) to break cyclic dependencies between variables.
    pub fn erase_infer_vars(&self, db: &dyn TyDatabase) -> Ty {
        match self.kind(db) {
            TyKind::InferenceVar(_) => Ty::reference(db, "java.lang.Object", Vec::new()),
            TyKind::Reference { name, args } => Ty::reference(
                db,
                name.clone(),
                args.iter().map(|arg| arg.erase_infer_vars(db)).collect(),
            ),
            TyKind::Array(inner) => Ty::array(db, inner.erase_infer_vars(db)),
            TyKind::Wildcard(bound) => Ty::wildcard(
                db,
                bound.as_deref().map(|b| {
                    Box::new(WildcardBound {
                        kind: b.kind,
                        ty: b.ty.erase_infer_vars(db),
                    })
                }),
            ),
            TyKind::Intersection(members) => {
                Ty::intersection(db, members.iter().map(|m| m.erase_infer_vars(db)).collect())
            }
            _ => *self,
        }
    }

    /// The erasure of this type ([JLS §4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6)):
    /// type arguments are dropped and a type variable erases to its leftmost
    /// bound, or `java.lang.Object` when it has no bounds
    /// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)).
    pub fn erasure(&self, db: &dyn TyDatabase) -> Ty {
        match self.kind(db) {
            TyKind::Reference { name, .. } => Ty::reference(db, name.clone(), Vec::new()),
            TyKind::Array(inner) => Ty::array(db, inner.erasure(db)),
            TyKind::TypeVar { bounds, .. } => bounds
                .first()
                .map(|bound| bound.erasure(db))
                .unwrap_or_else(|| Ty::reference(db, "java.lang.Object", Vec::new())),
            // The erasure of an intersection type is the erasure of its
            // first member (§4.9).
            TyKind::Intersection(members) => members
                .first()
                .map(|member| member.erasure(db))
                .unwrap_or_else(|| Ty::reference(db, "java.lang.Object", Vec::new())),
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
            TyKind::Null => f.write_str("null"),
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
            TyKind::TypeVar { name, .. } => f.write_str(name.as_str()),
            TyKind::Array(inner) => write!(f, "{}[]", inner.display(self.db)),
            TyKind::Intersection(members) => {
                for (i, member) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" & ")?;
                    }
                    write!(f, "{}", member.display(self.db))?;
                }
                Ok(())
            }
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
            TyKind::InferenceVar(id) => write!(f, "?{id}"),
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

/// The reference type a primitive boxes to ([JLS §5.1.7], table 5.1-D).
pub(crate) fn boxed_type(p: PrimitiveType) -> &'static str {
    match p {
        PrimitiveType::Boolean => "java.lang.Boolean",
        PrimitiveType::Byte => "java.lang.Byte",
        PrimitiveType::Short => "java.lang.Short",
        PrimitiveType::Char => "java.lang.Character",
        PrimitiveType::Int => "java.lang.Integer",
        PrimitiveType::Long => "java.lang.Long",
        PrimitiveType::Float => "java.lang.Float",
        PrimitiveType::Double => "java.lang.Double",
        PrimitiveType::Void => "java.lang.Void",
    }
}

/// The primitive a reference type unboxes to ([JLS §5.1.8], reverse of
/// [`boxed_type`]), or `None` for non-boxed reference types.
pub(crate) fn unboxed_primitive(fqn: &str) -> Option<PrimitiveType> {
    use PrimitiveType::*;
    match fqn {
        "java.lang.Boolean" => Some(Boolean),
        "java.lang.Byte" => Some(Byte),
        "java.lang.Short" => Some(Short),
        "java.lang.Character" => Some(Char),
        "java.lang.Integer" => Some(Int),
        "java.lang.Long" => Some(Long),
        "java.lang.Float" => Some(Float),
        "java.lang.Double" => Some(Double),
        "java.lang.Void" => Some(Void),
        _ => None,
    }
}

/// Unary numeric promotion ([§5.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1)):
/// `byte`, `short` and `char` promote to `int`; the other numeric types keep
/// their type. Applied to the unboxed operand of a binary expression
/// ([§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2),
/// [§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8)),
/// so `Character + Character` promotes to `int`.
pub(crate) fn numeric_promotion(p: PrimitiveType) -> PrimitiveType {
    use PrimitiveType::*;
    match p {
        Byte | Short | Char => Int,
        other => other,
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
        TypeRef::TypeVariable(v) => Ty::type_var(db, name(v), Vec::new()),
        TypeRef::Array(inner) => Ty::array(db, ty_from_type_ref(db, inner, name)),
        TypeRef::Error => Ty::error(db),
    }
}

/// Lowers a source [`TypeRef<Name>`] without name resolution (names kept
/// verbatim).
pub fn ty_from_source(db: &dyn TyDatabase, tyref: &TypeRef<Name>) -> Ty {
    ty_from_type_ref(db, tyref, &mut |n| n.clone())
}

/// The next capture-variable name: capture variables are ordinary type
/// variables ([`TyKind::TypeVar`]) interning by name, so distinct captures
/// must not share a name.
static NEXT_CAPTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Capture conversion ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10)):
/// replaces the wildcard type arguments of `ty` with fresh type variables,
/// so the member set of a wildcard-parameterized receiver sees a concrete
/// instantiation. `? extends T` becomes the fresh variable `CAP#<n>` bounded
/// by `T`; an unbounded `?` becomes `CAP#<n>` bounded by `Object`; `? super T`
/// becomes `CAP#<n>` bounded above by `Object` and below by `T`. Applied to
/// the receiver before the member set walk of
/// [`crate::method::member_set`], and only there: the capture variables never
/// reach the memoized subtype queries.
pub fn capture_conversion(db: &dyn TyDatabase, ty: Ty) -> Ty {
    let fresh = |bound: Ty| {
        let id = NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ty::type_var(
            db,
            Name::new(&format!("CAP#{id}")),
            vec![capture_conversion(db, bound)],
        )
    };
    match ty.kind(db) {
        TyKind::Reference { name, args } => Ty::reference(
            db,
            name.clone(),
            args.iter()
                .map(|arg| capture_conversion(db, *arg))
                .collect(),
        ),
        TyKind::Array(inner) => Ty::array(db, capture_conversion(db, **inner)),
        TyKind::Wildcard(Some(bound)) => match bound.kind {
            BoundKind::Upper => fresh(bound.ty),
            // `? super T`: a capture variable with the `Object` upper bound
            // and the `T` lower bound (§5.1.10).
            BoundKind::Lower => Ty::captured_var(db, capture_conversion(db, bound.ty)),
        },
        TyKind::Wildcard(None) => fresh(Ty::reference(db, "java.lang.Object", Vec::new())),
        // Intersection and type-variable receivers are not parameterized by
        // wildcards; leave them as-is.
        _ => ty,
    }
}
