//! Kotlin-specific syntax and semantics.
//!
//! Scaffold for the Kotlin declaration layer: the Kotlin `ItemTree` lowering
//! and the Kotlin-specific modifier model will live here, depending only on
//! the JVM substrate ([`crate::jvm`]) and never on [`crate::java`]. Nothing is
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
