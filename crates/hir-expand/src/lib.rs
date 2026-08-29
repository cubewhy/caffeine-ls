//! Language-agnostic scaffolding for the semantic model.
//!
//! Mirrors rust-analyzer's `hir-expand`: names, item ids, bodies (the lowered
//! per-file body IR), the salsa `DefDatabase` trait and the source spans of
//! lowered type references. The language-specific declaration layer — the item
//! tree — lives in `hir-def`'s `java` namespace on top of these primitives.

pub mod arena;
pub mod body;
pub mod db;
pub mod files;
pub mod ids;
pub mod name;
pub mod span;
