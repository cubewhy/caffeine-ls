//! The Kotlin type queries.
//!
//! Every type of a Kotlin declaration is computed by a salsa query keyed on the
//! interned `(file, item)` pair ([`KotlinItemKey`]), so it is memoized per
//! declaration and invalidated exactly when the file changes — the same shape
//! the Java layer uses ([`crate::jvm::db::ItemKey`]). A `.kts` script's body is
//! the one exception: no declaration owns it, so its query is keyed on the
//! *file* ([`script_body_types`]).
//!
//! Type *parameters* are not a query: they are derived by walking the item
//! tree's `parent` chain ([`KotlinResolver::for_item`]), which is a bounded
//! walk over the item's own ancestry with no interning to save.

use triomphe::Arc;
use vfs::FileId;

use hir::hir_def::kotlin::item_tree::{KotlinItemData, KotlinItemTree};
use hir_expand::ids::ItemId;
use hir_expand::name::Name;

use super::resolve::KotlinResolver;
use super::ty::ty_from_type_ref;
use crate::jvm::db::TyDatabase;
use crate::ty::{Ty, TyKind};

/// A workspace-unique Kotlin item id. Interned so it can key tracked queries;
/// the underlying [`FileId`]/[`ItemId`] pair is `Copy`, so `#[returns(copy)]`
/// keeps the accessors cheap.
#[salsa::interned]
pub struct KotlinItemKey {
    #[returns(copy)]
    pub file: FileId,
    #[returns(copy)]
    pub item: ItemId,
}

/// Which of a declaration's type queries an in-flight entry stands for. Each is
/// guarded separately, because the three legitimately nest: the item-type query
/// *is* what asks for the initializer types of the declaration it types.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InFlight {
    ItemType,
    Body,
    Initializer,
    /// The body of a `.kts` script, which no item owns: guarded per *file*
    /// (`item` is `None`).
    ScriptBody,
}

// The `(file, item)` types currently being computed on this thread, innermost
// last — the guard that keeps a *self-referential* declaration out of a salsa
// dependency cycle, which salsa 0.28.2 panics on rather than recovering.
//
// A declaration without a written type is typed by its body, and the body's
// inference resolves the names it writes, which asks for the type of the
// declaration they name: `val x = x`, `val a = b` with `val b = a`, and
// `fun f() = f()` each close that edge into a cycle. The guard lives in the
// *accessors* rather than in the query bodies because salsa notices the cycle
// before it re-enters a body: by the time `kotlin_item_ty_query` could answer
// the error type, the re-entrant fetch has already panicked. A re-entrant
// accessor answers the error type (or an empty result) instead.
thread_local! {
    static IN_FLIGHT: std::cell::RefCell<Vec<(FileId, Option<ItemId>, InFlight)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` for `(file, item)` under the in-flight guard of `query`, or `None`
/// when that query is already computing the same declaration on this thread.
/// `item` is `None` for a body no declaration owns — a `.kts` script's implicit
/// `main` — which is guarded by the file alone.
fn guarded<R>(
    file: FileId,
    item: Option<ItemId>,
    query: InFlight,
    f: impl FnOnce() -> R,
) -> Option<R> {
    let in_flight = IN_FLIGHT.with(|stack| {
        stack
            .borrow()
            .iter()
            .any(|&(f, i, q)| f == file && i == item && q == query)
    });
    if in_flight {
        return None;
    }
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            IN_FLIGHT.with(|stack| {
                stack.borrow_mut().pop();
            });
        }
    }
    IN_FLIGHT.with(|stack| stack.borrow_mut().push((file, item, query)));
    let _scope = Scope;
    Some(f())
}

/// The declared type of a Kotlin item, memoized per `(file, item)`.
///
/// A property: its declared type. A function or accessor: its declared return
/// type, or `kotlin.Unit` for a block-bodied one that declares none
/// ([KLS
/// `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)):
/// a block body returns `Unit` unless it writes a type. A classifier or an
/// object literal: the type of the class it declares. A constructor: its
/// class's type. A type alias: the type it aliases. An enum entry: its enum's
/// type. An `init` block: `Unit`.
///
/// A declaration without a declared type has an *inferred* one, which the body
/// inference produces ([`crate::kotlin::infer`]) — a property's from its
/// initializer or its delegate, a function's from an expression body.
#[salsa::tracked]
pub(crate) fn kotlin_item_ty_query<'db>(db: &'db dyn TyDatabase, key: KotlinItemKey<'db>) -> Ty {
    let file_id = key.file(db);
    let item_id = key.item(db);
    let tree = hir::file_item_tree(db, file_id);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return Ty::error(db);
    };
    let resolver = KotlinResolver::for_item(db, file_id, tree, item_id);
    let reference = |name: &str| match resolver.class_fqn(name) {
        Some(fqn) => Ty::reference(db, fqn, Vec::new()),
        None => Ty::error(db),
    };
    match tree.data(item_id) {
        KotlinItemData::Property(data) => {
            let ty = match &data.ty {
                Some(ty) => ty_from_type_ref(db, &resolver, &ty.ty),
                None => inferred_property_ty(db, file_id, item_id, tree, &resolver, data),
            };
            if data
                .modifiers
                .flags
                .contains(hir_def::kotlin::modifiers::KotlinModifierFlags::VARARG)
            {
                super::ty::vararg_array_ty(db, ty)
            } else {
                ty
            }
        }
        KotlinItemData::Function(data) => match &data.ret {
            Some(ret) => ty_from_type_ref(db, &resolver, &ret.ty),
            // No written return type: an *expression* body is the type that
            // expression has, and every other body (a block, or none at all —
            // an interface member) returns `kotlin.Unit` ([KLS
            // `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)).
            None if data.expression_body => body_value_ty(db, file_id, item_id),
            None => unit(db, &resolver),
        },
        KotlinItemData::Accessor(data) => match &data.params.first() {
            // A setter's own type is `Unit`; a getter's is the property's,
            // which the property item resolves.
            _ if data.is_setter => unit(db, &resolver),
            _ => match tree.parent_of(item_id) {
                Some(property) => item_ty(db, file_id, property),
                None => Ty::error(db),
            },
        },
        KotlinItemData::Class(data) => {
            // A *local* classifier — a local class or `object`, and an object
            // literal's anonymous class — has a simple name but no canonical
            // one ([KLS
            // `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)),
            // so it is identified by the declaration it is, exactly as a Java
            // local class is ([JLS §6.7]).
            if tree.is_local_type(item_id) {
                Ty::local_reference(
                    db,
                    hir::SourceClass {
                        file: file_id,
                        item: item_id,
                    },
                    data.name.clone(),
                    Vec::new(),
                )
            } else {
                reference(data.name.as_str())
            }
        }
        KotlinItemData::Constructor(_) => match tree.parent_of(item_id) {
            Some(class) => item_ty(db, file_id, class),
            None => Ty::error(db),
        },
        KotlinItemData::EnumEntry(_) => match tree.parent_of(item_id) {
            Some(class) => item_ty(db, file_id, class),
            None => Ty::error(db),
        },
        KotlinItemData::TypeAlias(data) => ty_from_type_ref(db, &resolver, &data.target.ty),
        KotlinItemData::AnonymousInitializer(_) => unit(db, &resolver),
    }
}

/// The declared supertypes of a classifier, memoized per `(file, item)`: its
/// delegation specifiers, or `kotlin.Any` when it declares none ([KLS
/// `declarations.html#class-declaration`](https://kotlinlang.org/spec/declarations.html#class-declaration)).
#[salsa::tracked(returns(clone))]
pub(crate) fn kotlin_supertypes_query<'db>(
    db: &'db dyn TyDatabase,
    key: KotlinItemKey<'db>,
) -> Arc<[Ty]> {
    let file_id = key.file(db);
    let item_id = key.item(db);
    let tree = hir::file_item_tree(db, file_id);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return Arc::from(Vec::new());
    };
    let KotlinItemData::Class(class) = tree.data(item_id) else {
        return Arc::from(Vec::new());
    };
    let resolver = KotlinResolver::for_item(db, file_id, tree, item_id);
    Arc::from(resolver.super_types(class))
}

/// The declared type parameters of a classifier, function, property or type
/// alias, in declaration order, each as a type variable, memoized per
/// `(file, item)`.
#[salsa::tracked(returns(clone))]
pub(crate) fn kotlin_type_params_query<'db>(
    db: &'db dyn TyDatabase,
    key: KotlinItemKey<'db>,
) -> Arc<[Ty]> {
    let file_id = key.file(db);
    let item_id = key.item(db);
    let tree = hir::file_item_tree(db, file_id);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return Arc::from(Vec::new());
    };
    let resolver = KotlinResolver::for_item(db, file_id, tree, item_id);
    Arc::from(
        resolver
            .declared_type_params()
            .into_iter()
            .map(|param| resolver.type_var(item_id, &param))
            .collect::<Vec<_>>(),
    )
}

/// `kotlin.Unit`, the type a Kotlin declaration returns when it declares none.
fn unit(db: &dyn TyDatabase, resolver: &KotlinResolver<'_>) -> Ty {
    match resolver.class_fqn("Unit") {
        Some(fqn) => Ty::reference(db, fqn, Vec::new()),
        // Without a classpath the name cannot be resolved: an error type is
        // honest about the missing library, and the caller renders `<error>`.
        None => Ty::error(db),
    }
}

/// The type of a property that declares none: the type of its initializer, of
/// its delegate through the delegated-property rule, or of its getter's
/// expression body — a property writes at most one of the three
/// ([KLS
/// `declarations.html#property-declaration`](https://kotlinlang.org/spec/declarations.html#property-declaration),
/// [`#delegated-property-declaration`](https://kotlinlang.org/spec/declarations.html#delegated-property-declaration)).
fn inferred_property_ty(
    db: &dyn TyDatabase,
    file_id: FileId,
    item_id: ItemId,
    tree: &KotlinItemTree,
    resolver: &KotlinResolver<'_>,
    data: &hir_def::kotlin::item_tree::PropertyData,
) -> Ty {
    if let Some(expr) = data.initializer_expr {
        return initializer_types(db, file_id, item_id).expr_ty(db, expr);
    }
    if let Some(expr) = data.delegate_expr {
        let delegate = initializer_types(db, file_id, item_id).expr_ty(db, expr);
        // The `thisRef` a `getValue` receives: the property's owner — the class
        // it is a member of — or, for a top-level property, `null`.
        let owner = match tree.parent_of(item_id) {
            Some(class) => item_ty(db, file_id, class),
            None => Ty::null(db),
        };
        return delegated_value_ty(db, file_id, Some(item_id), resolver, owner, delegate);
    }
    // `val p get() = expr`: the getter's expression body is the property's type.
    for &accessor in &data.accessors {
        if let KotlinItemData::Accessor(accessor_data) = tree.data(accessor)
            && !accessor_data.is_setter
            && accessor_data.expression_body
        {
            return body_value_ty(db, file_id, accessor);
        }
    }
    Ty::error(db)
}

/// The type a property's `by` delegate declares for it — the *delegated
/// property* rule, which KLS does not cover and the compiler documents at
/// <https://kotlinlang.org/docs/delegated-properties.html>:
///
/// * the standard library's `kotlin.Lazy<T>` contributes its type argument, so
///   `val x by lazy { 1 }` is an `Int` — `Lazy` is compared through
///   [`KotlinResolver::class_fqn`], so a `typealias` of it and the mapped name
///   both work;
/// * any other delegate contributes the return type of the `getValue` operator
///   it declares for the property's owner as `thisRef` — the second parameter,
///   `KProperty<*>`, has no type in this model, so the error type stands in for
///   it, which is assignable to it ([`crate::kotlin::subtyping`]).
pub(crate) fn delegated_value_ty(
    db: &dyn TyDatabase,
    file_id: FileId,
    item_id: Option<ItemId>,
    resolver: &KotlinResolver<'_>,
    owner: Ty,
    delegate: Ty,
) -> Ty {
    if let TyKind::Reference { name, args, .. } = delegate.kind(db)
        && let Some(lazy_name) = resolver.class_fqn("Lazy")
        && crate::kotlin::ty::mapped_type_name(name)
            == crate::kotlin::ty::mapped_type_name(&lazy_name)
    {
        return args.first().copied().unwrap_or_else(|| Ty::error(db));
    }
    let args = [
        crate::kotlin::method::CallArg {
            name: None,
            ty: owner,
            trailing: false,
        },
        crate::kotlin::method::CallArg {
            name: None,
            ty: Ty::error(db),
            trailing: false,
        },
    ];
    let site = crate::kotlin::method::CallSite {
        file: file_id,
        item: item_id,
        // A delegated property's `getValue` runs on the *delegate expression*,
        // a value.
        receiver: crate::kotlin::method::ReceiverKind::Value,
    };
    match crate::kotlin::method::pick_callable(
        db,
        resolver.scope(),
        &delegate,
        &Name::new("getValue"),
        &args,
        site,
    ) {
        Some(member) => member.ty(db),
        None => Ty::error(db),
    }
}

/// The type an *expression-bodied* declaration infers: the type of the single
/// expression its body holds
/// ([KLS
/// `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)
/// infers the return type from it). A body the lowering produced no expression
/// for — an erroneous tree — is the error type. Requires the item to be
/// `expression_body`.
fn body_value_ty(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Ty {
    let types = body_types(db, file_id, item_id);
    let Some(body) = types.body else {
        return Ty::error(db);
    };
    let bodies = hir::file_body_tree(db, file_id);
    let Some(&stmt) = bodies.body(body).stmts.first() else {
        return Ty::error(db);
    };
    match bodies.stmt(stmt) {
        hir_expand::body::StmtData::Expr(expr) => types.expr_ty(db, *expr),
        _ => Ty::error(db),
    }
}

/// The declared type of a Kotlin item, memoized per `(file, item)`. See
/// [`kotlin_item_ty_query`].
pub fn item_ty(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Ty {
    guarded(file_id, Some(item_id), InFlight::ItemType, || {
        *kotlin_item_ty_query(db, KotlinItemKey::new(db, file_id, item_id))
    })
    .unwrap_or_else(|| Ty::error(db))
}

/// The declared supertypes of a Kotlin classifier, memoized per `(file, item)`.
/// See [`kotlin_supertypes_query`].
pub fn supertypes(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Arc<[Ty]> {
    kotlin_supertypes_query(db, KotlinItemKey::new(db, file_id, item_id))
}

/// The declared type parameters of a Kotlin declaration, memoized per
/// `(file, item)`. See [`kotlin_type_params_query`].
pub fn type_params(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Arc<[Ty]> {
    kotlin_type_params_query(db, KotlinItemKey::new(db, file_id, item_id))
}

/// One package's facade scan within a scope: the memo key of
/// [`kotlin_facade_classes_query`].
#[salsa::interned(debug)]
pub struct FacadeKey<'db> {
    /// The scope whose classpath is scanned.
    #[returns(ref)]
    pub scope: hir::ResolutionScope,
    #[returns(ref)]
    pub package: Name,
}

/// The *Kotlin file facades* of one package: the classes the compiler emits for
/// the package's top-level declarations
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>).
///
/// A Kotlin library compiles a file's top-level callables into a class of the
/// package named after the file — `CollectionsKt`, or whatever `@file:JvmName`
/// says — and marks it with `@Metadata(k = 2)`; a *multi-file* facade (`k = 4` or
/// `5`) holds several files' declarations. A class whose simple name ends in `Kt`
/// is a facade as well, so one whose metadata the stub reader dropped still
/// resolves. The scan is bounded by one package's classes and memoized per
/// (scope, package), so it runs once per revision.
#[salsa::tracked(returns(clone))]
pub(crate) fn kotlin_facade_classes_query<'db>(
    db: &'db dyn TyDatabase,
    key: FacadeKey<'db>,
) -> Arc<[hir::Resolved]> {
    let scope = key.scope(db).clone();
    let package = key.package(db).clone();
    Arc::from(
        hir::classes_in_package(db, &scope, &package)
            .into_iter()
            .filter(|class| is_kotlin_facade(db, class))
            .collect::<Vec<_>>(),
    )
}

/// Whether a class of a package is one the Kotlin compiler synthesizes for the
/// package's top-level declarations: a class of the package itself — not the
/// nested type of a class declaration — whose simple name ends in `Kt` or that
/// carries a `@Metadata` annotation whose `k` is a file (`2`) or multi-file
/// (`4`, `5`) facade
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>).
///
/// The name rule is the compiler's own: a facade is named after the file it
/// holds — `CollectionsKt` — or after the `@file:JvmName` the file writes. The
/// metadata check is what accepts a facade whose name the file chose freely;
/// this model reads the annotation's *presence* only, since the rest of the
/// payload is not decoded ([`crate::kotlin::ty`] records the same deviation).
fn is_kotlin_facade(db: &dyn TyDatabase, class: &hir::Resolved) -> bool {
    let hir::Resolved::Library(entry) = class else {
        // A *source* file's facades are reached by name through the source
        // symbol index ([`super::resolve::KotlinResolver::source_declaration`]),
        // and a facade is not a declaration of its own there.
        return false;
    };
    let interner = &db.hir_state().interner;
    let fqn = interner.resolve(&entry.entry.fqn);
    if fqn.contains('$') || fqn.contains('/') {
        return false;
    }
    fqn.rsplit('.')
        .next()
        .is_some_and(|simple| simple.ends_with("Kt"))
        || class_is_kotlin_facade_metadata(db, entry)
}

/// Whether a class's `@Metadata` annotation marks it a *file facade*: its `k`
/// argument is `2` (a file's top-level declarations), `4` or `5` (a multi-file
/// facade of several files')
/// (<https://kotlinlang.org/docs/java-interop.html#package-level-functions>).
///
/// `k = 1` is an ordinary class, which every Kotlin *declaration* compiles to —
/// which is why the annotation alone is not enough.
fn class_is_kotlin_facade_metadata(db: &dyn TyDatabase, entry: &hir::ResolvedClass) -> bool {
    let interner = &db.hir_state().interner;
    let Some(record) = hir::class_record(db, entry) else {
        return false;
    };
    let hir::ClassOrModuleRecord::Class(class) = &*record else {
        return false;
    };
    class.annotations.iter().any(|annotation| {
        annotation
            .annotation_type
            .as_reference_name()
            .is_some_and(|name| interner.resolve(name) == "kotlin.Metadata")
            && annotation.arguments.iter().any(|(name, value)| {
                interner.resolve(name) == "k"
                    && matches!(
                        value,
                        syntax::stub::AnnotationValue::Primitive(
                            syntax::stub::PrimitiveValue::Int(2 | 4 | 5)
                        )
                    )
            })
    })
}

/// The facades of one package that declare a static member of each *name*: the
/// index that keeps a top-level lookup off the member table of every facade of
/// the package.
///
/// A package's facades are dozens of classes (`kotlin.collections` alone holds
/// one per standard-library file) and a Kotlin file asks for hundreds of names,
/// so reading each facade's members per name is quadratic in the package's size;
/// the index is built once per (scope, package) and answers a name with the one
/// or two facades that could declare it.
#[salsa::tracked(returns(clone))]
pub(crate) fn kotlin_facade_name_index_query<'db>(
    db: &'db dyn TyDatabase,
    key: FacadeKey<'db>,
) -> Arc<rustc_hash::FxHashMap<Name, Vec<hir::Resolved>>> {
    let scope = key.scope(db).clone();
    let package = key.package(db).clone();
    let mut index: rustc_hash::FxHashMap<Name, Vec<hir::Resolved>> = Default::default();
    for facade in facade_classes(db, &scope, &package).iter() {
        let hir::Resolved::Library(facade) = facade else {
            continue;
        };
        let Some(record) = hir::class_record(db, facade) else {
            continue;
        };
        let hir::ClassOrModuleRecord::Class(class) = &*record else {
            continue;
        };
        let interner = &db.hir_state().interner;
        for method in &class.methods {
            // A *static* method is a top-level declaration of the file the
            // facade holds; an instance method is the facade's own (there is
            // none the compiler emits).
            if !hir_def::jvm::access::JvmAccessFlags::from_bits_retain(method.flags).is_static() {
                continue;
            }
            index
                .entry(Name::new(interner.resolve(&method.name)))
                .or_default()
                .push(hir::Resolved::Library(facade.clone()));
        }
        for field in &class.fields {
            if !hir_def::jvm::access::JvmAccessFlags::from_bits_retain(field.flags).is_static() {
                continue;
            }
            index
                .entry(Name::new(interner.resolve(&field.name)))
                .or_default()
                .push(hir::Resolved::Library(facade.clone()));
        }
    }
    Arc::new(index)
}

/// The facades of `package` that declare a static member named `name`, from
/// [`kotlin_facade_name_index_query`].
pub fn facade_name_index(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    package: &Name,
) -> Arc<rustc_hash::FxHashMap<Name, Vec<hir::Resolved>>> {
    kotlin_facade_name_index_query(db, FacadeKey::new(db, scope.clone(), package.clone()))
}

/// The Kotlin file facades of `package` on `scope`'s classpath. See
/// [`kotlin_facade_classes_query`].
pub fn facade_classes(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    package: &Name,
) -> Arc<[hir::Resolved]> {
    kotlin_facade_classes_query(db, FacadeKey::new(db, scope.clone(), package.clone()))
}

/// The workspace declarations named `name` of one package, as the *extension*
/// candidates a receiver's member set looks through: the `Function` and
/// `Property` symbols whose fully qualified name is `<package>.<name>`.
///
/// Tracked per (source set, package, name), so a star import of a package holding
/// a hundred extensions is scanned once per revision rather than once per call
/// site — the extension lookup is the one name resolution that has no name to
/// index by ([KLS
/// `overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers)
/// resolves a call against the extensions *in scope*).
#[salsa::tracked(returns(clone))]
pub(crate) fn kotlin_extension_candidates_query<'db>(
    db: &'db dyn TyDatabase,
    source_set: hir::SourceSetId,
    package: Name,
    name: Name,
) -> Arc<[(FileId, ItemId)]> {
    let fqn = Name::new(&format!("{package}.{name}"));
    let symbols = hir::source_set_fqn_symbols(db, source_set, &package, &fqn);
    Arc::from(
        symbols
            .iter()
            .filter(|reference| {
                matches!(
                    reference.symbol.kind,
                    hir::SourceSymbolKind::Function | hir::SourceSymbolKind::Property
                )
            })
            .map(|reference| (reference.file, reference.symbol.item))
            .collect::<Vec<_>>(),
    )
}

/// The extension declarations named `name` in `package`, from the workspace's
/// source symbol index. See [`kotlin_extension_candidates_query`].
pub fn extension_candidates(
    db: &dyn TyDatabase,
    source_set: hir::SourceSetId,
    package: &Name,
    name: &Name,
) -> Arc<[(FileId, ItemId)]> {
    kotlin_extension_candidates_query(db, source_set, package.clone(), name.clone())
}

/// The inferred types of a declaration's body, memoized per `(file, item)`.
///
/// SAFETY: the value holds `Ty` (interned handles) and `KotlinTypeError`s over
/// `rowan::TextRange`s — no database-lifetime references — so salsa may retain
/// it across revisions.
#[salsa::tracked(returns(clone))]
pub(crate) fn kotlin_body_types_query<'db>(
    db: &'db dyn TyDatabase,
    key: KotlinItemKey<'db>,
) -> Arc<super::infer::KotlinBodyTypes> {
    let file_id = key.file(db);
    let item_id = key.item(db);
    Arc::new(super::infer::infer_item(db, file_id, item_id))
}

/// The inferred types of a declaration's body, memoized per `(file, item)`.
pub fn body_types(
    db: &dyn TyDatabase,
    file_id: FileId,
    item_id: ItemId,
) -> Arc<super::infer::KotlinBodyTypes> {
    guarded(file_id, Some(item_id), InFlight::Body, || {
        kotlin_body_types_query(db, KotlinItemKey::new(db, file_id, item_id))
    })
    .unwrap_or_else(|| Arc::new(super::infer::KotlinBodyTypes::default()))
}

/// The inferred types of a `.kts` script's body — the body of the implicit
/// `main` no declaration owns
/// ([`hir_def::kotlin::item_tree::KotlinItemTree::script_body`]) — memoized per
/// *file*, which is the only key such a body has
/// ([`super::infer::infer_script_body`]).
///
/// SAFETY: as [`kotlin_body_types_query`].
#[salsa::tracked(returns(clone))]
pub(crate) fn kotlin_script_body_types_query<'db>(
    db: &'db dyn TyDatabase,
    file: base_db::FileText,
) -> Arc<super::infer::KotlinBodyTypes> {
    Arc::new(super::infer::infer_script_body(db, *file.file_id(db)))
}

/// The inferred types of a `.kts` script's body — the file's own answer for the
/// body no declaration owns. Empty for a `.kt` file, which has no implicit
/// `main`.
pub fn script_body_types(
    db: &dyn TyDatabase,
    file_id: FileId,
) -> Arc<super::infer::KotlinBodyTypes> {
    guarded(file_id, None, InFlight::ScriptBody, || {
        kotlin_script_body_types_query(db, db.file_text(file_id))
    })
    .unwrap_or_else(|| Arc::new(super::infer::KotlinBodyTypes::default()))
}

/// The inferred types of a declaration's initializer expressions, memoized per
/// `(file, item)`.
///
/// SAFETY: as [`kotlin_body_types_query`] — `Ty` handles and
/// `KotlinTypeError`s over `TextRange`s, with no database-lifetime references.
#[salsa::tracked(returns(clone))]
pub(crate) fn kotlin_initializer_types_query<'db>(
    db: &'db dyn TyDatabase,
    key: KotlinItemKey<'db>,
) -> Arc<super::infer::KotlinBodyTypes> {
    let file_id = key.file(db);
    let item_id = key.item(db);
    Arc::new(super::infer::infer_initializer(db, file_id, item_id))
}

/// The inferred types of a declaration's initializer expressions — what a
/// property without a written type is typed by
/// ([`kotlin_initializer_types_query`]).
pub fn initializer_types(
    db: &dyn TyDatabase,
    file_id: FileId,
    item_id: ItemId,
) -> Arc<super::infer::KotlinBodyTypes> {
    guarded(file_id, Some(item_id), InFlight::Initializer, || {
        kotlin_initializer_types_query(db, KotlinItemKey::new(db, file_id, item_id))
    })
    .unwrap_or_else(|| Arc::new(super::infer::KotlinBodyTypes::default()))
}

/// The inferred types of a declaration: its body's when it has one, else its
/// initializer expressions' — a property or a function declares one or the
/// other, never both, so exactly one of the two queries answers.
///
/// The consumers that walk a *body* — navigation, inlay hints, the
/// unresolved-reference report — read this rather than
/// [`body_types`], so a property initializer's names are navigable and diagnosed
/// exactly as a function body's are.
pub fn declaration_types(
    db: &dyn TyDatabase,
    file_id: FileId,
    item_id: ItemId,
) -> Arc<super::infer::KotlinBodyTypes> {
    let tree = hir::file_item_tree(db, file_id);
    let has_body = hir_def::kotlin::plugin::model(&tree)
        .is_some_and(|tree| tree.data(item_id).body_id().is_some());
    match has_body {
        true => body_types(db, file_id, item_id),
        false => initializer_types(db, file_id, item_id),
    }
}
