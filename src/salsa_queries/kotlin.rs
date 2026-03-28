use super::Db;
use crate::index::{ClassMetadata, IndexScope, MethodSummary, ModuleId};
use crate::language::kotlin::{extract_kotlin_semantic_context_for_test, kotlin_type_to_internal};
use crate::language::rope_utils::rope_byte_offset_to_line_col;
use crate::language::ts_utils::capture_text;
use crate::salsa_db::SourceFile;
use crate::salsa_queries::context::{
    CompletionContextData, CursorLocationData, line_col_to_offset,
};
use crate::salsa_queries::hints::{InlayHintData, InlayHintKindData};
use crate::salsa_queries::symbols::{ResolvedSymbolData, SymbolKind};
use crate::semantic::SemanticContext;
use crate::semantic::types::parse_single_type_to_internal;
/// Kotlin-specific Salsa queries
///
/// These queries handle Kotlin-specific parsing and analysis with full parity to Java.
use ropey::Rope;
use std::sync::Arc;
use tree_sitter::Node;
use tree_sitter::Query;

/// Parse Kotlin source and extract class metadata
pub fn parse_kotlin_classes(db: &dyn Db, file: SourceFile) -> Vec<ClassMetadata> {
    let content = file.content(db);
    let file_id = file.file_id(db);
    let origin = crate::index::ClassOrigin::SourceFile(Arc::from(file_id.as_str()));
    crate::index::source::parse_kotlin_source(content, origin)
}

/// Extract Kotlin package declaration
pub fn extract_kotlin_package(db: &dyn Db, file: SourceFile) -> Option<Arc<str>> {
    let content = file.content(db);
    let tree = super::parse::parse_tree(db, file)?;
    let root = tree.root_node();
    let q = Query::new(
        &tree_sitter_kotlin::LANGUAGE.into(),
        r#"(package_header (identifier) @pkg)"#,
    )
    .ok()?;
    let idx = q.capture_index_for_name("pkg")?;
    let results = crate::language::ts_utils::run_query(&q, root, content.as_bytes(), None);
    let pkg = results
        .first()
        .and_then(|caps| capture_text(caps, idx, content.as_bytes()))?;
    Some(Arc::from(pkg.replace('.', "/").as_str()))
}

/// Extract Kotlin imports
pub fn extract_kotlin_imports(db: &dyn Db, file: SourceFile) -> Vec<Arc<str>> {
    let content = file.content(db);
    let Some(tree) = super::parse::parse_tree(db, file) else {
        return vec![];
    };
    let root = tree.root_node();
    let q = match Query::new(
        &tree_sitter_kotlin::LANGUAGE.into(),
        r#"(import_header) @import"#,
    ) {
        Ok(q) => q,
        Err(_) => return vec![],
    };
    let idx = q.capture_index_for_name("import").unwrap();

    crate::language::ts_utils::run_query(&q, root, content.as_bytes(), None)
        .into_iter()
        .filter_map(|captures| {
            let text = capture_text(&captures, idx, content.as_bytes())?;
            let cleaned = text.trim_start_matches("import").trim();
            if cleaned.is_empty() {
                None
            } else {
                Some(Arc::from(cleaned))
            }
        })
        .collect()
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
    let Some(tree) = super::parse::parse_tree(db, file) else {
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
    let package = extract_kotlin_package(db, file);
    let imports = Arc::new(extract_kotlin_imports(db, file));
    let enclosing_class = find_kotlin_enclosing_class_name(db, file, offset);
    let enclosing_internal_name = build_internal_name(&package, &enclosing_class);

    // Count locals (cached)
    let local_var_count = count_kotlin_locals_in_scope(db, file, offset);

    // Compute content hash for the relevant scope
    let content_hash = super::context::compute_scope_content_hash(db, file, offset);

    Arc::new(CompletionContextData {
        location,
        java_module_context: None,
        query,
        cursor_offset: offset,
        enclosing_class,
        enclosing_internal_name,
        enclosing_class_chain: vec![],
        enclosing_package: package,
        local_var_count,
        import_count: imports.len(),
        static_import_count: 0,
        statement_labels: vec![],
        char_after_cursor: None,
        is_class_member_position: false,
        functional_target_hint: None,
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
        let internal = receiver.replace('.', "/");
        CursorLocationData::MemberAccess {
            receiver_kind: crate::salsa_queries::context::AccessReceiverKindData::Type {
                class_internal_name: Arc::from(internal.as_str()),
            },
            receiver_expr: Arc::from(receiver.as_str()),
            member_prefix: Arc::from(member),
            receiver_type_hint: Some(Arc::from(internal.as_str())),
            arguments: None,
        }
    } else {
        CursorLocationData::MemberAccess {
            receiver_kind: crate::salsa_queries::context::AccessReceiverKindData::Unknown,
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
        CursorLocationData::MemberAccess {
            receiver_kind: crate::salsa_queries::context::AccessReceiverKindData::Type {
                class_internal_name: Arc::from(receiver.replace('.', "/")),
            },
            receiver_expr: Arc::from(receiver.clone()),
            member_prefix: Arc::from(""),
            receiver_type_hint: Some(Arc::from(receiver.replace('.', "/"))),
            arguments: None,
        }
    } else {
        CursorLocationData::MemberAccess {
            receiver_kind: crate::salsa_queries::context::AccessReceiverKindData::Unknown,
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
    let tree = super::parse::parse_tree(db, file)?;
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
    let tree = super::parse::parse_tree(db, file);
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
    let tree = super::parse::parse_tree(db, file)?;
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

    if let Some((receiver_expr, member_name)) = kotlin_member_access_at_offset(content, offset) {
        return resolve_kotlin_member_symbol(
            db,
            file,
            Arc::from(receiver_expr),
            Arc::from(member_name),
            offset,
        );
    }

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
            receiver_kind,
            ..
        } => match receiver_kind {
            crate::salsa_queries::context::AccessReceiverKindData::Type {
                class_internal_name,
            } => Some(Arc::new(ResolvedSymbolData {
                kind: SymbolKind::Class,
                target_internal_name: Arc::clone(class_internal_name),
                member_name: Some(Arc::clone(member_prefix)),
                descriptor: None,
            })),
            _ => resolve_kotlin_member_symbol(
                db,
                file,
                Arc::clone(receiver_expr),
                Arc::clone(member_prefix),
                offset,
            ),
        },
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

    let ctx = build_kotlin_semantic_context(db, file, offset)?;

    if let Some(enclosing) = ctx.enclosing_internal_name.as_ref() {
        if let Some(field) = find_kotlin_field_in_owner(db, file, enclosing, symbol_name.as_ref()) {
            return Some(Arc::new(ResolvedSymbolData {
                kind: SymbolKind::Field,
                target_internal_name: Arc::clone(enclosing),
                member_name: Some(Arc::clone(&field.name)),
                descriptor: Some(Arc::clone(&field.descriptor)),
            }));
        }

        if let Some(method) =
            find_kotlin_method_in_owner(db, file, enclosing, symbol_name.as_ref(), None)
        {
            return Some(Arc::new(ResolvedSymbolData {
                kind: SymbolKind::Method,
                target_internal_name: Arc::clone(enclosing),
                member_name: Some(Arc::clone(&method.name)),
                descriptor: Some(method.desc()),
            }));
        }
    }

    resolve_kotlin_type_name(db, file, &ctx, symbol_name.as_ref()).map(|internal| {
        Arc::new(ResolvedSymbolData {
            kind: SymbolKind::Class,
            target_internal_name: internal,
            member_name: None,
            descriptor: None,
        })
    })
}

#[salsa::tracked]
fn resolve_kotlin_member_symbol(
    db: &dyn Db,
    file: SourceFile,
    receiver_expr: Arc<str>,
    member_name: Arc<str>,
    offset: usize,
) -> Option<Arc<ResolvedSymbolData>> {
    let ctx = build_kotlin_semantic_context(db, file, offset)?;
    let receiver_internal =
        infer_kotlin_receiver_type(db, file, &ctx, receiver_expr.as_ref(), offset)?;

    if let Some(field) =
        find_kotlin_field_in_owner(db, file, receiver_internal.as_ref(), member_name.as_ref())
    {
        return Some(Arc::new(ResolvedSymbolData {
            kind: SymbolKind::Field,
            target_internal_name: Arc::clone(&receiver_internal),
            member_name: Some(Arc::clone(&field.name)),
            descriptor: Some(Arc::clone(&field.descriptor)),
        }));
    }

    if let Some(method) = find_kotlin_method_in_owner(
        db,
        file,
        receiver_internal.as_ref(),
        member_name.as_ref(),
        None,
    ) {
        return Some(Arc::new(ResolvedSymbolData {
            kind: SymbolKind::Method,
            target_internal_name: receiver_internal,
            member_name: Some(Arc::clone(&method.name)),
            descriptor: Some(method.desc()),
        }));
    }

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

    let Some(tree) = super::parse::parse_tree(db, file) else {
        return Arc::new(hints);
    };

    collect_kotlin_parameter_hints(
        db,
        file,
        tree.root_node(),
        content,
        start_offset,
        end_offset,
        &mut hints,
    );

    hints.sort_by(|a, b| {
        a.offset
            .cmp(&b.offset)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| kotlin_hint_kind_rank(a.kind).cmp(&kotlin_hint_kind_rank(b.kind)))
    });
    hints.dedup_by(|a, b| a.offset == b.offset && a.label == b.label && a.kind == b.kind);

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
    let tree = super::parse::parse_tree(db, file)?;
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

fn root_index_view(db: &dyn Db) -> crate::index::IndexView {
    let index = db.workspace_index();
    index.view(IndexScope {
        module: ModuleId::ROOT,
    })
}

fn build_kotlin_semantic_context(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
) -> Option<SemanticContext> {
    let content = file.content(db);
    let rope = Rope::from_str(content);
    let mut candidates = vec![offset.min(content.len())];
    if let Some(prev) = offset.checked_sub(1) {
        candidates.push(prev.min(content.len()));
    }

    for bounded in candidates {
        let (line, character) = rope_byte_offset_to_line_col(&rope, bounded);
        if let Some(ctx) = extract_kotlin_semantic_context_for_test(content, line, character, None)
        {
            return Some(ctx);
        }
    }

    None
}

fn resolve_kotlin_type_name(
    db: &dyn Db,
    file: SourceFile,
    ctx: &SemanticContext,
    name: &str,
) -> Option<Arc<str>> {
    let raw = name.trim();
    if raw.is_empty() {
        return None;
    }

    let view = root_index_view(db);
    let current_classes = parse_kotlin_classes(db, file);
    let normalized = raw.replace('.', "/");

    if let Some(class) = current_classes
        .iter()
        .find(|class| class.internal_name.as_ref() == normalized)
    {
        return Some(Arc::clone(&class.internal_name));
    }

    if let Some(class) = current_classes
        .iter()
        .find(|class| class.name.as_ref() == raw)
    {
        return Some(Arc::clone(&class.internal_name));
    }

    if let Some(pkg) = ctx.enclosing_package.as_ref() {
        let same_package = format!("{pkg}/{raw}");
        if let Some(class) = current_classes
            .iter()
            .find(|class| class.internal_name.as_ref() == same_package)
        {
            return Some(Arc::clone(&class.internal_name));
        }
        if view.get_class(&same_package).is_some() {
            return Some(Arc::from(same_package));
        }
    }

    for import in &ctx.existing_imports {
        let import = import.as_ref();
        if import.ends_with(".*") {
            let candidate = format!(
                "{}/{}",
                import.trim_end_matches(".*").replace('.', "/"),
                raw
            );
            if let Some(class) = current_classes
                .iter()
                .find(|class| class.internal_name.as_ref() == candidate)
            {
                return Some(Arc::clone(&class.internal_name));
            }
            if view.get_class(&candidate).is_some() {
                return Some(Arc::from(candidate));
            }
            continue;
        }

        if import == raw || import.ends_with(&format!(".{raw}")) {
            let candidate = import.replace('.', "/");
            if let Some(class) = current_classes
                .iter()
                .find(|class| class.internal_name.as_ref() == candidate)
            {
                return Some(Arc::clone(&class.internal_name));
            }
            if view.get_class(&candidate).is_some() {
                return Some(Arc::from(candidate));
            }
        }
    }

    let mapped = kotlin_type_to_internal(raw);
    if mapped.contains('/') && view.get_class(mapped).is_some() {
        return Some(Arc::from(mapped));
    }

    let globals = view.get_classes_by_simple_name(raw);
    if globals.len() == 1 {
        return Some(Arc::clone(&globals[0].internal_name));
    }

    None
}

fn infer_kotlin_receiver_type(
    db: &dyn Db,
    file: SourceFile,
    ctx: &SemanticContext,
    receiver_expr: &str,
    offset: usize,
) -> Option<Arc<str>> {
    let receiver_expr = receiver_expr.trim();
    if receiver_expr.is_empty() {
        return None;
    }

    if receiver_expr == "this" {
        return ctx.enclosing_internal_name.clone();
    }

    if let Some(local) = ctx
        .local_variables
        .iter()
        .find(|local| local.name.as_ref() == receiver_expr)
    {
        return Some(Arc::from(local.type_internal.erased_internal()));
    }

    if receiver_expr.contains('.') || receiver_expr.contains("?.") {
        let normalized = receiver_expr.replace("?.", ".");
        let mut parts = normalized.split('.');
        let first = parts.next()?;
        let mut current = infer_kotlin_receiver_type(db, file, ctx, first, offset)?;
        for part in parts {
            let field = find_kotlin_field_in_owner(db, file, current.as_ref(), part)?;
            let ty = parse_single_type_to_internal(&field.descriptor)?;
            current = Arc::from(ty.erased_internal());
        }
        return Some(current);
    }

    if let Some(enclosing) = ctx.enclosing_internal_name.as_ref()
        && let Some(field) = find_kotlin_field_in_owner(db, file, enclosing.as_ref(), receiver_expr)
        && let Some(ty) = parse_single_type_to_internal(&field.descriptor)
    {
        return Some(Arc::from(ty.erased_internal()));
    }

    let _ = offset;
    resolve_kotlin_type_name(db, file, ctx, receiver_expr)
}

fn find_kotlin_field_in_owner(
    db: &dyn Db,
    file: SourceFile,
    owner_internal: &str,
    field_name: &str,
) -> Option<crate::index::FieldSummary> {
    if let Some(class) = parse_kotlin_classes(db, file)
        .into_iter()
        .find(|class| class.internal_name.as_ref() == owner_internal)
        && let Some(field) = class
            .fields
            .iter()
            .find(|field| field.name.as_ref() == field_name)
    {
        return Some(field.clone());
    }

    let view = root_index_view(db);
    view.get_class(owner_internal).and_then(|class| {
        class
            .fields
            .iter()
            .find(|field| field.name.as_ref() == field_name)
            .cloned()
    })
}

fn find_kotlin_method_in_owner(
    db: &dyn Db,
    file: SourceFile,
    owner_internal: &str,
    method_name: &str,
    arg_count: Option<usize>,
) -> Option<MethodSummary> {
    if let Some(class) = parse_kotlin_classes(db, file)
        .into_iter()
        .find(|class| class.internal_name.as_ref() == owner_internal)
        && let Some(method) = select_kotlin_method(&class.methods, method_name, arg_count)
    {
        return Some(method.clone());
    }

    let view = root_index_view(db);
    view.get_class(owner_internal)
        .and_then(|class| select_kotlin_method(&class.methods, method_name, arg_count).cloned())
}

fn select_kotlin_method<'a>(
    methods: &'a [MethodSummary],
    method_name: &str,
    arg_count: Option<usize>,
) -> Option<&'a MethodSummary> {
    methods.iter().find(|method| {
        method.name.as_ref() == method_name
            && arg_count.is_none_or(|count| method.params.len() == count)
    })
}

fn collect_kotlin_parameter_hints(
    db: &dyn Db,
    file: SourceFile,
    node: Node,
    source: &str,
    start: usize,
    end: usize,
    hints: &mut Vec<InlayHintData>,
) {
    if node.end_byte() < start || node.start_byte() > end {
        return;
    }

    if let Some((callee_text, arguments)) = kotlin_call_site_parts(node, source) {
        append_kotlin_parameter_hints_for_call(
            db,
            file,
            callee_text.as_str(),
            arguments,
            source,
            hints,
        );
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_kotlin_parameter_hints(db, file, child, source, start, end, hints);
    }
}

fn append_kotlin_parameter_hints_for_call(
    db: &dyn Db,
    file: SourceFile,
    callee_text: &str,
    arguments: Node,
    source: &str,
    hints: &mut Vec<InlayHintData>,
) {
    let arg_nodes = kotlin_argument_nodes(arguments);
    if arg_nodes.is_empty() {
        return;
    }

    let (receiver_expr, function_name) = split_kotlin_callee(callee_text);
    let Some(param_names) = resolve_kotlin_parameter_names_for_call(
        db,
        file,
        arguments.start_byte(),
        receiver_expr.as_deref(),
        function_name.as_str(),
        arg_nodes.len(),
    ) else {
        return;
    };

    for (index, arg_node) in arg_nodes.into_iter().enumerate() {
        let Some(param_name) = param_names.get(index) else {
            continue;
        };
        if param_name.is_empty()
            || should_skip_kotlin_parameter_hint(param_name.as_ref(), arg_node, source)
        {
            continue;
        }

        hints.push(InlayHintData {
            offset: arg_node.start_byte(),
            label: format!("{param_name}:").into(),
            kind: InlayHintKindData::Parameter,
        });
    }
}

fn resolve_kotlin_parameter_names_for_call(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
    receiver_expr: Option<&str>,
    function_name: &str,
    arg_count: usize,
) -> Option<Vec<Arc<str>>> {
    let ctx = build_kotlin_semantic_context(db, file, offset)?;

    if let Some(receiver_expr) = receiver_expr {
        let owner = infer_kotlin_receiver_type(db, file, &ctx, receiver_expr, offset)?;
        let method =
            find_kotlin_method_in_owner(db, file, owner.as_ref(), function_name, Some(arg_count))?;
        return Some(method.params.param_names());
    }

    if let Some(enclosing) = ctx.enclosing_internal_name.as_ref()
        && let Some(method) = find_kotlin_method_in_owner(
            db,
            file,
            enclosing.as_ref(),
            function_name,
            Some(arg_count),
        )
    {
        return Some(method.params.param_names());
    }

    top_level_kotlin_function_param_names(db, file, function_name, arg_count)
}

fn top_level_kotlin_function_param_names(
    db: &dyn Db,
    file: SourceFile,
    function_name: &str,
    arg_count: usize,
) -> Option<Vec<Arc<str>>> {
    let content = file.content(db);
    let tree = super::parse::parse_tree(db, file)?;
    let root = tree.root_node();

    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "function_declaration" {
            continue;
        }
        let Some(name_node) = child
            .children(&mut child.walk())
            .find(|node| node.kind() == "simple_identifier")
        else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(content.as_bytes()) else {
            continue;
        };
        if name != function_name {
            continue;
        }

        let param_names = kotlin_function_parameter_names(child, content.as_bytes());
        if param_names.len() == arg_count {
            return Some(param_names);
        }
    }

    None
}

fn kotlin_function_parameter_names(function: Node, source: &[u8]) -> Vec<Arc<str>> {
    let Some(params_node) = function
        .children(&mut function.walk())
        .find(|node| node.kind() == "function_value_parameters")
    else {
        return Vec::new();
    };

    params_node
        .named_children(&mut params_node.walk())
        .filter_map(|param| {
            param
                .children(&mut param.walk())
                .find(|node| node.kind() == "simple_identifier")
                .and_then(|name| name.utf8_text(source).ok())
                .map(Arc::from)
        })
        .collect()
}

fn find_kotlin_value_arguments(node: Node) -> Option<Node> {
    if let Some(arguments) = node.child_by_field_name("arguments") {
        return Some(arguments);
    }

    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "value_arguments" {
            return Some(current);
        }

        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }

    None
}

fn kotlin_call_site_parts<'a>(node: Node<'a>, source: &'a str) -> Option<(String, Node<'a>)> {
    let arguments = find_kotlin_value_arguments(node)?;
    if arguments.start_byte() == node.start_byte() && arguments.end_byte() == node.end_byte() {
        return None;
    }

    let callee = node
        .child_by_field_name("callee")
        .or_else(|| node.named_child(0))?;
    let callee_text = callee.utf8_text(source.as_bytes()).ok()?.trim().to_string();
    if callee_text.is_empty() {
        return None;
    }

    Some((callee_text, arguments))
}

fn kotlin_argument_nodes(arguments: Node) -> Vec<Node> {
    arguments.named_children(&mut arguments.walk()).collect()
}

fn split_kotlin_callee(callee: &str) -> (Option<String>, String) {
    let normalized = callee.replace("?.", ".");
    let (receiver, name) = match normalized.rsplit_once('.') {
        Some((receiver, name)) => (Some(receiver.to_string()), name.to_string()),
        None => (None, normalized),
    };
    let name = name
        .split('<')
        .next()
        .unwrap_or(name.as_str())
        .trim()
        .to_string();
    (receiver, name)
}

fn should_skip_kotlin_parameter_hint(param_name: &str, arg_node: Node, source: &str) -> bool {
    let Ok(text) = arg_node.utf8_text(source.as_bytes()) else {
        return false;
    };
    let trimmed = text.trim();
    trimmed == param_name || trimmed.starts_with(&format!("{param_name} ="))
}

fn kotlin_hint_kind_rank(kind: InlayHintKindData) -> u8 {
    match kind {
        InlayHintKindData::Type => 0,
        InlayHintKindData::Parameter => 1,
    }
}

fn kotlin_member_access_at_offset(source: &str, offset: usize) -> Option<(String, String)> {
    let tree = super::parse::parse_tree_for_language(source, "kotlin")?;
    let root = tree.root_node();
    let bounded = offset.min(source.len().saturating_sub(1));
    let node = root.named_descendant_for_byte_range(bounded, bounded.saturating_add(1))?;
    let mut current = Some(node);

    while let Some(candidate) = current {
        if candidate.kind() == "navigation_expression" {
            let receiver = candidate
                .named_child(0)?
                .utf8_text(source.as_bytes())
                .ok()?;
            let suffix = candidate.named_child(1)?;
            let member = suffix
                .named_children(&mut suffix.walk())
                .find_map(|child| match child.kind() {
                    "simple_identifier" | "identifier" => {
                        child.utf8_text(source.as_bytes()).ok().map(str::to_string)
                    }
                    _ => None,
                })?;
            return Some((receiver.to_string(), member));
        }
        current = candidate.parent();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::rope_utils::byte_offset_to_line_col;
    use crate::salsa_db::{Database, FileId};
    use tower_lsp::lsp_types::Url;

    fn offset_to_line_col(source: &str, offset: usize) -> (u32, u32) {
        byte_offset_to_line_col(source, offset)
    }

    #[test]
    fn test_resolve_kotlin_symbol_for_current_class_field() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.kt").unwrap();
        let source = r#"
class Test {
    val name: String = "hi"

    fun printName() {
        val output = name
    }
}
"#;
        let offset = source.rfind("= name").unwrap() + 3;
        let (line, character) = offset_to_line_col(source, offset);
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            source.to_string(),
            Arc::from("kotlin"),
        );

        let resolved = resolve_kotlin_symbol(&db, file, line, character).expect("resolved");
        assert_eq!(resolved.kind, SymbolKind::Field);
        assert_eq!(resolved.target_internal_name.as_ref(), "Test");
        assert_eq!(resolved.member_name.as_deref(), Some("name"));
    }

    #[test]
    fn test_resolve_kotlin_member_symbol_from_local_receiver() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.kt").unwrap();
        let source = r#"
class User {
    val name: String = ""
}

class Test {
    fun run(user: User) {
        val output = user.name
    }
}
"#;
        let offset = source.rfind("user.name").unwrap() + "user.".len();
        let (line, character) = offset_to_line_col(source, offset);
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            source.to_string(),
            Arc::from("kotlin"),
        );

        let resolved = resolve_kotlin_symbol(&db, file, line, character).expect("resolved");
        assert_eq!(resolved.kind, SymbolKind::Field);
        assert_eq!(resolved.target_internal_name.as_ref(), "User");
        assert_eq!(resolved.member_name.as_deref(), Some("name"));
    }

    #[test]
    fn test_resolve_kotlin_member_symbol_with_utf16_columns() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.kt").unwrap();
        let source = r#"
class User {
    val name: String = ""
}

class Test {
    fun run(user: User) {
        val prefix = "😀"; val output = user.name
    }
}
"#;
        let offset = source.rfind("user.name").unwrap() + "user.".len();
        let (line, character) = offset_to_line_col(source, offset);
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            source.to_string(),
            Arc::from("kotlin"),
        );

        let resolved = resolve_kotlin_symbol(&db, file, line, character).expect("resolved");
        assert_eq!(resolved.kind, SymbolKind::Field);
        assert_eq!(resolved.target_internal_name.as_ref(), "User");
        assert_eq!(resolved.member_name.as_deref(), Some("name"));
    }

    #[test]
    fn test_compute_kotlin_inlay_hints_adds_parameter_hints() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.kt").unwrap();
        let source = r#"
class Test {
    fun greet(name: String, times: Int) {}

    fun run() {
        greet("hi", 2)
    }
}
"#;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            source.to_string(),
            Arc::from("kotlin"),
        );

        let hints = compute_kotlin_inlay_hints(&db, file, 0, 0, 6, 1);
        let labels: Vec<&str> = hints.iter().map(|hint| hint.label.as_ref()).collect();
        assert!(labels.contains(&"name:"), "{labels:?}");
        assert!(labels.contains(&"times:"), "{labels:?}");
    }
}
