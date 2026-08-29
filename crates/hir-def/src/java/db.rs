//! The Java database trait and its Salsa queries.
//!
//! [`JavaDatabase`] extends the JVM substrate ([`crate::jvm::db::JvmDatabase`])
//! with the Java file HIR: the item tree and body tree lowering of Java files.
//!
//! The item tree of a file is a tracked query keyed on the file's
//! [`base_db::FileText`] input: edits to the file invalidate exactly its tree.
//! Lowering itself is a pure function of the file text
//! ([`crate::java::lower::lower_source`]).
//!
//! The item tree and the body tree are lowered together by one query
//! ([`lower_source_query`]) so their ids line up, but only the *item* tree is
//! served to signature consumers. Because the item tree carries no body
//! content, an edit inside a method body changes the value of
//! `lower_source_query` yet leaves the item tree equal — salsa backdates it,
//! so file-level and workspace-level queries (`file_symbols_query`,
//! `supertypes_query`, ...) are not invalidated.

use std::sync::Arc;

use base_db::{FileText, LanguageKind, salsa};
use hir_expand::body::BodyTree;
use vfs::FileId;

use super::item_tree::{ItemTree, LoweredFile};
use crate::jvm::db::JvmDatabase;

/// The Java database: the JVM substrate plus the Java file HIR queries.
/// Concrete databases (e.g. `ide-db`'s `RootDatabase`) implement this and
/// salsa's `#[salsa::db]` machinery wires up the tracked queries.
#[salsa::db]
pub trait JavaDatabase: JvmDatabase {}

/// The full lowering of the file in `file`: its item tree and body tree,
/// computed in a single pass so the body ids stored in the item data index the
/// body arenas.
///
/// SAFETY: `LoweredFile` contains no database-lifetime references, so it is
/// safe for salsa to retain it across revisions even though it does not
/// implement `SalsaValue` (its `rowan::TextRange` fields are foreign types).
#[salsa::tracked(unsafe(non_salsa_values))]
fn lower_source_query(db: &dyn JavaDatabase, file: FileText) -> Arc<LoweredFile> {
    let file_id = *file.file_id(db);
    // A tracked read (see `base_db::file_language_kind`): resolves from the
    // file's source-root salsa inputs, so attaching the file to a source root
    // later recomputes the tree with the correct language instead of serving
    // an `Unknown`-lowered (empty) result.
    let language = base_db::file_language_kind(db, file_id).unwrap_or(LanguageKind::Unknown);
    Arc::new(crate::java::lower::lower_source(language, file.text(db)))
}

/// The lowered item tree of `file_id`: the declaration-only view of the file.
/// Its value is independent of method-body content, so it backdates across
/// body-only edits.
pub fn file_item_tree(db: &dyn JavaDatabase, file_id: FileId) -> Arc<ItemTree> {
    let lowered = lower_source_query(db, db.file_text(file_id));
    lowered.items.clone()
}

/// The lowered body tree of `file_id`: the statements and expressions of every
/// method body, initializer, field initializer, enum constant argument and
/// annotation element default, in the same arena layout the item tree's body
/// ids index into.
pub fn file_body_tree(db: &dyn JavaDatabase, file_id: FileId) -> Arc<BodyTree> {
    let lowered = lower_source_query(db, db.file_text(file_id));
    lowered.bodies.clone()
}
