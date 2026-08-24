//! Method resolution: member set, accessibility and applicability
//! ([JLS §15.12]).
//!
//! [`member_set`] computes the candidate methods for a name on a receiver
//! type ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)):
//! the methods of the receiver and, transitively, of all its superclasses and
//! superinterfaces, each instantiated with the receiver type's type arguments,
//! captured per [§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10)
//! so that wildcard arguments become fresh type variables. The [`InvocationContext`]
//! restricts the candidates to those allowed by the invocation mode
//! ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
//! [§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3))
//! and accessible at the invocation site ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
//! [`pick_method`] then runs the overload resolution of
//! [JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2):
//! the strict ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)),
//! loose ([§15.12.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.3))
//! and variable-arity
//! ([§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4))
//! applicability phases, choosing the most specific applicable method
//! ([§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)).
//!
//! A generic method's invocation type ([§15.12.2.6]) is computed by the
//! method invocation type inference of [JLS §18.5.2] ([`crate::inference`]):
//! the method's own type parameters become fresh inference variables whose
//! constraints are solved against the actual argument types, so
//! `Collections.identity("s")` is applicable as `identity(String)`, not as the
//! erasure `identity(Object)`. The returned [`MethodData`] is the instantiated
//! invocation: its parameters and return type carry the inferred type
//! arguments. Inference-derived bounds and captured wildcard types
//! ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
//! are modelled; target-type compatibility ([§18.5.2.4]) is incorporated
//! through the `target` argument of [`pick_method`].

use rustc_hash::{FxHashMap, FxHashSet};
use vfs::FileId;

use hir_expand::{
    item_tree::{ItemData, ItemId, TypeParam},
    name::Name,
};

use crate::{
    db::{
        ContextKey, ItemKey, ScopeId, ScopeKind, TyDatabase, access_context_key_query,
        item_ty_query, method_params_query, type_params_map_query,
    },
    inference::{Constraint, Inference, InvocationPhase},
    resolve::{Resolver, item_data, resolve_type_ref, scope_for_file, ty_from_library},
    subtyping::{is_subtype, supertypes_query},
    ty::{Ty, TyData, TyKind, boxed_type, capture_conversion},
};

/// How the method name is qualified: the invocation mode of
/// [JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1):
/// a static invocation (`TypeName.m`, §15.12.1), a super invocation
/// (`super.m` or `TypeName.super.m`), or a virtual invocation (via an
/// expression). The mode restricts which members are candidates
/// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvocationMode {
    /// `TypeName.m(...)`: only static members are candidates.
    Static,
    /// `super.m(...)`: only instance members are candidates.
    Super,
    /// `InterfaceName.super.m(...)`: only instance members are candidates.
    Interface,
    /// `expression.m(...)`: all members except static methods declared in an
    /// interface are candidates.
    Virtual,
}

/// The context of a method invocation: how the name is qualified (the
/// invocation mode, JLS §15.12.1/§15.12.3) and the lexical context used for
/// access control ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
///
/// Source call sites obtain a fully constrained context with [`access_context`]
/// and refine the mode per call site with [`InvocationContext::with_mode`];
/// [`InvocationContext::external`] models a library-only probe call site
/// outside the resolved scope.
#[derive(Debug, Clone)]
pub struct InvocationContext {
    /// The invocation mode.
    pub mode: InvocationMode,
    /// The fully qualified name of the class or interface in which the
    /// invocation appears, for `private` and `protected` access control
    /// ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1),
    /// [§6.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.2)).
    pub enclosing_class: Option<String>,
    /// The package of the compilation unit in which the invocation appears,
    /// for package and `protected` access control; the unnamed package is `""`.
    pub package: Option<String>,
}

impl InvocationContext {
    /// The access control of a probe call site that resides outside `scope` —
    /// a library-only caller that is not a member of any of the resolved
    /// classes. It is affected by access control: it is neither a subclass of,
    /// nor in the package of, any `scope` class, so only `public` members are
    /// candidates ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
    /// Source call sites use [`access_context`] instead.
    pub fn external(_scope: &hir::ResolutionScope) -> Self {
        Self {
            mode: InvocationMode::Virtual,
            // A fully qualified name that is not a subclass of anything in
            // `scope`, and not a member of any of its classes (§6.6.1).
            enclosing_class: Some("library.probe.Caller".to_owned()),
            // The unnamed package: package and `protected` members of named
            // packages are not accessible (§6.6.1).
            package: Some(String::new()),
        }
    }

    /// The context interned as `key` ([`ContextKey`]).
    pub fn from_key(db: &dyn TyDatabase, key: ContextKey) -> InvocationContext {
        InvocationContext {
            mode: *key.mode(db),
            enclosing_class: key
                .enclosing_class(db)
                .as_ref()
                .map(|name| name.as_str().to_owned()),
            package: key
                .package(db)
                .as_ref()
                .map(|name| name.as_str().to_owned()),
        }
    }

    /// The invocation context of the same access site with the invocation mode
    /// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
    /// of the call set to `mode`.
    pub fn with_mode(&self, mode: InvocationMode) -> InvocationContext {
        InvocationContext {
            mode,
            enclosing_class: self.enclosing_class.clone(),
            package: self.package.clone(),
        }
    }
}

/// The access-control context ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
/// of a source call site inside the method or field `item` of `file`: the
/// canonical fully qualified name ([§6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7))
/// of the nearest enclosing class or interface ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1))
/// and the compilation unit's package ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)),
/// with the unnamed package ([§7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
/// as `""`. A virtual invocation
/// ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
/// is assumed; the caller derives the per-call-site mode with
/// [`InvocationContext::with_mode`]. Items outside any class yield `None` for
/// the enclosing class.
pub fn access_context(db: &dyn TyDatabase, file: FileId, item: ItemId) -> InvocationContext {
    let key = access_context_key_query(db, ItemKey::new(db, file, item));
    InvocationContext::from_key(db, key)
}

/// The access of a member ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)),
/// derived from the classfile access flags (ACC_PUBLIC, ACC_PRIVATE,
/// ACC_PROTECTED, [JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1))
/// or the source modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Public,
    Protected,
    Package,
    Private,
}

impl Access {
    fn from_flags(flags: u16) -> Access {
        if flags & 0x0002 != 0 {
            // ACC_PRIVATE
            Access::Private
        } else if flags & 0x0004 != 0 {
            // ACC_PROTECTED
            Access::Protected
        } else if flags & 0x0001 != 0 {
            // ACC_PUBLIC
            Access::Public
        } else {
            Access::Package
        }
    }
}

/// A type parameter of a generic method
/// ([JLS §8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4))
/// with its declared bounds ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
/// kept so [`pick_method`] can run the invocation type inference of
/// [JLS §18.5.2].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodTypeParam {
    pub name: Name,
    pub bounds: Vec<Ty>,
}

/// A candidate method from the member set
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)),
/// instantiated for its declaring type: the class type parameters are
/// substituted with the receiver's actual type arguments, while the method's
/// own type parameters remain as type variables ([`TyKind::TypeVar`]) —
/// [`pick_method`] instantiates them by invocation type inference
/// ([JLS §18.5.2]). The [`MethodData`] returned by [`pick_method`] is the
/// fully instantiated invocation: parameters and return type carry the
/// inferred type arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodData {
    /// The simple name of the method.
    pub name: String,
    /// The fully qualified name of the declaring class or interface.
    pub owner: String,
    /// The parameter types, instantiated with the declaring type's type
    /// arguments; the method's own type parameters are not yet instantiated.
    pub params: Vec<Ty>,
    /// The return type, in the same partially instantiated form.
    pub ret: Ty,
    /// The thrown exceptions ([JLS §8.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.6)),
    /// instantiated with the declaring type's type arguments; the method's own
    /// type parameters are not yet instantiated.
    pub throws: Vec<Ty>,
    /// Whether the method is a variable-arity method
    /// ([JLS §8.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.1)).
    pub varargs: bool,
    /// Whether the method is static.
    pub is_static: bool,
    /// Whether the method is abstract
    /// ([JLS §8.4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.3)):
    /// the ACC_ABSTRACT flag of the classfile
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6))
    /// or the `abstract` modifier of the source. The single abstract method of
    /// a functional interface ([JLS §9.8]) is found from these.
    pub abstract_: bool,
    /// The access of the method
    /// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
    pub access: Access,
    /// The package of the declaring class, or `None` for the unnamed package.
    pub declaring_package: Option<String>,
    /// The fully qualified name of the top-level class of the declaring class
    /// ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
    pub declaring_top_level: Option<String>,
    /// Whether the declaring type is an interface (or annotation).
    pub declaring_interface: bool,
    /// The method's own type parameters
    /// ([JLS §8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4)).
    pub type_params: Vec<MethodTypeParam>,
}

impl MethodData {
    /// Formats this method as a callable signature, e.g.
    /// `java.util.List.add(java.lang.String)`.
    pub fn display<'a>(&'a self, db: &'a dyn TyDatabase) -> MethodDisplay<'a> {
        MethodDisplay { method: self, db }
    }
}

/// A displayable view of a [`MethodData`], produced by [`MethodData::display`].
pub struct MethodDisplay<'a> {
    method: &'a MethodData,
    db: &'a dyn TyDatabase,
}

impl std::fmt::Display for MethodDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}(", self.method.owner, self.method.name)?;
        for (i, param) in self.method.params.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", param.display(self.db))?;
        }
        write!(f, ")")?;
        if !self.method.throws.is_empty() {
            write!(f, " throws ")?;
            for (i, thrown) in self.method.throws.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", thrown.display(self.db))?;
            }
        }
        Ok(())
    }
}

/// The member set of a name on a receiver type
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)):
/// the methods named `name` of the receiver and, transitively, of all its
/// superclasses and superinterfaces, in an unspecified order. The receiver is
/// first captured ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
/// so that wildcard type arguments become fresh type variables, and the
/// candidates are restricted to those allowed by `ctx`'s invocation mode
/// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3))
/// and accessible at the invocation site
/// ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
/// For a type variable receiver the declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
/// are searched instead. Primitive and array receivers yield only the array
/// supertypes' methods. Memoized per (scope, receiver, name, context).
pub fn member_set(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    let scope = ScopeId::new(db, ScopeKind::from_scope(scope));
    let ctx = ContextKey::from_invocation(db, ctx);
    member_set_query(db, scope, receiver.id, Name::new(name), ctx)
}

/// All methods of `ty` across its supertype closure, most-derived first and
/// deduped by overriding signature
/// ([JLS §8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1)):
/// the raw material of the declaration-level checks ([§8.4.8.3],
/// [§9.4.1.3], [`crate::decl_check`]). The access-control context is that of
/// the declaring class itself ([§6.6.1]).
pub fn all_methods(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    member_set_impl(db, scope, receiver, "", ctx)
}

/// The default methods `receiver` inherits, **without** the most-derived
/// dedup of [`member_set_impl`](Self): two unrelated superinterfaces may both
/// declare a matching default without either overriding the other, and that
/// conflict is exactly what the §9.4.1.3 check must see
/// ([JLS §9.4.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.3)).
pub(crate) fn inherited_defaults(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let receiver = capture_conversion(db, *receiver);
    let mut stack = vec![receiver];
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some(ty) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        out.extend(
            class_methods(db, &scope_id, &ty, "")
                .into_iter()
                .filter(|method| {
                    method.declaring_interface && !method.is_static && !method.abstract_
                }),
        );
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push(parent);
        }
    }
    out
}

/// Memoized per (scope, receiver, name, context). See [`member_set`]. The
/// receiver and context are interned ids ([`TyData`], [`ContextKey`]), so
/// repeated member sets at the same call site hit the query cache instead of
/// re-walking the class hierarchy.
#[salsa::tracked(returns(clone))]
pub(crate) fn member_set_query<'db>(
    db: &'db dyn TyDatabase,
    scope: ScopeId,
    receiver: TyData,
    name: Name,
    ctx: ContextKey,
) -> Vec<MethodData> {
    member_set_impl(
        db,
        &scope.kind(db).to_scope(),
        &Ty { id: receiver },
        name.as_str(),
        &InvocationContext::from_key(db, ctx),
    )
}

/// The non-memoized form of [`member_set`].
fn member_set_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let receiver = capture_conversion(db, *receiver);
    let mut stack = match receiver.kind(db) {
        TyKind::TypeVar { bounds, .. } => bounds.to_vec(),
        _ => vec![receiver],
    };
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some(ty) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        out.extend(
            class_methods(db, &scope_id, &ty, name)
                .into_iter()
                .filter(|method| {
                    mode_allows(method, ctx) && is_accessible(db, scope, method, &receiver, ctx)
                }),
        );
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push(parent);
        }
    }
    // §8.4.8.1: an overriding method replaces the overridden one in the
    // member set — a subtype's declaration of a method with the same signature
    // shadows the supertype's — so only the most-derived declaration of each
    // signature survives. The walk is derived-first, so the first occurrence
    // is the most-derived. Without this, `List.iterator()` (overriding
    // `Collection.iterator()`/`Iterable.iterator()`) would surface three
    // identical candidates that the most-specific tie-break reports as
    // ambiguous.
    let mut deduped: Vec<MethodData> = Vec::with_capacity(out.len());
    for method in out {
        if !deduped
            .iter()
            .any(|seen| same_overriding_signature(seen, &method))
        {
            deduped.push(method);
        }
    }
    deduped
}

/// Whether two methods declare the same overriding signature
/// ([JLS §8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1)):
/// identical parameter types and return type, both static or neither, and the
/// same variable-arity behavior. Two such members differing only in declaring
/// type are the same method inherited and overridden down the hierarchy.
fn same_overriding_signature(a: &MethodData, b: &MethodData) -> bool {
    a.params == b.params && a.ret == b.ret && a.is_static == b.is_static && a.varargs == b.varargs
}

/// The abstract methods of the interface `ty` and its superinterfaces
/// ([JLS §9.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.2),
/// [§9.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.8)):
/// used to find the single abstract method that a lambda or method reference
/// is a value of the functional interface for. Memoized per (scope, type).
pub fn abstract_methods(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Vec<MethodData> {
    let scope = ScopeId::new(db, ScopeKind::from_scope(scope));
    abstract_methods_query(db, scope, ty.id)
}

/// Memoized per (scope, type). See [`abstract_methods`].
#[salsa::tracked(returns(clone))]
pub(crate) fn abstract_methods_query(
    db: &dyn TyDatabase,
    scope: ScopeId,
    ty: TyData,
) -> Vec<MethodData> {
    abstract_methods_impl(db, &scope.kind(db).to_scope(), &Ty { id: ty })
}

/// The non-memoized form of [`abstract_methods`].
fn abstract_methods_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let ty = capture_conversion(db, *ty);
    let mut stack = vec![ty];
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some(t) = stack.pop() {
        if !seen.insert(t.id) {
            continue;
        }
        let TyKind::Reference { name, args } = t.kind(db) else {
            continue;
        };
        let Some(resolved) = hir::fqn_resolve(db, scope, name.as_str()) else {
            continue;
        };
        let args = args.clone();
        match resolved {
            hir::Resolved::Library(class) => {
                let Some(record) = hir::class_record(db, &class) else {
                    continue;
                };
                let hir::ClassOrModuleStub::Class(stub) = record.as_ref() else {
                    continue;
                };
                let interner = &db.hir_state().interner;
                for method in &stub.methods {
                    if method.flags & 0x0400 == 0 {
                        continue;
                    }
                    let name = interner.resolve(&method.name);
                    out.extend(
                        library_class_methods(db, class.clone(), args.clone(), name)
                            .into_iter()
                            .filter(|m| m.abstract_),
                    );
                }
            }
            hir::Resolved::Source(source) => {
                let tree = hir::file_item_tree(db, source.file);
                let Some(ItemData::Interface(class)) = item_data(&tree, source.item) else {
                    continue;
                };
                for &item in &class.body {
                    let Some(ItemData::Method(method)) = item_data(&tree, item) else {
                        continue;
                    };
                    if !method.modifiers.abstract_ {
                        continue;
                    }
                    let name = method.name.as_str().to_owned();
                    out.extend(
                        source_class_methods(db, source, args.clone(), &name)
                            .into_iter()
                            .filter(|m| m.abstract_),
                    );
                }
            }
        }
        for parent in supertypes_query(db, scope_id, t.id) {
            stack.push(parent);
        }
    }
    out
}

/// The single abstract method of the functional interface `ty`
/// ([JLS §9.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.8)):
/// the unique abstract method of the interface, disregarding those that
/// override `Object` members (`equals`, `hashCode`, `toString`). `None` when
/// `ty` is not a functional interface.
pub fn single_abstract_method(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<MethodData> {
    let mut methods = abstract_methods(db, scope, ty);
    methods.retain(|m| !matches!(m.name.as_str(), "equals" | "hashCode" | "toString"));
    if methods.len() == 1 {
        methods.pop()
    } else {
        None
    }
}

/// The methods of a single class or interface, instantiated with `ty`'s type
/// arguments.
fn class_methods(db: &dyn TyDatabase, scope_id: &ScopeId, ty: &Ty, name: &str) -> Vec<MethodData> {
    let TyKind::Reference {
        name: class_name,
        args,
    } = ty.kind(db)
    else {
        return Vec::new();
    };
    let Some(resolved) = hir::fqn_resolve(db, &scope_id.kind(db).to_scope(), class_name.as_str())
    else {
        return Vec::new();
    };
    let args = args.clone();
    match resolved {
        hir::Resolved::Library(class) => library_class_methods(db, class, args, name),
        hir::Resolved::Source(source) => source_class_methods(db, source, args, name),
    }
}

/// The package of a fully qualified class name, or `None` for the unnamed
/// package.
fn package_of(fqn: &str) -> Option<String> {
    fqn.rfind('.').map(|i| fqn[..i].to_owned())
}

/// The fully qualified name of the top-level class of a *library* binary
/// name: the name up to the first `$`. Library nested classes are named
/// `Outer$Inner` ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2));
/// source names nest with dots and must use [`source_top_level`] instead —
/// `$` inside them is an ordinary identifier character ([JLS §3.8]).
fn top_level_of(fqn: &str) -> String {
    match fqn.find('$') {
        Some(i) => fqn[..i].to_owned(),
        None => fqn.to_owned(),
    }
}

/// The fully qualified name of the top-level class of a *source* `fqn`: the
/// known package plus the first enclosing type — `com.example.Outer.Inner` is
/// `com.example.Outer`, an unnamed-package `Outer.Inner` is `Outer`
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)).
/// Source names never separate nesting with `$`, so none is split off.
fn source_top_level(package: Option<&str>, fqn: &str) -> String {
    let rest = match package {
        Some(pkg) => fqn
            .strip_prefix(pkg)
            .and_then(|rest| rest.strip_prefix('.'))
            .unwrap_or(fqn),
        None => fqn,
    };
    let top = rest.split('.').next().unwrap_or(rest);
    match package {
        Some(pkg) if !pkg.is_empty() => format!("{pkg}.{top}"),
        _ => top.to_owned(),
    }
}

/// The methods of a library class, whose `Signature` attribute
/// ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1))
/// may declare type parameters. The class's parameters are bound to `args`;
/// the method's own parameters (generic methods) keep their type parameters —
/// [`pick_method`] instantiates them by invocation type inference
/// ([JLS §18.5.2]).
fn library_class_methods(
    db: &dyn TyDatabase,
    class: hir::ResolvedClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<MethodData> {
    let Some(record) = hir::class_record(db, &class) else {
        return Vec::new();
    };
    let hir::ClassOrModuleStub::Class(class) = record.as_ref() else {
        return Vec::new();
    };
    let interner = &db.hir_state().interner;
    let binding: FxHashMap<Name, Ty> = if args.is_empty() {
        FxHashMap::default()
    } else {
        class
            .type_params
            .iter()
            .zip(args.iter().copied())
            .map(|(tp, arg)| (Name::new(interner.resolve(&tp.name)), arg))
            .collect()
    };
    let fqn = interner.resolve(&class.fqn).to_owned();
    let declaring_package = package_of(&fqn);
    let declaring_top_level = Some(top_level_of(&fqn));
    let declaring_interface = matches!(
        hir::ClassKind::from_flags(class.flags, class.is_record),
        hir::ClassKind::Interface | hir::ClassKind::Annotation
    );
    let instantiate =
        |tyref: &hir::TypeRef<hir::Symbol>| ty_from_library(db, tyref).substitute(db, &binding);

    let mut out = Vec::new();
    for method in &class.methods {
        // An empty name is the wildcard of the declaration-level walk
        // ([§9.8], [`crate::decl_check`]); no method can be named "".
        if !name.is_empty() && interner.resolve(&method.name) != name {
            continue;
        }
        let type_params = method
            .type_params
            .iter()
            .map(|tp| MethodTypeParam {
                name: Name::new(interner.resolve(&tp.name)),
                // The bound is instantiated with the declaring class's type
                // arguments: a `<U extends T>` bound on a generic class
                // references the class type parameter `T`, which resolves to
                // the receiver's actual argument here (§18.5.2.2). A bound
                // over the method's own type parameters is untouched by the
                // class binding and stays a bare type variable.
                bounds: tp.bounds.iter().map(&instantiate).collect(),
            })
            .collect();
        out.push(MethodData {
            // The method's own name — not the lookup filter, which is the
            // empty wildcard in the declaration-level walk.
            name: interner.resolve(&method.name).to_owned(),
            owner: fqn.clone(),
            params: method
                .params
                .iter()
                .map(|param| instantiate(&param.param_type))
                .collect(),
            ret: instantiate(&method.return_type),
            throws: method.throws_list.iter().map(instantiate).collect(),
            varargs: method.flags & 0x0080 != 0,   // ACC_VARARGS
            is_static: method.flags & 0x0008 != 0, // ACC_STATIC
            abstract_: method.flags & 0x0400 != 0, // ACC_ABSTRACT
            access: Access::from_flags(method.flags),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface,
            type_params,
        });
    }
    out
}

/// The methods of a source class, resolved against the file's own scope and
/// instantiated with `args`. Class type parameters are bound to `args`; method
/// type parameters are kept — [`pick_method`] instantiates them.
fn source_class_methods(
    db: &dyn TyDatabase,
    source: hir::SourceClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<MethodData> {
    let tree = hir::file_item_tree(db, source.file);
    let Some(class_data) = item_data(&tree, source.item) else {
        return Vec::new();
    };
    let declared: &[TypeParam] = match class_data {
        ItemData::Class(d) | ItemData::Interface(d) => &d.type_params,
        ItemData::Record(d) => &d.type_params,
        _ => &[],
    };
    let binding: FxHashMap<Name, Ty> = if args.is_empty() {
        FxHashMap::default()
    } else {
        declared
            .iter()
            .map(|tp| tp.name.clone())
            .zip(args.iter().copied())
            .collect()
    };
    let scope = scope_for_file(db, source.file);
    let type_params = type_params_map_query(db, db.file_text(source.file));
    let resolver = Resolver::new(&tree, type_params, source.item);
    let fqn = hir::source_class_fqn(db, source.file, source.item)
        .map(|fqn| fqn.as_str().to_owned())
        .unwrap_or_default();
    let package = resolver.package().map(|p| p.as_str().to_owned());
    let declaring_package = Some(package.clone().unwrap_or_default());
    // Source names nest with dots: the top level is package + first type.
    let declaring_top_level = (!fqn.is_empty()).then(|| source_top_level(package.as_deref(), &fqn));
    let declaring_interface =
        matches!(class_data, ItemData::Interface(_) | ItemData::Annotation(_));

    let mut out = Vec::new();
    for item in class_data.body().to_vec() {
        let Some(ItemData::Method(method)) = item_data(&tree, item) else {
            continue;
        };
        // An empty name is the wildcard of the declaration-level walk
        // ([§9.8], [`crate::decl_check`]); no method can be named "".
        if !name.is_empty() && method.name.as_str() != name {
            continue;
        }
        let method_resolver = Resolver::new(&tree, type_params, item);
        let type_params = method
            .sig
            .type_params
            .iter()
            .map(|tp| MethodTypeParam {
                name: tp.name.clone(),
                // Same rule as the library side ([`library_class_methods`]):
                // a bound referencing the declaring class's type parameters is
                // replaced by the class's actual type arguments; one over the
                // method's own type parameters is untouched.
                bounds: tp
                    .bounds
                    .iter()
                    .map(|bound| resolve_type_ref(db, &scope, &method_resolver, bound))
                    .map(|bound| bound.substitute(db, &binding))
                    .collect(),
            })
            .collect();
        let key = ItemKey::new(db, source.file, item);
        let instantiate = |ty: &Ty| ty.substitute(db, &binding);
        let varargs = method.sig.params.last().is_some_and(|param| param.varargs);
        let mut params: Vec<Ty> = method_params_query(db, key)
            .iter()
            .map(instantiate)
            .collect();
        // A variable-arity parameter `T...` is lowered as the element type
        // `T`; its formal type is the array `T[]` ([JLS §8.4.1]).
        if varargs && let Some(last) = params.last_mut() {
            *last = Ty::array(db, *last);
        }
        let ret = instantiate(&item_ty_query(db, key));
        // The declared throws clause ([§8.4.6]): resolve and instantiate with
        // the declaring type's type arguments; method type parameters stay as
        // type variables for [`instantiate`] to solve.
        let mut throws: Vec<Ty> = method
            .sig
            .throws
            .iter()
            .map(|ex| resolve_type_ref(db, &scope, &method_resolver, ex))
            .map(|ty| instantiate(&ty))
            .collect();
        throws.dedup();
        out.push(MethodData {
            // The method's own name — not the lookup filter, which is the
            // empty wildcard in the declaration-level walk.
            name: method.name.as_str().to_owned(),
            owner: fqn.clone(),
            params,
            ret,
            throws,
            varargs,
            is_static: method.modifiers.static_,
            abstract_: method.modifiers.abstract_,
            access: access_of(&method.modifiers),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface,
            type_params,
        });
    }
    out
}

fn access_of(modifiers: &hir_expand::modifiers::Modifiers) -> Access {
    if modifiers.private {
        Access::Private
    } else if modifiers.protected {
        Access::Protected
    } else if modifiers.public {
        Access::Public
    } else {
        Access::Package
    }
}

/// Whether `method` is allowed by the invocation mode of `ctx`
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
/// [§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
fn mode_allows(method: &MethodData, ctx: &InvocationContext) -> bool {
    match ctx.mode {
        // A static invocation selects only static members.
        InvocationMode::Static => method.is_static,
        // Super and interface invocations select only instance members.
        InvocationMode::Super | InvocationMode::Interface => !method.is_static,
        // A virtual invocation must not select a static method declared in an
        // interface (§15.12.3); a static method of a class may be selected.
        InvocationMode::Virtual => !(method.is_static && method.declaring_interface),
    }
}

/// Whether `method` is accessible to the class in which the invocation appears
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)),
/// when accessed through the receiver expression of type `receiver`.
fn is_accessible(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    receiver: &Ty,
    ctx: &InvocationContext,
) -> bool {
    member_accessible(
        db,
        scope,
        method.access,
        method.declaring_package.as_deref(),
        method.owner.as_str(),
        method.declaring_top_level.as_deref(),
        receiver,
        method.is_static,
        ctx,
    )
}

/// Whether a member with `access` declared in `declaring_package` by the class
/// `owner` (whose top-level class is `declaring_top_level`) is accessible to
/// the class in which the access appears
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)),
/// when accessed through the receiver expression of type `receiver`. The two
/// names serve different rules: §6.6.1 scopes *private* access by the
/// top-level class, while §6.6.2 requires a subclass of the *declaring* class.
#[allow(clippy::too_many_arguments)]
fn member_accessible(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    access: Access,
    declaring_package: Option<&str>,
    owner: &str,
    declaring_top_level: Option<&str>,
    receiver: &Ty,
    static_member: bool,
    ctx: &InvocationContext,
) -> bool {
    match access {
        Access::Public => true,
        // §6.6.1: a private member is accessible throughout the top-level
        // class in which it is declared.
        Access::Private => match (&ctx.enclosing_class, declaring_top_level) {
            (Some(enclosing), Some(declaring)) => within_top_level(enclosing, declaring),
            _ => false,
        },
        // §6.6.1: a package member is accessible only within its own package;
        // the unnamed package ([§7.4.2]) is `""`.
        Access::Package => match (&ctx.package, declaring_package) {
            (Some(invocation), Some(declaring)) => invocation == declaring,
            _ => false,
        },
        // §6.6.2: a protected member is accessible within the declaring
        // package, or from a class that is a subclass of the declaring class.
        Access::Protected => {
            if let (Some(invocation), Some(declaring)) = (&ctx.package, declaring_package)
                && invocation == declaring
            {
                return true;
            }
            match &ctx.enclosing_class {
                Some(enclosing) => {
                    let enclosing = Ty::reference(db, enclosing.as_str(), Vec::new());
                    let declaring = Ty::reference(db, owner, Vec::new());
                    if !is_subtype(db, scope, &enclosing, &declaring) {
                        return false;
                    }
                    // §6.6.2: a protected instance member accessed outside the
                    // declaring package by a receiver expression requires the
                    // type of that expression to be a subtype of the enclosing
                    // class. A `super` invocation accesses the member through
                    // the `super` keyword, not an expression, so the rule does
                    // not apply ([§15.12.1]).
                    if static_member || ctx.mode == InvocationMode::Super {
                        true
                    } else {
                        is_subtype(db, scope, receiver, &enclosing)
                    }
                }
                None => false,
            }
        }
    }
}

/// Whether the class `enclosing` is the top-level class `declaring` or
/// lexically inside it ([JLS §6.6.1]): a private member is accessible
/// throughout the body of its top-level class, including from nested classes.
/// `declaring_top_level` is always the top level itself, so containment is
/// plain dot-prefix matching; the access site's enclosing class is a *source*
/// dotted name (inference only runs on source), so `$` never separates it.
fn within_top_level(enclosing: &str, declaring: &str) -> bool {
    enclosing == declaring || enclosing.starts_with(&format!("{}.", declaring))
}

/// An actual argument of a method invocation: either a concrete type, or a
/// poly expression — a lambda or method reference ([JLS §15.27.3],
/// [§15.13.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13.2)) —
/// whose type is the target functional interface of the applicable candidate
/// ([JLS §18.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2)).
/// A poly argument does not constrain the invocation type inference; once the
/// candidate is resolved the caller types it against the resolved formal
/// parameter. The lambda's parameter count ([§15.12.2.2/§15.12.2.3]) is
/// carried along so an overload candidate whose functional interface does not
/// fit the lambda is not applicable.
#[derive(Debug, Clone)]
pub enum PolyArg {
    /// An argument with a concrete standalone type.
    Concrete(Ty),
    /// A poly argument — the lambda or method reference expression. The second
    /// element is the lambda's parameter count; a method reference is not
    /// arity-checkable without resolving the referenced method, so it is `None`.
    Poly(hir_expand::body::ExprId, Option<usize>),
}

impl From<Ty> for PolyArg {
    fn from(ty: Ty) -> Self {
        PolyArg::Concrete(ty)
    }
}

/// Instantiates `method` to its invocation type
/// ([JLS §15.12.2.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.6))
/// for a call with `args` in `phase`, running the method invocation type
/// inference of [JLS §18.5.2] against the actual argument types. `target` is
/// the expected type of the invocation
/// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)):
/// when present and compatible with the invocation type's return type, the
/// constraint ⟨R → T⟩ joins the constraint set before resolution, so the
/// inference variables are also bounded by the target type. `None` when
/// `method` is not applicable in this phase. `varargs` selects the variable-
/// arity invocation rules of [§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4).
fn instantiate(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    args: &[PolyArg],
    phase: InvocationPhase,
    varargs: bool,
    target: Option<Ty>,
) -> Option<MethodData> {
    let mut inference = Inference::new();

    // The method's own type parameters become fresh inference variables; their
    // declared bounds are the initial upper bounds (§18.5.2.2), with the type
    // parameter names substituted by the variables.
    let mut subst: FxHashMap<Name, Ty> = FxHashMap::default();
    for tp in &method.type_params {
        let var = inference.fresh_var(db);
        subst.insert(tp.name.clone(), var);
        let bounds: Vec<Ty> = tp.bounds.iter().map(|b| b.substitute(db, &subst)).collect();
        if bounds.is_empty() {
            inference.add_upper(db, var, Ty::reference(db, "java.lang.Object", Vec::new()));
        } else {
            for bound in bounds {
                inference.add_upper(db, var, bound);
            }
        }
    }
    let formals: Vec<Ty> = method
        .params
        .iter()
        .map(|p| p.substitute(db, &subst))
        .collect();

    // The throws clause ([§8.4.6]) substitutes the same way; a method type
    // parameter that appears in it carries the `throws` α bound (§18.5.2.2,
    // §18.1.3), directing resolution to prefer an unchecked exception type.
    let throws_formals: Vec<Ty> = method
        .throws
        .iter()
        .map(|t| t.substitute(db, &subst))
        .collect();
    for thrown in &throws_formals {
        if thrown.is_infer_var(db) {
            inference.mark_throws(db, *thrown);
        }
    }

    // §15.12.2.2/§15.12.2.3: a lambda is compatible with a function type only
    // when the parameter list has the same arity as the single abstract
    // method ([§15.27.3]). A candidate whose functional interface does not
    // fit a lambda argument is not applicable; a method reference contributes
    // no arity check here (it is resolved against the SAM after selection).
    for (formal, arg) in formals.iter().zip(args) {
        if let PolyArg::Poly(_, Some(arity)) = arg
            && let Some(sam) = single_abstract_method(db, scope, formal)
            && sam.params.len() != *arity
        {
            return None;
        }
    }

    // In the loose phase (and for variable-arity invocation, §15.12.2.4)
    // primitive arguments are boxed, so `⟨int → α⟩` yields the boxed lower
    // bound. A poly argument (a lambda or method reference) is not boxed: its
    // type is the target functional interface, and it contributes no
    // constraint (§15.12.2.2/§15.12.2.3, §18.5.2.2).
    let args: Vec<Option<Ty>> = match phase {
        InvocationPhase::Strict => args
            .iter()
            .map(|arg| match arg {
                PolyArg::Concrete(ty) => Some(*ty),
                PolyArg::Poly(_, _) => None,
            })
            .collect(),
        InvocationPhase::Loose => args
            .iter()
            .map(|arg| match arg {
                PolyArg::Concrete(ty) => Some(match ty.kind(db) {
                    TyKind::Primitive(p) => Ty::reference(db, boxed_type(*p), Vec::new()),
                    _ => *ty,
                }),
                PolyArg::Poly(_, _) => None,
            })
            .collect(),
    };

    let mut constraints = Vec::new();
    if varargs {
        if !method.varargs || args.len() + 1 < formals.len() {
            return None;
        }
        let (fixed, last) = formals.split_at(formals.len() - 1);
        for (formal, arg) in fixed.iter().zip(&args) {
            if let Some(arg) = arg {
                constraints.push(Constraint::Sub(*arg, *formal));
            }
        }
        let rest = &args[fixed.len()..];
        if !rest.is_empty() {
            if rest.len() == 1 && rest[0].is_some_and(|t| t.is_array(db)) {
                // A single trailing actual of the array type is used as-is.
                if let Some(arg) = rest[0] {
                    constraints.push(Constraint::Sub(arg, last[0]));
                }
            } else {
                // Otherwise the trailing actuals are packed into the array: each
                // is related to the element type.
                let element = last[0].element(db)?;
                for arg in rest.iter().flatten() {
                    constraints.push(Constraint::Sub(*arg, *element));
                }
            }
        }
    } else {
        if formals.len() != args.len() {
            return None;
        }
        for (formal, arg) in formals.iter().zip(&args) {
            if let Some(arg) = arg {
                constraints.push(Constraint::Sub(*arg, *formal));
            }
        }
    }

    // §18.5.2.4 (resolution): when the invocation is a poly expression with an
    // expected type, the constraint ⟨R → T⟩ is incorporated with the
    // argument constraints, so the inference variables are bounded by the
    // target type as well. Only a generic method can have a poly invocation
    // ([JLS §15.12.2.6]): a non-generic method's return type is fixed, so a
    // mismatched target must not reject an otherwise-applicable invocation.
    if let Some(target) = target
        && !method.type_params.is_empty()
    {
        let invocation_ret = method.ret.substitute(db, &subst);
        constraints.push(Constraint::Sub(invocation_ret, target));
    }

    let resolved = inference.solve(db, scope, phase, constraints)?;

    // The invocation type's throws clause ([§18.5.2.3]): the method's throws
    // types with the resolved substitution applied.
    let mut throws: Vec<Ty> = throws_formals
        .iter()
        .map(|t| t.substitute_infer(db, &resolved))
        .collect();
    throws.dedup();

    Some(MethodData {
        name: method.name.clone(),
        owner: method.owner.clone(),
        params: formals
            .iter()
            .map(|p| p.substitute_infer(db, &resolved))
            .collect(),
        ret: method
            .ret
            .substitute(db, &subst)
            .substitute_infer(db, &resolved),
        throws,
        varargs: method.varargs,
        is_static: method.is_static,
        abstract_: method.abstract_,
        access: method.access,
        declaring_package: method.declaring_package.clone(),
        declaring_top_level: method.declaring_top_level.clone(),
        declaring_interface: method.declaring_interface,
        type_params: method.type_params.clone(),
    })
}

/// Whether `m1` is more specific than `m2`
/// ([JLS §15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)):
/// a non-variable-arity method beats a variable-arity one, a non-generic
/// method beats a generic one, and otherwise every formal parameter of `m1` is
/// a subtype of the corresponding formal of `m2`. For two generic methods the
/// type-parameter-relative signatures are compared: `m2`'s type parameters are
/// substituted by `m1`'s (by position), and `m1`'s declared bounds must be at
/// least as restrictive as `m2`'s, so `<T extends String>` is more specific
/// than `<T>`.
pub(crate) fn more_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    m1: &MethodData,
    m2: &MethodData,
) -> bool {
    if m1.params.len() != m2.params.len() {
        return false;
    }
    if m1.varargs != m2.varargs {
        return !m1.varargs;
    }
    // §15.12.2.5: a non-generic method is more specific than a generic one.
    if m1.type_params.is_empty() != m2.type_params.is_empty() {
        return m1.type_params.is_empty();
    }
    if !m1.type_params.is_empty() {
        // Both generic: compare the type-parameter-relative signatures. m2's
        // type parameters are substituted by m1's (excess become `Object`),
        // and m1 must be at least as restrictive: its declared bounds are
        // subtypes of m2's, and its parameters are subtypes of m2's.
        let object = Ty::reference(db, "java.lang.Object", Vec::new());
        let mut subst: FxHashMap<Name, Ty> = FxHashMap::default();
        for (i, tp2) in m2.type_params.iter().enumerate() {
            let t1 = match m1.type_params.get(i) {
                Some(tp1) => Ty::type_var(db, tp1.name.clone(), tp1.bounds.clone()),
                None => object,
            };
            subst.insert(tp2.name.clone(), t1);
        }
        let m2_params: Vec<Ty> = m2.params.iter().map(|p| p.substitute(db, &subst)).collect();
        let bounds_ok = m1
            .type_params
            .iter()
            .zip(&m2.type_params)
            .all(|(tp1, tp2)| {
                let b1: Vec<Ty> = if tp1.bounds.is_empty() {
                    vec![object]
                } else {
                    tp1.bounds.clone()
                };
                let b2: Vec<Ty> = if tp2.bounds.is_empty() {
                    vec![object]
                } else {
                    tp2.bounds.clone()
                };
                b1.iter()
                    .all(|x| b2.iter().any(|y| is_subtype(db, scope, x, y)))
            });
        return bounds_ok
            && m1
                .params
                .iter()
                .zip(&m2_params)
                .all(|(param1, param2)| is_subtype(db, scope, param1, param2));
    }
    m1.params
        .iter()
        .zip(&m2.params)
        .all(|(param1, param2)| is_subtype(db, scope, param1, param2))
}

/// The most specific method among `candidates`, all applicable by the same
/// phase ([JLS §15.12.2.5]). Each candidate is the `(generic member, inferred
/// invocation)` pair produced by [`instantiate`]; specificity is decided on the
/// generic member. `None` when no candidate is strictly more specific than
/// every other (an ambiguity error), or when the set is empty.
fn choose_most_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    candidates: &[(MethodData, MethodData)],
) -> Option<MethodData> {
    if candidates.len() == 1 {
        return candidates.first().map(|(_, invocation)| invocation.clone());
    }
    let mut best: Option<usize> = None;
    for (i, (candidate, _)) in candidates.iter().enumerate() {
        let wins = candidates
            .iter()
            .all(|(other, _)| other == candidate || more_specific(db, scope, candidate, other));
        if wins {
            if best.is_some() {
                return None;
            }
            best = Some(i);
        }
    }
    best.map(|i| candidates[i].1.clone())
}

/// Resolves a method call `receiver.name(args)` by the applicability phases of
/// [JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2):
/// strict invocation ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)),
/// then loose invocation ([§15.12.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.3)),
/// then variable arity ([§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4));
/// the most specific applicable method
/// ([§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5))
/// wins. `None` when no method is applicable or the applicable ones are
/// ambiguous. The candidate set is restricted by the invocation mode and
/// access of `ctx` ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
/// [§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
/// The returned [`MethodData`] is the inferred invocation type
/// ([JLS §18.5.2]), refined by `target` — the expected type of the
/// invocation in its context
/// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)) —
/// when the call is a poly expression.
pub fn pick_method(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    args: &[PolyArg],
    ctx: &InvocationContext,
    target: Option<Ty>,
) -> Option<MethodData> {
    let members = member_set(db, scope, receiver, name, ctx);

    // Phase 1: strict invocation (§15.12.2.2) — no boxing or unboxing, fixed
    // arity.
    let strict: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(
                db,
                scope,
                method,
                args,
                InvocationPhase::Strict,
                false,
                target,
            )
            .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !strict.is_empty() {
        return choose_most_specific(db, scope, &strict);
    }

    // Phase 2: loose invocation (§15.12.2.3) — boxing and unboxing allowed.
    let loose: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(
                db,
                scope,
                method,
                args,
                InvocationPhase::Loose,
                false,
                target,
            )
            .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !loose.is_empty() {
        return choose_most_specific(db, scope, &loose);
    }

    // Phase 3: variable arity (§15.12.2.4).
    let varargs: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(
                db,
                scope,
                method,
                args,
                InvocationPhase::Loose,
                true,
                target,
            )
            .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !varargs.is_empty() {
        return choose_most_specific(db, scope, &varargs);
    }

    None
}

/// A field resolved through the member set of a field access
/// ([JLS §15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)),
/// instantiated with the receiver type's type arguments (type variables are not
/// yet instantiated — fields carry no type parameters of their own, so the
/// field type is the declaration type with the receiver's type arguments
/// substituted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldData {
    /// The simple name of the field.
    pub name: String,
    /// The fully qualified name of the declaring class or interface.
    pub owner: String,
    /// The field's type, instantiated with the declaring type's type arguments.
    pub ty: Ty,
    /// Whether the field is static.
    pub is_static: bool,
    /// The access of the field
    /// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
    pub access: Access,
    /// The package of the declaring class, or `None` for the unnamed package.
    pub declaring_package: Option<String>,
    /// The fully qualified name of the top-level class of the declaring class
    /// ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
    pub declaring_top_level: Option<String>,
}

/// Resolves a field access `receiver.name`
/// ([JLS §15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)):
/// the field named `name` of the receiver type, or of the closest of its
/// superclasses and superinterfaces (the member set of
/// [§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)
/// — field hiding is resolved in favour of the most derived declaration).
/// The receiver is first captured ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
/// so wildcard type arguments become fresh type variables, and only fields
/// accessible at the access site ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
/// are returned. For a type variable receiver the declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
/// are searched instead. `None` when no field is found.
pub fn pick_field(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Option<FieldData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let receiver = capture_conversion(db, *receiver);
    let mut stack = match receiver.kind(db) {
        TyKind::TypeVar { bounds, .. } => bounds.to_vec(),
        _ => vec![receiver],
    };
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    while let Some(ty) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        for field in class_fields(db, &scope_id, &ty, name) {
            if member_accessible(
                db,
                scope,
                field.access,
                field.declaring_package.as_deref(),
                field.owner.as_str(),
                field.declaring_top_level.as_deref(),
                &receiver,
                field.is_static,
                ctx,
            ) {
                return Some(field);
            }
        }
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push(parent);
        }
    }
    None
}

/// The fields of a single class or interface, instantiated with `ty`'s type
/// arguments.
fn class_fields(db: &dyn TyDatabase, scope_id: &ScopeId, ty: &Ty, name: &str) -> Vec<FieldData> {
    let TyKind::Reference {
        name: class_name,
        args,
    } = ty.kind(db)
    else {
        return Vec::new();
    };
    let Some(resolved) = hir::fqn_resolve(db, &scope_id.kind(db).to_scope(), class_name.as_str())
    else {
        return Vec::new();
    };
    let args = args.clone();
    match resolved {
        hir::Resolved::Library(class) => library_class_fields(db, class, args, name),
        hir::Resolved::Source(source) => source_class_fields(db, source, args, name),
    }
}

/// The fields of a library class, instantiated with the class's type
/// parameters bound to `args`.
fn library_class_fields(
    db: &dyn TyDatabase,
    class: hir::ResolvedClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<FieldData> {
    let Some(record) = hir::class_record(db, &class) else {
        return Vec::new();
    };
    let hir::ClassOrModuleStub::Class(class) = record.as_ref() else {
        return Vec::new();
    };
    let interner = &db.hir_state().interner;
    let binding: FxHashMap<Name, Ty> = if args.is_empty() {
        FxHashMap::default()
    } else {
        class
            .type_params
            .iter()
            .zip(args.iter().copied())
            .map(|(tp, arg)| (Name::new(interner.resolve(&tp.name)), arg))
            .collect()
    };
    let fqn = interner.resolve(&class.fqn).to_owned();
    let declaring_package = package_of(&fqn);
    let declaring_top_level = Some(top_level_of(&fqn));
    let mut out = Vec::new();
    for field in &class.fields {
        if interner.resolve(&field.name) != name {
            continue;
        }
        out.push(FieldData {
            name: name.to_owned(),
            owner: fqn.clone(),
            ty: ty_from_library(db, &field.field_type).substitute(db, &binding),
            is_static: field.flags & 0x0008 != 0, // ACC_STATIC
            access: Access::from_flags(field.flags),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
        });
    }
    out
}

/// The fields of a source class, resolved against the file's own scope and
/// instantiated with `args`.
fn source_class_fields(
    db: &dyn TyDatabase,
    source: hir::SourceClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<FieldData> {
    let tree = hir::file_item_tree(db, source.file);
    let Some(class_data) = item_data(&tree, source.item) else {
        return Vec::new();
    };
    let declared: &[TypeParam] = match class_data {
        ItemData::Class(d) | ItemData::Interface(d) => &d.type_params,
        ItemData::Record(d) => &d.type_params,
        _ => &[],
    };
    let binding: FxHashMap<Name, Ty> = if args.is_empty() {
        FxHashMap::default()
    } else {
        declared
            .iter()
            .map(|tp| tp.name.clone())
            .zip(args.iter().copied())
            .collect()
    };
    let type_params = type_params_map_query(db, db.file_text(source.file));
    let resolver = Resolver::new(&tree, type_params, source.item);
    let fqn = hir::source_class_fqn(db, source.file, source.item)
        .map(|fqn| fqn.as_str().to_owned())
        .unwrap_or_default();
    let package = resolver.package().map(|p| p.as_str().to_owned());
    let declaring_package = Some(package.clone().unwrap_or_default());
    // Source names nest with dots: the top level is package + first type.
    let declaring_top_level = (!fqn.is_empty()).then(|| source_top_level(package.as_deref(), &fqn));

    let mut out = Vec::new();
    for &item in class_data.body() {
        let Some(ItemData::Field(field)) = item_data(&tree, item) else {
            continue;
        };
        if field.name.as_str() != name {
            continue;
        }
        let key = ItemKey::new(db, source.file, item);
        out.push(FieldData {
            name: name.to_owned(),
            owner: fqn.clone(),
            ty: item_ty_query(db, key).substitute(db, &binding),
            is_static: field.modifiers.static_,
            access: access_of(&field.modifiers),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_top_level_splits_first_dollar() {
        assert_eq!(top_level_of("java.util.Map$Entry"), "java.util.Map");
        assert_eq!(top_level_of("com.example.Foo"), "com.example.Foo");
    }

    #[test]
    fn source_top_level_keeps_dollar_identifiers() {
        // §3.8: `$` is part of the identifier; the top level of `A$B` is `A$B`.
        assert_eq!(
            source_top_level(Some("com.example"), "com.example.A$B"),
            "com.example.A$B"
        );
        // §6.6.1: the top level of a nested class is its first enclosing type.
        assert_eq!(
            source_top_level(Some("com.example"), "com.example.Outer.Inner"),
            "com.example.Outer"
        );
        // The unnamed package ([§7.4.2]) has no prefix to keep.
        assert_eq!(source_top_level(None, "Outer.Inner"), "Outer");
        assert_eq!(source_top_level(None, "A$B"), "A$B");
    }
}
