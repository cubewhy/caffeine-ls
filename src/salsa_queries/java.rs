use super::Db;
use crate::index::{ClassMetadata, NameTable};
use crate::salsa_db::SourceFile;
use crate::salsa_queries::context::{
    CompletionContextData, CursorLocationData, line_col_to_offset,
};
use crate::salsa_queries::hints::{InlayHintData, InlayHintKindData};
use crate::salsa_queries::symbols::{ResolvedSymbolData, SymbolKind};
use crate::semantic::CursorLocation;
/// Java-specific Salsa queries
///
/// These queries handle Java-specific parsing and analysis.
use std::sync::Arc;

/// Parse Java source and extract class metadata with full incremental support
///
/// This is the main entry point for Java file indexing.
/// Note: We return the classes directly, not wrapped in Arc, because Salsa
/// will handle the memoization.
pub fn parse_java_classes(db: &dyn Db, file: SourceFile) -> Vec<ClassMetadata> {
    let content = file.content(db);
    let file_id = file.file_id(db);

    // Get name table if available
    let name_table = get_name_table_for_java_file(db, file);

    let origin = crate::index::ClassOrigin::SourceFile(Arc::from(file_id.as_str()));

    crate::language::java::class_parser::parse_java_source(content, origin, name_table)
}

/// Get name table for a Java file's context
fn get_name_table_for_java_file(db: &dyn Db, file: SourceFile) -> Option<Arc<NameTable>> {
    let workspace_index = db.workspace_index();
    let index = workspace_index.read();
    let _ = file;
    Some(index.build_name_table(crate::index::IndexScope {
        module: crate::index::ModuleId::ROOT,
    }))
}

/// Extract Java package declaration
pub fn extract_java_package(db: &dyn Db, file: SourceFile) -> Option<Arc<str>> {
    let content = file.content(db);
    crate::language::java::class_parser::extract_package_from_source(content)
}

/// Extract Java imports
pub fn extract_java_imports(db: &dyn Db, file: SourceFile) -> Vec<Arc<str>> {
    let content = file.content(db);
    crate::language::java::class_parser::extract_imports_from_source(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::salsa_db::{Database, FileId};
    use ropey::Rope;
    use tower_lsp::lsp_types::Url;

    #[test]
    fn test_parse_java_classes() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "package com.example;\npublic class Test { void foo() {} }".to_string(),
            Arc::from("java"),
        );

        let classes = parse_java_classes(&db, file);
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name.as_ref(), "Test");
        assert_eq!(classes[0].package.as_deref(), Some("com/example"));
        assert_eq!(classes[0].methods.len(), 1);
        assert_eq!(classes[0].methods[0].name.as_ref(), "foo");
    }

    #[test]
    fn test_extract_java_package() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "package org.example.test;\npublic class Test {}".to_string(),
            Arc::from("java"),
        );

        let package = extract_java_package(&db, file);
        assert_eq!(package.as_deref(), Some("org/example/test"));
    }

    #[test]
    fn test_extract_java_imports() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            "import java.util.*;\nimport java.io.File;\npublic class Test {}".to_string(),
            Arc::from("java"),
        );

        let imports = extract_java_imports(&db, file);
        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn test_extract_java_context_keeps_system_out_as_member_access() {
        let db = Database::default();
        let uri = Url::parse("file:///test/Test.java").unwrap();
        let content = "class Test { void f() { System.out.println } }";
        let rope = Rope::from_str(content);
        let byte_offset = content.find("println").unwrap() + "println".len();
        let char_idx = rope.byte_to_char(byte_offset);
        let line = rope.char_to_line(char_idx) as u32;
        let character = (char_idx - rope.line_to_char(line as usize)) as u32;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );

        let ctx = extract_java_completion_context(&db, file, line, character, None);

        match &ctx.location {
            CursorLocationData::MemberAccess {
                receiver_expr,
                member_prefix,
                ..
            } => {
                assert_eq!(receiver_expr.as_ref(), "System.out");
                assert_eq!(member_prefix.as_ref(), "println");
            }
            other => panic!("expected MemberAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_java_context_keeps_user_dot_as_member_access() {
        let db = Database::default();
        let uri = Url::parse("file:///test/User.java").unwrap();
        let content = r#"
class User {
    void test() {
        User user = new User();
        user.
    }
}
"#;
        let rope = Rope::from_str(content);
        let byte_offset = content.find("user.").unwrap() + "user.".len();
        let char_idx = rope.byte_to_char(byte_offset);
        let line = rope.char_to_line(char_idx) as u32;
        let character = (char_idx - rope.line_to_char(line as usize)) as u32;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );

        let ctx = extract_java_completion_context(&db, file, line, character, Some('.'));

        match &ctx.location {
            CursorLocationData::MemberAccess {
                receiver_expr,
                member_prefix,
                ..
            } => {
                assert_eq!(receiver_expr.as_ref(), "user");
                assert!(member_prefix.is_empty());
            }
            other => panic!("expected MemberAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_java_context_keeps_new_prefix_as_expression() {
        let db = Database::default();
        let uri = Url::parse("file:///test/InvalidationTest.java").unwrap();
        let content = r#"
class InvalidationTest {
    void method() {
        String v = new
    }
}
"#;
        let rope = Rope::from_str(content);
        let byte_offset = content.rfind("new").unwrap() + "new".len();
        let char_idx = rope.byte_to_char(byte_offset);
        let line = rope.char_to_line(char_idx) as u32;
        let character = (char_idx - rope.line_to_char(line as usize)) as u32;
        let file = SourceFile::new(
            &db,
            FileId::new(uri),
            content.to_string(),
            Arc::from("java"),
        );

        let ctx = extract_java_completion_context(&db, file, line, character, None);

        match &ctx.location {
            CursorLocationData::Expression { prefix } => {
                assert_eq!(prefix.as_ref(), "new");
            }
            other => panic!("expected Expression, got {other:?}"),
        }
    }
}

// ============================================================================
// Java Completion Context Extraction
// ============================================================================

/// Extract Java completion context (CACHED)
#[salsa::tracked]
pub fn extract_java_completion_context(
    db: &dyn Db,
    file: SourceFile,
    line: u32,
    character: u32,
    trigger_char: Option<char>,
) -> Arc<CompletionContextData> {
    extract_java_completion_context_with_name_table(db, file, line, character, trigger_char, None)
}

pub fn extract_java_completion_context_with_name_table(
    db: &dyn Db,
    file: SourceFile,
    line: u32,
    character: u32,
    trigger_char: Option<char>,
    request_name_table: Option<Arc<NameTable>>,
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
    let Some(tree) = super::parse::parse_tree_for_language(content, "java") else {
        return Arc::new(empty_context(db, file));
    };

    let root = tree.root_node();
    let name_table = request_name_table.or_else(|| get_name_table_for_java_file(db, file));
    let extractor =
        crate::language::java::JavaContextExtractor::new(content.to_string(), offset, name_table);
    let cursor_node = extractor.find_cursor_node(root);
    let (rich_location, rich_query) =
        crate::language::java::location::determine_location(&extractor, cursor_node, trigger_char);
    let location = convert_rich_location(&rich_location);
    let query = Arc::from(rich_query.as_str());

    // Extract scope information (all cached separately)
    let package = super::parse::extract_package(db, file);
    let imports = super::parse::extract_imports(db, file);
    let enclosing_class = find_java_enclosing_class_name(db, file, offset);
    let enclosing_internal_name = build_internal_name(&package, &enclosing_class);

    // Count locals (cached)
    let local_var_count = count_java_locals_in_scope(db, file, offset);

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
        static_import_count: 0, // TODO: count static imports
        content_hash,
        file_uri: Arc::from(file.file_id(db).as_str()),
        language_id: Arc::from("java"),
    })
}

fn convert_rich_location(location: &CursorLocation) -> CursorLocationData {
    match location {
        CursorLocation::Expression { prefix } => CursorLocationData::Expression {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::MemberAccess {
            receiver_type,
            member_prefix,
            receiver_expr,
            arguments,
            ..
        } => CursorLocationData::MemberAccess {
            receiver_expr: Arc::from(receiver_expr.as_str()),
            member_prefix: Arc::from(member_prefix.as_str()),
            receiver_type_hint: receiver_type.clone(),
            arguments: arguments.as_ref().map(|s| Arc::from(s.as_str())),
        },
        CursorLocation::StaticAccess {
            class_internal_name,
            member_prefix,
        } => CursorLocationData::StaticAccess {
            class_internal_name: Arc::clone(class_internal_name),
            member_prefix: Arc::from(member_prefix.as_str()),
        },
        CursorLocation::Import { prefix } => CursorLocationData::Import {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::ImportStatic { prefix } => CursorLocationData::ImportStatic {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::MethodArgument { prefix } => CursorLocationData::MethodArgument {
            prefix: Arc::from(prefix.as_str()),
            method_name: None,
            arg_index: None,
        },
        CursorLocation::ConstructorCall { class_prefix, .. } => CursorLocationData::Expression {
            prefix: if class_prefix.is_empty() {
                Arc::from("new")
            } else {
                Arc::from(class_prefix.as_str())
            },
        },
        CursorLocation::TypeAnnotation { prefix } => CursorLocationData::Expression {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::VariableName { type_name } => CursorLocationData::Expression {
            prefix: Arc::from(type_name.as_str()),
        },
        CursorLocation::StringLiteral { prefix } => CursorLocationData::Expression {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::MethodReference {
            qualifier_expr,
            member_prefix,
            is_constructor,
        } => {
            if *is_constructor {
                CursorLocationData::Expression {
                    prefix: Arc::from(qualifier_expr.as_str()),
                }
            } else {
                CursorLocationData::MemberAccess {
                    receiver_expr: Arc::from(qualifier_expr.as_str()),
                    member_prefix: Arc::from(member_prefix.as_str()),
                    receiver_type_hint: None,
                    arguments: None,
                }
            }
        }
        CursorLocation::Annotation { prefix, .. } => CursorLocationData::Annotation {
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::StatementLabel { kind, prefix } => CursorLocationData::StatementLabel {
            kind: match kind {
                crate::semantic::context::StatementLabelCompletionKind::Break => {
                    crate::salsa_queries::StatementLabelKind::Break
                }
                crate::semantic::context::StatementLabelCompletionKind::Continue => {
                    crate::salsa_queries::StatementLabelKind::Continue
                }
            },
            prefix: Arc::from(prefix.as_str()),
        },
        CursorLocation::Unknown => CursorLocationData::Unknown,
    }
}

fn is_in_comment(content: &str, offset: usize) -> bool {
    // Simple check: look backwards for comment markers
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
        language_id: Arc::from("java"),
    }
}

// ============================================================================
// Java Scope Queries
// ============================================================================

/// Find the enclosing class name (CACHED)
#[salsa::tracked]
pub fn find_java_enclosing_class_name(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
) -> Option<Arc<str>> {
    if let Some((name, _, _)) = super::semantic::find_enclosing_class_bounds(db, file, offset) {
        Some(name)
    } else {
        None
    }
}

/// Count local variables in scope (CACHED)
#[salsa::tracked]
pub fn count_java_locals_in_scope(db: &dyn Db, file: SourceFile, offset: usize) -> usize {
    // Get method bounds
    let Some((method_start, method_end)) =
        super::semantic::find_enclosing_method_bounds(db, file, offset)
    else {
        return 0;
    };

    // Get metadata (which includes count)
    let metadata =
        super::semantic::extract_method_locals_metadata(db, file, method_start, method_end);
    metadata.local_count
}

fn build_internal_name(package: &Option<Arc<str>>, class: &Option<Arc<str>>) -> Option<Arc<str>> {
    match (package, class) {
        (Some(pkg), Some(cls)) => Some(Arc::from(format!("{}/{}", pkg, cls))),
        (None, Some(cls)) => Some(Arc::clone(cls)),
        _ => None,
    }
}

// ============================================================================
// Java Symbol Resolution
// ============================================================================

/// Resolve Java symbol at position (CACHED)
#[salsa::tracked]
pub fn resolve_java_symbol(
    db: &dyn Db,
    file: SourceFile,
    line: u32,
    character: u32,
) -> Option<Arc<ResolvedSymbolData>> {
    let content = file.content(db);
    let offset = line_col_to_offset(content, line, character)?;

    // Get context
    let context = extract_java_completion_context(db, file, line, character, None);

    // Resolve based on location
    match &context.location {
        CursorLocationData::Expression { prefix } => {
            resolve_java_expression_symbol(db, file, Arc::clone(prefix), offset)
        }
        CursorLocationData::MemberAccess {
            receiver_expr,
            member_prefix,
            ..
        } => resolve_java_member_symbol(
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
fn resolve_java_expression_symbol(
    db: &dyn Db,
    file: SourceFile,
    symbol_name: Arc<str>,
    offset: usize,
) -> Option<Arc<ResolvedSymbolData>> {
    // Check if it's a local variable
    if is_java_local_variable(db, file, Arc::clone(&symbol_name), offset) {
        return Some(Arc::new(ResolvedSymbolData {
            kind: SymbolKind::LocalVariable,
            target_internal_name: Arc::from(""),
            member_name: Some(symbol_name),
            descriptor: None,
        }));
    }

    // TODO: Check fields, methods, imports
    None
}

#[salsa::tracked]
fn resolve_java_member_symbol(
    _db: &dyn Db,
    _file: SourceFile,
    _receiver_expr: Arc<str>,
    _member_name: Arc<str>,
) -> Option<Arc<ResolvedSymbolData>> {
    // TODO: Implement member resolution
    // This requires type inference for the receiver
    None
}

/// Check if a symbol is a local variable (CACHED)
#[salsa::tracked]
pub fn is_java_local_variable(
    db: &dyn Db,
    file: SourceFile,
    symbol_name: Arc<str>,
    offset: usize,
) -> bool {
    super::symbols::find_local_variable_declaration(db, file, symbol_name, offset).is_some()
}

// ============================================================================
// Java Inlay Hints
// ============================================================================

/// Compute Java inlay hints (CACHED)
#[salsa::tracked]
pub fn compute_java_inlay_hints(
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
            && let Some(inferred_type) = infer_java_variable_type(db, file, decl.offset)
        {
            hints.push(InlayHintData {
                offset: decl.offset + decl.name.len(),
                label: format!(": {}", inferred_type).into(),
                kind: InlayHintKindData::Type,
            });
        }
    }

    // Find method calls (cached)
    let _method_calls =
        super::hints::find_method_calls_in_range(db, file, start_offset, end_offset);

    // TODO: Add parameter hints for method calls

    Arc::new(hints)
}

/// Infer Java variable type (CACHED)
#[salsa::tracked]
pub fn infer_java_variable_type(
    db: &dyn Db,
    file: SourceFile,
    decl_offset: usize,
) -> Option<Arc<str>> {
    let content = file.content(db);

    // Parse tree
    let tree = super::parse::parse_tree_for_language(content, "java")?;
    let root = tree.root_node();

    // Find the variable declarator at this offset
    let node = find_node_at_offset(root, decl_offset)?;

    // Find the variable_declarator ancestor
    let var_decl = find_ancestor_of_kind(node, "variable_declarator")?;

    // Get the initializer
    let init = var_decl.child_by_field_name("value")?;

    // Infer type from initializer
    infer_type_from_expression(init, content.as_bytes())
}

fn find_node_at_offset<'a>(
    root: tree_sitter::Node<'a>,
    offset: usize,
) -> Option<tree_sitter::Node<'a>> {
    root.named_descendant_for_byte_range(offset, offset + 1)
}

fn find_ancestor_of_kind<'a>(
    mut node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    loop {
        if node.kind() == kind {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn infer_type_from_expression(expr: tree_sitter::Node, source: &[u8]) -> Option<Arc<str>> {
    match expr.kind() {
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => Some(Arc::from("int")),
        "decimal_floating_point_literal" | "hex_floating_point_literal" => {
            Some(Arc::from("double"))
        }
        "string_literal" => Some(Arc::from("String")),
        "true" | "false" => Some(Arc::from("boolean")),
        "null_literal" => Some(Arc::from("Object")),
        "object_creation_expression" => {
            // Extract the type from "new Type()"
            if let Some(type_node) = expr.child_by_field_name("type") {
                type_node.utf8_text(source).ok().map(Arc::from)
            } else {
                None
            }
        }
        _ => None,
    }
}
