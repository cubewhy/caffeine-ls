//! Kotlin inlay-hint placeholder.
//!
//! Kotlin files have no HIR yet: [`hir_def::kotlin::lower`] is a placeholder,
//! so `hir-def` lowers a Kotlin CST to an empty item tree and body tree
//! (`crates/hir-def/src/java/lower.rs`, `lower_source`'s Kotlin arm) and there
//! is nothing to compute a hint from. A request in a Kotlin file therefore
//! answers nothing rather than walking an empty tree through the Java path.
//!
//! When the Kotlin lowering lands, this module calls into [`super::java`]'s
//! collectors rather than duplicating them: they read only the language-neutral
//! [`hir_expand::body::BodyTree`], [`hir_ty::BodyTypes`] and
//! [`hir_ty::Ty`], so a Kotlin item/body tree plus a Kotlin-aware
//! `hir_def::kotlin::ranges` is all the Java module's own `var`-keyword token
//! walk and item-range guard need replaced. The LSP surface
//! (`caffeine_ls::lsp::inlay_hints`) and the model in [`super`] are already
//! language-neutral.

use rowan::{TextRange, TextSize};
use vfs::FileId;

use super::{InlayHint, InlayHintDetail, InlayHintKind, InlayHintsConfig};
use crate::RootDatabase;

/// The file's hints in `range`. None: see the module documentation.
pub(super) fn hints(
    _db: &RootDatabase,
    _file: FileId,
    _range: TextRange,
    _config: &InlayHintsConfig,
) -> Vec<InlayHint> {
    Vec::new()
}

/// The detail of the hint a resolve names. None: see the module documentation.
pub(super) fn resolve(
    _db: &RootDatabase,
    _file: FileId,
    _offset: TextSize,
    _kind: InlayHintKind,
    _config: &InlayHintsConfig,
) -> Option<InlayHintDetail> {
    None
}
