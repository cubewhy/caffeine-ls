//! The JVM substrate database trait: the base salsa trait of the semantic
//! model.
//!
//! [`JvmDatabase`] is the language-agnostic floor every database implements —
//! a [`SourceDatabase`] plus `hir-expand`'s base definition trait. The
//! language layers extend it: [`crate::java::db::JavaDatabase`] adds the Java
//! file HIR and lowering, [`crate::kotlin::db::KotlinDatabase`] scaffolds the
//! Kotlin side. Class resolution and classpath access are served by the
//! concrete database's `hir_state` (see `hir::JvmDatabase`).

use base_db::salsa;
use hir_expand::db::DefDatabase as BaseDefDatabase;

/// Base trait of the definition database: a [`SourceDatabase`] plus the
/// source-level queries of the semantic model, extended by the language
/// layers (`JavaDatabase`, `KotlinDatabase`).
#[salsa::db]
pub trait JvmDatabase: BaseDefDatabase {}
