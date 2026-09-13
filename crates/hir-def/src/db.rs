//! The definition database of `hir-def`: the file-level HIR queries and the
//! root trait concrete databases implement.
//!
//! [`DefDatabase`] is composed from the JVM substrate
//! ([`crate::jvm::db::JvmDatabase`]) plus the per-language layers
//! ([`crate::java::db::JavaDatabase`] and
//! [`crate::kotlin::db::KotlinDatabase`]); a query that only needs the JVM
//! substrate takes `&dyn JvmDatabase` and stays reusable by both languages.
//!
//! Lowering a file's text is *not* language-specific, so the item tree, the
//! body tree and the [`AstIdMap`] live here, on the root trait: each
//! dispatches on the file's language and returns the language-neutral
//! [`FileItemTree`] facade.
//!
//! The item tree of a file is a tracked query keyed on the file's
//! [`base_db::FileText`] input: edits to the file invalidate exactly its tree.
//! Lowering itself is a pure function of the file text, its parse
//! ([`crate::lower::lower_source`]) and the file's [`AstIdMap`].
//!
//! The item tree and the body tree are lowered by *two* queries
//! ([`item_tree_query`] and [`body_tree_query`]); both lower from the same
//! CST in the same order, so the body ids stored in the item data index the
//! body arenas of the same (or a positionally identical) lower pass, and salsa
//! guarantees both reads at one revision come from that revision. The item
//! tree carries no body content and no source offsets — its [`FileAstId`]s
//! are a function of the declaration skeleton only — so an edit inside a
//! method body changes the body tree but leaves the item tree value equal.
//! Salsa backdates the equal result, so file-level and workspace-level
//! consumers (`file_symbols_query`, `supertypes_query`, ...) are not
//! invalidated by body-only edits.
//!
//! [`FileAstId`]: hir_expand::ast_id_map::FileAstId

use triomphe::Arc;

use base_db::{FileText, LanguageKind, salsa};
use hir_expand::{ast_id_map::AstIdMap, body::BodyTree};
use vfs::FileId;

use crate::item_tree::FileItemTree;

pub use crate::java::db::JavaDatabase;
pub use crate::jvm::db::JvmDatabase;
pub use crate::kotlin::db::KotlinDatabase;

/// The definition database: the JVM substrate, the language layers and the
/// language-dispatched file queries. Concrete databases (e.g. `ide-db`'s
/// `RootDatabase`) implement this and salsa's `#[salsa::db]` machinery wires up
/// the tracked queries.
#[salsa::db]
pub trait DefDatabase: JvmDatabase + JavaDatabase + KotlinDatabase {}

/// The per-file [`AstIdMap`]: the map from declaration-skeleton syntax nodes to
/// their [`FileAstId`](hir_expand::ast_id_map::FileAstId)s, keyed exactly like
/// [`base_db::parse_query`] (file id + language) so the item tree query can
/// resolve the anchors of the tree it lowers.
///
/// SAFETY: `AstIdMap` contains no database-lifetime references, so it is safe
/// for salsa to retain it across revisions.
#[salsa::tracked(returns(ref))]
fn ast_id_map_query(db: &dyn DefDatabase, file_id: FileId, language: LanguageKind) -> AstIdMap {
    if language == LanguageKind::Unknown {
        // An unknown-language file has no parse; its item tree is empty and
        // no declaration anchors are ever resolved (mirrors the `Unknown`
        // arm of `lower_source`).
        return AstIdMap::default();
    }
    let parse = base_db::parse(db, file_id, language);
    let source = parse.syntax_node(language);
    AstIdMap::from_source_file(&source)
}

/// The per-file [`AstIdMap`] (see [`ast_id_map_query`]).
pub fn ast_id_map(db: &dyn DefDatabase, file_id: FileId, language: LanguageKind) -> &AstIdMap {
    ast_id_map_query(db, file_id, language)
}

/// The lowered item tree of the file in `file`: the declaration-only view, in
/// which every declaration anchors to its syntax node through a
/// [`FileAstId`](hir_expand::ast_id_map::FileAstId) resolved from the file's
/// [`AstIdMap`]. Its value is independent of body content and of source
/// offsets, so it backdates across body-only edits.
///
/// SAFETY: `FileItemTree` contains no database-lifetime references, so it is
/// safe for salsa to retain it across revisions even though it does not
/// implement `SalsaValue` (its `FileAstId` markers and rowan fields are foreign
/// types).
#[salsa::tracked(unsafe(non_salsa_values))]
fn item_tree_query(db: &dyn DefDatabase, file: FileText) -> Arc<FileItemTree> {
    let file_id = *file.file_id(db);
    // A tracked read (see `base_db::file_language_kind`): resolves from the
    // file's source-root salsa inputs, so attaching the file to a source root
    // later recomputes the tree with the correct language instead of serving
    // an `Unknown`-lowered (empty) result.
    let language = base_db::file_language_kind(db, file_id).unwrap_or(LanguageKind::Unknown);
    let map = ast_id_map_query(db, file_id, language);
    Arc::new(crate::lower::lower_source(language, file.text(db), map).items)
}

/// The lowered item tree of `file_id`: the declaration-only view of the file.
/// Its value is independent of body content, so it backdates across body-only
/// edits.
pub fn file_item_tree(db: &dyn DefDatabase, file_id: FileId) -> Arc<FileItemTree> {
    item_tree_query(db, db.file_text(file_id)).clone()
}

/// The lowered body tree of the file in `file`: the statements and expressions
/// of every method body, initializer, field initializer, enum constant
/// argument and annotation element default, in the same arena layout the item
/// tree's body ids index into.
///
/// SAFETY: `BodyTree` contains no database-lifetime references, so it is safe
/// for salsa to retain it across revisions even though it does not implement
/// `SalsaValue` (its `rowan::TextRange` fields are foreign types).
#[salsa::tracked(unsafe(non_salsa_values))]
fn body_tree_query(db: &dyn DefDatabase, file: FileText) -> Arc<BodyTree> {
    let file_id = *file.file_id(db);
    let language = base_db::file_language_kind(db, file_id).unwrap_or(LanguageKind::Unknown);
    let map = ast_id_map_query(db, file_id, language);
    crate::lower::lower_source(language, file.text(db), map).bodies
}

/// The lowered body tree of `file_id`: the statements and expressions of every
/// method body, initializer, field initializer, enum constant argument and
/// annotation element default, in the same arena layout the item tree's body
/// ids index into.
pub fn file_body_tree(db: &dyn DefDatabase, file_id: FileId) -> Arc<BodyTree> {
    body_tree_query(db, db.file_text(file_id)).clone()
}
