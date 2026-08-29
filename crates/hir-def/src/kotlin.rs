//! Kotlin-specific syntax and semantics.
//!
//! Scaffold for the Kotlin declaration layer: the Kotlin modifier model and
//! the Kotlin `ItemTree` lowering will live here, depending only on the JVM
//! substrate ([`crate::jvm`]) and never on [`crate::java`]. Nothing is
//! implemented yet — the Kotlin CST is parsed but not yet lowered.

/// The Kotlin modifier model, prepared for the Kotlin ItemTree integration.
///
/// Kotlin's modifier set (`public`, `internal`, `protected`, `private`,
/// `open`, `final`, `abstract`, `sealed`, `data`, `value`, `inline`,
/// `suspend`, `operator`, ...) maps onto the same [`crate::jvm::access::JvmAccessFlags`]
/// substrate at the JVM boundary.
pub mod modifiers {
    /// A placeholder documenting the intended module shape. Removed when the
    /// Kotlin lowering lands.
    #[allow(dead_code)]
    pub const LAYOUT: &str = "kotlin::modifiers";
}

/// The Kotlin lowering scaffold: the Kotlin CST will be lowered here into a
/// Kotlin item tree on top of the JVM substrate. Kotlin files currently
/// produce an empty item tree (see [`crate::java::lower::lower_source`]).
pub mod lower;
