//! The `jvm` namespace of the `hir` crate: the classpath, bytecode and
//! stub-indexing substrate, re-exported under a single namespace so the
//! class-resolution machinery is addressable as `hir::jvm::*` (mirroring
//! `hir_def::jvm`). The physical modules stay where they are; this is a
//! logical namespace facade.

pub use crate::{index, lmdb_store, loader, modules, project, stubs};
