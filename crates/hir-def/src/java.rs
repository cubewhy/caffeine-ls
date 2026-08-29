//! Java-specific syntax and semantics.
//!
//! Everything that is specific to the Java source grammar — the syntax
//! modifier model ([`modifiers`]), the declaration lowering into the item
//! tree ([`crate::lower`]) and the Java-specific lookup rules — lives in this
//! namespace. It depends only on the JVM substrate ([`crate::jvm`]) and never
//! on [`crate::kotlin`].

pub mod modifiers;
