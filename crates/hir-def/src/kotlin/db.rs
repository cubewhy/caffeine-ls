//! The Kotlin database trait.
//!
//! [`KotlinDatabase`] scaffolds the Kotlin side of the definition database: it
//! extends the JVM substrate ([`crate::jvm::db::JvmDatabase`]) and will gain
//! the Kotlin file HIR queries when the Kotlin lowering lands. Nothing is
//! implemented yet.

use base_db::salsa;

use crate::jvm::db::JvmDatabase;

/// The Kotlin database: the JVM substrate plus (future) Kotlin file HIR
/// queries.
#[salsa::db]
pub trait KotlinDatabase: JvmDatabase {}
