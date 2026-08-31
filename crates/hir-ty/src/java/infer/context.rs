//! The per-item context helpers of inference: the forward-field-name set of
//! an initializer ([JLS §8.3.3]), the static-context and enclosing-self-type
//! of an item, the prior blank-final writes seeded from earlier initializer
//! bodies, and the source-item lookup used by constructor delegation
//! tracking ([§8.8.7.1]).

use hir_def::java::item_tree::{ItemData, ItemId};
use hir_expand::name::Name;
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::FileId;

use crate::java::{
    db::TyDatabase,
    resolve::{Resolver, resolve_type_ref},
    ty::Ty,
};

use super::body_types;

/// later static, or vice versa) is legal.
pub(super) fn forward_field_names(
    tree: &hir_def::java::item_tree::ItemTree,
    field: hir_def::java::item_tree::ItemId,
    static_field: bool,
) -> Vec<Name> {
    // The class-like declaration owning `field`.
    fn owner_of(
        tree: &hir_def::java::item_tree::ItemTree,
        id: hir_def::java::item_tree::ItemId,
        target: hir_def::java::item_tree::ItemId,
    ) -> Option<hir_def::java::item_tree::ItemId> {
        let data = tree.data(id);
        let class_like = data.is_type();
        for &child in data.body() {
            if child == target {
                return class_like.then_some(id);
            }
            if let Some(found) = owner_of(tree, child, target) {
                return Some(found);
            }
        }
        None
    }
    for top in &tree.top {
        if let Some(class_item) = owner_of(tree, *top, field) {
            return tree
                .data(class_item)
                .body()
                .iter()
                .filter(|&&item| item > field)
                .filter_map(|&item| match tree.data(item) {
                    ItemData::Field(later)
                        if later.modifiers.is_static() == static_field && item != field =>
                    {
                        Some(later.name.clone())
                    }
                    _ => None,
                })
                .collect();
        }
    }
    Vec::new()
}

/// implicitly static field, so its argument expressions are a static context.
pub(super) fn static_context_of(tree: &hir_def::java::item_tree::ItemTree, item: ItemId) -> bool {
    match tree.data(item) {
        ItemData::Method(method) => method.modifiers.is_static(),
        ItemData::Field(field) => field.modifiers.is_static(),
        ItemData::StaticInit(_) | ItemData::EnumConstant(_) => true,
        // Instance methods, constructors, instance initializers and instance
        // fields: `this` is available, so unqualified instance invocations are
        // legal ([§15.12.3]).
        _ => false,
    }
}

/// outside any class-like declaration.
pub(super) fn enclosing_self_ty(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    item: ItemId,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
) -> Option<Ty> {
    // Parent links, one walk (the same shape as
    // [`crate::java::resolve::enclosing_type_chain`]).
    fn parents(tree: &hir_def::java::item_tree::ItemTree, map: &mut FxHashMap<ItemId, ItemId>) {
        fn walk(
            tree: &hir_def::java::item_tree::ItemTree,
            id: ItemId,
            parents: &mut FxHashMap<ItemId, ItemId>,
        ) {
            for &child in tree.data(id).body() {
                parents.insert(child, id);
                walk(tree, child, parents);
            }
        }
        for &top in &tree.top {
            walk(tree, top, map);
        }
    }
    let mut links: FxHashMap<ItemId, ItemId> = FxHashMap::default();
    parents(tree, &mut links);

    let mut current = links.get(&item).copied();
    while let Some(id) = current {
        // Enums and annotations cannot declare type parameters ([§8.9],
        // [§9.6]); their self-type is always raw.
        let declared: Option<&[hir_def::java::item_tree::TypeParam]> = match tree.data(id) {
            hir_def::java::item_tree::ItemData::Class(d) => Some(&d.type_params),
            hir_def::java::item_tree::ItemData::Interface(d) => Some(&d.type_params),
            hir_def::java::item_tree::ItemData::Record(d) => Some(&d.type_params),
            hir_def::java::item_tree::ItemData::Enum(_)
            | hir_def::java::item_tree::ItemData::Annotation(_) => Some(&[]),
            _ => None,
        };
        if let Some(declared) = declared {
            let fqn = hir::source_class_fqn(db, file, id)?;
            // The declared type variables are in scope as types ([§8.1.2]);
            // their bounds are resolved against the file like any other type.
            let args = declared
                .iter()
                .map(|tp| {
                    let bounds = tp
                        .bounds
                        .iter()
                        .map(|b| resolve_type_ref(db, scope, resolver, b))
                        .collect();
                    Ty::type_var(db, tp.name.clone(), bounds)
                })
                .collect();
            return Some(Ty::reference(db, fqn.as_str(), args));
        }
        current = links.get(&id).copied();
    }
    None
}

/// later write to a seeded blank final is the already-assigned error.
pub(super) fn prior_initializer_writes(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    item: ItemId,
) -> FxHashSet<String> {
    use hir_def::java::item_tree::ItemData as I;
    // The item's parent links (the same shape as
    // [`enclosing_self_ty`]'s `parents`).
    fn parents(tree: &hir_def::java::item_tree::ItemTree, map: &mut FxHashMap<ItemId, ItemId>) {
        fn walk(
            tree: &hir_def::java::item_tree::ItemTree,
            id: ItemId,
            parents: &mut FxHashMap<ItemId, ItemId>,
        ) {
            for &child in tree.data(id).body() {
                parents.insert(child, id);
                walk(tree, child, parents);
            }
        }
        for &top in &tree.top {
            walk(tree, top, map);
        }
    }
    let mut links: FxHashMap<ItemId, ItemId> = FxHashMap::default();
    parents(tree, &mut links);
    // The innermost class-like declaration owning `item`; the sibling
    // initializers live in its body.
    let mut class_item = None;
    let mut current = links.get(&item).copied();
    while let Some(id) = current {
        if tree.data(id).is_type() {
            class_item = Some(id);
            break;
        }
        current = links.get(&id).copied();
    }
    let Some(class_item) = class_item else {
        return FxHashSet::default();
    };

    // The kind of sibling bodies that run *before* `item`: whether they are
    // the static or the instance initializers, and whether `item` is itself a
    // constructor (which runs after every instance initializer).
    let (sibling_is_static, all_prior) = match tree.data(item) {
        I::Field(field) => (field.modifiers.is_static(), false),
        I::StaticInit(_) => (true, false),
        I::InstanceInit(_) => (false, false),
        I::Method(method) => (false, method.is_constructor()),
        _ => return FxHashSet::default(),
    };

    let mut seeded = FxHashSet::default();
    for &child in tree.data(class_item).body() {
        // For a constructor, every instance field initializer and instance
        // initializer precedes the body ([§8.8.7.1]); for a field initializer
        // or initializer, only its earlier same-kind siblings do.
        let is_prior = match tree.data(child) {
            I::Field(f) if all_prior => !f.modifiers.is_static(),
            I::InstanceInit(_) if all_prior => true,
            I::Field(f) => {
                !all_prior && f.modifiers.is_static() == sibling_is_static && child < item
            }
            I::StaticInit(_) => !all_prior && sibling_is_static && child < item,
            I::InstanceInit(_) => !all_prior && !sibling_is_static && child < item,
            _ => false,
        };
        if !is_prior {
            continue;
        }
        // A body-less field has no initializer to run; every body-carrying
        // sibling contributes its already-touched set.
        if let Some(types) = body_types(db, file, child) {
            for touched in &types.field_touched {
                seeded.insert(touched.clone());
            }
        }
    }
    seeded
}

/// purposes of the blank-final-field delegation tracking ([§8.8.7.1]).
pub(super) fn find_method_item(
    db: &dyn TyDatabase,
    file: FileId,
    method: &crate::java::method::MethodData,
) -> Option<ItemId> {
    let tree = hir::file_item_tree(db, file);
    for top in &tree.top {
        if let Some(found) = find_method_rec(&tree, *top, method) {
            return Some(found);
        }
    }
    None
}

pub(super) fn find_method_rec(
    tree: &hir_def::java::item_tree::ItemTree,
    id: ItemId,
    method: &crate::java::method::MethodData,
) -> Option<ItemId> {
    use hir_def::java::item_tree::ItemData as I;
    match tree.data(id) {
        I::Method(m)
            if m.name.as_str() == method.name && m.sig.params.len() == method.params.len() =>
        {
            return Some(id);
        }
        _ => {}
    }
    for &child in tree.data(id).body() {
        if let Some(found) = find_method_rec(tree, child, method) {
            return Some(found);
        }
    }
    None
}
