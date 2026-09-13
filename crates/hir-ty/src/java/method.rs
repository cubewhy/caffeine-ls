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
//! method invocation type inference of [JLS §18.5.2] ([`crate::java::inference`]):
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
use smol_str::SmolStr;
use vfs::FileId;

use hir_def::java::item_tree::{ItemData, ItemId, ItemTree, TypeParam};
use hir_def::jvm::access::{JvmAccessFlags, JvmVisibility};
use hir_expand::name::Name;

use crate::{
    java::db::{
        ContextKey, ItemKey, ScopeId, ScopeKind, TyDatabase, access_context_key_query,
        item_ty_query, method_params_query,
    },
    java::inference::{Constraint, Inference, InvocationPhase},
    java::resolve::{Resolver, item_data, resolve_type_ref, scope_for_file},
    java::subtyping::{is_subtype, supertypes_query},
    java::ty::{Ty, TyData, TyKind, TypeVarScope, boxed_type, capture_conversion},
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
    /// `T::m` — a *type-qualified* method reference ([JLS §15.13.1]): the
    /// reference resolves a static member (declared in a class *or* an
    /// interface) or an unbound instance member, so no static/instance filter
    /// applies — the §15.12.3 virtual-invocation restriction that excludes
    /// static interface methods is specific to receiver-expression invocations.
    TypeQualified,
    /// `super.m(...)`: only instance members are candidates.
    Super,
    /// `InterfaceName.super.m(...)`: only instance members are candidates.
    Interface,
    /// An unqualified `m(...)` whose receiver is the implicit `this`
    /// ([JLS §15.12.1] MethodName form): the class-or-interface members are
    /// candidates as in a virtual invocation, but a *static* member is also
    /// reachable when it is declared in the class/interface the receiver names
    /// ([§15.12.3]): javac resolves `s()` inside the interface that declares
    /// `static void s()`, yet rejects `expr.s()` where `expr`'s type merely
    /// implements the interface — the static-interface-member exclusion of the
    /// virtual-invocation form applies only when the method is reached through
    /// an expression.
    MethodName,
    /// `expression.m(...)`: all members except static methods declared in an
    /// interface are candidates.
    Virtual,
}

/// The declaring class of a member, or the class a member access is made
/// from: a classpath or source class carrying a canonical fully qualified name
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)),
/// or a declaration with no canonical name — a *local* class-like declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)),
/// or a member type of one — which is identified by its declaration instead.
///
/// The distinction is load-bearing wherever a class is compared with another:
/// two same-named local declarations in different methods are different
/// classes, and neither is the class of that simple name elsewhere in the
/// file, so every comparison keys on this value rather than on a name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClassKey {
    /// A class named by its canonical fully qualified name ([§6.7]): a source
    /// class of the workspace, or a classpath (binary) name.
    Named(Name),
    /// A declaration with no canonical name ([§6.7]).
    Local(hir::SourceClass),
}

impl ClassKey {
    /// The key of the class-like declaration `item` of `file`: its canonical
    /// fully qualified name, or the declaration when it has none.
    pub fn of(tree: &ItemTree, file: FileId, item: ItemId) -> ClassKey {
        match crate::java::resolve::canonical_class_fqn(tree, item) {
            Some(fqn) => ClassKey::Named(fqn),
            None => ClassKey::Local(hir::SourceClass { file, item }),
        }
    }

    /// The key of a resolved class: a source class is keyed by its declaration
    /// (which keeps a *local* one identified), a library class by its binary
    /// name ([JVMS §4.2]).
    pub fn of_resolved(db: &dyn TyDatabase, resolved: &hir::Resolved) -> ClassKey {
        match resolved {
            hir::Resolved::Source(class) => {
                ClassKey::of(&hir::file_item_tree(db, class.file), class.file, class.item)
            }
            hir::Resolved::Library(_) => ClassKey::Named(resolved.fqn(db).as_name().clone()),
        }
    }

    /// The simple name of the class ([§6.7]): the last `.`-separated and
    /// `$`-separated segment of a canonical name, or the declaration's own
    /// name for a local one. This is what javac and the IDE render.
    pub fn simple_name(&self, db: &dyn TyDatabase) -> Name {
        match self {
            ClassKey::Named(fqn) => Name::new(fqn.simple_name()),
            ClassKey::Local(class) => {
                let tree = hir::file_item_tree(db, class.file);
                tree.data(class.item)
                    .name()
                    .cloned()
                    .unwrap_or_else(|| Name::new(""))
            }
        }
    }

    /// The key of the class a reference type denotes: its declaration for a
    /// *local* one ([§6.7]), else the class of its canonical name. The
    /// identity travels on the type itself, so no name lookup is needed.
    pub fn of_ty(db: &dyn TyDatabase, ty: &Ty) -> Option<ClassKey> {
        match ty.kind(db) {
            TyKind::Reference {
                local: Some(class), ..
            } => Some(ClassKey::Local(*class)),
            TyKind::Reference {
                name, local: None, ..
            } => Some(ClassKey::Named(name.clone())),
            _ => None,
        }
    }

    /// The name a diagnostic about the class renders ([§6.7]): its canonical
    /// name, or the *simple* name of a declaration that has none — a local
    /// declaration, which javac and the IDE render by its simple name.
    pub fn display_name(&self, db: &dyn TyDatabase) -> Name {
        match self {
            ClassKey::Named(fqn) => fqn.clone(),
            ClassKey::Local(_) => self.simple_name(db),
        }
    }

    /// The declaration a *source* class names, `None` for a classpath class.
    pub fn source(&self) -> Option<hir::SourceClass> {
        match self {
            ClassKey::Named(_) => None,
            ClassKey::Local(class) => Some(*class),
        }
    }

    /// The canonical fully qualified name of the class, when it has one.
    pub fn as_fqn(&self) -> Option<&Name> {
        match self {
            ClassKey::Named(fqn) => Some(fqn),
            ClassKey::Local(_) => None,
        }
    }

    /// The class as a type, for a subtype or receiver comparison: a reference
    /// to the named class, or to the local declaration ([§6.7]).
    pub fn as_ty(&self, db: &dyn TyDatabase, args: Vec<Ty>) -> Ty {
        match self {
            ClassKey::Named(fqn) => Ty::reference(db, fqn.as_str(), args),
            ClassKey::Local(class) => Ty::local_reference(db, *class, self.simple_name(db), args),
        }
    }

    /// The *top-level* class the declaration belongs to ([JLS §6.6.1]: a
    /// private member is accessible throughout the body of its top-level
    /// class, nested and local declarations included): the outermost enclosing
    /// class-like declaration's canonical name. A local declaration's top
    /// level is the class enclosing it.
    pub fn top_level(&self, db: &dyn TyDatabase, package: Option<&str>) -> Option<Name> {
        match self {
            // A canonical name nests with dots, so the top level is the
            // package plus the first type name ([§6.7]).
            ClassKey::Named(fqn) => Some(Name::new(&source_top_level(package, fqn.as_str()))),
            ClassKey::Local(class) => {
                let tree = hir::file_item_tree(db, class.file);
                crate::java::resolve::enclosing_type_chain(&tree, class.item)
                    .last()
                    .cloned()
            }
        }
    }
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
    /// The class or interface in which the invocation appears, for `private`
    /// and `protected` access control
    /// ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1),
    /// [§6.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.2)).
    /// A *local* declaration ([JLS §14.3]) is its own class here, which is what
    /// makes its private members accessible to the body that declares it.
    pub enclosing_class: Option<ClassKey>,
    /// The package of the compilation unit in which the invocation appears,
    /// for package and `protected` access control; the unnamed package is `""`.
    pub package: Option<String>,
    /// The class of which the access site is a (possibly anonymous) *subclass*,
    /// for the second half of [§6.6.2]: a protected member declared by that
    /// class is accessible from outside its package to code that is responsible
    /// for the implementation of an object of the subclass. An anonymous class
    /// creation `new C(args) { ... }` is exactly such a subclass — the
    /// anonymous body may invoke C's protected constructor and protected
    /// members even from another package (the Gson `new TypeToken<T>() {}`
    /// idiom) — even though no source item exists for the anonymous class to
    /// name as the enclosing class.
    pub subclass_of: Option<ClassKey>,
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
            enclosing_class: Some(ClassKey::Named(Name::new("library.probe.Caller"))),
            // The unnamed package: package and `protected` members of named
            // packages are not accessible (§6.6.1).
            package: Some(String::new()),
            subclass_of: None,
        }
    }

    /// The context interned as `key` ([`ContextKey`]).
    pub fn from_key(db: &dyn TyDatabase, key: ContextKey) -> InvocationContext {
        InvocationContext {
            mode: *key.mode(db),
            enclosing_class: key.enclosing_class(db).clone(),
            package: key
                .package(db)
                .as_ref()
                .map(|name| name.as_str().to_owned()),
            subclass_of: key.subclass_of(db).clone(),
        }
    }

    /// The access-control context of an import declaration
    /// ([§7.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.4)):
    /// an import appears at compilation-unit level, so the site has the unit's
    /// package ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1))
    /// and neither an enclosing class nor a superclass — the unnamed package is
    /// `""`, as in [`InvocationContext::external`]. A static import naming a
    /// package member the unit's own package declares is therefore valid, and
    /// the mode is a static access ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)).
    pub fn for_import(package: Option<&str>) -> Self {
        Self {
            mode: InvocationMode::Static,
            enclosing_class: None,
            package: Some(package.unwrap_or_default().to_owned()),
            subclass_of: None,
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
            subclass_of: self.subclass_of.clone(),
        }
    }

    /// The invocation context of the same access site, additionally *within an
    /// anonymous class body whose direct superclass (or implemented interface)
    /// is `superclass`* ([JLS §15.9.5], [§6.6.2]): the anonymous body is a
    /// subclass of `superclass`, so protected members the *superclass itself*
    /// declares are accessible to it from any package.
    pub fn with_anonymous_superclass(&self, superclass: Name) -> InvocationContext {
        InvocationContext {
            mode: self.mode,
            enclosing_class: self.enclosing_class.clone(),
            package: self.package.clone(),
            subclass_of: Some(ClassKey::Named(superclass)),
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

/// §15.9.2.1: whether the class named by `ty` (a resolved reference) declares
/// its own type parameters. `Some(true)`/`Some(false)` when the class
/// resolves; `None` when it does not (the caller then stays silent — the
/// missing type reports itself). A nested class's own parameters are counted
/// (`Outer<String>.Inner` declares none), and a non-generic class of a
/// parameterized outer is not generic either.
pub fn class_declares_type_params(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<bool> {
    let (fqn, _) = ty.as_reference(db)?;
    match hir::fqn_resolve(db, scope, fqn.as_str()) {
        Some(hir::Resolved::Library(class)) => {
            hir::class_generic_info(db, &hir::Resolved::Library(class))
                .map(|info| !info.type_params.is_empty())
        }
        Some(hir::Resolved::Source(source)) => {
            let tree = hir::file_item_tree(db, source.file);
            match crate::java::resolve::item_data(&tree, source.item) {
                Some(hir_def::java::item_tree::ItemData::Class(d)) => {
                    Some(!d.type_params.is_empty())
                }
                Some(hir_def::java::item_tree::ItemData::Interface(d)) => {
                    Some(!d.type_params.is_empty())
                }
                Some(hir_def::java::item_tree::ItemData::Record(d)) => {
                    Some(!d.type_params.is_empty())
                }
                Some(hir_def::java::item_tree::ItemData::Enum(_)) => Some(false),
                _ => None,
            }
        }
        None => None,
    }
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
    /// The access derived from the classfile access flags
    /// ([JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1))
    /// via the canonical [`JvmAccessFlags`] model.
    fn from_flags(flags: u16) -> Access {
        match JvmVisibility::from_access_flags(JvmAccessFlags::from_bits_retain(flags)) {
            JvmVisibility::Private => Access::Private,
            JvmVisibility::Protected => Access::Protected,
            JvmVisibility::Public => Access::Public,
            JvmVisibility::Package => Access::Package,
        }
    }
}

/// A type parameter of a generic method
/// ([JLS §8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4))
/// with its declared bounds ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
/// kept so [`pick_method`] can run the invocation type inference of
/// [JLS §18.5.2].
///
/// The parameter is identified by its [`TypeVarScope`] — the declaration that
/// introduces it ([§4.4], [§6.3]) — so the invocation's substitution
/// ([§18.5.2.2]) instantiates exactly the method's own variables and never a
/// same-named variable of the declaring class ([§6.4.1]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodTypeParam {
    pub scope: TypeVarScope,
    pub bounds: Vec<Ty>,
}

impl MethodTypeParam {
    /// The parameter's own name within its declaration, as javac renders it.
    pub fn name(&self) -> &Name {
        self.scope.name()
    }
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
    /// The declaring class or interface ([`ClassKey`]).
    pub owner: ClassKey,
    /// The workspace source file declaring this method, when it is a source
    /// declaration (including the synthesized implicit constructors, enum
    /// members and record accessors of a source class). `None` for library
    /// members and the synthetic `Object.clone` of array types.
    pub owner_file: Option<FileId>,
    /// The item id of the source declaration, when there is one — the anchor
    /// the declaration-level checks ([§8.4.2] duplicate methods) report at.
    pub decl_item: Option<ItemId>,
    /// The parameter types, instantiated with the declaring type's type
    /// arguments; the method's own type parameters are not yet instantiated.
    pub params: Vec<Ty>,
    /// The formal parameter *names* of a source method ([§8.4.1]), in order.
    /// `None` for library members (classfiles do not record them without a
    /// `MethodParameters` attribute) and synthesized implicit members; the
    /// record canonical-constructor parameter-name rule
    /// ([§8.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.4))
    /// is a source-only check.
    pub param_names: Option<Vec<String>>,
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
    /// Whether the method is `final`
    /// ([JLS §8.4.3.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.3.3)):
    /// the ACC_FINAL flag of the classfile
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6))
    /// or the `final` modifier of the source. A final instance method cannot be
    /// overridden and a final static method cannot be hidden by a subclass; the
    /// declaration-level checks ([JLS §8.4.3.3]) report the violation.
    pub is_final: bool,
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
    /// Whether this member's signature was *erased* because it was reached
    /// through a raw receiver ([JLS §4.8]) while its declaration mentions the
    /// declaring class's type parameters — the condition under which an
    /// invocation of it is an unchecked call
    /// ([§5.1.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.9),
    /// javac's `unchecked call to … as a member of the raw type …`).
    ///
    /// A member whose declared signature mentions no type parameter (`void
    /// m(String)`) keeps a fully-checked invocation even on a raw receiver,
    /// which is why the flag records the *declaration*, not merely the raw
    /// receiver.
    pub raw_erased: bool,
    /// The classfile descriptor of a library member
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6) for a
    /// method, [§4.5](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5) for a
    /// field). `None` for a source declaration and for a member this crate
    /// synthesizes (`Object.clone` on an array type, the implicit members of a
    /// source class).
    pub descriptor: Option<SmolStr>,
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
        write!(
            f,
            "{}.{}(",
            self.method.owner.display_name(self.db),
            self.method.name
        )?;
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

/// The methods named `name` on `receiver` and its supertypes *regardless of
/// access control* ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)):
/// the access-probe companion of [`member_set`]. When the accessible member
/// set is empty but this one is not, a method of the name exists yet is not
/// accessible from the access site — the §6.6 error reported by the body
/// inference layer as `IllegalAccess`. The invocation mode ([§15.12.3]) is
/// still honored, so a static/instance mismatch does not masquerade as an
/// access violation.
pub fn member_set_ignoring_access(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    member_set_impl(db, scope, receiver, name, ctx, true, false, false)
}

/// All methods of `ty` across its supertype closure, most-derived first and
/// deduped by overriding signature
/// ([JLS §8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1)):
/// the raw material of the declaration-level checks ([§8.4.8.3],
/// [§9.4.1.3], [`crate::java::decl_check`]). The access-control context is that of
/// the declaring class itself ([§6.6.1]).
///
/// This is a *declaration-level* enumeration ([JLS §8.2], [§9.2]): declared
/// plus inherited members, most-derived first, with the implicit members each
/// declaring class synthesizes ([§8.8.9], [§8.9.3], [§8.10.3/4]) — and with
/// none of the invocation-only transforms of [`member_set_impl`] (the
/// invocation-mode filter and the SELF return-re-pointing). The
/// static-interface-owner rule is *membership*, not an invocation transform
/// ([§9.2], [§9.4.1]), so it applies here as well.
pub fn all_methods(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    member_set_impl(db, scope, receiver, "", ctx, true, true, true)
}

/// Every member visible from `receiver` **without** the most-derived dedup
/// ([JLS §8.4.8.1]): the declaration-level checks ([§8.4.8.3], [§9.6.4.4])
/// must see the super declaration an override hides — a deduped member set
/// would report every correct `@Override` as orphaned. A *declaration-level*
/// enumeration like [`all_methods`] (per [§8.2], [§9.2], no invocation-only
/// transforms).
pub fn all_methods_raw(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    member_set_impl(db, scope, receiver, "", ctx, false, true, true)
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
    let receiver = capture_conversion(db, scope, *receiver);
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
pub(crate) fn member_set_query(
    db: &dyn TyDatabase,
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
        true,
        true,
        false,
    )
}

/// The non-memoized form of [`member_set`]. `dedupe` selects whether the
/// most-derived overriding-signature collapse runs; `strict_access` selects
/// whether the candidates are filtered by accessibility at `ctx`'s access
/// site ([§6.6]) — `false` is the access probe used by the `IllegalAccess`
/// diagnostics of the body inference layer (see
/// [`member_set_ignoring_access`]). `declaration` selects the
/// *declaration-level* enumeration ([`all_methods`], [`all_methods_raw`]):
/// the members are gathered per [§8.2]/[§9.2] (declared + inherited,
/// most-derived first, implicit members synthesized per declaring class) with
/// none of the invocation-only transforms — the invocation-mode filter
/// ([`mode_allows`]) and the SELF return-re-pointing do not apply; the
/// static-interface-owner membership rule and the access filter (the declaring
/// class's own context) still do; the raw-receiver guard (`name == ""` keeps
/// declaration walks un-erased) likewise.
#[allow(clippy::too_many_arguments)]
fn member_set_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
    dedupe: bool,
    strict_access: bool,
    declaration: bool,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let receiver = capture_conversion(db, scope, *receiver);
    // §9.2/[§9.4.1]/[§8.2]: a static method declared in an interface is a
    // member of *that interface only* — an interface inherits no static
    // methods from its superinterfaces ([§9.2]) and a class inherits none
    // from its superinterfaces either ([§8.2]), so the member set of any
    // other receiver type — a subinterface, an implementing class, or a
    // supertype walk that merely passes through the declaring interface —
    // excludes it. The receiver's own FQN (empty for non-reference
    // receivers, which keeps the rule inert: an array or type-variable
    // receiver inherits interface statics nowhere) is the only type whose
    // *declared* static interface methods are candidates. This is the
    // §15.12.3 static-interface-owner rule, applied uniformly: javac accepts
    // `I.s()` and `I::s` on the declaring interface `I`, and rejects
    // `J.s()`, `C.s()` and `J::s` for `J extends I` and `class C implements
    // I`. (A *static class* method remains inheritable — `MyThread.sleep` is
    // reached through the subclass — because `declaring_interface` is false
    // for it.)
    // §15.12.3: a *static interface* method is reachable by its simple name
    // only through the interface that declares it — not through a subinterface
    // or an implementing class. The declaring class is compared by identity: a
    // local declaration ([JLS §14.3]) is its own class here too.
    let receiver_key = ClassKey::of_ty(db, &receiver);
    let static_interface_owner_ok = |method: &MethodData| {
        let declared_by_receiver = receiver_key
            .as_ref()
            .is_some_and(|key| key == &method.owner);
        !(method.is_static && method.declaring_interface && !declared_by_receiver)
    };

    // §4.4: a type variable's *effective* upper bound is its declared bounds,
    // or `java.lang.Object` when it declares none — the member set of the
    // receiver is the member set of that bound. JLS §4.9: an intersection
    // type's members are those of every conjunct (`T extends MappedEntity &
    // CopyableEntity<T>` finds `copy` through the second bound, and a glb
    // `A & B` value finds members through either side).
    // JLS §4.8 ([§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8))
    // gives raw receivers two member rules that this walk implements by
    // erasing *at the supertype edges*, not on the whole member set:
    //
    // * "The superclass types (respectively, superinterface types) of a raw
    //   type are the erasures of the superclass types (superinterface types)
    //   of the named class or interface."
    // * "The type of an inherited instance method or non-static field of a raw
    //   type C, where the member was declared in a class or interface D, is the
    //   type of the member in the supertype of C that names D."
    //
    // So the walk carries an *erasure context* per stack entry: a raw use
    // turns its supertype edges into erasures, and the context is monotone
    // (once raw, always raw), which is what makes a generic ancestor erase
    // behind a non-generic intermediate (`class Sub<T> extends Mid` with
    // `class Mid extends Gen<String>` → the declared `Gen` is reached as the
    // erasure `Gen`, erasing its members) while a member *declared* in a
    // non-generic class keeps its declared type even when reached through
    // generic ancestors (`class Sub<T> extends Mid<T>` with
    // `class Mid<T> extends Base` → the declared `Base` keeps
    // `List<String> items()`).
    //
    // Members *declared* in a raw class are erased per class by the `is_raw`
    // binding in `source_class_methods`/`library_class_methods` ([§4.8]'s
    // first member sentence), and static members keep their generics (they do
    // not depend on the receiver's type arguments).
    //
    // Declaration enumerations (`declaration`, the `all_methods` walks of
    // `decl_check`) keep the declared supertypes: their receiver is the class
    // under its own declaration (`Ty::reference(fqn, Vec::new())`), and
    // [§8.4.8.1]/[§9.4.1.2] override-equivalence compares members as declared,
    // substituted over the class's own type parameters.
    let mut stack: Vec<(Ty, bool)> = match receiver.kind(db) {
        TyKind::TypeVar { bounds, .. } if bounds.is_empty() => {
            vec![(Ty::reference(db, "java.lang.Object", Vec::new()), false)]
        }
        TyKind::TypeVar { bounds, .. } => bounds.iter().map(|bound| (*bound, false)).collect(),
        TyKind::Intersection(members) => members.iter().map(|member| (*member, false)).collect(),
        _ => vec![(receiver, false)],
    };
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some((ty, erased)) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        // JLS §4.9: an intersection conjunct reached through a bound (a
        // `TypeVar` bound that is itself `A & B`) contributes every member.
        if let TyKind::Intersection(members) = ty.kind(db) {
            for member in members.clone() {
                stack.push((member, erased));
            }
            continue;
        }
        out.extend(
            class_methods(db, &scope_id, &ty, name)
                .into_iter()
                .filter(|method| {
                    // A declaration-level enumeration has no invocation mode
                    // to honor: the static/instance filter of `mode_allows` is
                    // an invocation rule and applies only to resolution
                    // ([§15.12.3]). The *static-interface-owner* rule is not —
                    // it is membership ([§9.2] "A class inherits from its
                    // direct superclass ... all the non-private member methods
                    // ... other than static methods"; [§9.4.1] "A static
                    // method in an interface ... is not inherited"), so it
                    // applies to every enumeration, declaration-level included:
                    // a subinterface or implementing class has no inherited
                    // copy of a superinterface's `static` method, and
                    // [§8.4.8.1]'s override/clash comparison must not see one.
                    // The access filter stays (the caller passes the declaring
                    // class's own context, so private/package supertype
                    // members are excluded from what a subtype "inherits").
                    (declaration || mode_allows(method, ctx))
                        && (!strict_access || is_accessible(db, scope, method, &receiver, ctx))
                        && static_interface_owner_ok(method)
                }),
        );
        let raws = !declaration && (erased || is_raw_use(db, scope, &ty));
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push((if raws { parent.erasure(db) } else { parent }, raws));
        }
    }
    // §8.4.8: a member whose result type is the class's own *SELF* type
    // parameter — a parameter whose declared bound renames the class itself,
    // the assertj idiom `SELF extends AbstractStringAssert<SELF>` resolved on a
    // receiver parameterized by a *captured wildcard* — returns the receiver's
    // captured type, not a bare capture variable. Re-pointing the result at
    // the full receiver keeps assertion chains
    // (`assertThat(s).contains(a).contains(b)`) member-resolution-capable: the
    // bare capture's recursively-referencing bound degrades every subsequent
    // chained call. A *plain* type-parameter return (`Enumeration<? extends
    // ZipEntry>.nextElement()` returning `E`) is not a SELF channel and keeps
    // the captured element type, which must stay assignable to `ZipEntry`.
    // Invocation-only: a declaration-level enumeration reports the declared
    // returns unchanged.
    //
    // The re-pointing is keyed to the *self parameter* — the declared type
    // parameter whose bound mentions the class — instantiated at the
    // receiver's argument: `Chain<E, SELF extends Chain<E, SELF>>.self()` on
    // `Chain<?, ?>` returns the receiver's second argument (a capture
    // variable whose interned id is exactly the substituted `SELF` handle),
    // not every capture that happens to flow through the method's return. An
    // ordinary captured element return (`Chain<E, SELF...>.element()` on
    // `Chain<?, ?>` → the capture of `E`) is left untouched: it is not a
    // SELF channel and must stay assignable to the element type. The capture
    // of a wildcard-arg self parameter is a `CAP#` type variable whose
    // interned id equals the substituted return — `Ty` is interned, so `==`
    // is identity of interning.
    let self_args: Vec<Ty> = if declaration {
        Vec::new()
    } else {
        match receiver.kind(db) {
            // The declared type parameters of the receiver's class whose
            // bounds mention the class, each mapped to the receiver's actual
            // argument at that index ([§5.1.10] captures make a wildcard arg
            // a `CAP#` variable; a concrete arg is itself).
            TyKind::Reference { name, args, .. } => {
                self_type_param_indexes(db, scope, name.as_str())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|i| args.get(i).copied())
                    .collect()
            }
            _ => Vec::new(),
        }
    };
    if !self_args.is_empty() {
        for method in &mut out {
            // The member's return type was instantiated with the receiver's
            // arguments by `class_methods`, so a SELF-typed return is exactly
            // one of the self-parameter arguments (`Ty` is interned, so `==`
            // is identity of interning). Re-point only those; an ordinary
            // captured return of a non-self parameter is a different
            // `CAP#`/concrete handle and stays.
            if self_args.contains(&method.ret) {
                method.ret = receiver;
            }
        }
    }
    // §8.4.8/§4.4: a member whose result type is a *type variable of the
    // declaring class* resolved on a type-variable receiver keeps the
    // receiver's own type variable, not the recursion-guarded bare handle that
    // named it. `interface G<T extends G<T>> { T noarg(); }` with a receiver
    // `v: T` (`T extends G<T>`, the method's own type parameter) looks its
    // members up through the bound `G<T>`, whose type argument is the *inner*
    // recursion-guarded `T` (no further bounds, [§4.4] interning). Instantiating
    // `noarg`'s return with that argument yields a bound-less `T` whose member
    // set is empty — the next chained call (`v.noarg().noarg()`) reports
    // `no-such-method`. The declared bound means the receiver's variable
    // itself, so re-point the result at it: the receiver `T` carries the full
    // `G<T>` bound and the chain stays member-resolution-capable.
    // Invocation-only, like the SELF channel above.
    if !declaration && let TyKind::TypeVar { scope, .. } = receiver.kind(db) {
        let receiver_scope = scope.clone();
        for method in &mut out {
            // §4.4/§6.3: the result is re-pointed only when it is *the
            // receiver's own* type variable — same declaring parameter, not
            // merely the same name (a same-named variable of another
            // declaration, [§6.4.1], is a different type).
            if let TyKind::TypeVar { scope, .. } = method.ret.kind(db)
                && *scope == receiver_scope
            {
                method.ret = receiver;
            }
        }
    }
    // §10.7: every array type has a public `clone` method with no parameters
    // and no checked exceptions, whose return type is the array type itself.
    // It overrides `Object.clone` (which is `protected`) as public, so it is
    // invokable from anywhere the array is visible.
    if name == "clone" && matches!(receiver.kind(db), TyKind::Array(_)) {
        out.push(MethodData {
            name: "clone".to_owned(),
            owner: ClassKey::Named(Name::new("java.lang.Object")),
            owner_file: None,
            decl_item: None,
            params: Vec::new(),
            param_names: None,
            // The return type is the array type itself ([§10.7]).
            ret: receiver,
            throws: Vec::new(),
            varargs: false,
            is_static: false,
            abstract_: false,
            is_final: false,
            access: Access::Public,
            declaring_package: Some("java.lang".to_owned()),
            declaring_top_level: Some("Object".to_owned()),
            declaring_interface: false,
            type_params: Vec::new(),
            raw_erased: false,
            descriptor: None,
        });
    }
    // §8.4.8.1: an overriding method replaces the overridden one in the
    // member set — a subtype's declaration of a method with the same signature
    // shadows the supertype's — so only the most-derived declaration of each
    // signature survives. The walk is derived-first, so the first occurrence
    // *tends* to be the most-derived — but the walk is a LIFO stack whose
    // parents are pushed superclass-first, so at a class/interface join the
    // interface's declaration pops first. Without this, `List.iterator()`
    // (overriding `Collection.iterator()`/`Iterable.iterator()`) would surface
    // three identical candidates that the most-specific tie-break reports as
    // ambiguous.
    //
    // [§8.4.8.1]/[§15.12.3.1]: the declaration that survives is the *most
    // specific* one — the declaration whose declaring type is a subtype of the
    // other's, which for a class declaration and an inherited abstract
    // interface declaration is the class's ([§8.4.8.1] "the method declared in
    // the class"), and when the owners are unrelated, the *class* declaration
    // (a superinterface's abstract member never displaces a superclass's). The
    // comparison is on the declaring types, not on walk order: with
    // `interface I { void m(); } class B implements I { public void m() {} }
    // class M extends B implements I {}` the LIFO walk yields `I.m()` first,
    // and keeping it made `super.m()` from a subclass a §15.12.3
    // `AbstractSuperAccess` error even though the selected member is `B.m()`.
    //
    // Return covariance ([§8.4.8.3]) is the *last* tie-break, for owners that
    // neither relate by subtyping nor differ in class-vs-interface: a covariant
    // override narrows the return (`B toBuilder()` overriding
    // `ComponentBuilder<?,?> toBuilder()`), and a diamond walk can surface the
    // wider-return declaration first (`TranslatableComponent` via
    // `ScopedComponent → Component` (wildcard) before `BuildableComponent`
    // (`Builder`)). Keeping the wider return would pin `toBuilder()` to
    // `ComponentBuilder<?,?>` and reject the `TranslatableComponent.Builder`
    // target. Equal returns keep the first (walk order).
    if !dedupe {
        return out;
    }
    let mut deduped: Vec<MethodData> = Vec::with_capacity(out.len());
    for method in out {
        let mut replaced = false;
        for seen in deduped.iter_mut() {
            if !same_overriding_signature(seen, &method) {
                continue;
            }
            let owner = |m: &MethodData| m.owner.as_ty(db, Vec::new());
            let (new_owner, seen_owner) = (owner(&method), owner(seen));
            let new_derives =
                crate::java::subtyping::is_subtype(db, scope, &new_owner, &seen_owner);
            let seen_derives =
                crate::java::subtyping::is_subtype(db, scope, &seen_owner, &new_owner);
            let new_wins = if new_derives != seen_derives {
                new_derives
            } else if seen.declaring_interface != method.declaring_interface {
                // Unrelated owners of different kinds at this join: the class
                // declaration is the inherited member ([§8.4.8.1]); only a
                // class's superinterfaces are searched for a member the class
                // chain does not provide.
                !method.declaring_interface
            } else {
                // Same declaring kind and unrelated owners (or the same
                // owner): fall back to return covariance, then walk order.
                crate::java::subtyping::is_subtype(db, scope, &method.ret, &seen.ret)
                    && !crate::java::subtyping::is_subtype(db, scope, &seen.ret, &method.ret)
            };
            if new_wins {
                *seen = method.clone();
            }
            replaced = true;
            break;
        }
        if !replaced {
            deduped.push(method);
        }
    }
    deduped
}

/// Whether `receiver` is a *raw* use of a generic class
/// ([JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)):
/// a reference type with no type arguments whose class declares type
/// parameters (`CheckContainer` with `CheckContainer<T>`, `ListBinaryTag.Builder`
/// with `Builder<T>`). A non-generic class (`String`) or a parameterized use
/// (`List<String>`) is not raw, even with empty args in the latter's case the
/// args are present.
fn is_raw_use(db: &dyn TyDatabase, scope: &hir::ResolutionScope, receiver: &Ty) -> bool {
    let crate::java::ty::TyKind::Reference { name, args, .. } = receiver.kind(db) else {
        return false;
    };
    if !args.is_empty() {
        return false;
    }
    let Some(resolved) = hir::fqn_resolve(db, scope, name.as_str()) else {
        return false;
    };
    match resolved {
        hir::Resolved::Library(library) => {
            hir::class_generic_info(db, &hir::Resolved::Library(library))
                .is_some_and(|info| !info.type_params.is_empty())
        }
        hir::Resolved::Source(source) => {
            let tree = hir::file_item_tree(db, source.file);
            let Some(data) = crate::java::resolve::item_data(&tree, source.item) else {
                return false;
            };
            match data {
                ItemData::Class(d) | ItemData::Interface(d) => !d.type_params.is_empty(),
                ItemData::Record(d) => !d.type_params.is_empty(),
                _ => false,
            }
        }
    }
}

/// Whether two methods declare the same overriding signature
/// ([JLS §8.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.2),
/// [§8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1),
/// [§9.4.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.2)):
/// the same name, identical parameter types, and both static or neither. The
/// signature — and with it override-equivalence — never includes
/// variable-arity-ness: a method's parameter types are already in the
/// array-lowered form (`String...` is `String[]`, [§8.4.1]), so a
/// `void m(String...)` and a `void m(String[])` declare the same signature
/// ([§8.4.2]) and one may override the other ([§8.4.8.1], [§9.4.1.2]).
///
/// The *name* is part of it: two methods of one type that differ only in name
/// (`void a()` and `void b()`) are unrelated members, so the wildcard
/// enumeration ([`all_methods`]) must keep both — only same-name candidates
/// can shadow one another. Two members with the same signature differing only
/// in declaring type are the same method inherited and overridden down the
/// hierarchy; the return type is deliberately *not* compared, since an
/// override may narrow it (covariant returns, [§8.4.8.3]).
fn same_overriding_signature(a: &MethodData, b: &MethodData) -> bool {
    a.name == b.name && a.params == b.params && a.is_static == b.is_static
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
    let ty = capture_conversion(db, scope, *ty);
    // §9.8/[§9.4]: the abstract members of a functional interface are the
    // *inherited* abstract methods not implemented by a `default` (or other
    // concrete) declaration anywhere closer to `ty`. Both kinds are gathered
    // across the supertype closure — an abstract in a distant superinterface
    // is discharged by a default declared in `ty` or an intermediate
    // interface — and a signature is abstract exactly when the gathered set
    // holds no concrete implementation of it.
    // JLS §4.8 ([§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)):
    // a raw receiver's superinterface types are the *erasures* of the declared
    // ones, and an inherited member's type is its type in the erased supertype
    // that names it. The walk therefore carries the monotone erasure context
    // as in `member_set_impl`: the SAM of a raw functional interface is the
    // erased descriptor (§4.6), so a raw `Sub<T> extends F<T>` finds the
    // `F`-inherited `void accept(T)` as `void accept(Bound)` — the erasure of
    // the type variable's first bound. This descriptor is what
    // [§9.8]/[§15.27.3] match the lambda parameters against.
    let mut stack: Vec<(Ty, bool)> = vec![(ty, false)];
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut declarations: Vec<MethodData> = Vec::new();
    while let Some((t, erased)) = stack.pop() {
        if !seen.insert(t.id) {
            continue;
        }
        let TyKind::Reference { args, .. } = t.kind(db) else {
            continue;
        };
        // §6.7: the closure is seeded with each class's *declaration* — a
        // local functional interface ([JLS §14.3]) is its own item.
        let Some(resolved) = crate::java::resolve::reference_class(db, scope, &t) else {
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
                    let flags = JvmAccessFlags::from_bits_retain(method.flags);
                    if !flags.is_abstract() {
                        continue;
                    }
                    let name = interner.resolve(&method.name);
                    declarations.extend(library_class_methods(
                        db,
                        class.clone(),
                        args.clone(),
                        name,
                    ));
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
                    let name = method.name.as_str().to_owned();
                    declarations.extend(source_class_methods(db, source, args.clone(), &name));
                }
            }
        }
        for parent in supertypes_query(db, scope_id, t.id) {
            let raws = erased || is_raw_use(db, scope, &t);
            stack.push((if raws { parent.erasure(db) } else { parent }, raws));
        }
    }
    // A default (concrete, non-static) method's signature discharges every
    // abstract of the same signature gathered from *other* interfaces; the
    // default itself is not an abstract member ([JLS §9.8], [§9.4.1.2]).
    // Override-equivalence never includes variable-arity-ness ([§8.4.2],
    // [§9.4.1.2]): the lowered parameter lists already compare `String...`
    // as `String[]`, so a default `void m(String...)` discharges an abstract
    // `void m(String[])` and vice versa.
    let has_default = |method: &MethodData| {
        declarations.iter().any(|candidate| {
            !candidate.abstract_
                && !candidate.is_static
                && candidate.name == method.name
                && candidate.params == method.params
                && candidate.type_params.len() == method.type_params.len()
        })
    };
    // §9.4.1.2/[§9.9]: abstract methods that override-equivalent one another
    // (same signature, possibly redeclared down a superinterface chain —
    // `Closeable.close` redeclaring `AutoCloseable.close`) are ONE abstract
    // method. Keep the first (most-derived) declaration of each signature.
    let mut deduped: Vec<MethodData> = Vec::with_capacity(declarations.len());
    for method in &declarations {
        if !method.abstract_ || has_default(method) {
            continue;
        }
        if !deduped.iter().any(|seen| {
            seen.name == method.name
                && seen.params == method.params
                && seen.type_params.len() == method.type_params.len()
        }) {
            deduped.push(method.clone());
        }
    }
    deduped
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
    // §9.8/[§9.4.1.2]: the abstract members of a functional interface are
    // those *not* matching a `public` member of `java.lang.Object` — the
    // class implementing the interface always provides those through
    // `Object`, so they are not abstract obligations
    // ([§9.4.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.2)).
    // A *user-declared* abstract method with an Object signature (an
    // interface restating `String toString()`) is equally discharged by
    // `Object`; only an abstract whose signature differs (`void go()`) is an
    // obligation.
    methods.retain(|m| !object_member_signature(db, scope, m));
    if methods.len() == 1 {
        let method = methods.pop().expect("one");
        // §9.8: the single abstract method of a functional interface must not
        // be generic — a generic method's parameter types depend on its own
        // type variables, so a lambda cannot provide an implementation for
        // every instantiation. javac: `invalid functional descriptor for
        // lambda expression … method (T)T in interface I is generic`.
        if !method.type_params.is_empty() {
            return None;
        }
        Some(method)
    } else {
        None
    }
}

/// §9.8/[§9.4.1.2]: whether `method`'s signature matches a `public` member
/// of `java.lang.Object` — the interface need not (indeed cannot) require a
/// lambda for it, because the implementing class inherits `Object`'s
/// concrete implementation. The public Object members ([§8.4.8.1] lists the
/// canonical set for the override rule; [§9.4.1.2] applies it to interface
/// members) discharge abstract interface redeclarations of the same
/// signature: the JDK `Object` classfile (or the test fixture's stub) is
/// consulted by name + parameter-erasure. `clone()`/`finalize()` are
/// `protected`, so an interface can never override them through `Object`
/// (the interface is in a different package) and they do not discharge;
/// `getClass()`, `hashCode()`, `equals(Object)`, `toString()`, `notify()`,
/// `notifyAll()` and the `wait` overloads are `public` and do.
fn object_member_signature(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
) -> bool {
    let object = Ty::reference(db, "java.lang.Object", Vec::new());
    // The invocation context of a library-only caller: Object's public
    // members are all visible to it, so the enumeration is the full public
    // Object surface.
    let ctx = InvocationContext::external(scope);
    member_set(db, scope, &object, &method.name, &ctx)
        .iter()
        .any(|object_method| {
            if object_method.access != Access::Public {
                return false;
            }
            object_method.params.len() == method.params.len()
                && object_method
                    .params
                    .iter()
                    .zip(&method.params)
                    .all(|(a, b)| a == b || a.erasure(db) == b.erasure(db))
        })
}

/// The methods of a single class or interface, instantiated with `ty`'s type
/// arguments.
fn class_methods(db: &dyn TyDatabase, scope_id: &ScopeId, ty: &Ty, name: &str) -> Vec<MethodData> {
    let TyKind::Reference { args, .. } = ty.kind(db) else {
        return Vec::new();
    };
    // §6.7: the receiver's declaration — a *local* class's own item, or the
    // class its canonical name resolves to.
    let Some(resolved) =
        crate::java::resolve::reference_class(db, &scope_id.kind(db).to_scope(), ty)
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
pub(crate) fn top_level_of(fqn: &str) -> String {
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
pub(crate) fn source_top_level(package: Option<&str>, fqn: &str) -> String {
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
    // JLS 4.8: a *raw* use of a generic class erases its members'
    // signatures; the erasure is applied to each constructed member below.
    let is_raw = args.is_empty() && !class.type_params.is_empty();
    let fqn = interner.resolve(&class.fqn).to_owned();
    let owner = Name::new(&fqn);
    // §4.10.2: the receiver's arguments instantiate the declaring class's
    // *own* parameters — the binding is keyed by each parameter's declaring
    // scope ([§4.4], [§6.3]), so a same-named type variable appearing in a
    // member's signature but declared by another declaration is not captured
    // ([§6.4.1]).
    let binding: FxHashMap<TypeVarScope, Ty> = if args.is_empty() {
        FxHashMap::default()
    } else {
        class
            .type_params
            .iter()
            .zip(args.iter().copied())
            .map(|(tp, arg)| {
                (
                    TypeVarScope::LibraryClass {
                        owner: owner.clone(),
                        name: Name::new(interner.resolve(&tp.name)),
                    },
                    arg,
                )
            })
            .collect()
    };
    let declaring_package = package_of(&fqn);
    let declaring_top_level = Some(top_level_of(&fqn));
    let declaring_interface = matches!(
        hir::ClassKind::from_flags(class.flags, class.is_record),
        hir::ClassKind::Interface | hir::ClassKind::Annotation
    );

    let mut out = Vec::new();
    for method in &class.methods {
        // An empty name is the wildcard of the declaration-level walk
        // ([§9.8], [`crate::java::decl_check`]); no method can be named "".
        if !name.is_empty() && interner.resolve(&method.name) != name {
            continue;
        }
        // [JVMS §4.6]: a method flagged `ACC_BRIDGE` or `ACC_SYNTHETIC` is
        // invisible to source-code member resolution. javac hides every
        // synthetic member of a classfile from name lookup — the covariant
        // bridge `Player[] getOnlinePlayers()` javac emits next to the real
        // `Collection<? extends Player> getOnlinePlayers()` ([§8.4.8.3]) is
        // ACC_BRIDGE|ACC_SYNTHETIC, so an invocation of `getOnlinePlayers()`
        // must resolve the real declaration and *not* report an ambiguity or
        // prefer the array overload. Only classfile members carry these flags
        // (source lowering never synthesizes a bridge), so filtering here
        // cannot hide a user declaration.
        let flags = JvmAccessFlags::from_bits_retain(method.flags);
        if flags.contains(JvmAccessFlags::BRIDGE) || flags.contains(JvmAccessFlags::SYNTHETIC) {
            continue;
        }
        // [§8.4.4]/[§6.4.1]: the method's own type parameters are declared by
        // *this* method, so their scope is the method's — distinct from a
        // same-named class parameter of the declaring class, which the class
        // binding cannot therefore capture ([§4.4] capture-avoidance).
        let method_names: Vec<Name> = method
            .type_params
            .iter()
            .map(|tp| Name::new(interner.resolve(&tp.name)))
            .collect();
        let method_name = Name::new(interner.resolve(&method.name));
        let signature = crate::java::resolve::LibrarySignature {
            owner: &owner,
            method: Some((&method_name, &method_names)),
        };
        let member_lower = |tyref: &hir::TypeRef<hir::Symbol>| {
            crate::java::resolve::ty_from_library_signature(db, tyref, &signature)
                .substitute(db, &binding)
        };
        // JLS §4.8 with §5.1.9: an instance member reached through a raw
        // receiver has an *erased* signature, so an invocation of it cannot be
        // statically checked — javac's `unchecked call to … as a member of the
        // raw type …`. The erasure is observable exactly when the declared
        // formal types mention a type variable (the class's or the method's
        // own) or the method declares its own type parameters; a member whose
        // formals are ground (`void m(String)`) stays checked.
        let raw_erased = is_raw
            && !flags.is_static()
            && (!method.type_params.is_empty()
                || method
                    .params
                    .iter()
                    .any(|param| member_lower(&param.param_type).contains_type_var(db)));
        let type_params = method
            .type_params
            .iter()
            .zip(method_names.iter())
            .map(|(tp, tp_name)| MethodTypeParam {
                scope: TypeVarScope::LibraryMethod {
                    owner: owner.clone(),
                    method: method_name.clone(),
                    name: tp_name.clone(),
                },
                // The bound is instantiated with the declaring class's type
                // arguments: a `<U extends T>` bound on a generic class
                // references the class type parameter `T`, which resolves to
                // the receiver's actual argument here (§18.5.2.2). A bound
                // over the method's own type parameters is untouched by the
                // class binding and stays a bare type variable.
                bounds: tp.bounds.iter().map(member_lower).collect(),
            })
            .collect();
        // JLS 4.8: the *instance* members of a raw type have erased
        // signatures. A static member does not depend on the receiver's
        // type arguments at all, so its own generics stay intact.
        let is_static_member = flags.is_static();
        let erase = |ty: Ty| {
            if is_raw && !is_static_member {
                ty.erasure(db)
            } else {
                ty
            }
        };
        out.push(MethodData {
            // The method's own name — not the lookup filter, which is the
            // empty wildcard in the declaration-level walk.
            name: interner.resolve(&method.name).to_owned(),
            owner: ClassKey::Named(Name::new(&fqn)),
            owner_file: None,
            decl_item: None,
            descriptor: Some(SmolStr::from(interner.resolve(&method.descriptor))),
            params: method
                .params
                .iter()
                .map(|param| erase(member_lower(&param.param_type)))
                .collect(),
            param_names: None,
            ret: erase(member_lower(&method.return_type)),
            throws: method
                .throws_list
                .iter()
                .map(&member_lower)
                .map(erase)
                .collect(),
            varargs: JvmAccessFlags::from_bits_retain(method.flags).is_varargs(),
            is_static: JvmAccessFlags::from_bits_retain(method.flags).is_static(),
            abstract_: JvmAccessFlags::from_bits_retain(method.flags).is_abstract(),
            is_final: JvmAccessFlags::from_bits_retain(method.flags).is_final(),
            access: Access::from_flags(method.flags),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface,
            type_params,
            raw_erased,
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
    // JLS 4.8: a *raw* use of a generic class erases its members'
    // signatures; the erasure is applied to each constructed member below.
    let is_raw = args.is_empty() && !declared.is_empty();
    // §4.10.2: the receiver's arguments instantiate the declaring class's
    // *own* parameters. The binding is keyed by each parameter's declaring
    // scope ([§4.4], [§6.3]), and a method type parameter is declared by the
    // *method* ([§8.4.4]) — a distinct scope — so a method's own variable can
    // never be captured by the class binding ([§6.4.1], [§4.4]
    // capture-avoidance), with no name-level exclusion needed.
    let binding: FxHashMap<TypeVarScope, Ty> =
        crate::java::resolve::source_class_binding(source.file, source.item, declared, &args);
    let scope = scope_for_file(db, source.file);
    let resolver = Resolver::for_item(db, source.file, &tree, source.item);
    // §6.7: the receiver's declaration — its canonical name, or the
    // declaration itself when it has none (a local class-like declaration,
    // [JLS §14.3]).
    let class_key = ClassKey::of(&tree, source.file, source.item);
    let simple = class_key.simple_name(db);
    let package = resolver.package().map(|p| p.as_str().to_owned());
    let declaring_package = Some(package.clone().unwrap_or_default());
    // §6.6.1: a private member's accessibility is scoped by the *top-level*
    // class — the outermost enclosing class-like declaration, which for a
    // local declaration is the class around it.
    let declaring_top_level = class_key
        .top_level(db, package.as_deref())
        .map(|name| name.as_str().to_owned());
    let declaring_interface =
        matches!(class_data, ItemData::Interface(_) | ItemData::Annotation(_));

    let mut out = Vec::new();
    for item in class_data.body().to_vec() {
        let Some(ItemData::Method(method)) = item_data(&tree, item) else {
            continue;
        };
        // An empty name is the wildcard of the declaration-level walk
        // ([§9.8], [`crate::java::decl_check`]); no method can be named "".
        if !name.is_empty() && method.name.as_str() != name {
            continue;
        }
        let method_resolver = Resolver::for_item(db, source.file, &tree, item);
        let type_params = method
            .sig
            .type_params
            .iter()
            .map(|tp| MethodTypeParam {
                scope: TypeVarScope::Method {
                    file: source.file,
                    item,
                    name: tp.name.clone(),
                },
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
        // JLS 4.8: the *instance* members of a raw type have erased
        // signatures. A static member does not depend on the receiver's
        // type arguments at all, so its own generics stay intact.
        let erase = |ty: Ty| {
            if is_raw && !method.modifiers.is_static() {
                ty.erasure(db)
            } else {
                ty
            }
        };
        let is_compact_ctor = method.is_compact_constructor();
        // JLS §4.8 with §5.1.9: an instance member reached through a raw
        // receiver has an *erased* signature, so an invocation of it is
        // unchecked — javac's `unchecked call to … as a member of the raw
        // type …`. The erasure is observable when the declared formal types
        // mention a type variable (the class's or a method's own) or the
        // method declares its own type parameters; a member whose formals are
        // ground (`void m(String)`) stays checked.
        let raw_erased = is_raw
            && !method.modifiers.is_static()
            && (!method.sig.type_params.is_empty()
                || method_params_query(db, key)
                    .iter()
                    .map(instantiate)
                    .any(|param| param.contains_type_var(db)));
        // §8.10.4: a record *compact* constructor (`record R(int x) { R { …
        // } }`) is declared without a formal parameter list — its signature
        // is the record's component list. The canonical constructor is the
        // component list as parameters (same types, in order); a varargs
        // component is the array type ([§8.4.1]). Synthesize that canonical
        // parameter list here so `new R(1)` resolves against the real
        // constructor, the later `declares_canonical` check sees the compact
        // form and suppresses the duplicate implicit canonical, and the
        // `declares_ctor` check counts it (no default constructor either).
        let (varargs, params): (bool, Vec<Ty>) = if is_compact_ctor {
            let mut canonical: Vec<Ty> = Vec::new();
            if let ItemData::Record(record) = class_data {
                for component in &record.components {
                    let mut ty = resolve_type_ref(db, &scope, &method_resolver, &component.ty);
                    if component.varargs {
                        ty = Ty::array(db, ty);
                    }
                    canonical.push(erase(instantiate(&ty)));
                }
            }
            let last_varargs = match class_data {
                ItemData::Record(record) => record.components.last().is_some_and(|c| c.varargs),
                _ => false,
            };
            (last_varargs, canonical)
        } else {
            let varargs = method.sig.params.last().is_some_and(|param| param.varargs);
            let mut params: Vec<Ty> = method_params_query(db, key)
                .iter()
                .map(instantiate)
                .map(erase)
                .collect();
            // A variable-arity parameter `T...` is lowered as the element type
            // `T`; its formal type is the array `T[]` ([JLS §8.4.1]).
            if varargs && let Some(last) = params.last_mut() {
                *last = Ty::array(db, *last);
            }
            (varargs, params)
        };
        let ret = erase(instantiate(&item_ty_query(db, key)));
        // The declared throws clause ([§8.4.6]): resolve and instantiate with
        // the declaring type's type arguments; method type parameters stay as
        // type variables for [`instantiate`] to solve.
        let mut throws: Vec<Ty> = method
            .sig
            .throws
            .iter()
            .map(|ex| resolve_type_ref(db, &scope, &method_resolver, ex))
            .map(|ty| instantiate(&ty))
            .map(erase)
            .collect();
        throws.dedup();
        // §9.4: an interface method without a body and without `static` is
        // implicitly `abstract`, whether or not the keyword is written.
        let abstract_ = if declaring_interface {
            method.body().is_none() && !method.modifiers.is_static()
        } else {
            method.modifiers.is_abstract()
        };
        out.push(MethodData {
            // The method's own name — not the lookup filter, which is the
            // empty wildcard in the declaration-level walk.
            name: method.name.as_str().to_owned(),
            owner: class_key.clone(),
            owner_file: Some(source.file),
            decl_item: Some(item),
            params,
            param_names: Some(
                method
                    .sig
                    .params
                    .iter()
                    .map(|param| param.name.as_str().to_owned())
                    .collect(),
            ),
            ret,
            throws,
            varargs,
            is_static: method.modifiers.is_static(),
            abstract_,
            is_final: method.modifiers.is_final(),
            access: interface_access_of(declaring_interface, &method.modifiers),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface,
            type_params,
            raw_erased,
            descriptor: None,
        });
    }
    // §8.8.9: a class with no constructor has an implicit *default*
    // constructor — no parameters, same access as the class. Explicit
    // `super(...)` delegation ([§8.8.7.1]) and instance creation expressions
    // resolve against it; without it, a subclass of a constructor-less source
    // base reports `cannot find symbol: method Base()`.
    let declares_ctor = class_data.body().iter().any(|item| {
        item_data(&tree, *item)
            .is_some_and(|data| matches!(data, ItemData::Method(method) if method.is_constructor()))
    });
    if !declares_ctor
        && !declaring_interface
        && matches!(class_data, ItemData::Class(_) | ItemData::Enum(_))
        && (name.is_empty() || name == simple.as_str())
    {
        let modifiers = match class_data {
            ItemData::Class(d) => Some(&d.modifiers),
            ItemData::Enum(d) => Some(&d.modifiers),
            _ => None,
        };
        let access = |public: bool| {
            if public {
                Access::Public
            } else {
                Access::Package
            }
        };
        out.push(MethodData {
            name: simple.as_str().to_owned(),
            owner: class_key.clone(),
            owner_file: Some(source.file),
            decl_item: None,
            params: Vec::new(),
            param_names: None,
            ret: class_key.as_ty(db, Vec::new()),
            throws: Vec::new(),
            varargs: false,
            is_static: false,
            abstract_: false,
            is_final: false,
            // §8.8.9: the default constructor has the same access modifier
            // as the class (package-private when the class has none).
            access: modifiers
                .map(|m| access(m.is_public()))
                .unwrap_or(Access::Package),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface: false,
            type_params: Vec::new(),
            raw_erased: false,
            descriptor: None,
        });
    }
    // §8.9.3: every enum type has two implicit static members —
    // `public static E[] values()` and `public static E valueOf(String name)`
    // — unless the body declares a member of the same name itself.
    if let ItemData::Enum(_) = class_data {
        let declared: FxHashSet<String> = out.iter().map(|m| m.name.to_string()).collect();
        // An enum type is never generic ([JLS §8.9]), so the implicit
        // members' return type is the raw reference itself.
        let self_ty = class_key.as_ty(db, Vec::new());
        if !declared.contains("values") && (name.is_empty() || name == "values") {
            out.push(MethodData {
                name: "values".to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                decl_item: None,
                params: Vec::new(),
                param_names: None,
                ret: Ty::array(db, self_ty),
                throws: Vec::new(),
                varargs: false,
                is_static: true,
                abstract_: false,
                is_final: false,
                access: Access::Public,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
        if !declared.contains("valueOf") && (name.is_empty() || name == "valueOf") {
            out.push(MethodData {
                name: "valueOf".to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                decl_item: None,
                params: vec![Ty::reference(db, "java.lang.String", Vec::new())],
                param_names: None,
                ret: self_ty,
                throws: Vec::new(),
                varargs: false,
                is_static: true,
                abstract_: false,
                is_final: false,
                access: Access::Public,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
    }
    // §8.10.3: every record component has a public accessor method named
    // after it. An accessor explicitly declared by the record body *replaces*
    // the implicit one only when it has the accessor signature itself (same
    // name, zero parameters); a same-name method with parameters leaves the
    // implicit accessor in place ([§8.10.3]). Its return type is the
    // component type; a varargs component (`String... names`) is carried as
    // the array type `String[]` ([§8.4.1]).
    if let ItemData::Record(record) = class_data {
        for component in &record.components {
            let component_name = component.name.as_str();
            if !name.is_empty() && component_name != name {
                continue;
            }
            if out
                .iter()
                .any(|m| m.name.as_str() == component_name && m.params.is_empty())
            {
                continue;
            }
            let mut ty = resolve_type_ref(db, &scope, &resolver, &component.ty);
            if component.varargs {
                ty = Ty::array(db, ty);
            }
            let ty = ty.substitute(db, &binding);
            // JLS 4.8: a raw record erases its component type.
            let ty = if is_raw { ty.erasure(db) } else { ty };
            out.push(MethodData {
                name: component_name.to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                decl_item: None,
                params: Vec::new(),
                param_names: None,
                ret: ty,
                throws: Vec::new(),
                varargs: false,
                is_static: false,
                abstract_: false,
                is_final: false,
                access: Access::Public,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
        // §8.10.3: every record implicitly implements `equals`, `hashCode`
        // and `toString`, whose signatures are those mandated by
        // `java.lang.Record` — a concrete `equals(Object): boolean`, a
        // concrete `hashCode(): int` and a concrete `toString(): String`.
        // An explicit declaration of the signature in the record body
        // *replaces* the implicit one ([§8.10.3]), so the implicit members
        // are only added when the record does not declare it itself. Without
        // them the abstract `equals`/`hashCode`/`toString` of
        // `java.lang.Record` look unimplemented ([§8.1.1.1]): `Object`'s
        // concrete members are its *supertypes'* methods, not its subtypes',
        // so they never count as implementations of the record's abstract
        // superclass.
        let string_ty = Ty::reference(db, "java.lang.String", Vec::new());
        let object_ty = Ty::reference(db, "java.lang.Object", Vec::new());
        let implicit_record_members: [(&str, Vec<Ty>, Ty); 3] = [
            (
                "equals",
                vec![object_ty],
                Ty::primitive(db, syntax::stub::PrimitiveType::Boolean),
            ),
            (
                "hashCode",
                Vec::new(),
                Ty::primitive(db, syntax::stub::PrimitiveType::Int),
            ),
            ("toString", Vec::new(), string_ty),
        ];
        for (member_name, params, ret) in implicit_record_members {
            // An empty name is the wildcard of the declaration-level walk
            // ([§9.8], [`crate::java::decl_check`]); a non-empty lookup name
            // filters the implicit members like every other member — a
            // constructor lookup for the record's simple name must not see
            // `equals(Object)` as a candidate, or `new Rec("s")` would
            // resolve against it.
            if !name.is_empty() && member_name != name {
                continue;
            }
            let declared = out.iter().any(|m| {
                m.name.as_str() == member_name
                    && m.params.len() == params.len()
                    && m.params.iter().zip(&params).all(|(x, y)| x == y)
            });
            if declared {
                continue;
            }
            out.push(MethodData {
                name: member_name.to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                decl_item: None,
                params,
                param_names: None,
                ret,
                throws: Vec::new(),
                varargs: false,
                is_static: false,
                abstract_: false,
                is_final: false,
                access: Access::Public,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
        // §8.10.4: a record has a *canonical constructor* whose parameters
        // mirror the component list — same names, types and order — unless
        // the body declares a constructor with that signature itself (a
        // `@Singular`-style or compact form is still the canonical one, but
        // an explicit full-form declaration of matching arity replaces the
        // implicit member). Its access equals the record's own access.
        let simple = simple.as_str();
        if name.is_empty() || name == simple {
            let component_tys: Option<Vec<Ty>> = record
                .components
                .iter()
                .map(|component| {
                    let mut ty = resolve_type_ref(db, &scope, &resolver, &component.ty);
                    // A varargs component's canonical parameter is the array
                    // type ([§8.10.4], [§8.4.1]) — but it stays variable-arity.
                    if component.varargs {
                        ty = Ty::array(db, ty);
                    }
                    Some(ty.substitute(db, &binding))
                })
                .collect();
            if let Some(component_tys) = component_tys {
                let declares_canonical = out.iter().any(|method| {
                    method.name.as_str() == simple
                        && method.params.len() == component_tys.len()
                        && method
                            .params
                            .iter()
                            .zip(&component_tys)
                            .all(|(declared, component)| declared == component)
                });
                if !declares_canonical {
                    let varargs = record.components.last().is_some_and(|c| c.varargs);
                    let mut params = component_tys;
                    // JLS 4.8: a raw record erases its members.
                    if is_raw {
                        for param in &mut params {
                            *param = param.erasure(db);
                        }
                    }
                    out.push(MethodData {
                        name: simple.to_owned(),
                        owner: class_key.clone(),
                        owner_file: Some(source.file),
                        decl_item: None,
                        params,
                        param_names: None,
                        // Constructors carry no return type ([§8.8]).
                        ret: Ty::error(db),
                        throws: Vec::new(),
                        varargs,
                        is_static: false,
                        abstract_: false,
                        is_final: false,
                        access: access_of(&record.modifiers),
                        declaring_package: declaring_package.clone(),
                        declaring_top_level: declaring_top_level.clone(),
                        declaring_interface: false,
                        type_params: Vec::new(),
                        raw_erased: false,
                        descriptor: None,
                    });
                }
            }
        }
    }
    out
}

fn access_of(modifiers: &hir_def::java::modifiers::JavaModifiers) -> Access {
    match modifiers.visibility {
        hir_def::java::modifiers::JavaVisibility::Private => Access::Private,
        hir_def::java::modifiers::JavaVisibility::Protected => Access::Protected,
        hir_def::java::modifiers::JavaVisibility::Public => Access::Public,
        hir_def::java::modifiers::JavaVisibility::Package => Access::Package,
    }
}

/// The access of an interface member: every member of an interface is
/// implicitly `public` ([JLS §9.4], [§9.3]), whether or not the source
/// spells the modifier out.
fn interface_access_of(
    declaring_interface: bool,
    modifiers: &hir_def::java::modifiers::JavaModifiers,
) -> Access {
    match access_of(modifiers) {
        Access::Package if declaring_interface => Access::Public,
        other => other,
    }
}

/// Whether `method` is allowed by the invocation mode of `ctx`
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
/// [§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
fn mode_allows(method: &MethodData, ctx: &InvocationContext) -> bool {
    match ctx.mode {
        // A static invocation selects only static members.
        InvocationMode::Static => method.is_static,
        // §15.13.1: a type-qualified method reference may reference a static or
        // an unbound instance member, so every member of the name is a
        // candidate.
        InvocationMode::TypeQualified => true,
        // Super and interface invocations select only instance members.
        InvocationMode::Super | InvocationMode::Interface => !method.is_static,
        // §15.12.3 MethodName form: the member set is the virtual one plus the
        // static interface methods the searched type itself declares. The
        // static-interface-owner rule (declared-in-receiver-only, never
        // inherited) is applied by [`member_set_impl`], which knows the
        // receiver; no blanket static filter applies here.
        InvocationMode::MethodName => true,
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
        &method.owner,
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
    owner: &ClassKey,
    declaring_top_level: Option<&str>,
    receiver: &Ty,
    static_member: bool,
    ctx: &InvocationContext,
) -> bool {
    match access {
        Access::Public => true,
        // §6.6.1: a private member is accessible throughout the top-level
        // class in which it is declared — a local declaration's top level is
        // the class enclosing it.
        Access::Private => match (&ctx.enclosing_class, declaring_top_level) {
            (Some(enclosing), Some(declaring)) => {
                let enclosing = enclosing.top_level(db, ctx.package.as_deref());
                match enclosing {
                    Some(enclosing) => within_top_level(enclosing.as_str(), declaring),
                    None => false,
                }
            }
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
            // §6.6.2: code that is "responsible for the implementation of an
            // object of the subclass" — an *anonymous class body* whose direct
            // superclass is the declaring class — may access the protected
            // members the superclass itself declares, from any package (the
            // Gson `new TypeToken<T>() {}` idiom: an anonymous subclass of the
            // generic `TypeToken` invokes its protected no-arg constructor).
            if let Some(subclass_of) = &ctx.subclass_of {
                let subclass_of = subclass_of.as_ty(db, Vec::new());
                let declaring = owner.as_ty(db, Vec::new());
                // The anonymous class subclasses the *created* type exactly;
                // members it inherits from a *grand*-supertype of that type are
                // reachable only when the created type itself is a subclass,
                // which this membership already entails via the transitive
                // subtype check below.
                if is_subtype(db, scope, &subclass_of, &declaring) {
                    return true;
                }
            }
            let declaring = owner.as_ty(db, Vec::new());
            let subclass = ctx.enclosing_class.as_ref().and_then(|enclosing| {
                // §6.6.2: the access may appear in the body of a *nested*
                // class of a subclass — `B.Inner2` inside `B extends A`
                // accessing A's protected members — where the *innermost*
                // enclosing class `app.B.Inner2` is not itself a subclass but
                // its enclosing `app.B` is. Walk the enclosing chain outward
                // and find the subclass `S` whose body contains the access.
                enclosing_class_keys(db, ctx.package.as_deref(), enclosing)
                    .into_iter()
                    .find(|candidate| {
                        let candidate = candidate.as_ty(db, Vec::new());
                        is_subtype(db, scope, &candidate, &declaring)
                    })
            });
            match subclass {
                // §6.6.2: a protected instance member accessed outside the
                // declaring package by a receiver expression requires the type
                // of that expression to be a subtype of the subclass `S` whose
                // body the access is in — a `B.this.value(...)` inside
                // `B.Inner2` accesses through a receiver of type `B` (= S). A
                // `super` invocation accesses the member through the `super`
                // keyword, not an expression, so the rule does not apply
                // ([§15.12.1]).
                Some(_subclass) if static_member || ctx.mode == InvocationMode::Super => true,
                Some(subclass) => {
                    let subclass = subclass.as_ty(db, Vec::new());
                    is_subtype(db, scope, receiver, &subclass)
                }
                None => false,
            }
        }
    }
}

/// Whether some declared type parameter of the class `fqn` has a bound that
/// references the class itself — the *SELF* channel of a fluent API
/// (`SELF extends AbstractStringAssert<SELF>`), as opposed to a plain element
/// parameter (`Enumeration<E>`).
/// The indexes of the declared type parameters of the class `fqn` whose
/// bounds reference the class itself — the *SELF* channel of a fluent API
/// (`SELF extends AbstractStringAssert<SELF>`), as opposed to a plain element
/// parameter (`Enumeration<E>`). Library-only resolution (source classes
/// return empty — their bounds are source references resolved per class, and
/// the fluent-SELF rewriting targets the assertj-style library stubs);
/// `Some`/`None` per resolved class, `None` when the class or its generic
/// info is unavailable.
fn self_type_param_indexes(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
) -> Option<Vec<usize>> {
    let resolved = hir::fqn_resolve(db, scope, fqn)?;
    let params = match resolved {
        hir::Resolved::Library(_) => hir::class_generic_info(db, &resolved)?.type_params,
        hir::Resolved::Source(_) => return Some(Vec::new()),
    };
    fn mention(
        bound: &syntax::stub::TypeRef<hir::Symbol>,
        self_fqn: &str,
        interner: &dyn hir::HirDatabase,
    ) -> bool {
        match bound {
            syntax::stub::TypeRef::Reference { name, generic_args } => {
                interner.hir_state().interner.resolve(name) == self_fqn
                    || generic_args
                        .iter()
                        .any(|arg| mention(arg, self_fqn, interner))
            }
            _ => false,
        }
    }
    Some(
        params
            .iter()
            .enumerate()
            .filter_map(|(i, param)| {
                param
                    .bounds
                    .iter()
                    .any(|bound| mention(bound, fqn, db))
                    .then_some(i)
            })
            .collect(),
    )
}

/// The class an access appears in and each class enclosing it, innermost first
/// ([JLS §6.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.2)):
/// the §6.6.2 protected-access rule looks for the enclosing subclass whose
/// body contains the access, which may be an enclosing class rather than the
/// innermost one. A *local* declaration walks its real declaration chain; a
/// canonical name is trimmed segment by segment, bounded by the package.
fn enclosing_class_keys(
    db: &dyn TyDatabase,
    package: Option<&str>,
    key: &ClassKey,
) -> Vec<ClassKey> {
    let mut out = vec![key.clone()];
    match key {
        ClassKey::Local(class) => {
            let tree = hir::file_item_tree(db, class.file);
            out.extend(
                crate::java::resolve::enclosing_type_chain(&tree, class.item)
                    .into_iter()
                    .map(ClassKey::Named),
            );
        }
        ClassKey::Named(fqn) => {
            let package_prefix = match package {
                Some(package) if !package.is_empty() => format!("{package}."),
                _ => String::new(),
            };
            let mut candidate = fqn.as_str().to_owned();
            while let Some((prefix, _)) = candidate.rsplit_once('.') {
                if !prefix.starts_with(&package_prefix) {
                    break;
                }
                out.push(ClassKey::Named(Name::new(prefix)));
                candidate = prefix.to_owned();
            }
        }
    }
    out
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
    let mut subst: FxHashMap<TypeVarScope, Ty> = FxHashMap::default();
    for tp in &method.type_params {
        let var = inference.fresh_var(db);
        subst.insert(tp.scope.clone(), var);
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
    //
    // For a variable-arity invocation ([§15.12.2.4]) the trailing actuals are
    // compatible with the *i*-th variable arity parameter type — the element
    // `Fn` for `i ≥ n` ([§8.4.1]) — so a trailing lambda's arity must match
    // the element's SAM ([§15.27.3]): the varargs array formal `Fn[]` is not
    // a functional interface, and a mismatch makes the candidate not
    // applicable ([§15.12.2.2]).
    let varargs_element = if varargs {
        formals.last().and_then(|f| f.element(db))
    } else {
        None
    };
    for (i, arg) in args.iter().enumerate() {
        if let PolyArg::Poly(_, Some(arity)) = arg {
            // Beyond the fixed prefix every trailing actual is packed into
            // the varargs array and is checked against the element type; an
            // element whose SAM is not (yet) resolvable — an inference
            // variable awaiting instantiation — skips the check rather than
            // falsely rejecting. (Phase 3 passes `varargs` for every member;
            // a non-varargs candidate with no formals hits `!method.varargs`
            // below, so the empty case must not panic here.)
            let target = if varargs && i >= formals.len().saturating_sub(1) {
                match varargs_element {
                    Some(element) => element,
                    None => continue,
                }
            } else {
                let Some(formal) = formals.get(i) else {
                    continue;
                };
                formal
            };
            if let Some(sam) = single_abstract_method(db, scope, target)
                && sam.params.len() != *arity
            {
                return None;
            }
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
        owner_file: method.owner_file,
        decl_item: method.decl_item,
        params: formals
            .iter()
            .map(|p| p.substitute_infer(db, &resolved))
            .collect(),
        param_names: method.param_names.clone(),
        ret: method
            .ret
            .substitute(db, &subst)
            .substitute_infer(db, &resolved),
        throws,
        varargs: method.varargs,
        is_static: method.is_static,
        abstract_: method.abstract_,
        is_final: method.is_final,
        access: method.access,
        declaring_package: method.declaring_package.clone(),
        declaring_top_level: method.declaring_top_level.clone(),
        declaring_interface: method.declaring_interface,
        type_params: method.type_params.clone(),
        raw_erased: method.raw_erased,
        // The identity of the member does not change with the type arguments
        // the invocation instantiates it at.
        descriptor: method.descriptor.clone(),
    })
}

/// Whether `m1` is more specific than `m2`
/// ([JLS §15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)):
/// every formal parameter of `m1` is more specific than the corresponding
/// formal of `m2`. A variable-arity method is treated as a fixed-arity method
/// in the first two applicability phases ([§15.12.2]), so a
/// declared-variable-arity candidate loses to a fixed-arity one only when its
/// array formal is *less* specific — `m(Object)` beats `m(Object...)` for a
/// non-array argument, decided by applicability, not here — and wins when its
/// array formal is more specific: for an array argument `m(Object[])` beats
/// `m(Object)`, as §15.12.2's own note records ("declaring `m(Object...)` in a
/// class which already declares `m(Object)` causes `m(Object)` to no longer be
/// chosen for some invocation expressions (such as `m(null)`), as `m(Object[])`
/// is more specific"). There is no declared-flag tie-break in §15.12.2.5.
/// Only *m2's* genericity gates the comparison: when `m2` is generic,
/// `m1` is more specific under some instantiation of *m2*'s type parameters
/// ([§18.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.4))
/// — approximated here by instantiating them to their declared bounds, so
/// `<T> that(T[])` loses to nothing merely for being generic and beats
/// `that(Object)` for an array argument (`T[] <: Object`) exactly as javac
/// resolves Truth's builder. For two generic methods the
/// type-parameter-relative signatures are compared: `m2`'s type parameters are
/// substituted by `m1`'s (by position), and `m1`'s declared bounds must be at
/// least as restrictive as `m2`'s, so `<T extends String>` is more specific
/// than `<T>`.
pub(crate) fn more_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    m1: &MethodData,
    m2: &MethodData,
    variable_arity: bool,
) -> bool {
    // §15.12.2.5 bullet 3: the variable-arity alignment applies only when
    // both candidates are applicable by *variable arity* invocation — i.e. in
    // the variable-arity phase (§15.12.2.4) and only for candidates declared
    // variable arity.
    // §15.12.2.5 (variable arity): when *both* candidates are applicable by
    // variable arity invocation, the more specific one is decided on their
    // fixed parameter prefixes and varargs element types, aligned at the
    // invocation's argument count. The declared parameter lists may have
    // different lengths — `style(TextColor, Decoration...)` vs
    // `style(StyleBuilderApplicable...)` — so each is normalized to the
    // longer length by repeating its varargs *element* type (the fixed
    // prefix first, then elements), and the position-wise subtype test runs
    // on the aligned lists. `m1` more specific than `m2` iff each of `m1`'s
    // normalized formals is a subtype of the corresponding `m2` formal:
    // `style(TextColor, Decoration...)` beats `style(SBApplicable...)`
    // because `TextColor <: SBApplicable` and `Decoration <: SBApplicable`.
    // Without the alignment both directions report false (different declared
    // lengths) and the invocation is ambiguous.
    if variable_arity && m1.varargs && m2.varargs {
        let norm = |m: &MethodData| -> Vec<Ty> {
            let split = m.params.len().saturating_sub(1);
            let fixed = &m.params[..split];
            let element = m
                .params
                .last()
                .and_then(|last| last.element(db))
                .copied()
                .unwrap_or_else(|| Ty::reference(db, "java.lang.Object", Vec::new()));
            let len = m1.params.len().max(m2.params.len());
            let mut out = fixed.to_vec();
            while out.len() < len {
                out.push(element);
            }
            out
        };
        let p1 = norm(m1);
        let mut inference = Inference::new();
        let (m2_formals, _, _) = inference.register_method(db, m2);
        if m2_formals.len() != m2.params.len() {
            return false;
        }
        // Map `m2`'s normalized formals: its own params (with the varargs
        // array) normalized the same way after registering its type params.
        let m2_norm = {
            let split = m2.params.len().saturating_sub(1);
            let mut out = m2_formals[..split].to_vec();
            let element = m2_formals
                .last()
                .and_then(|last| last.element(db))
                .copied()
                .unwrap_or_else(|| Ty::reference(db, "java.lang.Object", Vec::new()));
            let len = m1.params.len().max(m2.params.len());
            while out.len() < len {
                out.push(element);
            }
            out
        };
        for (a, b) in p1.iter().zip(&m2_norm) {
            inference.add_constraint(Constraint::Sub(*a, *b));
        }
        return inference.check_consistent(db, scope, InvocationPhase::Loose);
    }
    if m1.params.len() != m2.params.len() {
        return false;
    }
    // §15.12.2.5 (functional interface specificity): when the invocation's
    // argument is a lambda, a functional interface type `S` is more specific
    // than a functional interface type `T` when — beyond matching formal
    // parameter lists — `T`'s function type has a `void` return and `S`'s does
    // not, or `S`'s return type is a subtype of `T`'s (the `RS`/`RT`
    // conditions of §15.12.2.5). This is what resolves the
    // `CheckedSupplier<T>`/`CheckedRunnable` pair that the ordinary
    // subsignature test cannot: `Sub(CheckedSupplier<α>, CheckedRunnable)`
    // reduces to false (different erasures), yet a lambda whose body produces
    // a value must select the value-returning overload regardless of the
    // invocation's target (§15.12.2.5) — `return unchecked(() -> getFloat())`
    // selects `<T> T unchecked(CheckedSupplier<T>)`, not the `void`
    // `unchecked(CheckedRunnable)`. Without it the void overload wins the
    // tie-break and the enclosing `return` misreports the selected `void`
    // against the primitive return type.
    if let Some(win) = functional_interface_specificity(db, scope, m1, m2) {
        return win;
    }
    // §15.12.2.5 as javac implements it: `m1` is more specific than `m2` iff
    // `m2` is *applicable* to `m1`'s formal parameter types treated as the
    // invocation arguments (`signatureMoreSpecific`) — `m2`'s type
    // parameters are instantiated from `m1`'s parameter types, not from
    // their declared bounds. This resolves both the mixed-genericity cases
    // javac handles:
    //   `increment(Map<String,Integer>, String)` (non-generic) beats
    //     `<T> increment(Map<T,Integer>, T)` — the generic one instantiates
    //     `T := String` from the non-generic's parameters;
    //   `<T> Subject that(T[])` beats `that(Object)` for an array argument —
    //     the generic method's parameters are `T[]`, and `that(Object)` is
    //     applicable to them, while the reverse is not (`Object` is not an
    //     array). The constraint set is exactly an invocation type inference
    //     table ([JLS §18.5.2]); consistency means `m2` accepts `m1`'s
    //     parameters, i.e. `m1`'s signature is a subsignature of `m2`'s
    //     instantiated one.
    let mut inference = Inference::new();
    let (m2_formals, _, _) = inference.register_method(db, m2);
    if m2_formals.len() != m1.params.len() {
        return false;
    }
    for (param, formal) in m1.params.iter().zip(&m2_formals) {
        inference.add_constraint(Constraint::Sub(*param, *formal));
    }
    inference.check_consistent(db, scope, InvocationPhase::Loose)
}

/// The functional-interface half of the most-specific test
/// ([JLS §15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)):
/// whether `m1` is more specific than `m2` because their corresponding
/// parameters are *functional interface* types `S` (m1's) and `T` (m2's)
/// whose function types differ only in their return type — `T` returning
/// `void` while `S` returns a value, or `S`'s return being a subtype of
/// `T`'s. `Some(true)`/`Some(false)` when the rule decides the pair, `None`
/// when the parameters are not a comparable functional-interface pair (the
/// ordinary subsignature test must decide).
///
/// This is the rule javac applies to a lambda argument that is *value
/// compatible* with both overloads: `use(() -> now())` over
/// `void use(V)` / `<T> T use(I<T>)` selects the value-returning `I<T>`
/// overload even in a statement context with no target. The two functional
/// interfaces need not be subtypes of each other (indeed `CheckedSupplier`
/// and `CheckedRunnable` are unrelated), so the generic subsignature test
/// above cannot compare them.
fn functional_interface_specificity(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    m1: &MethodData,
    m2: &MethodData,
) -> Option<bool> {
    if m1.params.len() != m2.params.len() {
        return None;
    }
    let mut s_ret: Option<Ty> = None;
    let mut t_ret: Option<Ty> = None;
    for (s, t) in m1.params.iter().zip(&m2.params) {
        // The rule applies to a *lambda argument*: both formals must be
        // functional interfaces with matching SAM parameter lists.
        let (Some(ssam), Some(tsam)) = (
            single_abstract_method(db, scope, s),
            single_abstract_method(db, scope, t),
        ) else {
            return None;
        };
        if ssam.params != tsam.params {
            return None;
        }
        // The adapt-and-capture nuance of §15.12.2.5 (S's wildcards captured)
        // is approximated by the SAM returns as extracted.
        match (&s_ret, &t_ret) {
            (None, None) => {
                s_ret = Some(ssam.ret);
                t_ret = Some(tsam.ret);
            }
            (Some(a), Some(b)) => {
                if a != &ssam.ret || b != &tsam.ret {
                    return None;
                }
            }
            _ => unreachable!(),
        }
    }
    let (s_ret, t_ret) = (s_ret?, t_ret?);
    // `RT is void` (m2's SAM returns void) makes `S` more specific than `T`
    // for a lambda that is value-compatible with both — the value-returning
    // overload beats the void one. Otherwise `RS <: RT` (m1's return a subtype
    // of m2's) makes `S` more specific.
    if t_ret.is_void_like(db) && !s_ret.is_void_like(db) {
        return Some(true);
    }
    if s_ret.is_void_like(db) && !t_ret.is_void_like(db) {
        return Some(false);
    }
    if !s_ret.is_void_like(db) && !t_ret.is_void_like(db) {
        let s_cap = crate::java::ty::capture_conversion(db, scope, s_ret);
        if crate::java::subtyping::is_subtype(db, scope, &s_cap, &t_ret) {
            return Some(true);
        }
        let t_cap = crate::java::ty::capture_conversion(db, scope, t_ret);
        if crate::java::subtyping::is_subtype(db, scope, &t_cap, &s_ret) {
            return Some(false);
        }
    }
    None
}

/// Whether `param1` is a subtype of `param2` for the most-specific comparison
/// ([JLS §15.12.2.5], [§4.10.1]): reference types by subtyping, primitive
/// types by the primitive supertype order (double ≻ float ≻ long ≻ int ≻
/// {char, short, byte}).
pub(crate) fn choose_most_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    candidates: &[(MethodData, MethodData)],
    variable_arity: bool,
) -> Option<MethodData> {
    let mut winners: Vec<usize> = Vec::new();
    for (i, (candidate, _)) in candidates.iter().enumerate() {
        let wins = candidates.iter().all(|(other, _)| {
            other == candidate || more_specific(db, scope, candidate, other, variable_arity)
        });
        if wins {
            winners.push(i);
        }
    }

    // §15.12.2.5/§8.4.8.1: several equally-most-specific candidates whose
    // declared signatures are identical are one method seen through
    // overriding paths (covariant returns defeat exact-signature dedup).
    // Collapse them first, then prefer the most-derived declaring type;
    // genuinely unrelated declarations stay ambiguous.
    let mut unique: Vec<usize> = Vec::new();
    'outer: for &i in &winners {
        for &u in &unique {
            let (a, b) = (&candidates[u].0, &candidates[i].0);
            if a.params == b.params
                && a.ret == b.ret
                && a.is_static == b.is_static
                && a.varargs == b.varargs
            {
                continue 'outer;
            }
        }
        unique.push(i);
    }

    match unique.len() {
        0 => None,
        1 => Some(candidates[unique[0]].1.clone()),
        _ => {
            let chosen = unique.iter().copied().find(|&i| {
                let owner = candidates[i].0.owner.as_ty(db, Vec::new());
                unique.iter().all(|&j| {
                    j == i || {
                        let other = candidates[j].0.owner.as_ty(db, Vec::new());
                        is_subtype(db, scope, &owner, &other)
                    }
                })
            });
            chosen.map(|i| candidates[i].1.clone())
        }
    }
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
        return choose_most_specific(db, scope, &strict, false);
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
        return choose_most_specific(db, scope, &loose, false);
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
        return choose_most_specific(db, scope, &varargs, true);
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
    /// The declaring class or interface ([`ClassKey`]).
    pub owner: ClassKey,
    /// The workspace source file declaring this field, when it is a source
    /// declaration (including the implicit enum-constant and record-component
    /// fields of a source class). `None` for library members.
    pub owner_file: Option<FileId>,
    /// The item id of the source declaration, when there is one — the anchor
    /// the declaration-level checks report at (the deprecated-use warning
    /// reads the declaration's own `@Deprecated` from it).
    pub decl_item: Option<ItemId>,
    /// The field's type, instantiated with the declaring type's type arguments.
    pub ty: Ty,
    /// The classfile descriptor of a library field
    /// ([JVMS §4.5](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5)). `None`
    /// for a source declaration and for a field this crate synthesizes.
    pub descriptor: Option<SmolStr>,
    /// Whether the field is static.
    pub is_static: bool,
    /// The access of the field
    /// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
    pub access: Access,
    /// Whether the field is `final`
    /// ([JLS §8.3.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3.1.2)):
    /// the ACC_FINAL flag of the classfile
    /// ([JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1))
    /// or the `final` modifier of the source. A final field cannot be assigned
    /// after initialization ([§16]).
    pub is_final: bool,
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
    pick_field_impl(db, scope, receiver, name, ctx, true)
}

/// The most-derived field named `name` on the receiver *regardless of access
/// control* ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)):
/// the access-probe companion of [`pick_field`]. When the accessible
/// [`pick_field`] misses but this hits, a field of the name exists yet is not
/// accessible from the access site — the §6.6 error reported by the body
/// inference layer as `IllegalAccess`.
pub fn pick_field_ignoring_access(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Option<FieldData> {
    pick_field_impl(db, scope, receiver, name, ctx, false)
}

/// The non-memoized form of [`pick_field`] and
/// [`pick_field_ignoring_access`]: the most-derived declaration of `name` on
/// the receiver, filtered by accessibility only when `strict_access` is set.
fn pick_field_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
    strict_access: bool,
) -> Option<FieldData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let receiver = capture_conversion(db, scope, *receiver);
    // §4.4: an unbounded type variable's effective upper bound is
    // `java.lang.Object`, so its fields are the fields of `Object` (none).
    //
    // JLS §4.8 ([§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)):
    // "... the superclass types (respectively, superinterface types) of a raw
    // type are the erasures of the superclass types (superinterface types) of
    // the named class or interface", and "the type of an inherited instance
    // method or non-static field of a raw type C, where the member was
    // declared in a class or interface D, is the type of the member in the
    // supertype of C that names D." As in `member_set_impl`, the walk carries a
    // monotone erasure context per stack entry: a raw receiver erases the
    // supertype edges, so a field *declared* in a generic ancestor erases to
    // its §4.6 erasure (`Gen<T>` with `List<String> items` reached raw gives
    // `List`), while a field declared in a non-generic class keeps its
    // declared type even when reached through generic ancestors.
    let mut stack: Vec<(Ty, bool)> = match receiver.kind(db) {
        TyKind::TypeVar { bounds, .. } if bounds.is_empty() => {
            vec![(Ty::reference(db, "java.lang.Object", Vec::new()), false)]
        }
        TyKind::TypeVar { bounds, .. } => bounds.iter().map(|bound| (*bound, false)).collect(),
        _ => vec![(receiver, false)],
    };
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    while let Some((ty, erased)) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        for field in class_fields(db, &scope_id, &ty, name) {
            if !strict_access
                || member_accessible(
                    db,
                    scope,
                    field.access,
                    field.declaring_package.as_deref(),
                    &field.owner,
                    field.declaring_top_level.as_deref(),
                    &receiver,
                    field.is_static,
                    ctx,
                )
            {
                return Some(field);
            }
        }
        let raws = erased || is_raw_use(db, scope, &ty);
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push((if raws { parent.erasure(db) } else { parent }, raws));
        }
    }
    None
}

/// The fields of a single class or interface, instantiated with `ty`'s type
/// arguments.
fn class_fields(db: &dyn TyDatabase, scope_id: &ScopeId, ty: &Ty, name: &str) -> Vec<FieldData> {
    let TyKind::Reference { args, .. } = ty.kind(db) else {
        return Vec::new();
    };
    // §6.7: the receiver's declaration — a *local* class's own item, or the
    // class its canonical name resolves to.
    let Some(resolved) =
        crate::java::resolve::reference_class(db, &scope_id.kind(db).to_scope(), ty)
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
    // JLS 4.8: a *raw* use of a generic class erases its members'
    // signatures; the erasure is applied to each constructed member below.
    let is_raw = args.is_empty() && !class.type_params.is_empty();
    let fqn = interner.resolve(&class.fqn).to_owned();
    let owner = Name::new(&fqn);
    let class_names: Vec<Name> = class
        .type_params
        .iter()
        .map(|tp| Name::new(interner.resolve(&tp.name)))
        .collect();
    // §4.10.2 with [§4.4]/[§6.3]: the field's declared type is instantiated
    // with the receiver's arguments, keyed by the classfile parameters'
    // declaring scopes.
    let binding: FxHashMap<TypeVarScope, Ty> =
        crate::java::resolve::library_class_binding(&owner, &class_names, &args);
    let class_ctx = crate::java::resolve::LibrarySignature::class(&owner);
    let declaring_package = package_of(&fqn);
    let declaring_top_level = Some(top_level_of(&fqn));
    let mut out = Vec::new();
    for field in &class.fields {
        if interner.resolve(&field.name) != name {
            continue;
        }
        let is_static = JvmAccessFlags::from_bits_retain(field.flags).is_static();
        // JLS 4.8: the *instance* fields of a raw type have erased types. A
        // static field does not depend on the receiver's type arguments, so
        // its declared type stays intact.
        let ty = {
            let ty =
                crate::java::resolve::ty_from_library_signature(db, &field.field_type, &class_ctx)
                    .substitute(db, &binding);
            if is_raw && !is_static {
                ty.erasure(db)
            } else {
                ty
            }
        };
        out.push(FieldData {
            name: name.to_owned(),
            owner: ClassKey::Named(Name::new(&fqn)),
            owner_file: None,
            decl_item: None,
            ty,
            descriptor: Some(SmolStr::from(interner.resolve(&field.descriptor))),
            is_static,
            access: Access::from_flags(field.flags),
            is_final: JvmAccessFlags::from_bits_retain(field.flags).is_final(),
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
    // JLS 4.8: a *raw* use of a generic class erases its members'
    // signatures; the erasure is applied to each constructed member below.
    let is_raw = args.is_empty() && !declared.is_empty();
    // §4.10.2 with [§4.4]/[§6.3]: the fields' declared types are instantiated
    // with the receiver's arguments, keyed by the declaring parameters'
    // scopes so only the class's *own* variables are replaced.
    let binding: FxHashMap<TypeVarScope, Ty> =
        crate::java::resolve::source_class_binding(source.file, source.item, declared, &args);
    let resolver = Resolver::for_item(db, source.file, &tree, source.item);
    let scope = scope_for_file(db, source.file);
    // §6.7: the receiver's declaration — its canonical name, or the
    // declaration itself when it has none (a local class-like declaration,
    // [JLS §14.3]).
    let class_key = ClassKey::of(&tree, source.file, source.item);
    let package = resolver.package().map(|p| p.as_str().to_owned());
    let declaring_package = Some(package.clone().unwrap_or_default());
    // §6.6.1: a private member's accessibility is scoped by the *top-level*
    // class — the outermost enclosing class-like declaration, which for a
    // local declaration is the class around it.
    let declaring_top_level = class_key
        .top_level(db, package.as_deref())
        .map(|name| name.as_str().to_owned());
    let declaring_interface =
        matches!(class_data, ItemData::Interface(_) | ItemData::Annotation(_));
    let declaring_enum = matches!(class_data, ItemData::Enum(_));

    let mut out = Vec::new();
    for &item in class_data.body() {
        match item_data(&tree, item) {
            Some(ItemData::Field(field)) => {
                if field.name.as_str() != name {
                    continue;
                }
                let key = ItemKey::new(db, source.file, item);
                // §9.3/[§9.6]: every field of an interface or annotation type
                // is implicitly `public static final`, whether or not the
                // source spells the modifiers out. A static context may read
                // such a field by simple name ([§8.1.3]) and a qualified
                // access (`I.F`) resolves it as static; treating it as an
                // instance field would report a false
                // non-static-cannot-be-referenced.
                let (is_static, is_final) = if declaring_interface {
                    (true, true)
                } else {
                    (field.modifiers.is_static(), field.modifiers.is_final())
                };
                // JLS 4.8: the *instance* fields of a raw type have erased
                // types. A static field does not depend on the receiver's type
                // arguments at all, so its declared type stays intact — the
                // companion of the raw-source-method rule in
                // [`source_class_methods`]. Erasing `NBTType.STRING` to raw
                // `NBTType` would discard the `NBTType<NBTString>` argument
                // javac reads from the constant's declaration.
                let ty = {
                    let ty = item_ty_query(db, key).substitute_incl_bounds(db, &binding);
                    if is_raw && !is_static {
                        ty.erasure(db)
                    } else {
                        ty
                    }
                };
                out.push(FieldData {
                    name: name.to_owned(),
                    owner: class_key.clone(),
                    owner_file: Some(source.file),
                    decl_item: Some(item),
                    ty,
                    is_static,
                    access: interface_access_of(declaring_interface, &field.modifiers),
                    is_final,
                    declaring_package: declaring_package.clone(),
                    declaring_top_level: declaring_top_level.clone(),
                    descriptor: None,
                });
            }
            // §8.9.2: each enum constant is an implicitly `public static
            // final` field of the enum type, typed as the enum itself — a
            // static import (`import static E.CONSTANT`) and qualified reads
            // resolve through it.
            Some(ItemData::EnumConstant(constant))
                if declaring_enum && constant.name.as_str() == name =>
            {
                out.push(FieldData {
                    name: name.to_owned(),
                    owner: class_key.clone(),
                    owner_file: Some(source.file),
                    decl_item: Some(item),
                    ty: class_key.as_ty(db, binding.values().copied().collect()),
                    is_static: true,
                    access: Access::Public,
                    is_final: true,
                    declaring_package: declaring_package.clone(),
                    declaring_top_level: declaring_top_level.clone(),
                    descriptor: None,
                });
            }
            _ => {}
        }
    }
    // §8.10.3: each record component declares a private instance field of the
    // component type; a varargs component (`String... names`) is carried as
    // the array type `String[]` ([§8.4.1]). Reads inside the record body and
    // accessor synthesis resolve through it.
    if let ItemData::Record(record) = class_data {
        for component in &record.components {
            let component_name = component.name.as_str();
            if !name.is_empty() && component_name != name {
                continue;
            }
            let mut ty = resolve_type_ref(db, &scope, &resolver, &component.ty);
            if component.varargs {
                ty = Ty::array(db, ty);
            }
            out.push(FieldData {
                name: component_name.to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                // A record component is not an `ItemId`, so the synthesized
                // field has no declaration anchor; a `@Deprecated` on a
                // component is a javac *warning* about the annotation having
                // no effect here, not a deprecation of the component.
                decl_item: None,
                ty: ty.substitute(db, &binding),
                is_static: false,
                access: Access::Private,
                is_final: true,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                descriptor: None,
            });
        }
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
