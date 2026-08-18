//! Kotlin lowering placeholder.
//!
//! `hir-def` currently supports Java; Kotlin files produce an empty item tree.
//! The dispatch in [`super::lower_source`] keeps the door open for a real
//! Kotlin walker on top of `kotlin-syntax`'s CST.

use crate::lower::LowerCtx;

pub(super) fn lower_file(_ctx: &mut LowerCtx) {
    // TODO(kotlin): lower kotlin-syntax CST into an item tree.
}
