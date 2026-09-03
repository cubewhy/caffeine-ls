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
//! For source types the FQN is produced by [`crate::java::resolve`]; for library
//! types it comes straight out of the classfile stubs.
//!
//! [`Ty`] values are interned in the salsa database: each distinct
//! [`TyKind`] maps to one id, so a [`Ty`] is a cheap `Copy` handle with
//! `O(1)` equality that can key the memoized subtype/supertype queries in
//! [`crate::java::subtyping`]. Every accessor therefore takes the database.

use std::fmt;

use hir_expand::name::Name;
use rustc_hash::FxHashMap;
use stacksafe::stacksafe;
use syntax::stub::{PrimitiveType, TypeBound, TypeRef};

use crate::java::db::TyDatabase;

/// The maximum rewrite depth of [`rewrite_with`] before a recursive
/// cycle is declared and the remainder of the type degrades to
/// [`TyKind::Error`].
///
/// The memo-reserve breaks genuine cycles, so this is a backstop, not the
/// primary termination mechanism: `active` only grows along an *acyclic*
/// descent, so the guard can only trip on a legitimate type nested deeper
/// than any real Java signature. The iterative stack handles the depths
/// real inputs reach; a million levels is well beyond any source type.
const MAX_REWRITE_DEPTH: usize = 1_000_000;

// The JVM primitive naming, boxing and numeric-promotion tables live on the
// JVM substrate; re-export them here so the Java type layer keeps addressing
// them through `crate::java::ty` (and code using `crate::ty::boxed_type`
// keeps compiling unchanged).
pub use crate::jvm::ty::{boxed_type, numeric_promotion, primitive_name, unboxed_primitive};

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
    /// (recursive) references — the cycle guard in [`crate::java::resolve`] erases
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
    pub fn lower(&self, db: &dyn TyDatabase) -> Option<Ty> {
        match self.kind(db) {
            TyKind::TypeVar { lower, .. } => *lower,
            _ => None,
        }
    }

    /// A copy of this type variable carrying the given `lower` bound
    /// ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)).
    pub(crate) fn with_lower(self, db: &dyn TyDatabase, lower: Option<Ty>) -> Ty {
        match self.kind(db) {
            TyKind::TypeVar { name, bounds, .. } => Ty::new(
                db,
                TyKind::TypeVar {
                    name: name.clone(),
                    bounds: bounds.clone(),
                    lower,
                },
            ),
            _ => self,
        }
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

    /// Whether the type is `void` in either representation: the dedicated
    /// [`TyKind::Void`] or the `void` primitive (the declared return type of
    /// a `void` method lowers to the primitive).
    pub fn is_void_like(&self, db: &dyn TyDatabase) -> bool {
        match self.kind(db) {
            TyKind::Void => true,
            TyKind::Primitive(PrimitiveType::Void) => true,
            _ => false,
        }
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
    #[stacksafe]
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

    /// Whether any nested component is a capture variable (a `CAP#n` type
    /// variable produced by [§5.1.10] capture conversion).
    #[stacksafe]
    pub fn contains_type_var_named_capture(&self, db: &dyn TyDatabase) -> bool {
        match self.kind(db) {
            TyKind::TypeVar { name, .. } => name.as_str().starts_with("CAP#"),
            TyKind::Reference { args, .. } => args
                .iter()
                .any(|arg| arg.contains_type_var_named_capture(db)),
            TyKind::Array(inner) => inner.contains_type_var_named_capture(db),
            TyKind::Wildcard(bound) => bound
                .as_deref()
                .is_some_and(|b| b.ty.contains_type_var_named_capture(db)),
            TyKind::Intersection(members) => members
                .iter()
                .any(|m| m.contains_type_var_named_capture(db)),
            _ => false,
        }
    }

    /// Whether any nested component is a type variable.
    #[stacksafe]
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

    /// Whether this type is identical to `other` *by name*, ignoring the
    /// representation of type-variable bounds.
    ///
    /// [`Ty`] equality is interned-id equality, so two `Box<K, T>` handles
    /// with the *same* type variables are only equal when the variables'
    /// declared bounds intern identically. They usually do — bounds are
    /// resolved once per file — but a *self-referential* bound
    /// ([JLS §4.4]: `class Box<K, T extends Box<K, T>>`) is resolved
    /// independently by each `Resolver` context: the recursion guard of
    /// [§4.4] bound resolution truncates the recursive `T` reference at
    /// different depths, so the field type (from the receiver's args,
    /// [§4.10.2] substitution) and the parameter type (from the method's own
    /// scope) intern to *different* handles. Assignment and return
    /// ([§5.2], [§14.17]) then ask the subtype machinery to decide an
    /// identical pair and it fails. Per [§4.10.2] same erasure — and because
    /// both handles are the *same* declared type variable — the pair is
    /// identical regardless of how the recursive bound got truncated.
    #[stacksafe]
    pub fn same_shape(&self, db: &dyn TyDatabase, other: &Ty) -> bool {
        match (self.kind(db), other.kind(db)) {
            (TyKind::Reference { name: a, args: aa }, TyKind::Reference { name: b, args: bb }) => {
                a == b
                    && aa.len() == bb.len()
                    && aa.iter().zip(bb).all(|(x, y)| x.same_shape(db, y))
            }
            (TyKind::Array(a), TyKind::Array(b)) => a.same_shape(db, b),
            (TyKind::Wildcard(ab), TyKind::Wildcard(bb)) => match (ab, bb) {
                (None, None) => true,
                (Some(a), Some(b)) => a.kind == b.kind && a.ty.same_shape(db, &b.ty),
                _ => false,
            },
            (TyKind::TypeVar { name: a, .. }, TyKind::TypeVar { name: b, .. }) => a == b,
            (TyKind::Intersection(a), TyKind::Intersection(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same_shape(db, y))
            }
            _ => self == other,
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

    /// Visits the canonical fully qualified name ([JLS §6.7]) of every
    /// reference type reachable in `self`: the type itself, its type
    /// arguments, array elements, type-variable bounds, wildcard bounds and
    /// intersection members. Used by the cross-file dependency index
    /// ([`crate::java::dep_index`]) to recover the source files a [`Ty`] refers to.
    #[stacksafe]
    pub fn for_each_reference(&self, db: &dyn TyDatabase, f: &mut impl FnMut(&Name)) {
        match self.kind(db) {
            TyKind::Reference { name, args } => {
                f(name);
                for arg in args.iter() {
                    arg.for_each_reference(db, f);
                }
            }
            TyKind::Array(inner) => inner.for_each_reference(db, f),
            TyKind::Wildcard(bound) => {
                if let Some(bound) = bound.as_deref() {
                    bound.ty.for_each_reference(db, f);
                }
            }
            TyKind::TypeVar { bounds, lower, .. } => {
                for bound_ty in bounds.iter() {
                    bound_ty.for_each_reference(db, f);
                }
                if let Some(lower) = lower {
                    lower.for_each_reference(db, f);
                }
            }
            TyKind::Intersection(members) => {
                for member in members.iter() {
                    member.for_each_reference(db, f);
                }
            }
            _ => {}
        }
    }

    /// Replaces every type variable named in `binding` with its type argument.
    /// Used to instantiate the supertypes of a parameterized type
    /// ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)):
    /// the classfile signature of `ArrayList<E>` declares `extends AbstractList<E>`,
    /// and substituting `E → String` gives `AbstractList<String>`.
    pub fn substitute(&self, db: &dyn TyDatabase, binding: &FxHashMap<Name, Ty>) -> Ty {
        rewrite_with(db, *self, |_db, ty| match ty.kind(db) {
            TyKind::TypeVar { name, .. } => {
                RewriteVerdict::Done(binding.get(name).copied().unwrap_or(ty))
            }
            _ => RewriteVerdict::Recur,
        })
    }

    /// Replaces every type variable named in `binding` with its type argument,
    /// **including inside the bounds of a re-encountered type variable**. This
    /// is the one-pass analogue of the eager inlining that a non-recursive
    /// [`substitute`] performs through the interner: when the type variable's
    /// *bounds* reference the parameter being substituted (`T extends Box<K,T>`
    /// with `T → V`), inlining the name yields `V extends Box<K,V>` directly,
    /// where the plain [`substitute`] would leave the recursive `T` bound
    /// behind.
    ///
    /// Substituting a type variable into its own occurrence closes the
    /// recursion: the substituted variable is that occurrence's argument, so
    /// the bound references it by its *new* name only ([JLS §4.4] recursion,
    /// `Comparable<T>`-style). A distinct variable keeps its bounds exactly —
    /// the two names cannot recurse (the class's parameters are distinct), so
    /// substituting them is a shallow name replacement.
    pub fn substitute_incl_bounds(&self, db: &dyn TyDatabase, binding: &FxHashMap<Name, Ty>) -> Ty {
        rewrite_with(db, *self, |db, ty| match ty.kind(db) {
            // A variable bound by `binding` is replaced by its argument; the
            // argument's own bounds are plain-`substitute`d (a `class
            // Box<K,T>`'s parameters are distinct, so no recursion can close
            // through the two names) and the result is used as-is.
            TyKind::TypeVar { name, .. } => match binding.get(name) {
                Some(argument) => RewriteVerdict::Done(argument.substitute(db, binding)),
                // An unbound variable keeps its identity but its bounds
                // reference the substituted parameters ([JLS §4.4]
                // `T extends Box<K,T>`): rebuilding them in the same pass
                // yields `V extends Box<K,V>` where plain [`substitute`]
                // would leave the recursive `T` behind.
                None => {
                    let bounds = ty
                        .bounds(db)
                        .iter()
                        .map(|b| b.substitute(db, binding))
                        .collect::<Vec<_>>();
                    let rebuilt =
                        Ty::type_var(db, name.clone(), bounds).with_lower(db, ty.lower(db));
                    RewriteVerdict::Done(rebuilt)
                }
            },
            _ => RewriteVerdict::Recur,
        })
    }

    /// Replaces every inference variable ([`TyKind::InferenceVar`]) whose id is
    /// in `subst` with its instantiation. Used to apply the resolved
    /// substitution of invocation type inference
    /// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4))
    /// to the formal and return types of a generic method.
    pub fn substitute_infer(&self, db: &dyn TyDatabase, subst: &FxHashMap<u64, Ty>) -> Ty {
        rewrite_with(db, *self, |_db, ty| match ty.kind(db) {
            TyKind::InferenceVar(id) => RewriteVerdict::Done(subst.get(id).copied().unwrap_or(ty)),
            _ => RewriteVerdict::Recur,
        })
    }

    /// Replaces every inference variable ([`TyKind::InferenceVar`]) with
    /// `java.lang.Object`, erasing the still-unresolved unknowns of an
    /// inference table. Used by the estimate pass of bound set resolution
    /// ([JLS §18.4]) to break cyclic dependencies between variables.
    pub fn erase_infer_vars(&self, db: &dyn TyDatabase) -> Ty {
        rewrite_with(db, *self, |db, ty| match ty.kind(db) {
            TyKind::InferenceVar(_) => {
                RewriteVerdict::Done(Ty::reference(db, "java.lang.Object", Vec::new()))
            }
            _ => RewriteVerdict::Recur,
        })
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

    /// Formats this type for display with *simple* class names: reference
    /// types render their last `.`-segment ([`Name::simple_name`]) instead of
    /// the canonical fully qualified name, so `java.util.List<java.lang.String>`
    /// becomes `List<String>`. `$` is an ordinary identifier character
    /// ([JLS §3.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.8)),
    /// so a `$`-containing name keeps its `$`. Every non-reference kind
    /// renders identically to [`Ty::display`]. Used where javac renders the
    /// *simple* class name: LSP symbol signatures and diagnostic messages.
    pub fn display_simple<'a>(&'a self, db: &'a dyn TyDatabase) -> TySimpleDisplay<'a> {
        TySimpleDisplay { ty: self, db }
    }
}

/// The outcome of one node's rewrite policy consultation.
enum RewriteVerdict {
    /// The node is rewritten in place; the result is final and its children
    /// are not traversed.
    Done(Ty),
    /// The node's children must be rewritten and the node reconstructed.
    Recur,
}

/// A frame of the explicit rewrite stack: either a node whose children are
/// yet to be rewritten ([`Frame::Visit`]) or a node whose children have all
/// been rewritten and whose reconstruction is due ([`Frame::Build`]).
enum Frame {
    Visit(Ty),
    Build(Ty),
}

/// An iterative, bottom-up type rewrite.
///
/// The recursive [`Ty`] walks (`substitute`, `substitute_incl_bounds`,
/// `substitute_infer`, `erase_infer_vars`) each descended the interned type
/// DAG on the native stack — one frame per edge. The interner makes the DAG
/// *recursive* by construction ([JLS §4.4] `T extends Box<K,T>`), and the
/// 16 MiB worker stacks of `caffeine-ls` mask the overflow but do not fix it:
/// a substitution that maps a variable into a type re-referencing it grows a
/// chain no fixed stack size contains.
///
/// This is the same traversal, but the *explicit* stack replaces the native
/// one and a memo makes each node visited at most once:
///
/// * **bottom-up** — a node's rewritten children are built first
///   ([`Frame::Build`] runs after the last child finishes), so reconstruction
///   mirrors the original one-pass semantics exactly;
/// * **memoized** — a shared sub-DAG is rebuilt once and the handle reused,
///   so a genuinely deep-but-finite type terminates where the native
///   recursion would overflow;
/// * **bounded** — a chain that never settles (a substitution re-referencing
///   itself) trips the [`MAX_REWRITE_DEPTH`] guard and degrades the node to
///   [`TyKind::Error`] rather than overflowing.
///
/// `rewrite_with` drives the walk and asks the caller's `leaf` policy what to
/// do with each node: [`RewriteVerdict::Done`] short-circuits the structural
/// rebuild (a replaced inference variable or type-variable name, whose value
/// is used as-is — the recursion never descends into a substituted value,
/// exactly as the original code did not), [`RewriteVerdict::Recur`] defers to
/// it. The structural kinds — `Reference { args }`, `Array`, `Wildcard`,
/// `Intersection` — always recurse.
fn rewrite_with(
    db: &dyn TyDatabase,
    root: Ty,
    mut leaf: impl FnMut(&dyn TyDatabase, Ty) -> RewriteVerdict,
) -> Ty {
    let mut memo: FxHashMap<TyData, Ty> = FxHashMap::default();
    let mut stack: Vec<Frame> = vec![Frame::Visit(root)];
    // The number of nodes on the current rewrite path. Bounded by the depth
    // guard so a self-referential substitution cannot overflow the stack.
    let mut active: usize = 0;
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Visit(ty) => {
                // Already rewritten (or reserved while its children are being
                // visited): reuse the memoized handle.
                if memo.contains_key(&ty.id) {
                    continue;
                }
                if active >= MAX_REWRITE_DEPTH {
                    memo.insert(ty.id, Ty::error(db));
                    continue;
                }
                active += 1;
                match leaf(db, ty) {
                    RewriteVerdict::Done(done) => {
                        active -= 1;
                        memo.insert(ty.id, done);
                    }
                    RewriteVerdict::Recur => {
                        // Reserve the memo slot so a descendant that reaches
                        // this node again (a structural cycle) terminates by
                        // reusing the reservation; it is overwritten when the
                        // children finish.
                        memo.insert(ty.id, ty);
                        stack.push(Frame::Build(ty));
                        // Children are pushed below the build frame so they
                        // pop — and finish — first.
                        match ty.kind(db) {
                            TyKind::Reference { args, .. } => {
                                for arg in args.iter().rev() {
                                    stack.push(Frame::Visit(*arg));
                                }
                            }
                            TyKind::Array(inner) => stack.push(Frame::Visit(**inner)),
                            TyKind::Wildcard(bound) => {
                                if let Some(bound) = bound.as_deref() {
                                    stack.push(Frame::Visit(bound.ty));
                                }
                            }
                            TyKind::Intersection(members) => {
                                for member in members.iter().rev() {
                                    stack.push(Frame::Visit(*member));
                                }
                            }
                            // Type variables, inference variables and
                            // primitives carry no *rewritable* children: the
                            // leaf decides them (`Done`), or they rebuild to
                            // themselves below.
                            _ => {}
                        }
                    }
                }
            }
            Frame::Build(ty) => {
                let rebuilt = match ty.kind(db) {
                    TyKind::Reference { name, args } => Ty::reference(
                        db,
                        name.clone(),
                        args.iter().map(|arg| memo[&arg.id]).collect(),
                    ),
                    TyKind::Array(inner) => Ty::array(db, memo[&inner.id]),
                    TyKind::Wildcard(bound) => Ty::wildcard(
                        db,
                        bound.as_deref().map(|b| {
                            Box::new(WildcardBound {
                                kind: b.kind,
                                ty: memo[&b.ty.id],
                            })
                        }),
                    ),
                    TyKind::Intersection(members) => {
                        Ty::intersection(db, members.iter().map(|m| memo[&m.id]).collect())
                    }
                    // A leaf that chose `Recur` without children (a type
                    // variable or bare wildcard) rebuilds to its own handle.
                    _ => ty,
                };
                active -= 1;
                memo.insert(ty.id, rebuilt);
            }
        }
    }
    memo[&root.id]
}
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

/// A displayable view of a [`Ty`] with simple class names, produced by
/// [`Ty::display_simple`].
pub struct TySimpleDisplay<'a> {
    ty: &'a Ty,
    db: &'a dyn TyDatabase,
}

impl fmt::Display for TySimpleDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty.kind(self.db) {
            TyKind::Reference { name, args } => {
                f.write_str(name.simple_name())?;
                if !args.is_empty() {
                    f.write_str("<")?;
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{}", arg.display_simple(self.db))?;
                    }
                    f.write_str(">")?;
                }
                Ok(())
            }
            TyKind::Array(inner) => write!(f, "{}[]", inner.display_simple(self.db)),
            TyKind::Intersection(members) => {
                for (i, member) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" & ")?;
                    }
                    write!(f, "{}", member.display_simple(self.db))?;
                }
                Ok(())
            }
            TyKind::Wildcard(bound) => {
                f.write_str("?")?;
                if let Some(bound) = bound {
                    match bound.kind {
                        BoundKind::Upper => {
                            write!(f, " extends {}", bound.ty.display_simple(self.db))?
                        }
                        BoundKind::Lower => {
                            write!(f, " super {}", bound.ty.display_simple(self.db))?
                        }
                    }
                }
                Ok(())
            }
            _ => fmt::Display::fmt(
                &TyDisplay {
                    ty: self.ty,
                    db: self.db,
                },
                f,
            ),
        }
    }
}

/// Lowers a [`TypeRef`] into a [`Ty`], mapping names with `name`. Reference
/// names are used verbatim (no resolution): use
/// [`crate::java::resolve::resolve_type_ref`] for source-side resolution, and this
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
/// [`crate::java::method::member_set`], and only there: the capture variables never
/// reach the memoized subtype queries.
pub fn capture_conversion(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: Ty) -> Ty {
    let fresh = |bound: Ty| {
        let id = NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ty::type_var(db, Name::new(&format!("CAP#{id}")), vec![bound])
    };
    match ty.kind(db) {
        TyKind::Reference { name, args } => {
            // §5.1.10: the fresh variable of a *bare* `?` argument takes the
            // upper bound of the type parameter it fills — `AbstractLongAssert<?>`
            // captures to `AbstractLongAssert<CAP extends AbstractLongAssert<…>>`,
            // not to `AbstractLongAssert<CAP extends Object>`. Without it a
            // method returning the SELF type parameter (`as`, `contains`,
            // `isInstanceOf`) yields a bare wildcard, and the next chained
            // call degrades to "cannot find symbol" against the `Object`
            // bound.
            let placeholders: Vec<Option<Ty>> = args
                .iter()
                .map(|arg| {
                    if matches!(arg.kind(db), TyKind::Wildcard(_)) {
                        let id = NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Some(Ty::type_var(
                            db,
                            Name::new(&format!("CAP#{id}")),
                            Vec::new(),
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            let declared = type_param_upper_bounds(db, scope, name.as_str(), &args, &placeholders);
            Ty::reference(
                db,
                name.clone(),
                args.iter()
                    .enumerate()
                    .map(|(i, arg)| match arg.kind(db) {
                        TyKind::Wildcard(Some(bound)) => match bound.kind {
                            BoundKind::Upper => fresh(bound.ty.clone()),
                            // `? super T`: a capture variable with the `Object`
                            // upper bound and the `T` lower bound (§5.1.10).
                            BoundKind::Lower => Ty::captured_var(db, bound.ty.clone()),
                        },
                        TyKind::Wildcard(None) => {
                            let bound = declared.get(i).copied().unwrap_or_else(|| {
                                Ty::reference(db, "java.lang.Object", Vec::new())
                            });
                            fresh(bound)
                        }
                        // §5.1.10: capture conversion is *not* recursively
                        // applied to the non-wildcard type arguments — only the
                        // top-level wildcard arguments of the receiver's own
                        // type are replaced. A nested `Class<?>` inside
                        // `Map<Class<?>, String>` stays `Class<?>` (javac
                        // accepts `map.put(Class<Boolean>, …)` against it), and
                        // recursively capturing would turn it into a fresh
                        // `Class<CAP#n>` that no concrete `Class<B>` is a
                        // subtype of.
                        _ => *arg,
                    })
                    .collect(),
            )
        }
        TyKind::Array(inner) => Ty::array(db, **inner),
        TyKind::Wildcard(Some(bound)) => match bound.kind {
            BoundKind::Upper => fresh(bound.ty.clone()),
            // `? super T`: a capture variable with the `Object` upper bound
            // and the `T` lower bound (§5.1.10).
            BoundKind::Lower => Ty::captured_var(db, bound.ty.clone()),
        },
        TyKind::Wildcard(None) => fresh(Ty::reference(db, "java.lang.Object", Vec::new())),
        // Intersection and type-variable receivers are not parameterized by
        // wildcards; leave them as-is.
        _ => ty,
    }
}

/// The upper bound the type parameters of the resolved class declare, in
/// declaration order — what a *bare* `?` in each argument position captures
/// ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)).
/// The declared bounds may reference the class's own type parameters
/// (`SELF extends AbstractAssert<SELF, …>`); those are substituted by the
/// argument in the corresponding position — a concrete argument by itself, a
/// wildcard argument by a fresh placeholder variable. A parameter without a
/// declared bound yields `Object`. Source classes and unresolvable names
/// (whose bounds need the source item tree) fall back to `Object`.
fn type_param_upper_bounds(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
    args: &[Ty],
    placeholders: &[Option<Ty>],
) -> Vec<Ty> {
    let object = Ty::reference(db, "java.lang.Object", Vec::new());
    let Some(resolved) = hir::fqn_resolve(db, scope, fqn) else {
        return Vec::new();
    };
    let interner = &db.hir_state().interner;
    let params: Vec<(Name, Vec<syntax::stub::TypeRef<hir::Symbol>>)> = match resolved {
        hir::Resolved::Library(_) => match hir::class_generic_info(db, &resolved) {
            Some(info) => info
                .type_params
                .iter()
                .map(|tp| (Name::new(interner.resolve(&tp.name)), tp.bounds.clone()))
                .collect(),
            None => return Vec::new(),
        },
        hir::Resolved::Source(_) => return Vec::new(),
    };
    let mut binding: FxHashMap<Name, Ty> = FxHashMap::default();
    for (i, (name, _)) in params.iter().enumerate() {
        let arg = match (args.get(i), placeholders.get(i)) {
            (_, Some(Some(ph))) => *ph,
            (Some(arg), _) => *arg,
            _ => object,
        };
        binding.insert(name.clone(), arg);
    }
    params
        .iter()
        .map(|(_, bounds)| {
            let bound = bounds
                .first()
                .map(|tr| crate::java::resolve::ty_from_library(db, tr).substitute(db, &binding))
                .filter(|b| !b.is_object(db))
                .unwrap_or(object);
            bound
        })
        .collect()
}
