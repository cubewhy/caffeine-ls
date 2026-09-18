//! Kotlin type resolution.
//!
//! The Kotlin half of the type layer: how a Kotlin declaration's written types
//! become [`crate::ty::Ty`] values. It is a separate module from
//! [`crate::java`] because it resolves Kotlin's scopes and nullability, and it
//! shares the model, the interner and the classpath lookup
//! ([`hir::fqn_resolve`]) with it.
//!
//! # Reference
//!
//! The normative reference is the *Kotlin language specification: Kotlin/Core*,
//! v1.9-rfc+0.1 (<https://kotlinlang.org/spec/kotlin-spec.html>) — the type
//! system (`type-system.html`), the declaration rules
//! (`declarations.html`) and the package/import rules
//! (`packages-and-imports.html`) — cited per function. The empirical reference
//! is kotlinc 2.4.20 (JRE 25); where the two disagree the compiler wins and the
//! deviation is recorded where it is implemented.
//!
//! # What is here
//!
//! * [`resolve`] — the scopes a written type name is resolved in, including
//!   the default imports;
//! * [`ty`] — building a [`crate::ty::Ty`] from an item tree type reference;
//! * [`db`] — the memoized per-item queries;
//! * [`subtyping`] — Kotlin's subtype and assignability rules;
//! * [`method`] — a receiver's member set and overload selection;
//! * [`diagnostics`] — the Kotlin type errors, with the compiler's wordings;
//! * [`infer`] — body inference over the lowered body IR.

pub mod builtins;
pub mod db;
pub mod dep_index;
pub mod diagnostics;
pub mod infer;
pub mod jvm_view;
pub mod method;
pub mod operator;
pub mod plugin;
pub mod resolve;
pub mod subtyping;
pub mod ty;
