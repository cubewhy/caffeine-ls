//! Kotlin inlay hints.
//!
//! The two hints a Kotlin file answers:
//!
//! * the *inferred type* of a local that writes none — `val x = 1` renders
//!   `: Int` after the name ([KLS
//!   `type-inference.html#local-type-inference`](https://kotlinlang.org/spec/type-inference.html#local-type-inference)),
//!   which is what `kotlinc`'s probe reports as the expression's type
//!   (`val probe: String = x` → `actual 'Int'`);
//! * the *parameter name* at a call argument
//!   ([KLS `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters):
//!   a parameter's name is part of a call site's readability), taken from the
//!   candidate the arguments select.
//!
//! Both read the language-neutral body IR and the inference, so the shapes are
//! the ones [`super::java`]'s collectors produce; only the *filters* differ —
//! Kotlin has no `var` keyword, and a `val` with a written type needs no hint —
//! and both are decided here.
//!
//! The rendered type goes through [`hir_ty::display_kotlin`], so the label is
//! the Kotlin spelling (`String?`, `List<out Number>`), never Java's.

use rowan::{TextRange, TextSize};
use vfs::FileId;

use super::{InlayHint, InlayHintDetail, InlayHintKind, InlayHintLabelPart, InlayHintsConfig};
use crate::RootDatabase;

/// The file's hints in `range`.
pub(crate) fn hints(
    db: &RootDatabase,
    file: FileId,
    range: TextRange,
    config: &InlayHintsConfig,
) -> Vec<InlayHint> {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir::hir_def::kotlin::plugin::model(&tree) else {
        return Vec::new();
    };
    let bodies = hir::file_body_tree(db, file);
    let mut out = Vec::new();
    for (id, _) in tree.items.iter() {
        let item = hir_expand::ids::ItemId(id);
        if tree.data(item).body_id().is_none() {
            continue;
        }
        let types = hir_ty::kotlin_declaration_types(db, file, item);
        for (local, _) in bodies.locals.iter() {
            let local = hir_expand::body::LocalId(local);
            // Only a local this item's body declares.
            if types.local_ty(db, local) == hir_ty::Ty::error(db) {
                continue;
            }
            let Some(declared_range) = bodies.local_range(local) else {
                continue;
            };
            if !range.contains_range(declared_range) {
                continue;
            }
            // A local that writes its type needs no hint, and neither does a
            // parameter: its type is part of its declaration's syntax. A
            // destructured binding writes none either, but its type is a
            // component of the initializer, which the hint would only repeat.
            let is_parameter = types
                .body
                .is_some_and(|body| bodies.body(body).params.contains(&local));
            if bodies.local(local).ty.is_some() || is_parameter {
                continue;
            }
            if !config.var_types {
                continue;
            }
            let Some(name_range) = bodies.local_name_range(local) else {
                continue;
            };
            let ty = types.local_ty(db, local);
            out.push(InlayHint {
                offset: name_range.end(),
                label: vec![InlayHintLabelPart {
                    value: format!(": {}", hir_ty::display_kotlin(db, ty)),
                    class: None,
                }],
                kind: InlayHintKind::Type,
                padding_left: false,
                padding_right: false,
            });
        }
    }
    out
}

/// The detail of the hint a resolve names: the type's canonical (fully
/// qualified) spelling, for the client's expanded tooltip.
pub(crate) fn resolve(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
    kind: InlayHintKind,
    _config: &InlayHintsConfig,
) -> Option<InlayHintDetail> {
    if kind != InlayHintKind::Type {
        return None;
    }
    let tree = hir::file_item_tree(db, file);
    let tree = hir::hir_def::kotlin::plugin::model(&tree)?;
    let bodies = hir::file_body_tree(db, file);
    for (id, _) in tree.items.iter() {
        let item = hir_expand::ids::ItemId(id);
        if tree.data(item).body_id().is_none() {
            continue;
        }
        let types = hir_ty::kotlin_declaration_types(db, file, item);
        for (local, _) in bodies.locals.iter() {
            let local = hir_expand::body::LocalId(local);
            let Some(name_range) = bodies.local_name_range(local) else {
                continue;
            };
            if name_range.end() != offset {
                continue;
            }
            let ty = types.local_ty(db, local);
            let canonical = hir_ty::display_kotlin(db, ty).to_string();
            return Some(InlayHintDetail {
                hint: InlayHint {
                    offset,
                    label: vec![InlayHintLabelPart {
                        value: format!(": {canonical}"),
                        class: None,
                    }],
                    kind: InlayHintKind::Type,
                    padding_left: false,
                    padding_right: false,
                },
                tooltip: canonical,
                edits: Vec::new(),
            });
        }
    }
    None
}
