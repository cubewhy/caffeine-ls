//! Definition lowering: CST → item tree.
//!
//! `hir-def` turns the syntax tree of a single file into the flat, arena-based
//! [`hir_expand::item_tree::ItemTree`]. Java is fully lowered; Kotlin is a
//! placeholder for now. Lowering is a pure function of the parsed file, so it
//! is computed by a salsa query ([`crate::db`]) and cached per file.

pub mod db;
pub mod lower;

pub use db::{DefDatabase, file_body_tree, file_item_tree};

/// Lowers `text` for `language` into the file's item tree plus body IR
/// ([`hir_expand::item_tree::LoweredFile`]).
pub fn lower_source(
    language: base_db::LanguageKind,
    text: &str,
) -> hir_expand::item_tree::LoweredFile {
    lower::lower_source(language, text)
}
