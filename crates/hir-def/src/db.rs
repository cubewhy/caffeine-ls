//! The definition database of `hir-def`, composed from the language-specific
//! sub-traits.
//!
//! [`DefDatabase`] is the root trait concrete databases implement: the JVM
//! substrate ([`crate::jvm::db::JvmDatabase`]) plus the Java file HIR
//! ([`crate::java::db::JavaDatabase`]) and the Kotlin scaffold
//! ([`crate::kotlin::db::KotlinDatabase`]). The per-language queries live on
//! the sub-traits so a query that only needs the JVM substrate can take
//! `&dyn JvmDatabase` and stay reusable by both languages.

use base_db::salsa;

pub use crate::java::db::{JavaDatabase, file_body_tree, file_item_tree};
pub use crate::jvm::db::JvmDatabase;
pub use crate::kotlin::db::KotlinDatabase;

/// The definition database: the JVM substrate plus the language layers.
/// Concrete databases (e.g. `ide-db`'s `RootDatabase`) implement this and
/// salsa's `#[salsa::db]` machinery wires up the tracked queries.
#[salsa::db]
pub trait DefDatabase: JvmDatabase + JavaDatabase + KotlinDatabase {}
