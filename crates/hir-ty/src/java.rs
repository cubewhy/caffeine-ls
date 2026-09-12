//! The Java type layer: everything the type system computes for Java source
//! and Java classfiles.
//!
//! Resolution ([`resolve`]), subtyping ([`subtyping`]), method resolution
//! ([`method`]), expression inference ([`infer`], [`inference`]), the
//! structured diagnostics ([`decl_check`], [`name_check`], [`diagnostics`],
//! [`annotation_check`], [`deprecation`], [`raw_type`]), the cross-file
//! dependency index ([`dep_index`]) and constant evaluation ([`const_eval`])
//! are all Java-specific today. The JVM substrate they build on lives in
//! [`crate::jvm`]; a Kotlin type layer will be added in [`crate::kotlin`]
//! without touching this namespace.

pub mod annotation_check;
pub mod annotation_value;
pub mod const_eval;
pub mod db;
pub mod decl_check;
pub mod dep_index;
pub mod deprecation;
pub mod diagnostics;
pub mod infer;
pub mod inference;
pub mod level_check;
pub mod method;
pub mod name_check;
pub mod range_ctx;
pub mod raw_type;
pub mod release_api;
pub mod resolve;
pub mod subtyping;
pub mod ty;
