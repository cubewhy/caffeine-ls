//! Workspace-unique location of an item: the file plus its tree-local id.

use rowan::TextRange;
use vfs::FileId;

use crate::item_tree::ItemId;

/// Uniquely identifies an item across the whole workspace (analogous to
/// rust-analyzer's `ItemLoc`). `file_id` + [`ItemId`] recover the item from
/// the owning file's [`crate::item_tree::ItemTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ItemLoc {
    pub file_id: FileId,
    pub item: ItemId,
    pub range: TextRange,
}
