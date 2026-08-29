//! Universal JVM type primitives shared by every source language: the
//! primitive types ([JLS §4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.2))
//! and the type reference model that backs both source lowering and the
//! classfile stubs. Re-exported from [`syntax::stub`], where they are defined
//! once and shared with `hir-expand`, the `hir` indexer and `hir-ty`.

pub use syntax::stub::{PrimitiveType, PrimitiveValue, TypeBound, TypeRef};
