//! The Java database trait.
//!
//! [`JavaDatabase`] extends the JVM substrate ([`crate::jvm::db::JvmDatabase`])
//! with the Java-specific *semantic* queries. The file-level queries that
//! lower a file's text — the item tree, the body tree and the [`AstIdMap`]
//! they anchor into — are language-dispatched and live on
//! [`crate::db::DefDatabase`] instead.
//!
//! [`AstIdMap`]: hir_expand::ast_id_map::AstIdMap

use base_db::salsa;

use crate::jvm::db::JvmDatabase;

/// The Java database: the JVM substrate plus the Java-specific queries.
/// Concrete databases (e.g. `ide-db`'s `RootDatabase`) implement this and
/// salsa's `#[salsa::db]` machinery wires up the tracked queries.
#[salsa::db]
pub trait JavaDatabase: JvmDatabase {}
