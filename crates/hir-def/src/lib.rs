//! Definition lowering: CST → item tree.
//!
//! `hir-def` turns the syntax tree of a single file into the flat, arena-based
//! [`crate::java::item_tree::ItemTree`]. Java is fully lowered; Kotlin is a
//! placeholder for now. Lowering is a pure function of the parsed file, so it
//! is computed by a salsa query ([`crate::db`]) and cached per file.
//!
//! # Namespaces
//!
//! The crate is organized along the language boundary:
//!
//! * [`jvm`] — the language-agnostic JVM substrate: access flags, fully
//!   qualified names and the shared declaration stubs, free of any Java or
//!   Kotlin syntax concepts;
//! * [`java`] — Java-specific syntax and semantics: the modifier model, the
//!   declaration layer ([`crate::java::item_tree`]) and its lowering;
//! * [`kotlin`] — the Kotlin scaffold.

pub mod db;
pub mod java;
pub mod jvm;
pub mod kotlin;

pub use db::{DefDatabase, file_body_tree, file_item_tree};

/// Lowers `text` for `language` into the file's item tree plus body IR
/// ([`crate::java::item_tree::LoweredFile`]).
pub fn lower_source(
    language: base_db::LanguageKind,
    text: &str,
) -> crate::java::item_tree::LoweredFile {
    crate::java::lower::lower_source(language, text)
}
