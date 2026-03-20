use super::Db;
use crate::index::ClassMetadata;
use crate::salsa_db::SourceFile;
use crate::salsa_queries::context::{
    CompletionContextData, CursorLocationData, line_col_to_offset,
};
use crate::salsa_queries::hints::{InlayHintData, InlayHintKindData};
use crate::salsa_queries::symbols::{ResolvedSymbolData, SymbolKind};
/// Kotlin-specific Salsa queries
///
/// These queries handle Kotlin-specific parsing and analysis with full parity to Java.
use std::sync::Arc;

/// Parse Kotlin source and extract class metadata
///
/// TODO: Implement full Kotlin parsing with incremental support
pub fn parse_kotlin_classes(_db: &dyn Db, _file: SourceFile) -> Vec<ClassMetadata> {
    // Placeholder - return empty for now
    vec![]
}

/// Extract Kotlin package declaration
///
/// TODO: Implement Kotlin package extraction
pub fn extract_kotlin_package(_db: &dyn Db, _file: SourceFile) -> Option<Arc<str>> {
    None
}

/// Extract Kotlin imports
///
/// TODO: Implement Kotlin import extraction
pub fn extract_kotlin_imports(_db: &dyn Db, _file: SourceFile) -> Vec<Arc<str>> {
    vec![]
}

// ============================================================================
// Kotlin Completion Context Extraction
// ============================================================================

/// Extract Kotlin completion context (CACHED)
#[salsa::tracked]
pub fn extract_kotlin_completion_context(
    db: &dyn Db,
    file: SourceFile,
    line: u32,
    character: u32,
    trigger_char: Option<char>,
) -> Arc<CompletionContextData> {
    let content = file.content(db);
    let Some(offset) = line_col_to_offset(content, line, character) else {
        return Arc::new(empty_context(db, file));
    };

    // Check if in comment
    if is_in_comment(content, offset) {
        return Arc::new(empty_context(db, file));
    }

    // Parse tree
    let Some(tree) = super::parse::parse_tree_for_language(content, "kotlin") else {
        return Arc::new(empty_context(db, file));
    };

    let root = tree.root_node();

    // Find cursor node
    let cursor_node = root.named_descendant_for_byte_range(offset.saturating_sub(1), offset);

    // Determine location
    let location = determine_kotlin_cursor_location(content, cursor_node, offset, trigger_char);

    // Extract query string
    let query = extract_query_string(content, offset, &location);

    // Extract scope information (all cached separately)
    let package = None; // TODO: extract_kotlin_package(db, file);
    let imports: Arc<Vec<Arc<str>>> = Arc::new(Vec::new()); // TODO: extract_kotlin_imports(db, file);
    let enclosing_class = find_kotlin_enclosing_class_name(db, file, offset);
    let enclosing_internal_name = build_internal_name(&package, &enclosing_class);

    // Count locals (cached)
    let local_var_count = count_kotlin_locals_in_scope(db, file, offset);

    // Compute content hash for the relevant scope
    let content_hash = super::context::compute_scope_content_hash(db, file, offset);

    Arc::new(CompletionContextData {
        location,
        query,
        cursor_offset: offset,
        enclosing_class,
        enclosing_internal_name,
        enclosing_package: package,
        local_var_count,
        import_count: imports.len(),
        static_import_count: 0,
        content_hash,
        file_uri: Arc::from(file.file_id(db).as_str()),
        language_id: Arc::from("kotlin"),
    })
}

fn determine_kotlin_cursor_location(
    content: &str,
    cursor_node: Option<tree_sitter::Node>,
    offset: usize,
    trigger_char: Option<char>,
) -> CursorLocationData {
    let source = content.as_bytes();

    let Some(node) = cursor_node else {
        return fallback_location(content, offset, trigger_char);
    };

    // Check for member access (dot or safe call trigger)
    if matches!(trigger_char, Some('.') | Some('?')) {
        return handle_member_access(content, offset);
    }

    // Walk up the tree to find semantic context
    let mut current = node;
    loop {
        match current.kind() {
            "import_header" => {
                return handle_import(current, source);
            }
            "navigation_expression" => {
                return handle_navigation(current, source);
            }
            "simple_identifier" | "identifier" => {
                // Check parent context
                if let Some(parent) = current.parent() {
                    match parent.kind() {
                        "import_header" => {
                            return handle_import(parent, source);
                        }
                        "navigation_expression" => {
                            return handle_navigation(parent, source);
                        }
                        _ => {}
                    }
                }

                // Default to expression
                let text = current.utf8_text(source).unwrap_or("");
                return CursorLocationData::Expression {
                    prefix: Arc::from(text),
                };
            }
            _ => {}
        }

        // Move to parent
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    fallback_location(content, offset, trigger_char)
}

fn handle_import(node: tree_sitter::Node, source: &[u8]) -> CursorLocationData {
    let text = node.utf8_text(source).unwrap_or("");
    let prefix = text.trim_start_matches("import").trim().to_string();

    CursorLocationData::Import {
        prefix: Arc::from(prefix),
    }
}

fn handle_navigation(node: tree_sitter::Node, source: &[u8]) -> CursorLocationData {
    // Get receiver (first child)
    let receiver = if let Some(recv) = node.named_child(0) {
        recv.utf8_text(source).unwrap_or("").to_string()
    } else {
        String::new()
    };

    // Get member name (from navigation_suffix)
    let member = if let Some(suffix) = node.child_by_field_name("suffix") {
        let mut walker = suffix.walk();
        suffix
            .children(&mut walker)
            .find(|n| n.kind() != "." && n.kind() != "?.")
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    };

    // Check if receiver looks like a class name (starts with uppercase)
    if receiver.chars().next().is_some_and(|c| c.is_uppercase()) {
        CursorLocationData::StaticAccess {
            class_internal_name: Arc::from(receiver.replace('.', "/")),
            member_prefix: Arc::from(member),
        }
    } else {
        CursorLocationData::MemberAccess {
            receiver_expr: Arc::from(receiver),
            member_prefix: Arc::from(member),
            receiver_type_hint: None,
            arguments: None,
        }
    }
}

fn handle_member_access(content: &str, offset: usize) -> CursorLocationData {
    // Find the line containing the cursor
    let line_start = content[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &content[line_start..offset];

    // Handle both . and ?.
    let normalized = line.replace("?.", ".");

    // Find the last token before the dot
    let receiver = normalized
        .trim_end_matches('.')
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .next_back()
        .unwrap_or("")
        .to_string();

    if receiver.chars().next().is_some_and(|c| c.is_uppercase()) {
        CursorLocationData::StaticAccess {
            class_internal_name: Arc::from(receiver.replace('.', "/")),
            member_prefix: Arc::from(""),
        }
    } else {
        CursorLocationData::MemberAccess {
            receiver_expr: Arc::from(receiver),
            member_prefix: Arc::from(""),
            receiver_type_hint: None,
            arguments: None,
        }
    }
}

fn fallback_location(
    content: &str,
    offset: usize,
    _trigger_char: Option<char>,
) -> CursorLocationData {
    // Extract the word at cursor
    let before = &content[..offset];
    let after = &content[offset..];

    let word_start = before
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| i + 1)
        .unwrap_or(0);

    let word_end = after
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(after.len());

    let word = format!("{}{}", &before[word_start..], &after[..word_end]);

    CursorLocationData::Expression {
        prefix: Arc::from(word),
    }
}

fn extract_query_string(_content: &str, _offset: usize, location: &CursorLocationData) -> Arc<str> {
    match location {
        CursorLocationData::Expression { prefix } => Arc::clone(prefix),
        CursorLocationData::MemberAccess { member_prefix, .. } => Arc::clone(member_prefix),
        CursorLocationData::StaticAccess { member_prefix, .. } => Arc::clone(member_prefix),
        CursorLocationData::Import { prefix } => Arc::from(prefix.rsplit('.').next().unwrap_or("")),
        _ => Arc::from(""),
    }
}

fn is_in_comment(content: &str, offset: usize) -> bool {
    let before = &content[..offset];

    // Check for line comment
    if let Some(line_start) = before.rfind('\n') {
        let line = &before[line_start + 1..];
        if line.trim_start().starts_with("//") {
            return true;
        }
    }

    // Check for block comment
    let last_block_start = before.rfind("/*");
    let last_block_end = before.rfind("*/");

    match (last_block_start, last_block_end) {
        (Some(start), Some(end)) => start > end,
        (Some(_), None) => true,
        _ => false,
    }
}

fn empty_context(db: &dyn Db, file: SourceFile) -> CompletionContextData {
    CompletionContextData {
        location: CursorLocationData::Unknown,
        query: Arc::from(""),
        cursor_offset: 0,
        enclosing_class: None,
        enclosing_internal_name: None,
        enclosing_package: None,
        local_var_count: 0,
        import_count: 0,
        static_import_count: 0,
        content_hash: 0,
        file_uri: Arc::from(file.file_id(db).as_str()),
        language_id: Arc::from("kotlin"),
    }
}

// ============================================================================
// Kotlin Scope Queries
// ============================================================================

/// Find the enclosing class name (CACHED)
#[salsa::tracked]
pub fn find_kotlin_enclosing_class_name(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
) -> Option<Arc<str>> {
    let content = file.content(db);
    let tree = super::parse::parse_tree_for_language(content, "kotlin")?;
    let root = tree.root_node();

    // Find node at offset
    let node = root.named_descendant_for_byte_range(offset, offset + 1)?;

    // Walk up to find class/object declaration
    let mut current = node;
    loop {
        match current.kind() {
            "class_declaration" | "object_declaration" | "companion_object" => {
                // Find type_identifier child
                let mut walker = current.walk();
                for child in current.children(&mut walker) {
                    if child.kind() == "type_identifier" {
                        return child.utf8_text(content.as_bytes()).ok().map(Arc::from);
                    }
                }
                return None;
            }
            _ => {}
        }

        current = current.parent()?;
    }
}

/// Count local variables in scope (CACHED)
#[salsa::tracked]
pub fn count_kotlin_locals_in_scope(db: &dyn Db, file: SourceFile, offset: usize) -> usize {
    // Get method bounds
    let Some((method_start, method_end)) = find_kotlin_enclosing_function_bounds(db, file, offset)
    else {
        return 0;
    };

    // Count locals in range
    let content = file.content(db);
    let tree = super::parse::parse_tree_for_language(content, "kotlin");
    let Some(tree) = tree else {
        return 0;
    };

    count_kotlin_locals_in_range(tree.root_node(), method_start, method_end)
}

#[salsa::tracked]
fn find_kotlin_enclosing_function_bounds(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
) -> Option<(usize, usize)> {
    let content = file.content(db);
    let tree = super::parse::parse_tree_for_language(content, "kotlin")?;
    let root = tree.root_node();

    // Find node at offset
    let node = root.named_descendant_for_byte_range(offset, offset + 1)?;

    // Walk up to find function
    let mut current = node;
    loop {
        match current.kind() {
            "function_declaration" | "function_literal" | "anonymous_function" => {
                return Some((current.start_byte(), current.end_byte()));
            }
            _ => {}
        }

        current = current.parent()?;
    }
}

fn count_kotlin_locals_in_range(root: tree_sitter::Node, start: usize, end: usize) -> usize {
    let mut count = 0;
    let mut cursor = root.walk();

    count_kotlin_locals_recursive(&mut cursor, start, end, &mut count);

    count
}

fn count_kotlin_locals_recursive(
    cursor: &mut tree_sitter::TreeCursor,
    start: usize,
    end: usize,
    count: &mut usize,
) {
    let node = cursor.node();

    // Skip nodes outside range
    if node.end_byte() < start || node.start_byte() > end {
        return;
    }

    // Count property declarations (val/var)
    if node.kind() == "property_declaration" {
        *count += 1;
    }

    // Recurse into children
    if cursor.goto_first_child() {
        loop {
            count_kotlin_locals_recursive(cursor, start, end, count);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn build_internal_name(package: &Option<Arc<str>>, class: &Option<Arc<str>>) -> Option<Arc<str>> {
    match (package, class) {
        (Some(pkg), Some(cls)) => Some(Arc::from(format!("{}/{}", pkg, cls))),
        (None, Some(cls)) => Some(Arc::clone(cls)),
        _ => None,
    }
}

// ============================================================================
// Kotlin Symbol Resolution
// ============================================================================

/// Resolve Kotlin symbol at position (CACHED)
#[salsa::tracked]
pub fn resolve_kotlin_symbol(
    db: &dyn Db,
    file: SourceFile,
    line: u32,
    character: u32,
) -> Option<Arc<ResolvedSymbolData>> {
    let content = file.content(db);
    let offset = line_col_to_offset(content, line, character)?;

    // Get context
    let context = extract_kotlin_completion_context(db, file, line, character, None);

    // Resolve based on location
    match &context.location {
        CursorLocationData::Expression { prefix } => {
            resolve_kotlin_expression_symbol(db, file, Arc::clone(prefix), offset)
        }
        CursorLocationData::MemberAccess {
            receiver_expr,
            member_prefix,
            ..
        } => resolve_kotlin_member_symbol(
            db,
            file,
            Arc::clone(receiver_expr),
            Arc::clone(member_prefix),
        ),
        CursorLocationData::StaticAccess {
            class_internal_name,
            member_prefix,
        } => Some(Arc::new(ResolvedSymbolData {
            kind: SymbolKind::Class,
            target_internal_name: Arc::clone(class_internal_name),
            member_name: Some(Arc::clone(member_prefix)),
            descriptor: None,
        })),
        CursorLocationData::Import { prefix } => {
            let internal = prefix.replace('.', "/");
            Some(Arc::new(ResolvedSymbolData {
                kind: SymbolKind::Class,
                target_internal_name: Arc::from(internal),
                member_name: None,
                descriptor: None,
            }))
        }
        _ => None,
    }
}

#[salsa::tracked]
fn resolve_kotlin_expression_symbol(
    db: &dyn Db,
    file: SourceFile,
    symbol_name: Arc<str>,
    offset: usize,
) -> Option<Arc<ResolvedSymbolData>> {
    // Check if it's a local variable
    if is_kotlin_local_variable(db, file, Arc::clone(&symbol_name), offset) {
        return Some(Arc::new(ResolvedSymbolData {
            kind: SymbolKind::LocalVariable,
            target_internal_name: Arc::from(""),
            member_name: Some(symbol_name),
            descriptor: None,
        }));
    }

    // TODO: Check properties, functions, imports
    None
}

#[salsa::tracked]
fn resolve_kotlin_member_symbol(
    _db: &dyn Db,
    _file: SourceFile,
    _receiver_expr: Arc<str>,
    _member_name: Arc<str>,
) -> Option<Arc<ResolvedSymbolData>> {
    // TODO: Implement member resolution
    None
}

/// Check if a symbol is a local variable (CACHED)
#[salsa::tracked]
pub fn is_kotlin_local_variable(
    db: &dyn Db,
    file: SourceFile,
    symbol_name: Arc<str>,
    offset: usize,
) -> bool {
    super::symbols::find_local_variable_declaration(db, file, symbol_name, offset).is_some()
}

// ============================================================================
// Kotlin Inlay Hints
// ============================================================================

/// Compute Kotlin inlay hints (CACHED)
#[salsa::tracked]
pub fn compute_kotlin_inlay_hints(
    db: &dyn Db,
    file: SourceFile,
    start_line: u32,
    start_char: u32,
    end_line: u32,
    end_char: u32,
) -> Arc<Vec<InlayHintData>> {
    let content = file.content(db);
    let Some(start_offset) = line_col_to_offset(content, start_line, start_char) else {
        return Arc::new(Vec::new());
    };
    let Some(end_offset) = line_col_to_offset(content, end_line, end_char) else {
        return Arc::new(Vec::new());
    };

    let mut hints = Vec::new();

    // Find variable declarations (cached)
    let var_decls =
        super::hints::find_variable_declarations_in_range(db, file, start_offset, end_offset);

    // Add type hints for variables without explicit types
    for decl in var_decls.iter() {
        if !decl.has_explicit_type
            && let Some(inferred_type) = infer_kotlin_variable_type(db, file, decl.offset)
        {
            hints.push(InlayHintData {
                offset: decl.offset + decl.name.len(),
                label: format!(": {}", inferred_type).into(),
                kind: InlayHintKindData::Type,
            });
        }
    }

    // TODO: Add parameter hints for function calls

    Arc::new(hints)
}

/// Infer Kotlin variable type (CACHED)
#[salsa::tracked]
pub fn infer_kotlin_variable_type(
    db: &dyn Db,
    file: SourceFile,
    decl_offset: usize,
) -> Option<Arc<str>> {
    let content = file.content(db);

    // Parse tree
    let tree = super::parse::parse_tree_for_language(content, "kotlin")?;
    let root = tree.root_node();

    // Find the property declaration at this offset
    let node = root.named_descendant_for_byte_range(decl_offset, decl_offset + 1)?;

    // Find the property_declaration ancestor
    let mut current = node;
    let prop_decl = loop {
        if current.kind() == "property_declaration" {
            break Some(current);
        }
        current = current.parent()?;
    }?;

    // Get the initializer
    let init = prop_decl.child_by_field_name("value")?;

    // Infer type from initializer
    infer_kotlin_type_from_expression(init, content.as_bytes())
}

fn infer_kotlin_type_from_expression(expr: tree_sitter::Node, source: &[u8]) -> Option<Arc<str>> {
    match expr.kind() {
        "integer_literal" => Some(Arc::from("Int")),
        "real_literal" => Some(Arc::from("Double")),
        "string_literal" => Some(Arc::from("String")),
        "boolean_literal" => Some(Arc::from("Boolean")),
        "null_literal" => Some(Arc::from("Any?")),
        "call_expression" => {
            // Try to extract constructor call: Type()
            if let Some(callee) = expr.child_by_field_name("callee")
                && let Ok(text) = callee.utf8_text(source)
            {
                // If starts with uppercase, it's likely a constructor
                if text.chars().next().is_some_and(|c| c.is_uppercase()) {
                    return Some(Arc::from(text));
                }
            }
            None
        }
        _ => None,
    }
}
