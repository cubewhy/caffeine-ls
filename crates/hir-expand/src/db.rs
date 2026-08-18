//! The salsa database trait of the semantic model. Language-specific queries
//! (notably item tree lowering) live in `hir-def` on top of this base trait.

use base_db::SourceDatabase;

/// Base trait of the definition database: a [`SourceDatabase`] plus the
/// source-level queries of the semantic model. `hir-def` extends this with
/// language-specific queries (item tree lowering, name resolution).
#[salsa::db]
pub trait DefDatabase: SourceDatabase {}
