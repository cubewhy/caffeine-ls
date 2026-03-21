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

    /// Latest parse snapshot for a file, used to drive incremental tree-sitter reparses.
    fn cached_parse_tree(
        &self,
        file_id: &crate::salsa_db::FileId,
    ) -> Option<crate::salsa_db::ParseTreeSnapshot>;

    /// Store the most recent parse snapshot for a file.
    fn store_parse_tree(
        &self,
        file_id: crate::salsa_db::FileId,
        snapshot: crate::salsa_db::ParseTreeSnapshot,
    );

    /// Clear a file's parse snapshot when the file leaves the Salsa workspace.
    fn remove_parse_tree(&self, file_id: &crate::salsa_db::FileId);
}

// Re-export commonly used types and functions
pub use completion::{
    CompletionContextKey, CompletionContextMetadata, cached_completion_context_metadata,
    compute_relevant_content_hash, extract_relevant_scope,
};
pub use context::{
    CompletionContextData, CursorLocationData, ExpectedTypeSourceData, FunctionalExprShapeData,
    FunctionalMethodCallHintData, FunctionalTargetHintData, LocalVarData,
    MethodRefQualifierKindData, MethodSummaryData, NodeMetadata, StatementLabelData,
    StatementLabelKind, StatementLabelTargetKindData, compute_scope_content_hash,
    extract_completion_context, find_node_at_position, line_col_to_offset,
};
pub use conversion::{FromSalsaData, convert_local_var};
pub use hints::{
    InlayHintData, InlayHintKindData, MethodCallMetadata, VarDeclMetadata, compute_inlay_hints,
    find_method_calls_in_range, find_variable_declarations_in_range, infer_variable_type,
};
pub use index::{
    cached_index_view_metadata, cached_name_table, extract_classes, get_extracted_classes,
    get_index_view_for_context, get_name_table_for_context, visible_classpath_for_context,
};
pub use parse::{extract_imports, extract_package, parse_file};
pub use resolve::resolve_type_in_context;
pub use semantic::{
    ClassMembersMetadata, FileStructureMetadata, FlowTypeOverrideData, MethodLocalsMetadata,
    extract_active_lambda_param_names_from_source, extract_active_lambda_param_names_incremental,
    extract_class_members_incremental, extract_class_members_metadata, extract_file_structure,
    extract_java_flow_type_overrides, extract_method_locals_incremental,
    extract_method_locals_metadata, extract_visible_method_locals_from_source,
    extract_visible_method_locals_incremental, find_enclosing_class_bounds,
    find_enclosing_method_bounds,
};
pub use symbols::{
    ResolvedSymbolData, SymbolKind, find_local_variable_declaration, is_local_variable,
    resolve_symbol_at_position,
};
