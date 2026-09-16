//! Definition lowering: CST → item tree.
//!
//! `hir-def` turns the syntax tree of a single file into a flat, arena-based
//! declaration model. Every language lowers into its own model
//! ([`java::item_tree::ItemTree`] today) and hands it to the IDE through the
//! language-neutral facade ([`FileItemTree`]). Lowering is a pure function of
//! the parsed file, so it is computed by a salsa query ([`db`]) and cached per
//! file.
//!
//! # Namespaces
//!
//! The crate is organized along the language boundary:
//!
//! * [`jvm`] — the language-agnostic JVM substrate: access flags, fully
//!   qualified names and the shared declaration stubs, free of any Java or
//!   Kotlin syntax concepts;
//! * [`java`] — Java-specific syntax and semantics: the modifier model, the
//!   declaration layer ([`java::item_tree`]) and its lowering;
//! * [`kotlin`] — the Kotlin scaffold;
//! * [`item_tree`] / [`lower`] / [`pretty`] — the language-neutral facade:
//!   what the IDE reads ([`FileItemTree`]), the lowering entry point that
//!   dispatches on the file's language, and the snapshot surface.

pub mod db;
pub mod item_tree;
pub mod java;
pub mod jvm;
pub mod kotlin;
pub mod lang;
pub mod lower;
pub mod pretty;

pub use db::{DefDatabase, file_body_tree, file_item_tree};
pub use item_tree::{FileItemTree, LoweredFile};
pub use lower::lower_source;
