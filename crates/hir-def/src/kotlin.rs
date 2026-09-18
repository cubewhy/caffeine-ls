//! Kotlin-specific syntax and semantics.
//!
//! Everything that is specific to the Kotlin source grammar — the modifier
//! model ([`modifiers`]), the declaration layer ([`item_tree`]), the
//! CST-to-item-tree lowering ([`lower`]), the pretty snapshot surface
//! ([`pretty`]) and the declaration ranges ([`ranges`]) — lives in this
//! namespace. It depends only on the JVM substrate ([`crate::jvm`]) and
//! `hir-expand`'s primitives, and never on [`crate::java`].
//!
//! # Reference
//!
//! The normative reference for every Kotlin rule implemented here is the
//! *Kotlin language specification: Kotlin/Core*, v1.9-rfc+0.1
//! (<https://kotlinlang.org/spec/kotlin-spec.html>), cited per function. The
//! empirical reference is kotlinc 2.4.20 (JRE 25): KLS is explicitly
//! experimental and predates K2, so where the two disagree the implementation
//! follows the compiler and records the deviation in the doc comment with the
//! compiler message.
//!
//! # What is lowered
//!
//! Both halves of a file: the declaration model
//! ([`item_tree::KotlinItemTree`]) and the body IR
//! ([`hir_expand::body::BodyTree`]), which holds the bodies of functions,
//! accessors, initializers, `init` blocks, enum-entry arguments and — for a
//! `.kts` script — the top-level statements of its implicit `main`
//! ([`lower::lower_kotlin_source`]).
//!
//! The lowered model carries no source offsets: a declaration anchors itself to
//! its syntax node with a [`FileAstId`](hir_expand::ast_id_map::FileAstId) and
//! the ranges are resolved on demand ([`ranges`]).

pub mod annotations;
pub mod db;
pub mod item_tree;
pub mod lower;
pub mod modifiers;
pub mod plugin;
pub mod pretty;
pub mod ranges;
