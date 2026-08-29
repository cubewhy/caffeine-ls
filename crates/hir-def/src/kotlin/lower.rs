//! Kotlin lowering placeholder.
//!
//! `hir-def` currently lowers Java; Kotlin files produce an empty item tree
//! ([`crate::java::lower::lower_source`]). The Kotlin CST is parsed by
//! `kotlin-syntax`; lowering it into a Kotlin item tree on top of the JVM
//! substrate ([`crate::jvm`]) is the next step. Nothing here yet — the module
//! exists so the Kotlin namespace ([`crate::kotlin`]) has a place to grow
//! without touching [`crate::java`].

/// A placeholder documenting the intended module shape. Removed when the
/// Kotlin lowering lands.
#[allow(dead_code)]
pub const LAYOUT: &str = "kotlin::lower";
