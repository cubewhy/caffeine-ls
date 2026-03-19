pub mod completion;
pub mod index;
/// Salsa query infrastructure for incremental computation
///
/// This module provides the query layer that sits between the LSP handlers
/// and the actual parsing/indexing logic. All queries are memoized by Salsa
/// and automatically invalidated when inputs change.
pub mod java;
pub mod kotlin;
pub mod parse;
pub mod resolve;

use parking_lot::RwLock;
use std::sync::Arc;

use crate::index::WorkspaceIndex;

/// Database trait that extends Salsa's Database with workspace-specific accessors
#[salsa::db]
pub trait Db: salsa::Database {
    /// Get reference to the workspace index
    /// This allows queries to access the global index state
    fn workspace_index(&self) -> Arc<RwLock<WorkspaceIndex>>;
}

pub use completion::{
    CompletionContextKey, CompletionContextMetadata, cached_completion_context_metadata,
    compute_relevant_content_hash, extract_relevant_scope,
};
pub use index::{
    build_name_table_for_context, cached_index_view_metadata, cached_name_table, extract_classes,
    get_extracted_classes, get_index_view_for_context, get_name_table_for_context,
    visible_classpath_for_context,
};
/// Re-export commonly used query functions
pub use parse::{extract_imports, extract_package, parse_file};
pub use resolve::resolve_type_in_context;
