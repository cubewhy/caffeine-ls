/// Incremental semantic analysis queries
///
/// These queries break down the expensive context extraction into
/// smaller, cacheable pieces that can be reused across completions.
///
/// Strategy: Return lightweight metadata (offsets, counts) from Salsa,
/// then reconstruct full objects on-demand. This avoids needing to make
/// complex types like LocalVar/TypeName hashable.
use crate::salsa_db::SourceFile;
use crate::semantic::LocalVar;
use std::sync::Arc;
use tree_sitter::{Parser, Tree};

/// Helper to parse a tree from source content
///
/// This is NOT a Salsa query because Tree doesn't implement the required traits.
/// Instead, we parse on-demand when needed by Salsa queries.
fn parse_tree(content: &str, language_id: &str) -> Option<Tree> {
    let mut parser = Parser::new();

    match language_id {
        "java" => {
            parser
                .set_language(&tree_sitter_java::LANGUAGE.into())
                .ok()?;
        }
        "kotlin" => {
            parser
                .set_language(&tree_sitter_kotlin::LANGUAGE.into())
                .ok()?;
        }
        _ => return None,
    }

    parser.parse(content, None)
}

/// Extract actual local variables from a method (uses cache)
///
/// This is the incremental version that uses the PSI-style cache.
/// Call this instead of the old extract_locals_with_type_ctx().
pub fn extract_method_locals_incremental(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
    workspace: &crate::workspace::Workspace,
) -> Vec<LocalVar> {
    // Step 1: Find method bounds (Salsa cached)
    let Some((method_start, method_end)) = find_enclosing_method_bounds(db, file, cursor_offset)
    else {
        return Vec::new();
    };

    // Step 2: Get metadata (Salsa cached)
    let metadata = extract_method_locals_metadata(db, file, method_start, method_end);

    // Step 3: Check PSI cache
    if let Some(cached) = workspace.get_cached_method_locals(metadata.content_hash) {
        tracing::debug!(
            content_hash = metadata.content_hash,
            local_count = cached.len(),
            "extract_method_locals_incremental: cache hit!"
        );
        return cached;
    }

    // Step 4: Cache miss - parse locals
    tracing::debug!(
        content_hash = metadata.content_hash,
        "extract_method_locals_incremental: cache miss, parsing..."
    );

    let locals = parse_method_locals(db, file, method_start, method_end);

    // Step 5: Cache the result
    workspace.cache_method_locals(metadata.content_hash, locals.clone());

    locals
}

/// Parse method locals (called on cache miss)
fn parse_method_locals(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    _method_start: usize,
    method_end: usize,
) -> Vec<LocalVar> {
    use crate::language::java::locals::extract_locals_with_type_ctx;
    use crate::language::java::type_ctx::SourceTypeCtx;
    use crate::language::java::{JavaContextExtractor, scope};

    let content = file.content(db);
    let language_id = file.language_id(db);

    // Only handle Java for now
    if language_id.as_ref() != "java" {
        return Vec::new();
    }

    // Parse tree
    let Some(tree) = parse_tree(content, language_id.as_ref()) else {
        return Vec::new();
    };

    let root = tree.root_node();

    let name_table = resolve_name_table_for_file(db, file);

    // Create a context extractor with cursor at the end of the method
    // Use method_end - 1 to ensure we're inside the method (not at the closing brace)
    // This ensures we extract all locals declared in the method
    let cursor_pos = method_end.saturating_sub(1);
    let ctx = JavaContextExtractor::new(content.to_string(), cursor_pos, name_table.clone());

    // Extract package and imports for type resolution
    let package = scope::extract_package(&ctx, root);
    let imports = scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::new(package, imports, name_table);

    // Pass None as cursor_node and let extract_locals_with_type_ctx find the method by offset
    // This is simpler and more reliable than trying to pass the right node
    extract_locals_with_type_ctx(&ctx, root, None, Some(&type_ctx))
}

/// Metadata about a file's structure (cached by Salsa)
///
/// This tracks high-level structure so we can detect when
/// specific parts of the file change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileStructureMetadata {
    /// Package name
    pub package: Option<Arc<str>>,
    /// Number of imports
    pub import_count: usize,
    /// Number of static imports
    pub static_import_count: usize,
    /// Number of top-level classes
    pub class_count: usize,
    /// Hash of the file structure
    pub structure_hash: u64,
}

/// Extract file structure metadata (cached by Salsa)
///
/// This is very cheap - just counts and hashes, no deep parsing.
/// When this changes, we know we need to re-parse.
#[salsa::tracked]
pub fn extract_file_structure(
    _db: &dyn crate::salsa_queries::Db,
    _file: SourceFile,
) -> FileStructureMetadata {
    // TODO: Implement actual structure extraction
    FileStructureMetadata {
        package: None,
        import_count: 0,
        static_import_count: 0,
        class_count: 0,
        structure_hash: 0,
    }
}

/// Metadata about a method's local variables (cached by Salsa)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodLocalsMetadata {
    /// Method start offset
    pub method_start: usize,
    /// Method end offset
    pub method_end: usize,
    /// Number of local variables
    pub local_count: usize,
    /// Hash of method content
    pub content_hash: u64,
}

/// Extract method locals metadata (cached by Salsa)
///
/// This is keyed by method offsets, so it only recomputes when
/// the specific method changes.
#[salsa::tracked]
pub fn extract_method_locals_metadata(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    method_start: usize,
    method_end: usize,
) -> MethodLocalsMetadata {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let content = file.content(db);
    let language_id = file.language_id(db);

    // Hash the method content for change detection
    let method_content = if method_end <= content.len() {
        &content[method_start..method_end]
    } else {
        ""
    };

    let mut hasher = DefaultHasher::new();
    method_content.hash(&mut hasher);
    let content_hash = hasher.finish();

    // Parse the tree and count locals
    let local_count = if let Some(tree) = parse_tree(content, language_id.as_ref()) {
        count_locals_in_range(
            tree.root_node(),
            content.as_bytes(),
            method_start,
            method_end,
        )
    } else {
        0
    };

    tracing::debug!(
        file_uri = file.file_id(db).as_str(),
        method_start = method_start,
        method_end = method_end,
        local_count = local_count,
        content_hash = content_hash,
        "extract_method_locals_metadata: counted locals"
    );

    MethodLocalsMetadata {
        method_start,
        method_end,
        local_count,
        content_hash,
    }
}

/// Count local variables in a specific range
fn count_locals_in_range(
    root: tree_sitter::Node,
    source: &[u8],
    start: usize,
    end: usize,
) -> usize {
    let mut count = 0;

    // Find the node that contains our range
    let mut cursor = root.walk();
    let mut current = root;

    // Navigate to the node at start position
    loop {
        let mut found_child = false;
        let children: Vec<_> = current.children(&mut cursor).collect();
        for child in children {
            if child.start_byte() <= start && end <= child.end_byte() {
                current = child;
                found_child = true;
                break;
            }
        }

        if !found_child {
            break;
        }
    }

    // Now traverse from this node
    traverse_and_count(&mut current.walk(), source, start, end, &mut count);

    count
}

/// Recursive traversal to count local variable declarations
fn traverse_and_count(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    start: usize,
    end: usize,
    count: &mut usize,
) {
    traverse_and_count_with_depth(cursor, source, start, end, count, 0);
}

/// Recursive traversal with depth limit to prevent stack overflow
fn traverse_and_count_with_depth(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    start: usize,
    end: usize,
    count: &mut usize,
    depth: usize,
) {
    // Prevent stack overflow with depth limit
    const MAX_DEPTH: usize = 500;
    if depth > MAX_DEPTH {
        tracing::warn!(
            "traverse_and_count: max depth {} exceeded, stopping",
            MAX_DEPTH
        );
        return;
    }

    let node = cursor.node();

    // Skip nodes completely outside our range
    if node.end_byte() < start || node.start_byte() > end {
        return;
    }

    // Count local variable declarations
    if node.kind() == "local_variable_declaration" {
        // Count the number of declarators (can have multiple: int x, y, z;)
        let mut child_cursor = node.walk();
        let declarator_count = node
            .children(&mut child_cursor)
            .filter(|n| n.kind() == "variable_declarator")
            .count();
        *count += declarator_count;
    }

    // Also count for loop init variables (for (int i = 0; ...))
    if node.kind() == "for_statement"
        && let Some(init) = node.child_by_field_name("init")
        && init.kind() == "local_variable_declaration"
    {
        let mut child_cursor = init.walk();
        let declarator_count = init
            .children(&mut child_cursor)
            .filter(|n| n.kind() == "variable_declarator")
            .count();
        *count += declarator_count;
    }

    // Also count enhanced for loop variables
    if node.kind() == "enhanced_for_statement" {
        // The loop variable is a local
        *count += 1;
    }

    // Also count catch parameters
    if node.kind() == "catch_clause"
        && let Some(_param) = node.child_by_field_name("parameter")
    {
        *count += 1;
    }

    // Recurse into children with incremented depth
    if cursor.goto_first_child() {
        loop {
            traverse_and_count_with_depth(cursor, source, start, end, count, depth + 1);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// Metadata about a class's members (cached by Salsa)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClassMembersMetadata {
    /// Class start offset
    pub class_start: usize,
    /// Class end offset
    pub class_end: usize,
    /// Number of methods
    pub method_count: usize,
    /// Number of fields
    pub field_count: usize,
    /// Hash of class content
    pub content_hash: u64,
}

/// Extract class members metadata (cached by Salsa)
///
/// This is keyed by class offsets, so it only recomputes when
/// the specific class changes.
#[salsa::tracked]
pub fn extract_class_members_metadata(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    class_start: usize,
    class_end: usize,
) -> ClassMembersMetadata {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let content = file.content(db);
    let language_id = file.language_id(db);

    // Hash the class content for change detection
    let class_content = if class_end <= content.len() {
        &content[class_start..class_end]
    } else {
        ""
    };

    let mut hasher = DefaultHasher::new();
    class_content.hash(&mut hasher);
    let content_hash = hasher.finish();

    // Parse the tree and count methods/fields
    let (method_count, field_count) = if let Some(tree) = parse_tree(content, language_id.as_ref())
    {
        count_members_in_range(tree.root_node(), content.as_bytes(), class_start, class_end)
    } else {
        (0, 0)
    };

    tracing::debug!(
        file_uri = file.file_id(db).as_str(),
        class_start = class_start,
        class_end = class_end,
        method_count = method_count,
        field_count = field_count,
        content_hash = content_hash,
        "extract_class_members_metadata: counted members"
    );

    ClassMembersMetadata {
        class_start,
        class_end,
        method_count,
        field_count,
        content_hash,
    }
}

/// Count methods and fields in a specific range
fn count_members_in_range(
    root: tree_sitter::Node,
    source: &[u8],
    start: usize,
    end: usize,
) -> (usize, usize) {
    let mut method_count = 0;
    let mut field_count = 0;

    // Find the node that contains our range
    let mut cursor = root.walk();
    let mut current = root;

    // Navigate to the node at start position
    loop {
        let mut found_child = false;
        let children: Vec<_> = current.children(&mut cursor).collect();
        for child in children {
            if child.start_byte() <= start && end <= child.end_byte() {
                current = child;
                found_child = true;
                break;
            }
        }

        if !found_child {
            break;
        }
    }

    // Now traverse from this node
    traverse_and_count_members(
        &mut current.walk(),
        source,
        start,
        end,
        &mut method_count,
        &mut field_count,
    );

    (method_count, field_count)
}

/// Recursive traversal to count methods and fields
fn traverse_and_count_members(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    start: usize,
    end: usize,
    method_count: &mut usize,
    field_count: &mut usize,
) {
    traverse_and_count_members_with_depth(cursor, source, start, end, method_count, field_count, 0);
}

/// Recursive traversal with depth limit to prevent stack overflow
fn traverse_and_count_members_with_depth(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    start: usize,
    end: usize,
    method_count: &mut usize,
    field_count: &mut usize,
    depth: usize,
) {
    // Prevent stack overflow with depth limit
    const MAX_DEPTH: usize = 500;
    if depth > MAX_DEPTH {
        tracing::warn!(
            "traverse_and_count_members: max depth {} exceeded, stopping",
            MAX_DEPTH
        );
        return;
    }

    let node = cursor.node();

    // Skip nodes completely outside our range
    if node.end_byte() < start || node.start_byte() > end {
        return;
    }

    // Count methods
    match node.kind() {
        "method_declaration" | "constructor_declaration" => {
            *method_count += 1;
        }
        "field_declaration" => {
            // Count the number of declarators (can have multiple: int x, y, z;)
            let mut child_cursor = node.walk();
            let declarator_count = node
                .children(&mut child_cursor)
                .filter(|n| n.kind() == "variable_declarator")
                .count();
            *field_count += declarator_count;
        }
        _ => {}
    }

    // Recurse into children with incremented depth
    if cursor.goto_first_child() {
        loop {
            traverse_and_count_members_with_depth(
                cursor,
                source,
                start,
                end,
                method_count,
                field_count,
                depth + 1,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// Find the enclosing method bounds for a cursor position (cached by Salsa)
///
/// Returns (method_start, method_end) if cursor is inside a method.
/// This is cached per (file, cursor_offset) so it's very fast.
#[salsa::tracked]
pub fn find_enclosing_method_bounds(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> Option<(usize, usize)> {
    use tree_sitter_utils::traversal::{ancestor_of_kinds, find_node_by_offset};

    let content = file.content(db);
    let language_id = file.language_id(db);

    // Parse the tree
    let tree = parse_tree(content, language_id.as_ref())?;
    let root = tree.root_node();

    // Find any node at the cursor position first
    let node_at_cursor = find_node_by_offset(root, "identifier", cursor_offset)
        .or_else(|| find_node_by_offset(root, "block", cursor_offset))
        .or_else(|| {
            // Fallback: find the deepest node at cursor
            find_deepest_node_at_offset(root, cursor_offset)
        })?;

    // Walk up to find method or constructor
    let method_node = ancestor_of_kinds(
        node_at_cursor,
        &["method_declaration", "constructor_declaration"],
    )?;

    let start = method_node.start_byte();
    let end = method_node.end_byte();

    tracing::debug!(
        file_uri = file.file_id(db).as_str(),
        cursor_offset = cursor_offset,
        method_start = start,
        method_end = end,
        "find_enclosing_method_bounds: found method"
    );

    Some((start, end))
}

/// Find the deepest node at a given offset (fallback when specific kinds don't match)
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

/// Find a node of specific kinds at a given offset
fn find_node_at_offset<'a>(
    root: tree_sitter::Node<'a>,
    offset: usize,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    use tree_sitter_utils::traversal::find_node_by_offset;

    // Try each kind directly
    for kind in kinds {
        if let Some(node) = find_node_by_offset(root, kind, offset) {
            return Some(node);
        }
    }

    // Fallback: find any node at offset and walk up
    use tree_sitter_utils::traversal::ancestor_of_kinds;
    let node_at_offset = find_node_by_offset(root, "identifier", offset)
        .or_else(|| find_node_by_offset(root, "block", offset))
        .or_else(|| find_deepest_node_at_offset(root, offset))?;

    ancestor_of_kinds(node_at_offset, kinds)
}

/// Find the enclosing class bounds for a cursor position (cached by Salsa)
///
/// Returns (class_name, class_start, class_end) if cursor is inside a class.
/// This is cached per (file, cursor_offset) so it's very fast.
#[salsa::tracked]
pub fn find_enclosing_class_bounds(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
) -> Option<(Arc<str>, usize, usize)> {
    use tree_sitter_utils::traversal::{ancestor_of_kinds, find_node_by_offset};

    let content = file.content(db);
    let language_id = file.language_id(db);

    // Parse the tree
    let tree = parse_tree(content, language_id.as_ref())?;
    let root = tree.root_node();

    // Find any node at the cursor position first
    let node_at_cursor = find_node_by_offset(root, "identifier", cursor_offset)
        .or_else(|| find_node_by_offset(root, "block", cursor_offset))
        .or_else(|| find_deepest_node_at_offset(root, cursor_offset))?;

    // Walk up to find class/interface/enum/record
    let class_node = ancestor_of_kinds(
        node_at_cursor,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
    )?;

    // Extract the class name
    let class_name = class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(content.as_bytes()).ok())
        .map(Arc::from)
        .unwrap_or_else(|| Arc::from("Unknown"));

    let start = class_node.start_byte();
    let end = class_node.end_byte();

    tracing::debug!(
        file_uri = file.file_id(db).as_str(),
        cursor_offset = cursor_offset,
        class_name = %class_name,
        class_start = start,
        class_end = end,
        "find_enclosing_class_bounds: found class"
    );

    Some((class_name, start, end))
}

/// Extract actual class members from a class (uses cache)
///
/// This is the incremental version that uses the PSI-style cache.
/// Call this instead of extract_type_members_with_synthetics().
pub fn extract_class_members_incremental(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    cursor_offset: usize,
    workspace: &crate::workspace::Workspace,
) -> std::collections::HashMap<Arc<str>, crate::semantic::context::CurrentClassMember> {
    use std::collections::HashMap;

    // Step 1: Find class bounds (Salsa cached)
    let Some((class_name, class_start, class_end)) =
        find_enclosing_class_bounds(db, file, cursor_offset)
    else {
        return HashMap::new();
    };

    // Step 2: Get metadata (Salsa cached)
    let metadata = extract_class_members_metadata(db, file, class_start, class_end);

    // Step 3: Check PSI cache
    if let Some(cached) = workspace.get_cached_class_members(metadata.content_hash) {
        tracing::debug!(
            content_hash = metadata.content_hash,
            member_count = cached.len(),
            class_name = %class_name,
            "extract_class_members_incremental: cache hit!"
        );
        return cached;
    }

    // Step 4: Cache miss - parse members
    tracing::debug!(
        content_hash = metadata.content_hash,
        class_name = %class_name,
        "extract_class_members_incremental: cache miss, parsing..."
    );

    let members = parse_class_members(db, file, class_start, class_end);

    // Step 5: Cache the result
    workspace.cache_class_members(metadata.content_hash, members.clone());

    members
}

/// Parse class members (called on cache miss)
fn parse_class_members(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
    class_start: usize,
    _class_end: usize,
) -> std::collections::HashMap<Arc<str>, crate::semantic::context::CurrentClassMember> {
    use crate::language::java::synthetic::extract_type_members_with_synthetics;
    use crate::language::java::type_ctx::SourceTypeCtx;
    use crate::language::java::{JavaContextExtractor, scope};
    use std::collections::HashMap;

    let content = file.content(db);
    let language_id = file.language_id(db);

    // Only handle Java for now
    if language_id.as_ref() != "java" {
        return HashMap::new();
    }

    // Parse tree
    let Some(tree) = parse_tree(content, language_id.as_ref()) else {
        return HashMap::new();
    };

    let root = tree.root_node();

    let name_table = resolve_name_table_for_file(db, file);

    // Create a minimal context extractor for parsing
    let ctx = JavaContextExtractor::for_indexing(content, name_table.clone());

    // Extract package and imports for type resolution
    let package = scope::extract_package(&ctx, root);
    let imports = scope::extract_imports(&ctx, root);
    let type_ctx = SourceTypeCtx::new(package, imports, name_table);

    // Find the class node at class_start
    let class_node = find_node_at_offset(
        root,
        class_start,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
    );

    let Some(class_node) = class_node else {
        return HashMap::new();
    };

    // Extract members with synthetics (Lombok, etc.)
    let members = extract_type_members_with_synthetics(&ctx, class_node, &type_ctx, None);

    // Convert Vec to HashMap keyed by member name
    members.into_iter().map(|m| (m.name(), m)).collect()
}

fn resolve_name_table_for_file(
    db: &dyn crate::salsa_queries::Db,
    file: SourceFile,
) -> Option<Arc<crate::index::NameTable>> {
    let workspace_index = db.workspace_index();
    let index = workspace_index.read();
    let _ = file;
    Some(index.build_name_table(crate::index::IndexScope {
        module: crate::index::ModuleId::ROOT,
    }))
}
