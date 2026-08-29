//! The shared, language-agnostic item ids of the semantic model.
//!
//! [`ItemId`] identifies a lowered declaration within its owning file's item
//! tree. It is defined here — rather than in `hir-def` with the item tree it
//! indexes — because the lowered *body* IR of a file ([`crate::body::Body`])
//! stores the id of the item that owns each body, and the body IR lives in
//! this crate. A pure arena index, `ItemId` carries no language-specific
//! meaning; the JVM substrate re-exports it as [`crate::jvm::ids`] and the
//! Java declaration layer gives it typed views (`ClassId`, `FieldId`,
//! `MethodId`).

use crate::arena::ArenaId;

/// The id of an item within its owning `ItemTree`. Stable across salsa
/// queries; combine with a `FileId` for a workspace-unique id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId(pub ArenaId);
