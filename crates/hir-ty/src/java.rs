//! The Java type layer: everything the type system computes for Java source
//! and Java classfiles.
//!
//! Resolution ([`resolve`]), subtyping ([`subtyping`]), method resolution
//! ([`method`]), expression inference ([`infer`], [`inference`]), the
//! declaration diagnostics ([`decl_check`], [`name_check`],
//! [`diagnostics`]), the cross-file dependency index ([`dep_index`]) and
//! constant evaluation ([`const_eval`]) are all Java-specific today. The
//! JVM substrate they build on lives in [`crate::jvm`]; a Kotlin type layer
//! will be added in [`crate::kotlin`] without touching this namespace.

pub mod const_eval;
pub mod db;
pub mod decl_check;
pub mod dep_index;
pub mod diagnostics;
pub mod infer;
pub mod inference;
pub mod method;
pub mod name_check;
pub mod resolve;
pub mod subtyping;
pub mod ty;
