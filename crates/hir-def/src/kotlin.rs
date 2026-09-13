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
//! # What is not implemented yet
//!
//! The Kotlin *body* IR: `.kt` files lower their declarations, and the bodies
//! of functions, accessors, initializers and enum-entry arguments are not yet
//! lowered (every `body`/`initializer_expr`/`delegate_expr`/`argument_exprs`
//! field is empty — see [`crate::kotlin::lower::lower_kotlin_source`]).
//!
//! `.kts` scripts are out of scope: a script's top-level statements declare no
//! file item, so [`crate::lower::lower_source`] leaves a
//! [`LanguageKind::KotlinScript`](base_db::LanguageKind::KotlinScript) file
//! empty.

pub mod db;
pub mod item_tree;
pub mod lower;
pub mod modifiers;
pub mod pretty;
pub mod ranges;
