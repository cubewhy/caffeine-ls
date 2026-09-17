//! The member set of a Kotlin receiver, and overload selection.
//!
//! KLS
//! `overload-resolution.html#overload-resolution`](https://kotlinlang.org/spec/overload-resolution.html#overload-resolution)
//! fixes the candidate set a call is resolved against and the order candidates
//! are chosen in:
//!
//! * the *receivers* a call can be written on: the class itself (declared and
//!   inherited members, plus its `companion object`'s when the receiver is the
//!   class), and — for a classifier receiver — the extension functions and
//!   properties in scope ([`#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers));
//! * applicability: arity with *default parameters* filled
//!   ([`declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)),
//!   a `vararg` parameter absorbing the rest
//!   ([`#variable-length-parameters`](https://kotlinlang.org/spec/declarations.html#variable-length-parameters)),
//!   named arguments matched by parameter name, then assignability of every
//!   argument to its parameter ([`#determining-function-applicability-for-a-specific-call`](https://kotlinlang.org/spec/overload-resolution.html#determining-function-applicability-for-a-specific-call));
//! * selection: among the applicable candidates, the one every *other*
//!   applicable candidate's arguments can be passed to
//!   ([`#choosing-the-most-specific-candidate-from-the-overload-candidate-set`](https://kotlinlang.org/spec/overload-resolution.html#choosing-the-most-specific-candidate-from-the-overload-candidate-set)).
//!
//! # Scope
//!
//! Inheritance is walked over the Kotlin item tree for a source class and over
//! the classfile stubs for a library one; a member's *parameter types* are
//! resolved from the declaring item ([`super::db::item_ty`] and
//! [`crate::java::resolve::method_params`] for the library half, where the
//! classfile gives the descriptors directly).
//!
//! Extension members declared in *other* files are not collected yet — the
//! file's own extensions are, through the same member walk — and ties in the
//! most-specific step fall back to declaration order rather than KLS's full
//! `MSC` algorithm. Both are recorded deviations; the call sites of this
//! milestone (the body inference's calls and the IDE's hover) resolve declared
//! members, and every candidate that survives applicability has the *same*
//! parameter types in the tests that pin them.

use triomphe::Arc;

use hir::hir_def::kotlin::item_tree::{KotlinItemData, KotlinItemTree};
use hir_expand::name::Name;
use vfs::FileId;

use super::resolve::KotlinResolver;
use crate::jvm::db::TyDatabase;
use crate::jvm::member::{FieldData, MethodData};
use crate::jvm::member_set::{InvocationContext, InvocationMode};
use crate::kotlin::ty::ty_from_java;
use crate::ty::{Ty, TyKind};

/// The declaration a member resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberTarget {
    /// A Kotlin source declaration.
    Kotlin {
        file: FileId,
        item: hir_expand::ids::ItemId,
    },
    /// A Java source method or constructor, or a classfile method — the
    /// instantiated form [`crate::jvm::member_set::member_set`] returns.
    Java(Box<MethodData>),
    /// A Java source field or a classfile field, or a synthesized property.
    JavaField(Box<FieldData>),
    /// A member the *language* declares on a built-in classifier: the numeric
    /// conversion functions (`Double.toInt`) and the properties of an array
    /// (`Array.size`) are compiler intrinsics, so no declaration carries them
    /// ([`super::builtins`]).
    Builtin { ret: Ty },
}

/// A member reachable on a receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    /// The declaration the member resolves to.
    pub target: MemberTarget,
    pub name: Name,
    pub kind: MemberKind,
    /// The declared parameter types of a function (empty for a property).
    pub params: Vec<Ty>,
    /// The declared parameter *names*, in parameter order — what a named
    /// argument is matched against ([KLS
    /// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
    /// Empty for a member whose names the declaration does not carry (a
    /// classfile method without a `MethodParameters` attribute).
    pub param_names: Arc<[Name]>,
    /// One flag per parameter, in parameter order: whether it declares a default
    /// value, which a call may therefore omit ([KLS
    /// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
    /// All `false` for a classfile member, which records no defaults.
    pub defaulted: Arc<[bool]>,
    /// Whether the last parameter is a `vararg`.
    pub vararg: bool,
    /// Whether the declaration is an *extension* — written with a receiver
    /// (`fun String.twice()`) — rather than a member of the receiver's class
    /// ([KLS
    /// `overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers)
    /// resolves a member first and an extension in scope only after it).
    pub extension: bool,
}

impl Member {
    /// The type of the member: a Kotlin declaration's from its item, a Java
    /// method's return type, a field's type — each converted to the Kotlin type
    /// it denotes ([`ty_from_java`]), because a Java or classfile type is a
    /// *platform* type in Kotlin.
    pub fn ty(&self, db: &dyn TyDatabase) -> Ty {
        match &self.target {
            // A Kotlin constructor's item type *is* the class it constructs.
            MemberTarget::Kotlin { file, item } => super::db::item_ty(db, *file, *item),
            // A JVM constructor returns `void`, so its own type is the class
            // it constructs — the owner, raw, since the constructor's
            // `MethodData` carries no type arguments. A call site knows the
            // parameterized type it constructed and uses that
            // ([`CallSite`]'s caller), this is the fallback.
            MemberTarget::Java(method) if self.kind == MemberKind::Constructor => {
                method.owner.as_ty(db, Vec::new())
            }
            MemberTarget::Java(method) => match self.kind {
                // A setter's own return is `void`; the type it *writes* is its
                // parameter's — already the Kotlin type of the property.
                MemberKind::Setter => self
                    .params
                    .first()
                    .copied()
                    .unwrap_or_else(|| ty_from_java(db, method.ret)),
                _ => ty_from_java(db, method.ret),
            },
            MemberTarget::JavaField(field) => ty_from_java(db, field.ty),
            // A built-in member's type is the one the language declares for it.
            MemberTarget::Builtin { ret } => *ret,
        }
    }

    /// The source file of the declaration, `None` for a classfile member.
    pub fn file(&self) -> Option<FileId> {
        match &self.target {
            MemberTarget::Kotlin { file, .. } => Some(*file),
            MemberTarget::Java(method) => method.owner_file,
            MemberTarget::Builtin { .. } => None,
            MemberTarget::JavaField(field) => field.owner_file,
        }
    }

    /// The element type a `vararg` parameter takes its arguments as: the
    /// parameter's own type, or the element of the array type it compiles to
    /// ([KLS
    /// `declarations.html#variable-length-parameters`](https://kotlinlang.org/spec/declarations.html#variable-length-parameters)).
    fn varargs_element(&self, db: &dyn TyDatabase) -> Option<Ty> {
        if !self.vararg {
            return None;
        }
        let param = self.params.last().copied()?;
        Some(match param.kind(db) {
            TyKind::Array(inner) => **inner,
            _ => param,
        })
    }

    /// The type of a *call* to this member with the written argument types: its
    /// own type with the type parameters the arguments determine substituted
    /// ([KLS
    /// `type-inference.html#call-completion`](https://kotlinlang.org/spec/type-inference.html#call-completion)
    /// infers a call's type arguments, of which this is the positional
    /// approximation).
    ///
    /// It is exact for the shapes a Kotlin signature writes positionally:
    /// `fun <T> lazy(initializer: () -> T): Lazy<T>` called as `lazy { 1 }` is a
    /// `Lazy<Int>`, and `fun <T> id(value: T): T` called as `id(1)` an `Int`.
    /// A parameter whose type variable the *body* determines rather than an
    /// argument — `fun <T> empty(): List<T>` — stays a type variable, and a
    /// candidate reached with no arguments keeps its own type.
    pub fn call_ty(&self, db: &dyn TyDatabase, args: &[Ty]) -> Ty {
        let call_args: Vec<CallArg<'_>> = args
            .iter()
            .map(|ty| CallArg {
                name: None,
                ty: *ty,
                trailing: false,
            })
            .collect();
        let binding = argument_binding(db, self, &call_args);
        if binding.is_empty() {
            return self.ty(db);
        }
        self.ty(db).substitute(db, &binding)
    }
}

/// The unification of a candidate's parameter types against a call's argument
/// types, as a binding of the type variables the arguments determine.
fn argument_binding(
    db: &dyn TyDatabase,
    member: &Member,
    args: &[CallArg<'_>],
) -> rustc_hash::FxHashMap<crate::ty::TypeVarScope, Ty> {
    let mut binding = rustc_hash::FxHashMap::default();
    let Some(landing) = argument_parameters(member, args) else {
        return binding;
    };
    for (arg, index) in args.iter().zip(&landing) {
        let param = match index {
            Some(index) => member.params[*index],
            // An argument past the last parameter is a `vararg`'s, whose
            // *element* type is what it is.
            None => match member.varargs_element(db) {
                Some(element) => element,
                None => continue,
            },
        };
        unify(db, &param, &arg.ty, &mut binding);
    }
    binding
}

/// Unifies a parameter type with an argument type, recording the type variables
/// of `param` that `arg` determines: a parameter that *is* a type variable binds
/// directly, and a reference parameter binds the corresponding positions of its
/// own arguments — which is what a function type's parameter and return positions
/// are (`(Int) -> T` against `Function1<Int, Int>`).
fn unify(
    db: &dyn TyDatabase,
    param: &Ty,
    arg: &Ty,
    binding: &mut rustc_hash::FxHashMap<crate::ty::TypeVarScope, Ty>,
) {
    match (param.kind(db), arg.kind(db)) {
        (TyKind::TypeVar { scope, .. }, TyKind::Flexible { lower, .. }) => {
            unify(db, param, lower, binding);
            let _ = scope;
        }
        // A *platform* type is the pair it is, and a classfile parameter is one:
        // what it determines is what its lower half does.
        (TyKind::Flexible { lower, .. }, _) => unify(db, &lower, arg, binding),
        (_, TyKind::Flexible { lower, .. }) => unify(db, param, &lower, binding),
        (TyKind::TypeVar { scope, .. }, _) => {
            // An unresolved argument determines nothing: it is compatible with
            // every parameter ([`crate::kotlin::subtyping`]), so binding the
            // variable to it would erase the type the callee declares.
            if !matches!(arg.kind(db), TyKind::Error) {
                binding.entry(scope.clone()).or_insert(*arg);
            }
        }
        (TyKind::Reference { args: params, .. }, TyKind::Reference { args: written, .. }) => {
            for (param, arg) in params.iter().zip(written.iter()) {
                unify(db, param, arg, binding);
            }
        }
        (TyKind::Array(param), TyKind::Array(arg)) => unify(db, param, arg, binding),
        // A `vararg` parameter is an array in the classfile
        // ([JVMS §4.3.3](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.3.3))
        // while the call writes its elements one by one: `mutableListOf("a")`
        // determines `T` from `"a"` against the *element* type the array
        // parameter holds.
        (TyKind::Array(param), TyKind::Reference { .. } | TyKind::TypeVar { .. }) => {
            unify(db, param, arg, binding);
        }
        // A *projection* in the parameter position — `Iterable<? extends T>`,
        // which is how a classfile writes `T`'s use site
        // ([JLS §4.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5.1))
        // — is what it bounds.
        (TyKind::Wildcard(Some(bound)), _) => unify(db, &bound.ty, arg, binding),
        // A source-written `Array<T>` is the classifier the JVM spells `T[]`
        // (<https://kotlinlang.org/docs/java-interop.html#mapped-types>): the two
        // are one type, and either side stands for the other.
        (TyKind::Array(param), TyKind::Reference { args, .. }) => {
            if let Some(arg) = args.first() {
                unify(db, param, arg, binding);
            }
        }
        (TyKind::Reference { args, .. }, TyKind::Array(arg)) => {
            if let Some(param) = args.first() {
                unify(db, param, arg, binding);
            }
        }
        _ => {}
    }
}

/// Whether two types have the same *shape* — both arrays, or both non-arrays:
/// what the permissive arm of [`applies`] needs to keep a `vararg`'s array
/// parameter from matching a scalar argument.
fn same_shape(db: &dyn TyDatabase, arg: &Ty, param: &Ty) -> bool {
    let array = |ty: &Ty| {
        let mut ty = *ty;
        loop {
            match ty.kind(db) {
                TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => ty = *inner,
                TyKind::Flexible { lower, .. } => ty = *lower,
                _ => break,
            }
        }
        matches!(ty.kind(db), TyKind::Array(_))
    };
    array(arg) == array(param)
}

/// Whether a member is a function, a property or a constructor — the kinds a
/// Kotlin call or read selects between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Function,
    Property,
    /// A property's accessor: what a read (the getter) or a write (the setter)
    /// resolves to, Kotlin's own and the synthetic property of a Java
    /// getter/setter pair alike ([KLS
    /// `declarations.html#getters-and-setters`](https://kotlinlang.org/spec/declarations.html#getters-and-setters),
    /// <https://kotlinlang.org/docs/java-interop.html#getters-and-setters>).
    Getter,
    Setter,
    /// A constructor: what `Foo(args)` resolves to, the Kotlin item tree's
    /// constructors for a Kotlin class and the JVM's for a Java or classfile
    /// one.
    Constructor,
}

/// The declaration a call site stands in: what a *Java* member's access control
/// is checked against ([`access_context_for_kotlin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSite {
    pub file: FileId,
    pub item: hir_expand::ids::ItemId,
}

/// One written argument of a call: its type, and the parameter name it was
/// written with (`None` for a positional argument).
pub struct CallArg<'a> {
    pub name: Option<&'a str>,
    pub ty: Ty,
    /// Whether the argument is the call's *trailing* lambda — the one written
    /// after the argument list — which binds to the **last** parameter
    /// (<https://kotlinlang.org/docs/lambdas.html#passing-trailing-lambdas>).
    pub trailing: bool,
}

/// Every member `name` names on the receiver `receiver`.
///
/// The receiver's classifiers are walked from the receiver to its supertypes,
/// so an inherited member is found at the declaration it comes from; a
/// classifier receiver also carries its `companion object`'s members, which is
/// how `Point.ORIGIN` resolves, and — when `name` is the class's own simple
/// name — its constructors ([KLS
/// `declarations.html#companion-objects`](https://kotlinlang.org/spec/declarations.html#companion-objects),
/// [`#constructors`](https://kotlinlang.org/spec/declarations.html#classifier-declaration)).
///
/// A *classfile* or *Java source* receiver's members come from the Java layer
/// ([`java_members`]), which is what makes `"ab".length`, `list.size`,
/// `StringBuilder.append(x)` and `ArrayList<String>()` resolvable from Kotlin.
pub fn member_set(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    site: CallSite,
) -> Vec<Member> {
    let mut out = declared_members(db, scope, receiver, name, site);
    // An extension of the same name is a candidate only *after* every declared
    // member ([`select`] prefers the declared ones, which is KLS's own order) —
    // and only when the receiver's own classes declare none of that name, or the
    // caller says the declared ones do not apply ([`pick_callable`]).
    if out.is_empty() {
        collect_extension_members(db, scope, site, receiver, name, &mut out);
    }
    out
}

/// The members the receiver's own classes declare under `name`, inherited
/// members included — everything [`member_set`] answers without the extension
/// scopes, which are a scan of the classpath and the workspace.
pub fn declared_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    site: CallSite,
) -> Vec<Member> {
    let ctx = access_context_for_kotlin(db, site.file, site.item);
    let mut out = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    let constructors = names_the_class(db, scope, receiver, name);
    collect_members(
        db,
        scope,
        receiver,
        name,
        &ctx,
        constructors,
        &mut seen,
        &mut out,
        true,
    );
    out
}

/// The *extensions* `name` names that are in scope for the receiver — the
/// scanned half of [`member_set`], asked for only when the declared members do
/// not answer.
pub fn extension_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    site: CallSite,
) -> Vec<Member> {
    let mut out = Vec::new();
    collect_extension_members(db, scope, site, receiver, name, &mut out);
    out
}

/// The extensions `name` names that are *in scope* for a call written on
/// `receiver` ([KLS
/// `overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers)
/// resolves a call against the extensions in scope when no member of the same
/// name applies), in scope order:
///
/// 1. the extensions the enclosing classifiers declare, innermost first;
/// 2. the file's own top-level extensions;
/// 3. the top-level extensions of every package in scope
///    ([`KotlinResolver::packages_in_scope`]) — read from the workspace's symbol
///    index ([`super::db::extension_candidates`]) and confirmed against the
///    receiver type there.
///
/// A declaration that is not an extension is skipped: it is a member, and the
/// member walk already has it.
fn collect_extension_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    site: CallSite,
    receiver: &Ty,
    name: &Name,
    out: &mut Vec<Member>,
) {
    let outer = hir::file_item_tree(db, site.file);
    let Some(tree) = hir_def::kotlin::plugin::model(&outer) else {
        return;
    };
    // 1. The enclosing classifiers' own extensions, innermost first.
    let mut current = Some(site.item);
    while let Some(item) = current {
        if tree.as_class(item).is_some() {
            let resolver = KotlinResolver::for_item(db, site.file, tree, item);
            for &member in tree.data(item).body() {
                if let Some(extension) = extension_of(
                    db, scope, site.file, tree, member, name, &resolver, receiver,
                ) {
                    out.push(extension);
                }
            }
        }
        current = tree.parent_of(item);
    }
    // 2. The library's extensions: the classpath's facades carry them as static
    //    members whose first parameter is the receiver.
    if std::env::var_os("CAFFEINE_KT_NO_LIBRARY_EXT").is_none() {
        library_extension_members(db, scope, site.file, receiver, name, out);
    }
    // 3. The file's own top-level extensions.
    let resolver = KotlinResolver::for_item(db, site.file, tree, site.item);
    for &top in &tree.top {
        if let Some(extension) =
            extension_of(db, scope, site.file, tree, top, name, &resolver, receiver)
        {
            out.push(extension);
        }
    }
    // 4. Another file's: only a *source set* has a symbol index, and only a
    // source-set receiver can reach one.
    let hir::ResolutionScope::SourceSet(source_set) = scope else {
        return;
    };
    for package in resolver.packages_in_scope() {
        for (file, item) in
            super::db::extension_candidates(db, source_set.clone(), &package, name).iter()
        {
            let outer = hir::file_item_tree(db, *file);
            let Some(tree) = hir_def::kotlin::plugin::model(&outer) else {
                continue;
            };
            let resolver = KotlinResolver::for_item(db, *file, tree, *item);
            if let Some(extension) =
                extension_of(db, scope, *file, tree, *item, name, &resolver, receiver)
            {
                out.push(extension);
            }
        }
    }
}

/// The member an *extension* declaration is, when its name is `name` and the
/// receiver type it declares accepts the receiver the call is written on
/// ([KLS
/// `overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers)):
/// its parameters are the declared ones — the extension receiver is written
/// before the `.` of the declaration and is not a call parameter.
fn extension_of(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    file: FileId,
    tree: &KotlinItemTree,
    item: hir_expand::ids::ItemId,
    name: &Name,
    resolver: &KotlinResolver<'_>,
    receiver: &Ty,
) -> Option<Member> {
    let data = tree.data(item);
    if data.name() != Some(name) {
        return None;
    }
    let receiver_ref = match data {
        KotlinItemData::Function(function) => function.receiver.as_ref(),
        KotlinItemData::Property(property) => property.receiver.as_ref(),
        _ => return None,
    }?;
    let extended = super::ty::ty_from_type_ref(db, resolver, &receiver_ref.ty);
    if !super::subtyping::is_assignable(db, scope, receiver, &extended) {
        return None;
    }
    match data {
        KotlinItemData::Function(function) => {
            Some(kotlin_function_member(db, file, item, name, tree, function))
        }
        KotlinItemData::Property(property) => {
            let ty = super::db::item_ty(db, file, item);
            let declared_setter = property.accessors.iter().any(|&accessor| {
                matches!(tree.data(accessor), KotlinItemData::Accessor(data) if data.is_setter)
            });
            let writes = property.is_var && !declared_setter;
            Some(Member {
                target: MemberTarget::Kotlin { file, item },
                name: name.clone(),
                kind: match writes {
                    true => MemberKind::Setter,
                    false => MemberKind::Getter,
                },
                params: match writes {
                    true => vec![ty],
                    false => Vec::new(),
                },
                param_names: match writes {
                    true => Arc::from(vec![Name::new("value")]),
                    false => Arc::from(Vec::new()),
                },
                defaulted: match writes {
                    true => Arc::from(vec![false]),
                    false => Arc::from(Vec::new()),
                },
                vararg: false,
                extension: true,
            })
        }
        _ => None,
    }
}

/// The identity of a receiver's classifier, for the member walk: the canonical
/// name of a named one, the declaration of a *local* one — a local class or an
/// object literal's anonymous class, which has no canonical name to be keyed by
/// ([KLS
/// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)).
#[derive(Clone, PartialEq, Eq, Hash)]
enum ReceiverKey {
    Named(Name),
    Local(hir::SourceClass),
}

/// The key the member walk visits `receiver` under, if it names a classifier.
fn receiver_key(db: &dyn TyDatabase, ty: &Ty) -> Option<ReceiverKey> {
    match ty.kind(db) {
        TyKind::Reference {
            local: Some(local), ..
        } => Some(ReceiverKey::Local(local.clone())),
        TyKind::Reference { name, .. } => Some(ReceiverKey::Named(name.clone())),
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => receiver_key(db, inner),
        _ => None,
    }
}

/// The *local* classifier `ty` is, if it is one: a local class, a local `object`
/// or an object literal's anonymous class, each identified by its declaration
/// rather than by a name.
pub fn local_class_of(db: &dyn TyDatabase, ty: &Ty) -> Option<hir::SourceClass> {
    match ty.kind(db) {
        TyKind::Reference {
            local: Some(local), ..
        } => Some(local.clone()),
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => local_class_of(db, inner),
        _ => None,
    }
}

/// The members of the receiver and of every supertype of its supertype
/// closure, most-derived first, each classifier visited once.
fn collect_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    ctx: &InvocationContext,
    constructors: bool,
    seen: &mut rustc_hash::FxHashSet<ReceiverKey>,
    out: &mut Vec<Member>,
    include_companion: bool,
) {
    let Some(key) = receiver_key(db, receiver) else {
        return;
    };
    if !seen.insert(key.clone()) {
        return;
    }
    match &key {
        // A local classifier is walked from the item tree of its own file: it is
        // a declaration of that file exactly as a named one is, and the
        // Kotlin twin of the Java layer's `ClassKey::Local` member walk.
        ReceiverKey::Local(class) => {
            let tree = hir::file_item_tree(db, class.file);
            if let Some(tree) = hir_def::kotlin::plugin::model(&tree) {
                let resolver = KotlinResolver::for_item(db, class.file, tree, class.item);
                kotlin_members(
                    db,
                    tree,
                    class.file,
                    class.item,
                    receiver,
                    name,
                    &resolver,
                    constructors,
                    out,
                    include_companion,
                );
            }
        }
        ReceiverKey::Named(fqn) => {
            // A member the *language* declares on the receiver's built-in
            // classifier comes first: it has no declaration to look up, and the
            // classfile the built-in maps to has no method for it.
            let receiver_args: Vec<Ty> = match receiver.kind(db) {
                TyKind::Reference { args, .. } => args.to_vec(),
                _ => Vec::new(),
            };
            let declared = super::builtins::member_return(fqn.as_str(), name.as_str())
                .map(|ret| Ty::reference(db, ret, Vec::new()))
                .or_else(|| {
                    super::builtins::collection_property(
                        db,
                        fqn.as_str(),
                        &receiver_args,
                        name.as_str(),
                    )
                })
                .or_else(|| {
                    super::builtins::mutable_iterator(
                        db,
                        fqn.as_str(),
                        &receiver_args,
                        name.as_str(),
                    )
                });
            let declared_only = declared.is_some();
            if let Some(ret) = declared {
                out.push(Member {
                    target: MemberTarget::Builtin { ret },
                    name: name.clone(),
                    kind: if super::builtins::member_is_property(fqn.as_str(), name.as_str())
                        || super::builtins::is_collection_property(fqn.as_str(), name.as_str())
                    {
                        MemberKind::Property
                    } else {
                        MemberKind::Function
                    },
                    params: Vec::new(),
                    param_names: Arc::from(Vec::new()),
                    defaulted: Arc::from(Vec::new()),
                    vararg: false,
                    extension: false,
                });
            }
            // A member the language declares *replaces* the classfile's: the
            // JVM method of another name is a different declaration, and a
            // same-named one (`MutableSet.iterator()` against
            // `java.util.Set.iterator()`) says something else — the mutable
            // view's iterator, not the read-only one.
            if declared_only {
                return;
            }
            // A built-in Kotlin classifier has no classfile of its own — the
            // compiler maps it onto a JVM type ([`super::builtins`]) — so the
            // mapped class answers for the members a classfile *does* know
            // (`List.size` is `java.util.List.size()`, `Int.compareTo` is
            // `java.lang.Integer.compareTo`), their types converted back to
            // Kotlin's by [`member_of_method`].
            let Some(resolved) = hir::fqn_resolve(db, scope, fqn.as_str()) else {
                if let Some(jvm) = super::builtins::jvm_ty(db, *receiver) {
                    java_members(db, scope, &jvm, name, ctx, constructors, out);
                }
                return;
            };
            match &resolved {
                hir::Resolved::Source(class) => {
                    let tree = hir::file_item_tree(db, class.file);
                    match hir_def::kotlin::plugin::model(&tree) {
                        Some(tree) => {
                            let resolver =
                                KotlinResolver::for_item(db, class.file, tree, class.item);
                            kotlin_members(
                                db,
                                tree,
                                class.file,
                                class.item,
                                receiver,
                                name,
                                &resolver,
                                constructors,
                                out,
                                include_companion,
                            );
                        }
                        // A Java source class: its members are the Java layer's.
                        None => java_members(db, scope, receiver, name, ctx, constructors, out),
                    }
                }
                hir::Resolved::Library(_) => {
                    java_members(db, scope, receiver, name, ctx, constructors, out)
                }
                // A Kotlin file's facade class: Kotlin reaches a file's top-level
                // declarations by *import*, not through the facade's name, so the
                // arm contributes nothing here — the file's own top level is what
                // [`super::infer`] consults, and a caller of another language
                // reaches them through the facade, which this language's JVM view
                // answers ([`crate::kotlin::jvm_view`]).
                hir::Resolved::Facade { .. } => {}
            }
        }
    }
    // Inherited: the declared supertypes, then their own. The walk goes through
    // the Kotlin subtyping relation, which substitutes the receiver's arguments
    // into a source supertype list and converts a Java or classfile supertype
    // to the Kotlin type it denotes.
    for supertype in super::subtyping::supertypes(db, scope, receiver) {
        collect_members(db, scope, &supertype, name, ctx, false, seen, out, false);
    }
}

/// Whether `name` is the simple name of the class `receiver` denotes — the form
/// a Kotlin *constructor call* is written in (`Foo(1)`), where the candidate
/// set is the constructors rather than the members.
fn names_the_class(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
) -> bool {
    let Some(fqn) = reference_fqn(db, receiver) else {
        return false;
    };
    // A local classifier exists by construction — the receiver *is* its
    // declaration — and answers by its simple name; a named one is confirmed
    // against the classpath.
    if local_class_of(db, receiver).is_some() {
        return fqn.simple_name() == name.simple_name();
    }
    // The written name may be qualified (`a.Util`); the call writes the simple
    // name.
    // A Kotlin *spelling* of a library class — `ArrayList` for
    // `java.util.ArrayList`, `MutableList` for `java.util.List` — classifies
    // the same way its JVM name does ([`super::builtins::jvm_class`]).
    let jvm = super::builtins::jvm_class(fqn.as_str()).unwrap_or(fqn.as_str());
    hir::fqn_resolve(db, scope, jvm).is_some() && fqn.simple_name() == name.as_str()
}

/// The members of one Kotlin declaration's body, plus its companion's when
/// asked, plus its constructors when the name is the class's own.
#[allow(clippy::too_many_arguments)]
fn kotlin_members(
    db: &dyn TyDatabase,
    tree: &KotlinItemTree,
    file: FileId,
    item: hir_expand::ids::ItemId,
    receiver: &Ty,
    name: &Name,
    resolver: &KotlinResolver<'_>,
    constructors: bool,
    out: &mut Vec<Member>,
    include_companion: bool,
) {
    let _ = receiver;
    // A classifier's *primary* constructor is not one of its body members — it
    // hangs off the header — but it is what `Foo(1)` resolves to for a class
    // that declares one ([KLS
    // `declarations.html#primary-constructor`](https://kotlinlang.org/spec/declarations.html#primary-constructor)),
    // so it is a candidate beside the secondary constructors the body declares.
    let class = match tree.data(item) {
        KotlinItemData::Class(class) => Some(class),
        _ => None,
    };
    let primary = class.and_then(|class| class.primary_constructor);
    // An `enum class` declares `entries` through the compiler, not in its body:
    // `Enum.entries` is a `List` of the enum's own type ([KLS
    // `declarations.html#enum-class-declaration`](https://kotlinlang.org/spec/declarations.html#enum-class-declaration)
    // gives the declaration form; the classifier's members are the standard
    // library's, and `entries` is one of them since Kotlin 1.9).
    if name.as_str() == "entries"
        && matches!(class, Some(class) if class.kind == hir::hir_def::kotlin::item_tree::KotlinClassKind::Enum)
    {
        out.push(Member {
            target: MemberTarget::Builtin {
                ret: Ty::reference(
                    db,
                    "kotlin.collections.List",
                    vec![match reference_fqn(db, receiver) {
                        Some(fqn) => Ty::reference(db, fqn, Vec::new()),
                        None => Ty::reference(db, "kotlin.Any", Vec::new()),
                    }],
                ),
            },
            name: name.clone(),
            kind: MemberKind::Property,
            params: Vec::new(),
            param_names: Arc::from(Vec::new()),
            defaulted: Arc::from(Vec::new()),
            vararg: false,
            extension: false,
        });
    }
    for member in tree.data(item).body().iter().copied().chain(primary) {
        let data = tree.data(member);
        let member_name = data.name();
        match data {
            KotlinItemData::Function(function) if member_name == Some(name) => {
                out.push(kotlin_function_member(
                    db, file, member, name, tree, function,
                ));
            }
            // `Foo(1)`: the constructors of the class the receiver denotes.
            KotlinItemData::Constructor(constructor) if constructors => {
                out.push(Member {
                    target: MemberTarget::Kotlin { file, item: member },
                    name: name.clone(),
                    kind: MemberKind::Constructor,
                    params: constructor
                        .params
                        .iter()
                        .map(|param| super::ty::ty_from_type_ref(db, &resolver, &param.param.ty.ty))
                        .collect(),
                    param_names: constructor
                        .params
                        .iter()
                        .map(|param| param.param.name.clone())
                        .collect(),
                    defaulted: constructor
                        .defaults
                        .iter()
                        .map(|default| default.is_some())
                        .collect(),
                    vararg: constructor
                        .params
                        .last()
                        .is_some_and(|param| param.param.varargs),
                    // A constructor is a member of the class it constructs.
                    extension: false,
                });
            }
            KotlinItemData::Property(property) if member_name == Some(name) => {
                // A member's own type is its item's, written or *inferred*: a
                // `val p = 2` has no written type and is typed by its
                // initializer ([`super::db::item_ty`]).
                let ty = super::db::item_ty(db, file, member);
                // A read resolves to the getter, a write to the setter
                // ([KLS `declarations.html#getters-and-setters`]); a `val` has
                // no setter, and a `private set` one is not a member of the
                // *class* for a receiver outside it.
                let declared_setter = property.accessors.iter().any(|&accessor| {
                    matches!(tree.data(accessor), KotlinItemData::Accessor(data) if data.is_setter)
                });
                out.push(Member {
                    target: MemberTarget::Kotlin { file, item: member },
                    name: name.clone(),
                    kind: if property.is_var && !declared_setter {
                        MemberKind::Setter
                    } else {
                        MemberKind::Getter
                    },
                    params: if property.is_var && !declared_setter {
                        vec![ty]
                    } else {
                        Vec::new()
                    },
                    // A setter's parameter is the property's own `value`; a
                    // getter takes none.
                    param_names: if property.is_var && !declared_setter {
                        Arc::from(vec![Name::new("value")])
                    } else {
                        Arc::from(Vec::new())
                    },
                    defaulted: match property.is_var && !declared_setter {
                        true => Arc::from(vec![false]),
                        false => Arc::from(Vec::new()),
                    },
                    vararg: false,
                    extension: property.receiver.is_some(),
                });
            }
            // An enum entry is a *value* of its enum's type, not a classifier
            // of its own ([KLS
            // `declarations.html#enum-class-declaration`](https://kotlinlang.org/spec/declarations.html#enum-class-declaration)):
            // inside the enum, `CN` is a `Category`.
            KotlinItemData::EnumEntry(_) if member_name == Some(name) => {
                out.push(Member {
                    target: MemberTarget::Kotlin { file, item: member },
                    name: name.clone(),
                    kind: MemberKind::Getter,
                    params: Vec::new(),
                    param_names: Arc::from(Vec::new()),
                    defaulted: Arc::from(Vec::new()),
                    vararg: false,
                    extension: false,
                });
            }
            KotlinItemData::Class(class)
                if include_companion
                    && class.kind
                        == hir::hir_def::kotlin::item_tree::KotlinClassKind::CompanionObject =>
            {
                kotlin_members(
                    db,
                    tree,
                    file,
                    member,
                    receiver,
                    name,
                    resolver,
                    constructors,
                    out,
                    false,
                );
            }
            _ => {}
        }
    }
    // "If a class does not have neither primary, nor secondary constructors, it
    // is assumed to implicitly have a default parameterless primary
    // constructor." ([KLS
    // `declarations.html#constructor-declaration`](https://kotlinlang.org/spec/declarations.html#constructor-declaration))
    // The constructor has no declaration of its own, so the member's target is
    // the class it belongs to — which is also the type it constructs.
    if constructors
        && let Some(class) = class
        && class.kind == hir::hir_def::kotlin::item_tree::KotlinClassKind::Class
        && primary.is_none()
        && !class
            .body
            .iter()
            .any(|&member| matches!(tree.data(member), KotlinItemData::Constructor(_)))
    {
        out.push(Member {
            target: MemberTarget::Kotlin { file, item },
            name: name.clone(),
            kind: MemberKind::Constructor,
            params: Vec::new(),
            param_names: Arc::from(Vec::new()),
            defaulted: Arc::from(Vec::new()),
            vararg: false,
            extension: false,
        });
    }
}

/// The members `name` names on a Java or classfile receiver ([KLS
/// `overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers)
/// for the Kotlin rules that consume them; the *Java* declaration shapes come
/// from [`crate::jvm::member_set::member_set`] and are projected through
/// [`MemberTarget`]).
///
/// Three Kotlin-specific rules sit on top of the Java member set:
///
/// * a Java method's written name is a Kotlin *function* of the same name, with
///   its parameter and return types converted to the Kotlin types they denote
///   (a classfile type is a platform type);
/// * the *synthetic property* of a Java getter/setter pair is a Kotlin property
///   (<https://kotlinlang.org/docs/java-interop.html#getters-and-setters>, which
///   KLS does not cover): the `getFoo()`/`setFoo(v)` pair is the property
///   `foo`, and a `Boolean` `isFoo()` is the property `isFoo` — so `list.size`,
///   `sb.length` and `x.name = "y"` resolve. A Kotlin declaration never gets
///   the treatment: it declares the property itself;
/// * a Java field is a Kotlin *property* of its own name — Kotlin reads a Java
///   field directly.
fn java_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    ctx: &InvocationContext,
    constructors: bool,
    out: &mut Vec<Member>,
) {
    // Kotlin draws no static/instance distinction at a *class* receiver
    // (`Integer.parseInt`, `Util.INSTANCE`), and an instance receiver's calls
    // are instance members — the mode a Kotlin access site has is therefore
    // "everything the type declares", which is the Java layer's
    // `TypeQualified` (the one mode that filters nothing).
    let ctx = &ctx.with_mode(InvocationMode::TypeQualified);
    // The *properties* come first: a name read position takes the property
    // (`container.layout` is `getLayout()`'s property even where the class also
    // declares a `void layout()` method), while a *call* filters to the
    // functions anyway, so both `x.layout` and `x.layout()` resolve.
    for accessor in property_getters(name) {
        for method in
            crate::jvm::member_set::member_set(db, scope, receiver, accessor.as_str(), ctx)
        {
            // A getter takes no arguments and returns the property's type.
            let mut member = member_of_method(db, name.clone(), MemberKind::Getter, method);
            member.params = Vec::new();
            out.push(member);
        }
    }
    for accessor in property_setters(name) {
        for method in
            crate::jvm::member_set::member_set(db, scope, receiver, accessor.as_str(), ctx)
        {
            // A setter's parameter is the property's type, which
            // [`member_of_method`] converts.
            out.push(member_of_method(
                db,
                name.clone(),
                MemberKind::Setter,
                method,
            ));
        }
    }
    // A Java field is the Kotlin property of its own name — Kotlin reads a Java
    // field directly — and comes before a method of the same name.
    if let Some(field) = crate::jvm::member_set::pick_field(db, scope, receiver, name.as_str(), ctx)
    {
        out.push(Member {
            target: MemberTarget::JavaField(Box::new(field)),
            name: name.clone(),
            kind: MemberKind::Property,
            params: Vec::new(),
            param_names: Arc::from(Vec::new()),
            defaulted: Arc::from(Vec::new()),
            vararg: false,
            extension: false,
        });
    }
    for method in crate::jvm::member_set::member_set(db, scope, receiver, name.as_str(), ctx) {
        out.push(member_of_method(
            db,
            name.clone(),
            MemberKind::Function,
            method.clone(),
        ));
        // The method is *also* the property of its own name when it takes no
        // arguments: kotlinc 2.4.20 accepts `x.size` for a `Collection.size()`
        // and `sb.length` for a `StringBuilder.length()`. The link between a
        // standard-library property and its JVM accessor lives in the library's
        // `@Metadata`, which this model does not decode, so the method is
        // exposed under both names instead — permissive where kotlinc reports
        // `function invocation 'toString()' expected.` for a method the library
        // declares as a function, never a false `unresolved reference` for the
        // properties it does declare.
        if method.params.is_empty() && !method.is_static {
            out.push(member_of_method(
                db,
                name.clone(),
                MemberKind::Getter,
                method,
            ));
        }
    }
    if constructors {
        // The Java `new` path's own naming ([`crate::java::infer::new_expr`]):
        // a classfile constructor is `<init>`, a *source* one is a method named
        // after its class.
        let constructor_name = match hir::fqn_resolve(
            db,
            scope,
            reference_fqn(db, receiver)
                .as_ref()
                .map(|fqn| fqn.as_str())
                .unwrap_or_default(),
        ) {
            Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
            _ => name.simple_name().to_owned(),
        };
        for method in
            crate::jvm::member_set::member_set(db, scope, receiver, &constructor_name, ctx)
        {
            out.push(member_of_method(
                db,
                name.clone(),
                MemberKind::Constructor,
                method,
            ));
        }
    }
}

/// One Java method as a Kotlin member: its parameter types are converted to the
/// Kotlin types they denote — a classfile type is a *platform* type — and its
/// return type is converted by [`Member::ty`].
fn member_of_method(
    db: &dyn TyDatabase,
    name: Name,
    kind: MemberKind,
    method: MethodData,
) -> Member {
    Member {
        params: method
            .params
            .iter()
            .map(|param| ty_from_java(db, *param))
            .collect(),
        // A classfile records no parameter names without a `MethodParameters`
        // attribute and no default values at all: a Java member answers with the
        // names it carries and all-`false` defaults.
        param_names: method
            .param_names
            .as_ref()
            .map(|names| names.iter().map(|name| Name::new(name)).collect::<Vec<_>>())
            .unwrap_or_default()
            .into(),
        defaulted: vec![false; method.params.len()].into(),
        vararg: method.varargs,
        target: MemberTarget::Java(Box::new(method)),
        name,
        kind,
        // A Java member is never an extension: the static-extension convention
        // (<https://kotlinlang.org/docs/java-interop.html#static-methods>) is
        // the *library* half's, which reads the first parameter as the receiver.
        extension: false,
    }
}

/// One Kotlin function declaration as a member, with the parameter names a named
/// argument is matched against and the defaults a call may omit.
fn kotlin_function_member(
    db: &dyn TyDatabase,
    file: FileId,
    item: hir_expand::ids::ItemId,
    name: &Name,
    tree: &KotlinItemTree,
    function: &hir::hir_def::kotlin::item_tree::FunctionData,
) -> Member {
    // The parameter types are resolved in the *declaring item's* scope: a
    // function may declare type parameters of its own
    // (`private fun <T> update(value: T, setter: (T) -> Unit)`), and the class's
    // scope does not carry them.
    let resolver = KotlinResolver::for_item(db, file, tree, item);
    Member {
        target: MemberTarget::Kotlin { file, item },
        name: name.clone(),
        kind: MemberKind::Function,
        params: function
            .params
            .iter()
            .map(|param| super::ty::ty_from_type_ref(db, &resolver, &param.param.ty.ty))
            .collect(),
        param_names: function
            .params
            .iter()
            .map(|param| param.param.name.clone())
            .collect(),
        defaulted: function
            .defaults
            .iter()
            .map(|default| default.is_some())
            .collect(),
        vararg: function
            .params
            .last()
            .is_some_and(|param| param.param.varargs),
        extension: function.receiver.is_some(),
    }
}

/// The Java getters a Kotlin property name stands for
/// (<https://kotlinlang.org/docs/java-interop.html#getters-and-setters>): the
/// `get`-prefixed form with the name capitalized, and — for a name that is
/// itself `is` + an uppercase letter, which is how Kotlin keeps a `Boolean
/// isFoo()`'s name — the name itself. kotlinc 2.4.20 reads
/// `j.isDragEnabled` for an `isDragEnabled()`/`setDragEnabled` pair and
/// `j.dragEnabled` for a `getDragEnabled()`/`setDragEnabled` one.
fn property_getters(name: &Name) -> Vec<String> {
    let mut out = vec![format!("get{}", capitalize(name.as_str()))];
    if is_prefix_name(name.as_str()) {
        out.push(name.to_string());
    }
    out
}

/// The Java setters a Kotlin property name stands for: `set` followed by the
/// name capitalized — and for the `is`-named property `isFoo`, `set` followed
/// by the name without its `is`, which is how Kotlin writes a `setFoo(v)`.
fn property_setters(name: &Name) -> Vec<String> {
    let base = match is_prefix_name(name.as_str()) {
        true => &name.as_str()[2..],
        false => name.as_str(),
    };
    vec![format!("set{}", capitalize(base))]
}

/// Whether a Kotlin property name is the `is`-prefixed form Kotlin keeps from a
/// `Boolean isFoo()`: `is` followed by an uppercase letter.
fn is_prefix_name(name: &str) -> bool {
    name.strip_prefix("is")
        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_uppercase))
}

/// `name` with its first character uppercased — the inverse of the JavaBeans
/// decapitalization the compiler applies to `getFoo`/`setFoo`.
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The access-control context of a Kotlin call site, for a *Java* member
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)):
/// the enclosing class is the innermost Kotlin classifier the item is nested
/// in, the package the Kotlin file's, and `subclass_of` the enclosing
/// classifier's first source or library supertype — a Kotlin class extending a
/// Java class may reach its `protected` members exactly as a Java subclass can.
/// Kotlin's own visibility ([KLS
/// `declarations.html#visibility`](https://kotlinlang.org/spec/declarations.html#visibility))
/// is not a JLS §6.6 question and is checked separately.
pub fn access_context_for_kotlin(
    db: &dyn TyDatabase,
    file: FileId,
    item: hir_expand::ids::ItemId,
) -> InvocationContext {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        // A call site of another language — a Java file, or a `.kts` script no
        // language lowered: the context is the classfile-shaped one the entry
        // that reads classfiles derives.
        return crate::lang::classfile().access_context(db, file, item);
    };
    let package = tree
        .package
        .as_ref()
        .map(|package| package.as_str().to_owned());
    // The innermost enclosing classifier, and its first declared supertype —
    // the class a `protected` member is accessed through from a subclass.
    let mut enclosing = None;
    let mut current = Some(item);
    while let Some(id) = current {
        if tree.as_class(id).is_some() {
            enclosing = Some(id);
            break;
        }
        current = tree.parent_of(id);
    }
    let (enclosing_class, subclass_of) = match enclosing {
        Some(class) => {
            let key =
                hir::source_class_fqn(db, file, class).map(crate::jvm::member::ClassKey::Named);
            let subclass = super::db::supertypes(db, file, class)
                .first()
                .and_then(|supertype| match supertype.kind(db) {
                    TyKind::Reference { name, .. } => {
                        Some(crate::jvm::member::ClassKey::Named(name.clone()))
                    }
                    _ => None,
                });
            (key, subclass)
        }
        None => (None, None),
    };
    InvocationContext {
        mode: InvocationMode::Virtual,
        enclosing_class,
        package,
        subclass_of,
    }
}

/// The callable a call written *without a receiver* selects among the library's
/// top-level declarations ([KLS
/// `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)):
/// a Kotlin library compiles a file's top-level callables into the `<File>Kt`
/// facade class of its package
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>), so
/// the candidates are those facades' *static members* named `name` — a static
/// field or a static getter being the property form, exactly as a Java static
/// member is.
///
/// The packages searched are the ones in scope
/// ([`KotlinResolver::packages_in_scope`]), in their order, and the facades of a
/// package are memoized ([`super::db::facade_classes`]).
pub fn library_top_level_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    file: FileId,
    name: &Name,
    args: &[CallArg<'_>],
) -> Option<Member> {
    let tree = hir::file_item_tree(db, file);
    let tree = hir_def::kotlin::plugin::model(&tree)?;
    // The resolver only needs an item for the *declaring* context of the names
    // it resolves; a file-level package list is the same for every item of the
    // file, and the file's first item is one.
    let Some(&item) = tree.top.first() else {
        return None;
    };
    let resolver = KotlinResolver::for_item(db, file, tree, item);
    let mut out = Vec::new();
    for package in resolver.packages_in_scope() {
        facade_members(db, scope, &package, name, &mut out);
    }
    select(db, scope, out, args)
}

/// The members `name` names on the facade classes of one package, as *members of
/// the facade class* — the Kotlin form of a top-level declaration of that
/// package. Memoized per (scope, package, name) by the query.
fn facade_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    package: &Name,
    name: &Name,
    out: &mut Vec<Member>,
) {
    for facade in facades_declaring(db, scope, package, name) {
        let fqn = facade.fqn(db).as_name().clone();
        let ty = Ty::reference(db, fqn, Vec::new());
        for member in facade_class_members(db, scope, &ty, name) {
            // A *top-level* property is read with no arguments: the getter's
            // parameter is the receiver an extension property declares, and a
            // top-level one has none.
            let mut member = member;
            if member.kind == MemberKind::Getter {
                member.params = Vec::new();
                member.param_names = Arc::from(Vec::new());
                member.defaulted = Arc::from(Vec::new());
            }
            out.push(member);
        }
    }
}

/// The facades of `package` that declare a static member named `name`, or an
/// accessor a Kotlin *property* read stands for (`getFoo`/`isFoo` for `foo`)
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>).
///
/// The package's facades are dozens of classes and a file asks for hundreds of
/// names, so the *names* they declare are indexed once
/// ([`super::db::facade_name_index`]) and a lookup reads the one or two that
/// could answer.
fn facades_declaring(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    package: &Name,
    name: &Name,
) -> Vec<hir::Resolved> {
    let index = super::db::facade_name_index(db, scope, package);
    let mut out = Vec::new();
    for candidate in std::iter::once(name.as_str().to_owned()).chain(property_getters(name)) {
        if let Some(facades) = index.get(&Name::new(&candidate)) {
            for facade in facades {
                if !out.contains(facade) {
                    out.push(facade.clone());
                }
            }
        }
    }
    out
}

/// The members `name` names on one facade class, in the Kotlin shapes a
/// top-level declaration has: a static method is a *function*, a static getter
/// its property, and a static field a property of its own name.
pub(crate) fn facade_class_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    facade: &Ty,
    name: &Name,
) -> Vec<Member> {
    // Kotlin draws no static/instance distinction at a class receiver, and the
    // facade is reached by its *class*, so the mode that filters nothing is the
    // one a Kotlin access site has.
    let ctx = InvocationContext {
        mode: InvocationMode::TypeQualified,
        enclosing_class: None,
        package: None,
        subclass_of: None,
    };
    let mut out = Vec::new();
    for accessor in property_getters(name) {
        for method in
            crate::jvm::member_set::member_set_ignoring_access(db, scope, facade, &accessor, &ctx)
        {
            if !method.is_static {
                continue;
            }
            // The parameters stay on the member: a getter's first parameter is
            // the *receiver* of a Kotlin extension property (`val <T : Any>
            // T.javaClass: Class<T>` compiles to `getJavaClass(Object)`), which
            // [`as_extension`] is what strips. A *top-level* property read takes
            // the getter without arguments ([`facade_members`] clears them).
            out.push(member_of_method(
                db,
                name.clone(),
                MemberKind::Getter,
                method,
            ));
        }
    }
    if let Some(field) =
        crate::jvm::member_set::pick_field_ignoring_access(db, scope, facade, name.as_str(), &ctx)
    {
        out.push(Member {
            target: MemberTarget::JavaField(Box::new(field)),
            name: name.clone(),
            kind: MemberKind::Property,
            params: Vec::new(),
            param_names: Arc::from(Vec::new()),
            defaulted: Arc::from(Vec::new()),
            vararg: false,
            extension: false,
        });
    }
    for method in
        crate::jvm::member_set::member_set_ignoring_access(db, scope, facade, name.as_str(), &ctx)
    {
        if !method.is_static {
            continue;
        }
        let mut member = member_of_method(db, name.clone(), MemberKind::Function, method.clone());
        // A Kotlin *library*'s default parameter values live in the `@Metadata`
        // annotation, which this model does not decode, and the compiler's
        // `$default` overload carries them as a bit mask the classfile reader
        // would have to interpret. A facade's parameters are therefore read as
        // all-defaulted: `joinToString(",", … ) { it }` omits five of the six
        // parameters between its separator and its transform, and kotlinc
        // accepts the same call.
        member.defaulted = vec![true; member.params.len()].into();
        out.push(member);
    }
    out
}

/// The *extension* members a library declares for `name` and a receiver
/// (`"a".isBlank()`): the Kotlin compiler compiles `fun String.isBlank()` into a
/// static method of the file's facade whose **first parameter is the receiver**
/// (<https://kotlinlang.org/docs/java-interop.html#static-methods>), and a
/// static `getFoo(first)` is the extension-property form.
///
/// The first parameter is *not* a call parameter: it is the receiver the call is
/// written on, so the member's `params` are the declared parameters after it.
pub fn library_extension_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    file: FileId,
    receiver: &Ty,
    name: &Name,
    out: &mut Vec<Member>,
) {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return;
    };
    let Some(&item) = tree.top.first() else {
        return;
    };
    let resolver = KotlinResolver::for_item(db, file, tree, item);
    for package in resolver.packages_in_scope() {
        for facade in facades_declaring(db, scope, &package, name) {
            let fqn = facade.fqn(db).as_name().clone();
            let ty = Ty::reference(db, fqn, Vec::new());
            for member in facade_class_members(db, scope, &ty, name) {
                if let Some(member) = as_extension(db, scope, receiver, member) {
                    out.push(member);
                }
            }
        }
    }
}

/// The extension form of a facade member: one whose **first parameter** accepts
/// the receiver becomes an extension whose own parameters are the rest
/// (<https://kotlinlang.org/docs/java-interop.html#static-methods>). `None` for
/// a member whose first parameter the receiver does not satisfy — it is a
/// top-level declaration, not an extension of this receiver.
fn as_extension(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    member: Member,
) -> Option<Member> {
    let first = *member.params.first()?;
    // The declared receiver type is a *parameter* for the purposes of the
    // declaration's type parameters: `fun <T> Array<T>.forEach(action: (T) -> Unit)`
    // compiles to `forEach(T[] array, Function1<? super T, Unit>)`, and the
    // receiver's element type is what `T` is — the positional unification the
    // call's own type arguments are bound by ([`Member::call_ty`]), applied to
    // the receiver position.
    let mut binding = rustc_hash::FxHashMap::default();
    unify(db, &first, receiver, &mut binding);
    let first = first.substitute(db, &binding);
    // The unification *is* the check when it determined anything: the receiver
    // position was a type variable (`T`, `Array<T>`), and what it stands for is
    // what the receiver satisfies. Otherwise the position is a concrete class
    // and the receiver has to be assignable to it.
    if binding.is_empty() && !super::subtyping::is_assignable(db, scope, receiver, &first) {
        return None;
    }
    let mut member = member;
    member.params = member
        .params
        .iter()
        .skip(1)
        .map(|param| param.substitute(db, &binding))
        .collect();
    if !member.param_names.is_empty() {
        let names: Vec<Name> = member.param_names.iter().skip(1).cloned().collect();
        member.param_names = names.into();
    }
    if !member.defaulted.is_empty() {
        let defaults: Vec<bool> = member.defaulted.iter().skip(1).copied().collect();
        member.defaulted = defaults.into();
    }
    // The *return* type carries the same binding — `T` is the element type, not
    // a type variable — which the member's own type is read from.
    if let MemberTarget::Java(method) = &mut member.target {
        method.ret = method.ret.substitute(db, &binding);
    }
    member.extension = true;
    Some(member)
}

/// The callable a Kotlin *operator convention* resolves to on `receiver`
/// ([KLS
/// `operator-overloading.html`](https://kotlinlang.org/spec/operator-overloading.html)
/// names the function each operator is spelled as): a thin [`pick_callable`]
/// for the convention's name at the operator's arity — `arg` is the right-hand
/// operand, `None` for the operators that write none (`iterator`, `componentN`).
pub fn pick_operator_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    arg: Option<Ty>,
    site: CallSite,
) -> Option<Member> {
    let args: Vec<CallArg<'_>> = match arg {
        Some(arg) => vec![CallArg {
            name: None,
            ty: arg,
            trailing: false,
        }],
        None => Vec::new(),
    };
    pick_callable(db, scope, receiver, name, &args, site)
}

/// The function a call `name(args)` selects on `receiver`, or `None` when no
/// candidate applies.
pub fn pick_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    args: &[CallArg<'_>],
    site: CallSite,
) -> Option<Member> {
    // The declared members are tried first — a member beats an extension of the
    // same name ([KLS
    // `overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers))
    // — and the extension scopes are scanned only when none of them applies.
    let declared = declared_members(db, scope, receiver, name, site);
    if let Some(member) = select(db, scope, declared, args) {
        return Some(member);
    }
    let extensions = extension_members(db, scope, receiver, name, site);
    select(db, scope, extensions, args)
}

/// The callable a call `name(args)` selects among the file's *top-level*
/// declarations — the implicit receivers of an unqualified call that names
/// neither a local nor a member of an enclosing classifier ([KLS
/// `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)).
pub fn top_level_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    file: FileId,
    name: &Name,
    args: &[CallArg<'_>],
) -> Option<Member> {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return None;
    };
    let mut candidates = Vec::new();
    for &top in &tree.top {
        if let Some(member) = declaration_callable(db, scope, file, top, name, args) {
            candidates.push(member);
        }
    }
    select(db, scope, candidates, args)
}

/// The callable a *top-level* declaration is, when its name is `name` and the
/// arguments apply — [`top_level_callable`]'s twin for a declaration a caller
/// has already found, which is what an *imported* name resolves to.
pub fn declaration_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    file: FileId,
    item: hir_expand::ids::ItemId,
    name: &Name,
    args: &[CallArg<'_>],
) -> Option<Member> {
    let tree = hir::file_item_tree(db, file);
    let tree = hir_def::kotlin::plugin::model(&tree)?;
    let KotlinItemData::Function(function) = tree.data(item) else {
        return None;
    };
    if function.name != *name {
        return None;
    }
    let member = kotlin_function_member(db, file, item, name, tree, function);
    applies(db, scope, &member, args).then_some(member)
}

/// The most specific of the applicable candidates ([KLS
/// `overload-resolution.html#choosing-the-most-specific-candidate-from-the-overload-candidate-set`](https://kotlinlang.org/spec/overload-resolution.html#choosing-the-most-specific-candidate-from-the-overload-candidate-set)),
/// or `None` when none applies.
fn select(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    candidates: Vec<Member>,
    args: &[CallArg<'_>],
) -> Option<Member> {
    let mut declared = Vec::new();
    let mut extensions = Vec::new();
    for member in candidates {
        if !matches!(member.kind, MemberKind::Function | MemberKind::Constructor)
            || !applies(db, scope, &member, args)
        {
            continue;
        }
        // KLS resolves a *member* before an extension of the same name
        // ([`overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers)),
        // so the two sets are selected separately and the extensions are only
        // reached when no member applies.
        match member.extension {
            true => extensions.push(member),
            false => declared.push(member),
        }
    }
    let mut applicable = match declared.is_empty() {
        true => extensions,
        false => declared,
    };
    if applicable.is_empty() {
        return None;
    }
    // Most specific: the first candidate whose parameters every *other*
    // applicable candidate's arguments are assignable to — a plain
    // subtype-comparison, with declaration order breaking ties.
    let picked = applicable
        .iter()
        .position(|member| {
            applicable.iter().all(|other| {
                member.params.len() == other.params.len()
                    && member
                        .params
                        .iter()
                        .zip(&other.params)
                        .all(|(a, b)| crate::kotlin::subtyping::is_assignable(db, scope, b, a))
            })
        })
        .unwrap_or(0);
    Some(applicable.swap_remove(picked))
}
/// Whether a candidate accepts `args`
/// ([KLS `overload-resolution.html#determining-function-applicability-for-a-specific-call`](https://kotlinlang.org/spec/overload-resolution.html#determining-function-applicability-for-a-specific-call)):
/// arity with defaults and `vararg` filled, named arguments matched by
/// parameter name, then assignability.
fn applies(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    member: &Member,
    args: &[CallArg<'_>],
) -> bool {
    let Some(landing) = argument_parameters(member, args) else {
        // A written name that matches no parameter: the candidate does not
        // apply ([KLS
        // `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
        return false;
    };
    let mut filled = vec![false; member.params.len()];
    for (arg, index) in args.iter().zip(&landing) {
        let Some(index) = *index else {
            // Past the last parameter: only a `vararg` absorbs it, as its
            // element type — which the classfile cannot tell apart from the
            // array type it compiles to, so the array type is accepted too.
            let Some(param) = member.varargs_element(db) else {
                return false;
            };
            if !crate::kotlin::subtyping::is_assignable(db, scope, &arg.ty, &param) {
                return false;
            }
            continue;
        };
        if filled[index] {
            // The same parameter twice: a named argument a positional one
            // already took, or a name written twice.
            return false;
        }
        filled[index] = true;
        // A parameter whose type mentions a variable this model cannot bind is
        // accepted for any argument — but its *shape* must still correspond: a
        // `T[]` parameter is the array signature of a `vararg`, and matching it
        // against a scalar argument (`"a" + "b"` against `Array<T> plus(Array<T>,
        // Array<out T>)`) would let an inapplicable candidate answer the call.
        if !crate::kotlin::subtyping::is_assignable(db, scope, &arg.ty, &member.params[index])
            && !(contains_type_var(db, &member.params[index])
                && same_shape(db, &arg.ty, &member.params[index]))
        {
            return false;
        }
    }
    // Every parameter the call leaves unfilled must declare a default
    // ([KLS
    // `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters));
    // a `vararg` may be empty.
    let last = member.params.len().saturating_sub(1);
    filled.iter().enumerate().all(|(index, filled)| {
        *filled
            || member.defaulted.get(index).copied().unwrap_or(false)
            || (member.vararg && index == last)
    })
}

/// Which parameter each written argument lands on: by *name* when it writes one,
/// and by position otherwise, from the first parameter no earlier argument took
/// ([KLS
/// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
///
/// `None` for the whole call when a written name matches no parameter; `None` for
/// one argument when it lands past the last parameter, which only a `vararg`
/// accepts.
fn argument_parameters(member: &Member, args: &[CallArg<'_>]) -> Option<Vec<Option<usize>>> {
    let mut filled = vec![false; member.params.len()];
    let mut landing = Vec::with_capacity(args.len());
    for arg in args {
        let index = match arg.name {
            Some(written) => match member
                .param_names
                .iter()
                .position(|name| name.as_str() == written)
            {
                Some(index) => Some(index),
                None => return None,
            },
            // A trailing lambda is the **last** parameter's argument, whatever
            // the written arguments left unfilled
            // (<https://kotlinlang.org/docs/lambdas.html#passing-trailing-lambdas>).
            None if arg.trailing => member.params.len().checked_sub(1),
            None => filled.iter().position(|filled| !*filled),
        };
        match index {
            Some(index) if index < member.params.len() => {
                filled[index] = true;
                landing.push(Some(index));
            }
            _ => landing.push(None),
        }
    }
    Some(landing)
}

/// Whether a type mentions a type variable of its declaration.
///
/// A parameter whose type the *call* determines — `fun <T> update(newValue: T,
/// setter: (T) -> Unit)`, where the argument is what `T` is — accepts any
/// argument: the call's type arguments are inferred from the arguments ([KLS
/// `type-inference.html#call-completion`](https://kotlinlang.org/spec/type-inference.html#call-completion)),
/// and this model's inference is the unification
/// [`Member::call_ty`] reads the call's own type with. Requiring assignability
/// here would reject every generic call whose type argument comes from a
/// parameter, which is most of them.
pub fn contains_type_var(db: &dyn TyDatabase, ty: &Ty) -> bool {
    match ty.kind(db) {
        TyKind::TypeVar { .. } => true,
        TyKind::Reference { args, .. } => args.iter().any(|arg| contains_type_var(db, arg)),
        TyKind::Array(inner) => contains_type_var(db, &inner),
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => contains_type_var(db, &inner),
        TyKind::Wildcard(Some(bound)) => contains_type_var(db, &bound.ty),
        _ => false,
    }
}

/// The canonical name of a reference type, for a classpath lookup.
fn reference_fqn(db: &dyn TyDatabase, ty: &Ty) -> Option<Name> {
    match ty.kind(db) {
        TyKind::Reference { name, .. } => Some(name.clone()),
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => reference_fqn(db, inner),
        _ => None,
    }
}
