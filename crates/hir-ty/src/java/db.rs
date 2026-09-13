//! Salsa glue of the type database.
//!
//! [`TyDatabase`] extends [`hir::HirDatabase`] with everything the type layer
//! needs. Heavy per-item work — type resolution, method parameter lowering,
//! body inference and the access-control context of a call site — is memoized
//! as tracked queries keyed on the interned [`ItemKey`], so repeated lookups of
//! the same item (the IDE pattern) hit the query cache instead of re-walking
//! the item tree. The member set of a name
//! ([`crate::java::method::member_set_query`]) is likewise memoized per interned
//! (scope, receiver, name, context), and the subtype/supertype walks
//! ([`crate::java::subtyping`]) per interned pair. The type parameters in scope of
//! every item of a file ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3))
//! are computed once per file in [`type_params_map_query`].

use triomphe::Arc;

use base_db::{FileText, salsa};
use hir_def::java::item_tree::{ItemData, ItemId};
use hir_expand::name::Name;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::TypeRef;
use vfs::FileId;

use crate::{
    java::method::{InvocationContext, InvocationMode},
    java::resolve::{self, Resolver, item_data, resolve_type_ref, scope_for_file},
    java::ty::Ty,
};

/// The type database: [`hir::HirDatabase`] plus the type-system API of this
/// crate. Concrete databases (e.g. `ide-db`'s `RootDatabase`) implement this
/// and salsa's `#[salsa::db]` machinery wires up the tracked queries.
#[salsa::db]
pub trait TyDatabase: hir::HirDatabase {}

/// A workspace-unique item id. Interned so it can key tracked queries; the
/// underlying [`FileId`]/[`ItemId`] pair is `Copy`, so `#[returns(copy)]` keeps
/// the accessors cheap.
#[salsa::interned]
pub struct ItemKey {
    #[returns(copy)]
    pub file: FileId,
    #[returns(copy)]
    pub item: ItemId,
}

/// The set of libraries a type query may see: the interned analogue of
/// [`hir::ResolutionScope`]. Interned (rather than passed as a plain value) so
/// it can key the memoized subtype/supertype queries in [`crate::java::subtyping`].
#[salsa::interned(unsafe(no_lifetime), debug, revisions = usize::MAX)]
pub struct ScopeId {
    pub kind: ScopeKind,
}

/// The interned form of [`hir::ResolutionScope`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// A workspace source set: its ordered classpath.
    SourceSet(hir::SourceSetId),
    /// An explicit, ordered library list (tests / synthetic scopes).
    Classpath(Vec<hir::LibraryId>),
    /// Only the JDK built-ins (jimage / rt.jar).
    JdkBuiltins,
}

impl ScopeKind {
    /// The interning data for a [`hir::ResolutionScope`].
    pub fn from_scope(scope: &hir::ResolutionScope) -> Self {
        match scope {
            hir::ResolutionScope::SourceSet(source_set) => ScopeKind::SourceSet(source_set.clone()),
            hir::ResolutionScope::Classpath(libraries) => ScopeKind::Classpath(libraries.clone()),
            hir::ResolutionScope::JdkBuiltins => ScopeKind::JdkBuiltins,
        }
    }

    /// The [`hir::ResolutionScope`] this kind was interned from.
    pub fn to_scope(&self) -> hir::ResolutionScope {
        match self {
            ScopeKind::SourceSet(source_set) => hir::ResolutionScope::SourceSet(source_set.clone()),
            ScopeKind::Classpath(libraries) => hir::ResolutionScope::Classpath(libraries.clone()),
            ScopeKind::JdkBuiltins => hir::ResolutionScope::JdkBuiltins,
        }
    }
}

/// The invocation context of a method or field access, interned so it can key
/// the memoized member-set query
/// ([`crate::java::method::member_set_query`]): the invocation mode
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
/// plus the access-control context
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
/// of the access site. Interned (rather than passed as a plain value) so it can
/// key the memoized member-set query.
#[salsa::interned(unsafe(no_lifetime), debug, revisions = usize::MAX)]
pub struct ContextKey {
    pub mode: InvocationMode,
    /// The class or interface the access appears in
    /// ([`crate::java::method::ClassKey`]: a *local* declaration is its own
    /// class, so the key round-trips it).
    pub enclosing_class: Option<crate::java::method::ClassKey>,
    pub package: Option<Name>,
    pub subclass_of: Option<crate::java::method::ClassKey>,
}

impl ContextKey {
    /// The interning data of an [`InvocationContext`].
    pub fn from_invocation(db: &dyn TyDatabase, ctx: &InvocationContext) -> ContextKey {
        ContextKey::new(
            db,
            ctx.mode,
            ctx.enclosing_class.clone(),
            ctx.package.as_deref().map(Name::new),
            ctx.subclass_of.clone(),
        )
    }
}

/// The type parameters in scope at every item of `file`
/// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)),
/// computed in a single tree walk per file and memoized. Invalidated together
/// with the file's item tree when the file text changes.
#[salsa::tracked(returns(ref))]
pub(crate) fn type_params_map_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashMap<ItemId, Vec<resolve::ScopedTypeParam>>> {
    let file_id = *file.file_id(db);
    let tree = hir::file_item_tree(db, file_id);
    Arc::new(resolve::type_params_map(&tree, file_id))
}

/// The local declarations in scope at every item of `file`
/// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3),
/// [§6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4.1)),
/// computed in a single walk of the file's declaration tree and body IR and
/// memoized. Invalidated together with the file's item tree when the file text
/// changes.
#[salsa::tracked(returns(ref))]
pub(crate) fn local_decl_sites_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashMap<ItemId, resolve::LocalDeclSite>> {
    let file_id = *file.file_id(db);
    let tree = hir::file_item_tree(db, file_id);
    let bodies = hir::file_body_tree(db, file_id);
    Arc::new(resolve::local_decl_sites(&tree, &bodies, file_id))
}

/// Whether a declaration is deprecated, and whether it or any enclosing
/// declaration is ([JLS §9.6.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.6)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeprecationInfo {
    /// The declaration's own `@Deprecated`, when it carries one.
    pub own: Option<crate::java::deprecation::Deprecation>,
    /// `own` or, failing that, the innermost enclosing declaration's — the
    /// deprecation that exempts the body from ordinary deprecation warnings.
    pub enclosing: Option<crate::java::deprecation::Deprecation>,
}

/// For every item of `file`: whether the declaration itself is deprecated, and
/// whether it or any enclosing declaration is ([JLS §9.6.4.6]). Computed in a
/// single tree walk per file and memoized; invalidated together with the
/// file's item tree when the file text changes.
#[salsa::tracked(returns(ref))]
pub(crate) fn deprecated_enclosing_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashMap<ItemId, DeprecationInfo>> {
    let file_id = *file.file_id(db);
    let tree = hir::file_item_tree(db, file_id);
    let scope = scope_for_file(db, file_id);
    let mut map: FxHashMap<ItemId, DeprecationInfo> = FxHashMap::default();
    fn walk(
        db: &dyn TyDatabase,
        file_id: FileId,
        scope: &hir::ResolutionScope,
        tree: &hir_def::java::item_tree::ItemTree,
        id: ItemId,
        inherited: Option<crate::java::deprecation::Deprecation>,
        map: &mut FxHashMap<ItemId, DeprecationInfo>,
    ) {
        let resolver = Resolver::for_item(db, file_id, tree, id);
        let own = crate::java::deprecation::annotation_deprecation(
            db,
            scope,
            &resolver,
            crate::java::deprecation::item_annotations(tree.data(id)),
        );
        let enclosing = own.or(inherited);
        map.insert(id, DeprecationInfo { own, enclosing });
        for &child in tree.data(id).body() {
            walk(db, file_id, scope, tree, child, enclosing, map);
        }
        for local in tree.local_types_of(id) {
            walk(db, file_id, scope, tree, local, enclosing, map);
        }
    }
    for &top in &tree.top {
        walk(db, file_id, &scope, &tree, top, None, &mut map);
    }
    Arc::new(map)
}

/// The nearest enclosing class or interface declaration of every item of
/// `file` ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)),
/// as a [`ClassKey`](crate::java::method::ClassKey).
/// Class-like items map to themselves; non-class items map to the type whose
/// member they are — a *local* declaration maps to itself like any other, so
/// the access control of its members sees it as their class. Items outside any
/// class (imports, module-info) are absent. Computed in a single tree walk per
/// file and memoized; invalidated together with the file's item tree when the
/// file text changes.
#[salsa::tracked(returns(ref))]
pub(crate) fn enclosing_class_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashMap<ItemId, crate::java::method::ClassKey>> {
    let file_id = *file.file_id(db);
    let tree = hir::file_item_tree(db, file_id);
    let mut map: FxHashMap<ItemId, crate::java::method::ClassKey> = FxHashMap::default();
    fn walk(
        tree: &hir_def::java::item_tree::ItemTree,
        file_id: FileId,
        id: ItemId,
        enclosing: Option<ItemId>,
        map: &mut FxHashMap<ItemId, crate::java::method::ClassKey>,
    ) {
        let data = tree.data(id);
        let is_type = data.is_type();
        let current = if is_type { Some(id) } else { enclosing };
        if let Some(enclosing) = current {
            map.insert(
                id,
                crate::java::method::ClassKey::of(tree, file_id, enclosing),
            );
        }
        for &child in data.body() {
            walk(tree, file_id, child, current, map);
        }
        // A local class-like declaration ([JLS §14.3]) is not a member of
        // anything — its own members' enclosing class is it.
        for local in tree.local_types_of(id) {
            walk(tree, file_id, local, current, map);
        }
    }
    for &top in &tree.top {
        walk(&tree, file_id, top, None, &mut map);
    }
    Arc::new(map)
}

/// The declared type of `item` in `file`, memoized per (file, item). The
/// resolution scope is derived from the file ([`scope_for_file`]); see
/// [`resolve::item_ty`].
#[salsa::tracked(returns(clone))]
pub(crate) fn item_ty_query<'db>(db: &'db dyn TyDatabase, key: ItemKey<'db>) -> Ty {
    let file_id = key.file(db);
    let item_id = key.item(db);
    let tree = hir::file_item_tree(db, file_id);
    let Some(data) = item_data(&tree, item_id) else {
        return Ty::error(db);
    };
    let scope = scope_for_file(db, file_id);
    let resolver = Resolver::for_item(db, file_id, &tree, item_id);
    let reference = |name: &Name| {
        let tyref = TypeRef::Reference {
            name: name.clone(),
            generic_args: Vec::new(),
        };
        resolve_type_ref(db, &scope, &resolver, &tyref)
    };
    match data {
        ItemData::Field(field) => resolve_type_ref(db, &scope, &resolver, &field.ty),
        ItemData::Method(method) => match &method.sig.ret {
            Some(ret) => resolve_type_ref(db, &scope, &resolver, ret),
            None => Ty::error(db), // constructors have no declared return type
        },
        ItemData::Class(data) | ItemData::Interface(data) => reference(&data.name),
        ItemData::Enum(data) => reference(&data.name),
        ItemData::Record(data) => reference(&data.name),
        ItemData::Annotation(data) => reference(&data.name),
        _ => Ty::error(db),
    }
}

/// The parameter types of a method or constructor of `item` in `file`,
/// memoized per (file, item). See [`resolve::method_params`].
#[salsa::tracked(returns(clone))]
pub(crate) fn method_params_query<'db>(db: &'db dyn TyDatabase, key: ItemKey<'db>) -> Vec<Ty> {
    let file_id = key.file(db);
    let item_id = key.item(db);
    let tree = hir::file_item_tree(db, file_id);
    let Some(data) = item_data(&tree, item_id) else {
        return Vec::new();
    };
    let scope = scope_for_file(db, file_id);
    let resolver = Resolver::for_item(db, file_id, &tree, item_id);
    match data {
        ItemData::Method(method) => method
            .sig
            .params
            .iter()
            .map(|param| resolve_type_ref(db, &scope, &resolver, &param.ty))
            .collect(),
        _ => Vec::new(),
    }
}

/// The types of the record components of `item` in `file`, memoized per
/// (file, item). See [`resolve::record_component_types`].
#[salsa::tracked(returns(clone))]
pub(crate) fn record_component_types_query<'db>(
    db: &'db dyn TyDatabase,
    key: ItemKey<'db>,
) -> Vec<Ty> {
    let file_id = key.file(db);
    let item_id = key.item(db);
    let tree = hir::file_item_tree(db, file_id);
    let Some(data) = item_data(&tree, item_id) else {
        return Vec::new();
    };
    let scope = scope_for_file(db, file_id);
    let resolver = Resolver::for_item(db, file_id, &tree, item_id);
    let ItemData::Record(record) = data else {
        return Vec::new();
    };
    // Each component's *element* type, in declaration order. Like a varargs
    // parameter ([§8.4.1], cf. [`method_params_query`]), a varargs component
    // resolves to its element type; the IDE renders the accessor's array form
    // and the canonical constructor's ellipsis form from it.
    record
        .components
        .iter()
        .map(|component| resolve_type_ref(db, &scope, &resolver, &component.ty))
        .collect()
}
/// The inferred types of the expressions and locals of the body of `item` in
/// `file` ([JLS §15], [§14.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.4)),
/// memoized per (file, item). See [`crate::java::infer::body_types_impl`].
#[salsa::tracked(returns(clone))]
pub(crate) fn body_types_query<'db>(
    db: &'db dyn TyDatabase,
    key: ItemKey<'db>,
) -> Option<Arc<crate::java::infer::BodyTypes>> {
    let file_id = key.file(db);
    let item_id = key.item(db);
    crate::java::infer::body_types_impl(db, file_id, item_id).map(Arc::new)
}

/// The declaration-level diagnostics
/// ([JLS §8.4.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.3),
/// [§9.4.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.3))
/// of every class in `file`, memoized per file. See
/// [`crate::java::decl_check::class_diagnostics_impl`].
#[salsa::tracked(returns(clone))]
pub(crate) fn class_diagnostics_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Vec<crate::java::decl_check::DeclDiagnostic> {
    let file_id = *file.file_id(db);
    crate::java::decl_check::class_diagnostics_impl(db, file_id)
}

/// The module-directive diagnostics of `file`'s `module-info.java`
/// ([JLS §7.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7)),
/// memoized per file. See
/// [`crate::java::decl_check::module_diagnostics_impl`].
#[salsa::tracked(returns(clone))]
pub(crate) fn module_diagnostics_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Vec<crate::java::decl_check::DeclDiagnostic> {
    let file_id = *file.file_id(db);
    crate::java::decl_check::module_diagnostics_impl(db, file_id)
}

/// The source-level diagnostics of `file`: every construct gated on a Java
/// source level newer than the one `file`'s source set declares. Memoized per
/// file; reading the level through [`hir::language_level_for_file`] makes the
/// memo invalidate on a workspace reload at a different level. See
/// [`crate::java::level_check::level_diagnostics_impl`].
#[salsa::tracked(returns(clone))]
pub(crate) fn level_diagnostics_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Vec<crate::java::decl_check::DeclDiagnostic> {
    let file_id = *file.file_id(db);
    crate::java::level_check::level_diagnostics_impl(db, file_id)
}

/// The workspace source files whose declarations `file`'s type outputs resolve
/// against — the exact cross-file dependency set of `file`. Tracked on the
/// interned [`FileText`], so a text edit to `file` invalidates exactly its
/// dependency set and leaves every other file's memo untouched. See
/// [`crate::java::dep_index::file_resolved_deps_impl`].
#[salsa::tracked(returns(ref))]
pub(crate) fn file_resolved_deps_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashSet<FileId>> {
    let file_id = *file.file_id(db);
    Arc::new(crate::java::dep_index::file_resolved_deps_impl(db, file_id))
}

/// The workspace source files whose declarations `file`'s type outputs
/// resolve against.
pub fn file_resolved_deps(db: &dyn TyDatabase, file_id: FileId) -> Arc<FxHashSet<FileId>> {
    let deps = file_resolved_deps_query(db, db.file_text(file_id));
    deps.clone()
}

/// The resolution-relevant *names* of `file` — the sound name-level fallback
/// of the cross-file dependency index. See
/// [`crate::java::dep_index::file_dependency_refs_impl`].
#[salsa::tracked(returns(ref))]
pub(crate) fn file_dependency_refs_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashSet<Name>> {
    let file_id = *file.file_id(db);
    Arc::new(crate::java::dep_index::file_dependency_refs_impl(
        db, file_id,
    ))
}

/// The resolution-relevant names of `file`.
pub fn file_dependency_refs(db: &dyn TyDatabase, file_id: FileId) -> Arc<FxHashSet<Name>> {
    let refs = file_dependency_refs_query(db, db.file_text(file_id));
    refs.clone()
}

/// The access-control context
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
/// of a source access site inside the method or field `item` of `file`,
/// memoized per (file, item): the canonical fully qualified name of the
/// nearest enclosing class or interface ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1))
/// and the compilation unit's package ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)),
/// with the unnamed package ([§7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
/// as `""`. A virtual invocation
/// ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
/// is assumed; the per-call-site mode
/// ([`crate::java::method::InvocationContext::with_mode`]) refines it. See
/// [`crate::java::method::access_context`].
#[salsa::tracked(returns(copy))]
pub(crate) fn access_context_key_query<'db>(
    db: &'db dyn TyDatabase,
    key: ItemKey<'db>,
) -> ContextKey {
    let file = key.file(db);
    let item = key.item(db);
    let enclosing_class = enclosing_class_query(db, db.file_text(file))
        .get(&item)
        .cloned();
    // The compilation unit's package ([§6.6.1]); the unnamed package
    // ([§7.4.2]) is `""`, so a `None` context package is never permissive.
    let package = Some(
        hir::file_item_tree(db, file)
            .package
            .clone()
            .unwrap_or_else(|| Name::new("")),
    );
    ContextKey::new(db, InvocationMode::Virtual, enclosing_class, package, None)
}
