//! Kotlin navigation placeholder.
//!
//! Kotlin files have no HIR yet: [`hir_def::kotlin::lower`] is a placeholder,
//! so `hir-def` lowers a Kotlin CST to an empty item tree and body tree
//! (`crates/hir-def/src/java/lower.rs`, `lower_source`'s Kotlin arm) and there
//! is nothing to resolve. A request in a Kotlin file therefore answers nothing
//! rather than walking an empty tree through the Java path.
//!
//! When the Kotlin lowering lands, this module mirrors
//! [`super::java_definition`]: the lowering dispatch is `lower_source`'s Kotlin
//! arm, the declaration skeleton needs the Kotlin arm of
//! `AstIdMap::from_source_file` (`crates/hir-expand/src/ast_id_map.rs`), the
//! ranges need a Kotlin-aware `hir_def::kotlin::ranges`, and the classpath
//! deferral is already language-neutral ([`hir::fqn_resolve`],
//! [`hir::library_source_decl`], [`hir::library_sources`]).

use rowan::TextSize;
use vfs::FileId;

use super::{LibrarySourceRef, NavigationTarget, RootDatabase};

/// The declarations the reference at `offset` resolves to in a Kotlin file.
/// Nothing does: see the module documentation.
pub(super) fn definition(
    _db: &RootDatabase,
    _file: FileId,
    _offset: TextSize,
) -> Vec<NavigationTarget> {
    Vec::new()
}

/// The library sources a Kotlin reference would need. Nothing resolves, so
/// nothing has to be materialized.
pub(super) fn pending_library_sources(
    _db: &RootDatabase,
    _file: FileId,
    _offset: TextSize,
) -> Vec<LibrarySourceRef> {
    Vec::new()
}
