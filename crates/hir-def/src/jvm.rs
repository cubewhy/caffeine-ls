//! The JVM substrate: language-agnostic semantics shared by every language
//! this LSP lowers to the Java Virtual Machine.
//!
//! This namespace must stay free of any Java (or Kotlin) syntax concepts:
//! it hosts the JVM access-flag model ([`access`]), fully qualified names
//! ([`fqn`]), the shared item ids ([`ids`]) and the re-exports of the JVM
//! declaration stubs ([`primitives`], [`stubs`]) that the classfile readers
//! produce. Language-specific declaration lowering lives in [`crate::java`]
//! and [`crate::kotlin`], both of which depend on this module and never on
//! each other.

pub mod access;
pub mod class;
pub mod db;
pub mod fqn;
pub mod ids;
pub mod primitives;
pub mod stubs;
