//! Language-agnostic scaffolding for the semantic model.
//!
//! Mirrors rust-analyzer's `hir-expand`: names, modifiers, item trees (the
//! lowered per-file declaration IR), item ids/locations and the salsa
//! `DefDatabase` trait. Language-specific lowering lives in `hir-def`.

pub mod arena;
pub mod body;
pub mod db;
pub mod files;
pub mod item_loc;
pub mod item_tree;
pub mod modifiers;
pub mod name;
pub mod pretty;
pub mod span;
