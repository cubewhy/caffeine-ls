//! The type model.
//!
//! [`Ty`] is the internal representation of a JVM-level type, following the
//! taxonomy of the type kinds both languages share: primitive types
//! ([JLS §4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.2)),
//! reference types ([§4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3)),
//! type variables ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
//! parameterized types ([§4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5)),
//! array types ([§10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.1))
//! and the two Kotlin source-level wrappers [`TyKind::Nullable`] and
//! [`TyKind::DefinitelyNonNull`] ([KLS
//! `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types)).
//!
//! The model is *not* Java's, so it does not live in [`crate::java`]: a
//! Kotlin type is interned, compared, substituted and displayed by this
//! machinery. What stays Java-only is how a type is *built* from a source
//! reference — [`crate::java::ty`] keeps `ty_from_type_ref`, `ty_from_source`
//! and `capture_conversion`, which resolve Java names, defaults and
//! wildcards.
//!
//! Reference types carry a canonical fully qualified name ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)).
//! For source types the FQN is produced by [`crate::java::resolve`]; for library
//! types it comes straight out of the classfile stubs.
//!
//! [`Ty`] values are interned in the salsa database: each distinct
//! [`TyKind`] maps to one id, so a [`Ty`] is a cheap `Copy` handle with
//! `O(1)` equality that can key the memoized subtype/supertype queries. Every
//! accessor therefore takes the database.
use std::fmt;

use hir_def::java::item_tree::ItemId;
use hir_expand::name::Name;
use rustc_hash::FxHashMap;
use stacksafe::stacksafe;
use syntax::stub::PrimitiveType;
use vfs::FileId;

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
    ///
    /// `local` is `Some` exactly for a *local* class, interface, enum or record
    /// type
    /// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)),
    /// which has a simple name but neither a fully qualified nor a canonical
    /// name ([§6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)):
    /// it is identified by its declaration — the same identity
    /// [`hir::Resolved::Source`] carries — while `name` stays the declaration's
    /// *simple* name, which is what javac and the IDE render. The plain-name
    /// form cannot collide: only one declaration of that name is in scope at a
    /// use site ([§6.4.1]).
    Reference {
        name: Name,
        args: Vec<Ty>,
        local: Option<hir::SourceClass>,
    },
    /// A type variable ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
    /// with its declared bounds ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)).
    /// `bounds` is empty for unbounded type variables and for re-entrant
    /// (recursive) references — the cycle guard in [`crate::java::resolve`] erases
    /// bounds on re-entry so interning terminates. `lower` is set only for the
    /// fresh type variables of capture conversion
    /// ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// `? super T` captures to a variable with the `Object` upper bound and
    /// the `T` lower bound.
    ///
    /// The variable is identified by its declaring parameter ([§6.3], [§8.4.4]):
    /// [`TypeVarScope`] carries the declaring declaration and the parameter's
    /// own name, so a method type parameter that shadows a class type
    /// parameter of the same name is a *distinct type*
    /// ([§6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4.1),
    /// javac's `T#1`/`T#2`), and a substitution declared over one declaration's
    /// parameters never captures another's ([§4.4] capture-avoidance).
    TypeVar {
        scope: TypeVarScope,
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
    /// A Kotlin nullable type `T?` ([KLS
    /// `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types)):
    /// the notation `T?` of the type system, whose values are the values of
    /// `T` and `null`.
    ///
    /// A Kotlin source-level wrapper only: a Java type is never nullable, so
    /// the Java layer never constructs one and every Java-side match treats it
    /// as unreachable.
    Nullable(Ty),
    /// A Kotlin definitely-non-nullable type `T & Any` ([KLS
    /// `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types),
    /// spelled `T!!` in the type system's notation): `T` with its nullability
    /// removed, which is what a smart cast produces from a nullable value.
    /// Kotlin source only, like [`TyKind::Nullable`].
    DefinitelyNonNull(Ty),
}

/// The next capture-variable name: capture variables are ordinary type
/// variables ([`TyKind::TypeVar`]) interning by name, so distinct captures
/// must not share a name.
static NEXT_CAPTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The next capture-variable name ([`Ty::fresh_capture`]).
pub(crate) fn next_capture() -> u64 {
    NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
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

/// The declaring parameter of a type variable ([JLS §4.4], [§6.3]): the
/// declaration that introduces it and the parameter's own name within that
/// declaration — the identity a type variable is interned and substituted by.
///
/// [§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)
/// introduces a type variable as `{TypeParameter}` — *which* declaration
/// introduced it is part of what the variable is, not an annotation on it.
/// [§6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)
/// scopes that declaration, and
/// [§6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4.1)
/// makes a shadowing declaration introduce a *different* variable: a method
/// type parameter named `T` and an enclosing class type parameter named `T`
/// are two types javac renders `T#1`/`T#2`, not one. A substitution is
/// declared over one declaration's parameters ([§4.4] capture-avoidance) and
/// must therefore match on this identity, never on the bare name — a
/// same-named variable of another declaration that merely *occurs* inside the
/// substituted type is untouched.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeVarScope {
    /// A type parameter of a source class/interface/enum/record declaration
    /// ([§8.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.2),
    /// [§9.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.1.2)).
    Class {
        file: FileId,
        item: ItemId,
        name: Name,
    },
    /// A type parameter of a source method or constructor declaration
    /// ([§8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4),
    /// [§8.8.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.4)).
    Method {
        file: FileId,
        item: ItemId,
        name: Name,
    },
    /// A type parameter of a classfile class or interface, identified by the
    /// declaring class's binary name ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1)).
    LibraryClass { owner: Name, name: Name },
    /// A type parameter of a classfile method or constructor, identified by
    /// the declaring class's binary name and the method's name
    /// ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1)).
    LibraryMethod {
        owner: Name,
        method: Name,
        name: Name,
    },
    /// A capture variable ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// a fresh variable, distinct per capture, not a declared parameter. `id`
    /// is the session-wide serial that makes it fresh.
    Capture { id: u64, name: Name },
    /// A type variable whose declaring parameter is not recoverable at the
    /// position that built it — a classfile signature lowered without its
    /// declaring declaration, and a unit-test fixture. Identified by name
    /// alone, so no declaration-scoped substitution ever matches it: a
    /// variable that cannot be attributed to a declaration must not be
    /// captured by another declaration's substitution.
    Unnamed { name: Name },
}

impl TypeVarScope {
    /// The variable's own name within its declaring parameter list — the name
    /// javac renders.
    pub fn name(&self) -> &Name {
        match self {
            TypeVarScope::Class { name, .. }
            | TypeVarScope::Method { name, .. }
            | TypeVarScope::LibraryClass { name, .. }
            | TypeVarScope::LibraryMethod { name, .. }
            | TypeVarScope::Capture { name, .. }
            | TypeVarScope::Unnamed { name } => name,
        }
    }

    /// Whether this is a capture variable ([§5.1.10]) rather than a declared
    /// type parameter.
    pub fn is_capture(&self) -> bool {
        matches!(self, TypeVarScope::Capture { .. })
    }

    /// The scope of the type variable named `name` in a classfile `Signature`
    /// attribute ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1))
    /// of the class `owner`: the declaring *method's* own parameter when the
    /// signature belongs to a method that declares the name, otherwise the
    /// declaring class's. A classfile signature resolves type variables
    /// innermost-first exactly as source does ([§6.4.1]): a method type
    /// parameter shadows a class type parameter of the same name. A name
    /// declared by neither — an enclosing class's parameter, whose arguments
    /// the receiver does not carry — keeps the class-scoped identity, which no
    /// binding of that class can match (bindings carry only the class's own
    /// declared parameters), so it is left a variable rather than captured by
    /// a same-named parameter of the class.
    pub fn library(owner: &Name, method: Option<(&Name, &[Name])>, name: &Name) -> TypeVarScope {
        if let Some((method_name, method_params)) = method
            && method_params.contains(name)
        {
            return TypeVarScope::LibraryMethod {
                owner: owner.clone(),
                method: method_name.clone(),
                name: name.clone(),
            };
        }
        TypeVarScope::LibraryClass {
            owner: owner.clone(),
            name: name.clone(),
        }
    }
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

    /// A Kotlin nullable type `T?` ([KLS
    /// `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types)).
    /// `inner` already nullable stays as it is: `T??` is `T?`.
    pub fn nullable(db: &dyn TyDatabase, inner: Ty) -> Self {
        if matches!(inner.kind(db), TyKind::Nullable(_)) {
            return inner;
        }
        Self::new(db, TyKind::Nullable(inner))
    }

    /// A Kotlin definitely-non-nullable type `T & Any` ([KLS
    /// `type-system.html#nullable-types`](https://kotlinlang.org/spec/type-system.html#nullable-types)):
    /// what a smart cast produces from a nullable value. An already
    /// definitely-non-nullable inner type stays as it is.
    pub fn definitely_non_null(db: &dyn TyDatabase, inner: Ty) -> Self {
        if matches!(inner.kind(db), TyKind::DefinitelyNonNull(_)) {
            return inner;
        }
        Self::new(db, TyKind::DefinitelyNonNull(inner))
    }

    /// The inner type of a nullable or definitely-non-nullable wrapper, or
    /// `self` for every other type — the type with its `?`/`!!` notation
    /// stripped.
    pub fn strip_nullability(&self, db: &dyn TyDatabase) -> Ty {
        match self.kind(db) {
            TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => *inner,
            _ => *self,
        }
    }

    /// Whether the type is Kotlin's `T?`.
    pub fn is_nullable(&self, db: &dyn TyDatabase) -> bool {
        matches!(self.kind(db), TyKind::Nullable(_))
    }

    pub fn reference(db: &dyn TyDatabase, name: impl Into<Name>, args: Vec<Ty>) -> Self {
        Self::new(
            db,
            TyKind::Reference {
                name: name.into(),
                args,
                local: None,
            },
        )
    }

    /// A reference type to a *local* class, interface, enum or record
    /// declaration ([JLS §14.3]): `simple` is the declaration's own name and
    /// `class` its declaration, which is the type's identity ([§6.7] — a local
    /// class has no canonical name). See [`TyKind::Reference`].
    pub fn local_reference(
        db: &dyn TyDatabase,
        class: hir::SourceClass,
        simple: impl Into<Name>,
        args: Vec<Ty>,
    ) -> Self {
        Self::new(
            db,
            TyKind::Reference {
                name: simple.into(),
                args,
                local: Some(class),
            },
        )
    }

    /// The same reference type as `self` — the class it names, local or not —
    /// with `args` as its type arguments. This is the single carry-through for
    /// every *rebuild* of a reference type (erasure, rewriting, capture
    /// conversion, decapture, the least-common-parameter/type-argument pairs):
    /// rebuilding through [`Ty::reference`] would silently drop the declaration
    /// a local type is identified by.
    ///
    /// # Panics
    /// If `self` is not a reference type.
    pub(crate) fn with_args(&self, db: &dyn TyDatabase, args: Vec<Ty>) -> Ty {
        match self.kind(db) {
            TyKind::Reference { name, local, .. } => Self::new(
                db,
                TyKind::Reference {
                    name: name.clone(),
                    args,
                    local: *local,
                },
            ),
            _ => unreachable!("with_args on a non-reference type"),
        }
    }

    /// A type variable declared by `scope` ([JLS §4.4], [§6.3]).
    ///
    /// The scope carries both the declaring declaration and the parameter's
    /// own name: the two names javac renders `T#1`/`T#2` are distinct types
    /// ([§6.4.1]), and a substitution declared over one declaration's
    /// parameters is capture-avoiding ([§4.4]) because it matches on the
    /// scope, never on the bare name.
    pub fn type_var(db: &dyn TyDatabase, scope: TypeVarScope, bounds: Vec<Ty>) -> Self {
        Self::new(
            db,
            TyKind::TypeVar {
                scope,
                bounds,
                lower: None,
            },
        )
    }

    /// A type variable with its `lower` bound set
    /// ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// the shape of a `? super T` capture variable.
    pub(crate) fn type_var_with(
        db: &dyn TyDatabase,
        scope: TypeVarScope,
        bounds: Vec<Ty>,
        lower: Option<Ty>,
    ) -> Self {
        Self::new(
            db,
            TyKind::TypeVar {
                scope,
                bounds,
                lower,
            },
        )
    }

    /// A type variable with no recoverable declaring declaration — the
    /// lowering fallback for a classfile signature built without its class
    /// (e.g. an annotation element type) and the unit-test fixture. See
    /// [`TypeVarScope::Unnamed`].
    pub fn unscoped_var(db: &dyn TyDatabase, name: impl Into<Name>, bounds: Vec<Ty>) -> Self {
        Self::type_var(db, TypeVarScope::Unnamed { name: name.into() }, bounds)
    }

    /// The declaring parameter of this type variable, or `None` for any other
    /// type.
    pub fn type_var_scope<'a>(&self, db: &'a dyn TyDatabase) -> Option<&'a TypeVarScope> {
        match self.kind(db) {
            TyKind::TypeVar { scope, .. } => Some(scope),
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
    pub fn lower(&self, db: &dyn TyDatabase) -> Option<Ty> {
        match self.kind(db) {
            TyKind::TypeVar { lower, .. } => *lower,
            _ => None,
        }
    }

    /// A capture variable ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// a fresh type variable with the `Object` upper bound and the `lower`
    /// bound (the `? super T` capture). Freshness comes from the session-wide
    /// serial in [`TypeVarScope::Capture`], so distinct captures never intern
    /// to the same variable ([§5.1.10] requires a *fresh* variable per
    /// capture).
    pub(crate) fn captured_var(db: &dyn TyDatabase, lower: Ty) -> Self {
        let id = NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let scope = TypeVarScope::Capture {
            id,
            name: Name::new(&format!("CAP#{id}")),
        };
        Ty::type_var_with(
            db,
            scope,
            vec![Ty::reference(db, "java.lang.Object", Vec::new())],
            Some(lower),
        )
    }

    /// A fresh capture variable with an upper bound
    /// ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// the shape a `? extends T` / bare `?` capture takes.
    pub(crate) fn fresh_capture(db: &dyn TyDatabase, bound: Ty) -> Ty {
        let id = NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let scope = TypeVarScope::Capture {
            id,
            name: Name::new(&format!("CAP#{id}")),
        };
        Ty::type_var(db, scope, vec![bound])
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
        matches!(
            self.kind(db),
            TyKind::Void | TyKind::Primitive(PrimitiveType::Void)
        )
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

    /// Whether `ty` mentions the inference variable `id` ([JLS §18.3.2]): a
    /// bound whose type mentions the variable it bounds is a *dependency*
    /// bound, which cannot instantiate that variable and is only validated
    /// once the variable has been resolved from its other bounds.
    #[stacksafe]
    pub fn contains_infer_var_id(&self, db: &dyn TyDatabase, id: u64) -> bool {
        match self.kind(db) {
            TyKind::InferenceVar(var) => *var == id,
            TyKind::Reference { args, .. } => {
                args.iter().any(|arg| arg.contains_infer_var_id(db, id))
            }
            TyKind::Array(inner) => inner.contains_infer_var_id(db, id),
            TyKind::Wildcard(bound) => bound
                .as_deref()
                .is_some_and(|b| b.ty.contains_infer_var_id(db, id)),
            TyKind::Intersection(members) => {
                members.iter().any(|m| m.contains_infer_var_id(db, id))
            }
            _ => false,
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
            TyKind::TypeVar { scope, .. } => scope.is_capture(),
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

    /// Whether the type mentions a *declared* type variable
    /// ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
    /// — a type parameter that is not a capture of a wildcard. Such a value is
    /// known only to lie within the variable's declared bounds, which the
    /// declaration that introduced the parameter carries.
    pub fn contains_declared_type_var(&self, db: &dyn TyDatabase) -> bool {
        match self.kind(db) {
            TyKind::TypeVar { scope, .. } => !scope.is_capture(),
            TyKind::Reference { args, .. } => {
                args.iter().any(|arg| arg.contains_declared_type_var(db))
            }
            TyKind::Array(inner) => inner.contains_declared_type_var(db),
            TyKind::Wildcard(bound) => bound
                .as_deref()
                .is_some_and(|b| b.ty.contains_declared_type_var(db)),
            TyKind::Intersection(members) => {
                members.iter().any(|m| m.contains_declared_type_var(db))
            }
            _ => false,
        }
    }

    /// Whether any nested type argument is a wildcard
    /// ([JLS §4.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5.1)).
    /// A wildcard-parameterized reference is a candidate for capture conversion
    /// ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
    /// when a value of it participates in invocation inference.
    #[stacksafe]
    pub fn contains_wildcard(&self, db: &dyn TyDatabase) -> bool {
        match self.kind(db) {
            TyKind::Wildcard(_) => true,
            TyKind::Reference { args, .. } => args.iter().any(|arg| arg.contains_wildcard(db)),
            TyKind::Array(inner) => inner.contains_wildcard(db),
            TyKind::Intersection(members) => members.iter().any(|m| m.contains_wildcard(db)),
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
            (
                TyKind::Reference {
                    name: a,
                    args: aa,
                    local: a_local,
                },
                TyKind::Reference {
                    name: b,
                    args: bb,
                    local: b_local,
                },
            ) => {
                // §6.7: a *local* type has only its declaration as identity, so
                // two same-named references to different local declarations are
                // different types.
                a == b
                    && a_local == b_local
                    && aa.len() == bb.len()
                    && aa.iter().zip(bb).all(|(x, y)| x.same_shape(db, y))
            }
            (TyKind::Array(a), TyKind::Array(b)) => a.same_shape(db, b),
            (TyKind::Wildcard(ab), TyKind::Wildcard(bb)) => match (ab, bb) {
                (None, None) => true,
                (Some(a), Some(b)) => a.kind == b.kind && a.ty.same_shape(db, &b.ty),
                _ => false,
            },
            (TyKind::TypeVar { scope: sa, .. }, TyKind::TypeVar { scope: sb, .. }) => {
                // §4.4/§6.4.1: a type variable is identified by its declaring
                // parameter — a method type parameter shadows and *differs*
                // from the same-named class parameter, so a `T`-for-`T` pair
                // across declarations is not the same shape. Same-scope pairs
                // (the self-referential bound of §4.4 truncated at different
                // depths) stay identical.
                sa == sb
            }
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
            TyKind::Reference { name, args, .. }
                if name.as_str() == "java.lang.Object" && args.is_empty()
        )
    }

    /// `(name, args)` if this is a reference type.
    pub fn as_reference<'a>(&self, db: &'a dyn TyDatabase) -> Option<(&'a Name, &'a [Ty])> {
        match self.kind(db) {
            TyKind::Reference { name, args, .. } => Some((name, args)),
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
    pub fn for_each_reference(
        &self,
        db: &dyn TyDatabase,
        f: &mut impl FnMut(&Name, Option<hir::SourceClass>),
    ) {
        match self.kind(db) {
            TyKind::Reference { name, args, local } => {
                f(name, *local);
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

    /// Replaces every type variable declared by a parameter in `binding` with
    /// its type argument. Used to instantiate the supertypes of a
    /// parameterized type
    /// ([JLS §4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)):
    /// the classfile signature of `ArrayList<E>` declares `extends AbstractList<E>`,
    /// and substituting `E → String` gives `AbstractList<String>`.
    ///
    /// The substitution is *capture-avoiding* ([JLS §4.4]): `binding` maps a
    /// [`TypeVarScope`] — a declaring parameter — to its argument, and only a
    /// variable with that exact scope is replaced. A same-named variable
    /// declared by a *different* declaration that merely occurs inside the
    /// substituted type ([§6.4.1] shadowing) is a different type and is left
    /// untouched; a name-keyed substitution would rewrite it into the
    /// argument of an unrelated variable.
    pub fn substitute(&self, db: &dyn TyDatabase, binding: &FxHashMap<TypeVarScope, Ty>) -> Ty {
        rewrite_with(db, *self, |_db, ty| match ty.kind(db) {
            TyKind::TypeVar { scope, .. } => {
                RewriteVerdict::Done(binding.get(scope).copied().unwrap_or(ty))
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
    pub fn substitute_incl_bounds(
        &self,
        db: &dyn TyDatabase,
        binding: &FxHashMap<TypeVarScope, Ty>,
    ) -> Ty {
        rewrite_with(db, *self, |db, ty| match ty.kind(db) {
            // A variable bound by `binding` is replaced by its argument; the
            // argument's own bounds are plain-`substitute`d (a `class
            // Box<K,T>`'s parameters are distinct declarations, so no
            // recursion can close through the two scopes) and the result is
            // used as-is.
            TyKind::TypeVar { scope, .. } => match binding.get(scope) {
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
                    let rebuilt = Ty::type_var_with(db, scope.clone(), bounds, ty.lower(db));
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
            // §4.6: the erasure of a reference type is the *same* class with
            // its type arguments dropped.
            TyKind::Reference { .. } => self.with_args(db, Vec::new()),
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
    /// *simple* class name: LSP symbol signatures, hover signatures and
    /// diagnostic messages.
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
                    TyKind::Reference { args, .. } => {
                        ty.with_args(db, args.iter().map(|arg| memo[&arg.id]).collect())
                    }
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
            TyKind::Reference { name, args, .. } => {
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
            TyKind::TypeVar { scope, .. } => f.write_str(scope.name().as_str()),
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
            // The Kotlin spellings (KLS `type-system.html#nullable-types`):
            // `T?` and the intersection notation `T & Any`.
            TyKind::Nullable(inner) => write!(f, "{}?", inner.display(self.db)),
            TyKind::DefinitelyNonNull(inner) => write!(f, "{} & Any", inner.display(self.db)),
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
            // A wrapper renders its inner type simply too, so `kotlin.String?`
            // is `String?` — the simple display is *simple* throughout.
            TyKind::Nullable(inner) => write!(f, "{}?", inner.display_simple(self.db)),
            TyKind::DefinitelyNonNull(inner) => {
                write!(f, "{} & Any", inner.display_simple(self.db))
            }
            TyKind::Reference { name, args, .. } => {
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
