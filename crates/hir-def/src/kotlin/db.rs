//! The Kotlin database trait.
//!
//! [`KotlinDatabase`] is the Kotlin half of the *per-language* marker traits
//! ([`crate::db::DefDatabase`] requires it beside
//! [`JavaDatabase`](crate::java::db::JavaDatabase)): it extends the JVM
//! substrate ([`crate::jvm::db::JvmDatabase`]) so a query can be written
//! against Kotlin's own surface. The Kotlin queries are language-dispatched
//! through [`crate::lower`] and [`crate::db`]'s file queries rather than
//! declared here, so the trait adds no query of its own.

use base_db::salsa;

use crate::jvm::db::JvmDatabase;

/// The Kotlin database: the JVM substrate with Kotlin's marker on it.
#[salsa::db]
pub trait KotlinDatabase: JvmDatabase {}
