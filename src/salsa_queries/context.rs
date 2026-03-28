/// Salsa queries for completion context extraction
///
/// This module provides incremental, cached context extraction for code completion.
/// All queries are memoized by Salsa and automatically invalidated when inputs change.
use super::Db;
use crate::language::rope_utils;
use crate::salsa_db::SourceFile;
use std::sync::Arc;

// ============================================================================
// Salsa-Compatible Data Structures
// ============================================================================

/// Lightweight completion context data (Salsa-compatible)
///
/// This is a simplified version of SemanticContext that only contains
/// data that can be hashed and compared. Complex types like TypeName
/// are converted to strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompletionContextData {
    pub location: CursorLocationData,
    pub java_module_context: Option<JavaModuleContextKindData>,
    pub query: Arc<str>,
    pub cursor_offset: usize,
    pub enclosing_class: Option<Arc<str>>,
    pub enclosing_internal_name: Option<Arc<str>>,
    pub enclosing_class_chain: Vec<Arc<str>>,
    pub enclosing_package: Option<Arc<str>>,
    pub local_var_count: usize,
    pub import_count: usize,
    pub static_import_count: usize,
    pub statement_labels: Vec<StatementLabelData>,
    pub char_after_cursor: Option<char>,
    pub is_class_member_position: bool,
    pub functional_target_hint: Option<FunctionalTargetHintData>,
    pub content_hash: u64,
    pub file_uri: Arc<str>,
    pub language_id: Arc<str>,
}

/// Cursor location data (Salsa-compatible)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AccessReceiverKindData {
    Unknown,
    Expression,
    Type { class_internal_name: Arc<str> },
}

/// Cursor location data (Salsa-compatible)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CursorLocationData {
    Expression {
        prefix: Arc<str>,
    },
    ConstructorCall {
        class_prefix: Arc<str>,
        expected_type: Option<Arc<str>>,
        qualifier_expr: Option<Arc<str>>,
        qualifier_owner_internal: Option<Arc<str>>,
    },
    MemberAccess {
        receiver_kind: AccessReceiverKindData,
        receiver_expr: Arc<str>,
        member_prefix: Arc<str>,
        receiver_type_hint: Option<Arc<str>>,
        /// Serialized CallArgs for method calls
        arguments: Option<Arc<str>>,
    },
    Import {
        prefix: Arc<str>,
    },
    ImportStatic {
        prefix: Arc<str>,
    },
    MethodArgument {
        prefix: Arc<str>,
        method_name: Option<Arc<str>>,
        arg_index: Option<usize>,
    },
    TypeAnnotation {
        prefix: Arc<str>,
    },
    VariableName {
        type_name: Arc<str>,
    },
    StringLiteral {
        prefix: Arc<str>,
    },
    MethodReference {
        qualifier_expr: Arc<str>,
        member_prefix: Arc<str>,
        is_constructor: bool,
    },
    Annotation {
        prefix: Arc<str>,
        target_element_type: Option<Arc<str>>,
    },
    AnnotationParam {
        prefix: Arc<str>,
        annotation_name: Option<Arc<str>>,
        used_keys: Vec<Arc<str>>,
        fresh_slot: bool,
    },
    StatementLabel {
        kind: StatementLabelKind,
        prefix: Arc<str>,
    },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JavaModuleContextKindData {
    DirectiveKeyword,
    RequiresModifier,
    RequiresModule,
    ExportsPackage,
    OpensPackage,
    TargetModule,
    UsesType,
    ProvidesService,
    ProvidesImplementation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatementLabelKind {
    Break,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatementLabelTargetKindData {
    Block,
    While,
    DoWhile,
    For,
    EnhancedFor,
    Switch,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatementLabelData {
    pub name: Arc<str>,
    pub target_kind: StatementLabelTargetKindData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpectedTypeSourceData {
    VariableInitializer,
    AssignmentRhs,
    ReturnExpr,
    MethodArgument { arg_index: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FunctionalMethodCallHintData {
    pub receiver_expr: Arc<str>,
    pub method_name: Arc<str>,
    pub arg_index: usize,
    pub arg_texts: Vec<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MethodRefQualifierKindData {
    Type,
    Expr,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FunctionalExprShapeData {
    MethodReference {
        qualifier_expr: Arc<str>,
        member_name: Arc<str>,
        is_constructor: bool,
        qualifier_kind: MethodRefQualifierKindData,
    },
    Lambda {
        param_count: usize,
        expression_body: Option<Arc<str>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FunctionalTargetHintData {
    pub expected_type_source: Option<Arc<str>>,
    pub expected_type_context: Option<ExpectedTypeSourceData>,
    pub assignment_lhs_expr: Option<Arc<str>>,
    pub method_call: Option<FunctionalMethodCallHintData>,
    pub expr_shape: Option<FunctionalExprShapeData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodSummaryData {
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub param_names: Vec<Arc<str>>,
    pub access_flags: u16,
    pub is_synthetic: bool,
    pub generic_signature: Option<Arc<str>>,
    pub return_type: Option<Arc<str>>,
}

/// Local variable metadata (Salsa-compatible)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalVarData {
    pub name: Arc<str>,
    pub type_internal: Arc<str>,
    pub init_expr: Option<Arc<str>>,
}

/// AST node metadata (Salsa-compatible)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeMetadata {
    pub kind: Arc<str>,
    pub start_byte: usize,
    pub end_byte: usize,
    pub parent_kind: Option<Arc<str>>,
    pub text: Arc<str>,
}

// ============================================================================
// Core Salsa Queries
// ============================================================================

/// Extract completion context for a position in a file (CACHED)
///
/// This is the main entry point for context extraction. It's memoized by
/// (file, line, character, trigger_char) so repeated requests at the same
/// position are instant.
#[salsa::tracked]
pub fn extract_completion_context(
    db: &dyn Db,
    file: SourceFile,
    line: u32,
    character: u32,
    trigger_char: Option<char>,
) -> Arc<CompletionContextData> {
    let language_id = file.language_id(db);

    if let Some(language) = crate::language::lookup_language(language_id.as_ref())
        && let Some(context) =
            language.extract_completion_context_salsa(db, file, line, character, trigger_char)
    {
        return context;
    }

    Arc::new(CompletionContextData {
        location: CursorLocationData::Unknown,
        java_module_context: None,
        query: Arc::from(""),
        cursor_offset: 0,
        enclosing_class: None,
        enclosing_internal_name: None,
        enclosing_class_chain: vec![],
        enclosing_package: None,
        local_var_count: 0,
        import_count: 0,
        static_import_count: 0,
        statement_labels: vec![],
        char_after_cursor: None,
        is_class_member_position: false,
        functional_target_hint: None,
        content_hash: 0,
        file_uri: Arc::from(file.file_id(db).as_str()),
        language_id: Arc::clone(&language_id),
    })
}

/// Find the AST node at a specific position (CACHED)
///
/// This is cached per (file, offset) so navigating to the same position
/// multiple times is instant.
#[salsa::tracked]
pub fn find_node_at_position(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
) -> Option<Arc<NodeMetadata>> {
    let content = file.content(db);

    // Parse tree
    let tree = super::parse::parse_tree(db, file)?;
    let root = tree.root_node();

    // Find deepest node at offset
    let node = find_deepest_node_at_offset(root, offset)?;

    let parent_kind = node.parent().map(|p| Arc::from(p.kind()));
    let text = node.utf8_text(content.as_bytes()).ok()?;

    Some(Arc::new(NodeMetadata {
        kind: Arc::from(node.kind()),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        parent_kind,
        text: Arc::from(text),
    }))
}

/// Helper: Find the deepest node at a given offset
fn find_deepest_node_at_offset<'a>(
    root: tree_sitter::Node<'a>,
    offset: usize,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = root.walk();
    let mut node = root;

    loop {
        let mut found_child = false;
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children {
            if child.start_byte() <= offset && offset < child.end_byte() {
                node = child;
                found_child = true;
                break;
            }
        }

        if !found_child {
            return Some(node);
        }
    }
}

/// Compute content hash for a scope around the cursor (CACHED)
///
/// This is used to detect when the relevant scope has changed.
/// Only the method/class containing the cursor is hashed, not the entire file.
#[salsa::tracked]
pub fn compute_scope_content_hash(db: &dyn Db, file: SourceFile, offset: usize) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let content = file.content(db);

    // Find enclosing method or class
    let scope_range = find_enclosing_scope_range(db, file, offset).unwrap_or((0, content.len()));

    let scope_content = &content[scope_range.0..scope_range.1.min(content.len())];

    let mut hasher = DefaultHasher::new();
    scope_content.hash(&mut hasher);
    hasher.finish()
}

/// Find the byte range of the enclosing scope (method or class)
fn find_enclosing_scope_range(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
) -> Option<(usize, usize)> {
    // Try to find enclosing method first
    if let Some((start, end)) = super::semantic::find_enclosing_method_bounds(db, file, offset) {
        return Some((start, end));
    }

    // Fall back to enclosing class
    if let Some((_, start, end)) = super::semantic::find_enclosing_class_bounds(db, file, offset) {
        return Some((start, end));
    }

    None
}

// ============================================================================
// Conversion Utilities
// ============================================================================

/// Convert line/column to byte offset
pub fn line_col_to_offset(content: &str, line: u32, character: u32) -> Option<usize> {
    rope_utils::line_col_to_offset(content, line, character)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_col_to_offset() {
        let content = "line 0\nline 1\nline 2";
        assert_eq!(line_col_to_offset(content, 0, 0), Some(0));
        assert_eq!(line_col_to_offset(content, 1, 0), Some(7));
        assert_eq!(line_col_to_offset(content, 2, 0), Some(14));
    }
}
