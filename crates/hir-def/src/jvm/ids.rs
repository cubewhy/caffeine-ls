//! The shared item ids of the JVM substrate.
//!
//! [`ItemId`] identifies a lowered declaration within its owning file's item
//! tree. It is defined by `hir-expand` (whose [`body`] IR references it from
//! every lowered method body) and re-exported here so the JVM substrate can
//! name declarations without depending on any language-specific item data.

pub use hir_expand::ids::ItemId;
