use std::sync::Arc;

/// Cache key for completion context
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompletionContextKey {
    pub file_uri: Arc<str>,
    /// Hash of the relevant portion of file content (current method/class scope)
    pub content_hash: u64,
    pub line: u32,
    pub character: u32,
    pub trigger_char: Option<char>,
}

/// Metadata for completion context caching
///
/// This lightweight struct is used with Salsa to track when context needs recomputation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompletionContextMetadata {
    pub file_uri: Arc<str>,
    pub content_hash: u64,
    pub line: u32,
    pub character: u32,
    pub trigger_char: Option<char>,
    /// Timestamp when context was computed (for cache eviction)
    pub computed_at: u64,
}

/// Cached completion context metadata query
///
/// This query tracks when we need to recompute the completion context.
/// The actual SemanticContext is stored in a separate cache (not in Salsa)
/// because it contains non-hashable types.
///
/// Cache invalidation happens when:
/// - File content changes (content_hash differs)
/// - Cursor position changes (line/character differs)
/// - Trigger character changes
#[salsa::tracked]
pub fn cached_completion_context_metadata(
    _db: &dyn crate::salsa_queries::Db,
    file_uri: Arc<str>,
    content_hash: u64,
    line: u32,
    character: u32,
    trigger_char: Option<char>,
) -> CompletionContextMetadata {
    tracing::debug!(
        file_uri = %file_uri,
        line = line,
        character = character,
        trigger = ?trigger_char,
        content_hash = content_hash,
        "cached_completion_context_metadata: computing (cache miss)"
    );

    let computed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    CompletionContextMetadata {
        file_uri,
        content_hash,
        line,
        character,
        trigger_char,
        computed_at,
    }
}

/// Helper function to compute content hash for relevant file portion
///
/// This hashes only the relevant scope (current method/class) to reduce
/// cache invalidation when unrelated parts of the file change.
pub fn compute_relevant_content_hash(source: &str, line: u32, character: u32) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // For now, hash the entire file content
    // TODO: Optimize to hash only relevant scope (current method/class)
    // This requires tree-sitter parsing to find the enclosing scope
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    line.hash(&mut hasher);
    character.hash(&mut hasher);
    hasher.finish()
}

/// Helper to extract relevant source scope for hashing
///
/// Extracts the minimal source region that affects completion context:
/// - For method body: the entire method
/// - For class body: the entire class
/// - For top-level: the entire file
///
/// This allows caching to survive edits in unrelated methods/classes.
pub fn extract_relevant_scope<'a>(
    source: &'a str,
    tree_root: Option<tree_sitter::Node<'a>>,
    cursor_offset: usize,
) -> &'a str {
    // If we have a tree, find the enclosing method or class
    if let Some(root) = tree_root {
        if let Some(enclosing) = find_enclosing_scope_node(root, cursor_offset) {
            if let Ok(text) = enclosing.utf8_text(source.as_bytes()) {
                return text;
            }
        }
    }

    // Fallback: return entire file
    source
}

/// Find the smallest enclosing scope node (method_declaration or class_declaration)
fn find_enclosing_scope_node(root: tree_sitter::Node, offset: usize) -> Option<tree_sitter::Node> {
    use tree_sitter_utils::traversal::find_node_by_offset;

    // Try to find method first, then class
    if let Some(method) = find_node_by_offset(root, "method_declaration", offset) {
        return Some(method);
    }

    if let Some(constructor) = find_node_by_offset(root, "constructor_declaration", offset) {
        return Some(constructor);
    }

    // Try to find any class-like declaration
    let class_kinds = &[
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "record_declaration",
    ];

    for kind in class_kinds {
        if let Some(class) = find_node_by_offset(root, kind, offset) {
            return Some(class);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_changes_with_content() {
        let source1 = "class A { void foo() {} }";
        let source2 = "class A { void bar() {} }";

        let hash1 = compute_relevant_content_hash(source1, 0, 10);
        let hash2 = compute_relevant_content_hash(source2, 0, 10);

        assert_ne!(
            hash1, hash2,
            "Different content should produce different hashes"
        );
    }

    #[test]
    fn test_content_hash_changes_with_position() {
        let source = "class A { void foo() {} }";

        let hash1 = compute_relevant_content_hash(source, 0, 10);
        let hash2 = compute_relevant_content_hash(source, 0, 20);

        assert_ne!(
            hash1, hash2,
            "Different positions should produce different hashes"
        );
    }

    #[test]
    fn test_cache_key_equality() {
        let key1 = CompletionContextKey {
            file_uri: Arc::from("file:///test.java"),
            content_hash: 12345,
            line: 10,
            character: 5,
            trigger_char: Some('.'),
        };

        let key2 = CompletionContextKey {
            file_uri: Arc::from("file:///test.java"),
            content_hash: 12345,
            line: 10,
            character: 5,
            trigger_char: Some('.'),
        };

        assert_eq!(key1, key2, "Identical keys should be equal");
    }

    #[test]
    fn test_cache_key_inequality_different_position() {
        let key1 = CompletionContextKey {
            file_uri: Arc::from("file:///test.java"),
            content_hash: 12345,
            line: 10,
            character: 5,
            trigger_char: Some('.'),
        };

        let key2 = CompletionContextKey {
            file_uri: Arc::from("file:///test.java"),
            content_hash: 12345,
            line: 10,
            character: 6, // Different character
            trigger_char: Some('.'),
        };

        assert_ne!(
            key1, key2,
            "Different positions should produce different keys"
        );
    }

    #[test]
    fn test_extract_relevant_scope_without_tree() {
        let source = "class A { void foo() {} }";
        let scope = extract_relevant_scope(source, None, 10);
        assert_eq!(scope, source, "Without tree, should return entire source");
    }
}
