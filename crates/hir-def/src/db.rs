//! Salsa queries of the definition database.
//!
//! The item tree of a file is a tracked query keyed on the file's
//! [`base_db::FileText`] input: edits to the file invalidate exactly its tree.
//! Lowering itself is a pure function of the file text
//! (`crate::lower::lower_source`).

use std::sync::Arc;

use base_db::{FileText, LanguageKind, salsa};
use hir_expand::item_tree::ItemTree;
use vfs::FileId;

/// Definition database: `hir-expand`'s base trait plus the language-specific
/// queries. Concrete databases (e.g. `ide-db`'s `RootDatabase`) implement this
/// and salsa's `#[salsa::db]` machinery wires up the tracked queries.
#[salsa::db]
pub trait DefDatabase: hir_expand::db::DefDatabase {}

/// The lowered item tree of the file in `file`.
///
/// SAFETY: `ItemTree` contains no database-lifetime references, so it is safe
/// for salsa to retain it across revisions even though it does not implement
/// `SalsaValue` (its `rowan::TextRange` fields are foreign types).
#[salsa::tracked(unsafe(non_salsa_values))]
fn item_tree_query(db: &dyn DefDatabase, file: FileText) -> Arc<ItemTree> {
    let file_id = *file.file_id(db);
    let language = db
        .file_language_kind(file_id)
        .unwrap_or(LanguageKind::Unknown);
    Arc::new(crate::lower::lower_source(language, file.text(db)))
}

/// The lowered item tree of `file_id`.
pub fn file_item_tree(db: &dyn DefDatabase, file_id: FileId) -> Arc<ItemTree> {
    item_tree_query(db, db.file_text(file_id)).clone()
}
