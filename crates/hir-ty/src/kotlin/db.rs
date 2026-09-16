//! The Kotlin type queries.
//!
//! Every type of a Kotlin declaration is computed by a salsa query keyed on the
//! interned `(file, item)` pair ([`KotlinItemKey`]), so it is memoized per
//! declaration and invalidated exactly when the file changes — the same shape
//! the Java layer uses ([`crate::java::db::ItemKey`]).
//!
//! Type *parameters* are not a query: they are derived by walking the item
//! tree's `parent` chain ([`KotlinResolver::for_item`]), which is a bounded
//! walk over the item's own ancestry with no interning to save.

use triomphe::Arc;
use vfs::FileId;

use hir::hir_def::kotlin::item_tree::KotlinItemData;
use hir_expand::ids::ItemId;

use super::resolve::KotlinResolver;
use super::ty::ty_from_type_ref;
use crate::java::db::TyDatabase;
use crate::ty::Ty;

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
/// A property without a declared type has an *inferred* one, which the body
/// inference produces ([`crate::kotlin::infer`]); until then it is
/// [`Ty::error`] rather than a guess.
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
        KotlinItemData::Property(data) => match &data.ty {
            Some(ty) => ty_from_type_ref(db, &resolver, &ty.ty),
            None => Ty::error(db),
        },
        KotlinItemData::Function(data) => match &data.ret {
            Some(ret) => ty_from_type_ref(db, &resolver, &ret.ty),
            None if data.body.is_some() => unit(db, &resolver),
            None => Ty::error(db),
        },
        KotlinItemData::Accessor(data) => match &data.params.first() {
            // A setter's own type is `Unit`; a getter's is the property's,
            // which the property item resolves.
            _ if data.is_setter => unit(db, &resolver),
            _ => match tree.parent_of(item_id) {
                Some(property) => {
                    *kotlin_item_ty_query(db, KotlinItemKey::new(db, file_id, property))
                }
                None => Ty::error(db),
            },
        },
        KotlinItemData::Class(data) => reference(data.name.as_str()),
        KotlinItemData::Constructor(_) => match tree.parent_of(item_id) {
            Some(class) => *kotlin_item_ty_query(db, KotlinItemKey::new(db, file_id, class)),
            None => Ty::error(db),
        },
        KotlinItemData::EnumEntry(_) => match tree.parent_of(item_id) {
            Some(class) => *kotlin_item_ty_query(db, KotlinItemKey::new(db, file_id, class)),
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

/// The declared type of a Kotlin item, memoized per `(file, item)`. See
/// [`kotlin_item_ty_query`].
pub fn item_ty(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Ty {
    *kotlin_item_ty_query(db, KotlinItemKey::new(db, file_id, item_id))
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
    kotlin_body_types_query(db, KotlinItemKey::new(db, file_id, item_id))
}
