//! Java-specific syntax and semantics.
//!
//! Everything that is specific to the Java source grammar — the syntax
//! modifier model ([`modifiers`]), the declaration layer ([`item_tree`]),
//! the AST-to-item-tree lowering ([`crate::lower`]) and the pretty snapshot
//! surface ([`pretty`]) — lives in this namespace. It depends only on the JVM
//! substrate ([`crate::jvm`]) and `hir-expand`'s primitives, and never on
//! [`crate::kotlin`].

pub mod item_loc;
pub mod item_tree;
pub mod lower;
pub mod modifiers;
pub mod pretty;
