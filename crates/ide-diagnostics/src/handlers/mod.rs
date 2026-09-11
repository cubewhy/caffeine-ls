//! The presentation of each diagnostic: its stable code, its user-facing
//! message and its secondary detail.
//!
//! One module per producing checker — the body-inference diagnostics
//! ([`body`]), the declaration checks ([`decl`]), the source-level checks
//! ([`level`]) and the platform-release checks ([`release`]). The type layer
//! stays free of every user-facing string: it records structured diagnostics,
//! and these handlers render them.

pub mod body;
pub mod decl;
pub mod level;
pub mod release;
