/// Salsa query infrastructure for incremental computation
///
/// This module provides the query layer that sits between the LSP handlers
/// and the actual parsing/indexing logic. All queries are memoized by Salsa
/// and automatically invalidated when inputs change.
pub mod completion;
pub mod context;
pub mod conversion;
pub mod hints;
pub mod index;
pub mod java;
pub mod kotlin;
pub mod parse;
pub mod resolve;
pub mod semantic;
pub mod symbols;

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

// Re-export commonly used types and functions
pub use completion::{
    CompletionContextKey, CompletionContextMetadata, cached_completion_context_metadata,
    compute_relevant_content_hash, extract_relevant_scope,
};
pub use context::{
    CompletionContextData, CursorLocationData, LocalVarData, NodeMetadata, StatementLabelKind,
    compute_scope_content_hash, extract_completion_context, find_node_at_position,
    line_col_to_offset,
};
pub use conversion::{FromSalsaData, convert_local_var};
pub use hints::{
    InlayHintData, InlayHintKindData, MethodCallMetadata, VarDeclMetadata, compute_inlay_hints,
    find_method_calls_in_range, find_variable_declarations_in_range, infer_variable_type,
};
pub use index::{
    build_name_table_for_context, cached_index_view_metadata, cached_name_table, extract_classes,
    get_extracted_classes, get_index_view_for_context, get_name_table_for_context,
    visible_classpath_for_context,
};
pub use parse::{extract_imports, extract_package, parse_file};
pub use resolve::resolve_type_in_context;
pub use semantic::{
    ClassMembersMetadata, FileStructureMetadata, MethodLocalsMetadata,
    extract_class_members_incremental, extract_class_members_metadata, extract_file_structure,
    extract_method_locals_incremental, extract_method_locals_metadata, find_enclosing_class_bounds,
    find_enclosing_method_bounds,
};
pub use symbols::{
    ResolvedSymbolData, SymbolKind, find_local_variable_declaration, is_local_variable,
    resolve_symbol_at_position,
};
