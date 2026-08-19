//! Salsa glue of the type database.
//!
//! [`TyDatabase`] extends [`hir::HirDatabase`] with everything the type layer
//! needs. Heavy per-item work — type resolution and method parameter lowering —
//! is memoized as tracked queries keyed on the interned [`ItemKey`], so
//! repeated lookups of the same item (the IDE pattern) hit the query cache
//! instead of re-walking the item tree. The type parameters in scope of every
//! item of a file ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3))
//! are computed once per file in [`type_params_map_query`].

use std::sync::Arc;

use base_db::{FileText, salsa};
use hir_expand::{
    item_tree::{ItemData, ItemId},
    name::Name,
};
use rustc_hash::FxHashMap;
use syntax::stub::{TypeParameter, TypeRef};
use vfs::FileId;

use crate::{
    resolve::{self, Resolver, item_data, resolve_type_ref, scope_for_file},
    ty::Ty,
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
/// it can key the memoized subtype/supertype queries in [`crate::subtyping`].
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

/// The type parameters in scope at every item of `file`
/// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)),
/// computed in a single tree walk per file and memoized. Invalidated together
/// with the file's item tree when the file text changes.
#[salsa::tracked(returns(ref))]
pub(crate) fn type_params_map_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashMap<ItemId, Vec<TypeParameter<Name>>>> {
    let file_id = *file.file_id(db);
    let tree = hir::file_item_tree(db, file_id);
    Arc::new(resolve::type_params_map(&tree))
}

/// The canonical fully qualified name
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7))
/// of the nearest enclosing class or interface declaration of every item of
/// `file` ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
/// Class-like items map to themselves; non-class items map to the type whose
/// member they are. Items outside any class (imports, module-info) are absent.
/// Computed in a single tree walk per file and memoized; invalidated together
/// with the file's item tree when the file text changes.
#[salsa::tracked(returns(ref))]
pub(crate) fn enclosing_class_query(
    db: &dyn TyDatabase,
    file: FileText,
) -> Arc<FxHashMap<ItemId, Name>> {
    let file_id = *file.file_id(db);
    let tree = hir::file_item_tree(db, file_id);
    let mut map: FxHashMap<ItemId, Name> = FxHashMap::default();
    fn walk(
        db: &dyn TyDatabase,
        file_id: FileId,
        tree: &hir_expand::item_tree::ItemTree,
        id: ItemId,
        enclosing: Option<ItemId>,
        map: &mut FxHashMap<ItemId, Name>,
    ) {
        let data = tree.data(id);
        let is_type = matches!(
            data,
            ItemData::Class(_)
                | ItemData::Interface(_)
                | ItemData::Enum(_)
                | ItemData::Record(_)
                | ItemData::Annotation(_)
        );
        let current = if is_type { Some(id) } else { enclosing };
        if let Some(enclosing) = current {
            if let Some(fqn) = hir::source_class_fqn(db, file_id, enclosing) {
                map.insert(id, fqn);
            }
        }
        for &child in data.body() {
            walk(db, file_id, tree, child, current, map);
        }
    }
    for &top in &tree.top {
        walk(db, file_id, &tree, top, None, &mut map);
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
    let type_params = type_params_map_query(db, db.file_text(file_id));
    let resolver = Resolver::new(&tree, type_params, item_id);
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
    let type_params = type_params_map_query(db, db.file_text(file_id));
    let resolver = Resolver::new(&tree, type_params, item_id);
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
