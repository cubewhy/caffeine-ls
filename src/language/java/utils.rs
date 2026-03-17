use crate::language::java::SENTINEL;
use crate::semantic::context::StatementLabelTargetKind;
use ropey::Rope;
use rust_asm::constants::{
    ACC_ABSTRACT, ACC_FINAL, ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC, ACC_STATIC,
};
use std::sync::Arc;
use tree_sitter::Node;
use tree_sitter_utils::traversal;

pub(crate) fn find_top_error_node(root: Node) -> Option<Node> {
    traversal::first_child_of_kind(root, "ERROR")
}

/// parse access flags from modifiers text
pub fn parse_java_modifiers(text: &str) -> u16 {
    let mut flags: u16 = 0;
    if text.contains("public") {
        flags |= ACC_PUBLIC;
    }
    if text.contains("private") {
        flags |= ACC_PRIVATE;
    }
    if text.contains("protected") {
        flags |= ACC_PROTECTED;
    }
    if text.contains("static") {
        flags |= ACC_STATIC;
    }
    if text.contains("final") {
        flags |= ACC_FINAL;
    }
    if text.contains("abstract") {
        flags |= ACC_ABSTRACT;
    }
    flags
}

fn node_text<'a>(node: Node, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

/// Extracts generic parameters from a class or method and constructs them into a generic signature according to the JVM specification.
/// For example, extracts `<T:Ljava/lang/Object;E:Ljava/lang/Object;>Ljava/lang/Object;` from `class List<T, E>`.
pub fn extract_generic_signature(node: Node, bytes: &[u8], suffix: &str) -> Option<Arc<str>> {
    let mut sig = extract_type_parameters_prefix(node, bytes)?;
    sig.push_str(suffix);
    Some(Arc::from(sig))
}

/// Extract only the `<...>` type-parameter prefix from class/method declarations.
pub fn extract_type_parameters_prefix(node: Node, bytes: &[u8]) -> Option<String> {
    // Compatible with Java (child_by_field_name) and Kotlin (directly search for kind)
    let tp_node = node
        .child_by_field_name("type_parameters")
        .or_else(|| traversal::first_child_of_kind(node, "type_parameters"))?;

    let mut sig = String::from("<");
    let mut has_params = false;
    let mut walker = tp_node.walk();

    for child in tp_node.named_children(&mut walker) {
        if child.kind() == "type_parameter" {
            // Java 是 identifier，Kotlin 是 type_identifier
            if let Some(id_node) =
                traversal::first_child_of_kinds(child, &["identifier", "type_identifier"])
            {
                let name = node_text(id_node, bytes).trim();
                if !name.is_empty() {
                    sig.push_str(name);
                    // Erasure to Object uniformly, because our engine currently only cares about the name mapping of parameter placeholders.
                    sig.push_str(":Ljava/lang/Object;");
                    has_params = true;
                }
            }
        }
    }

    if !has_params {
        return None;
    }

    sig.push('>');
    Some(sig)
}

pub(crate) fn build_internal_name(
    package: &Option<Arc<str>>,
    class: &Option<Arc<str>>,
) -> Option<Arc<str>> {
    match (package, class) {
        (Some(pkg), Some(cls)) => Some(Arc::from(format!("{}/{}", pkg, cls).as_str())),
        (None, Some(cls)) => Some(Arc::clone(cls)),
        _ => None,
    }
}

pub(crate) fn is_comment_kind(kind: &str) -> bool {
    kind == "line_comment" || kind == "block_comment"
}

pub(crate) fn find_ancestor<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    traversal::ancestor_of_kind(node, kind)
}

/// Remove SENTINEL from the string (the prefix in the injection path may contain it).
pub(crate) fn strip_sentinel(s: &str) -> String {
    s.replace(SENTINEL, "")
}

pub(crate) fn get_initializer_text(type_node: Node, bytes: &[u8]) -> Option<String> {
    let decl = type_node.parent()?;
    if decl.kind() != "local_variable_declaration" {
        return None;
    }
    let declarator = traversal::first_child_of_kind(decl, "variable_declarator")?;
    let init = declarator.named_child(1)?;
    init.utf8_text(bytes).ok().map(|s| s.to_string())
}

pub(crate) fn find_enclosing_method_in_error(root: Node, offset: usize) -> Option<Node> {
    use tree_sitter_utils::traversal::find_node_by_offset;
    find_node_by_offset(root, "method_declaration", offset)
}

pub(crate) fn statement_label_target_kind(node: Node) -> StatementLabelTargetKind {
    let target = unwrap_labeled_statement_target(node);
    match target.kind() {
        "block" => StatementLabelTargetKind::Block,
        "while_statement" => StatementLabelTargetKind::While,
        "do_statement" => StatementLabelTargetKind::DoWhile,
        "for_statement" => StatementLabelTargetKind::For,
        "enhanced_for_statement" => StatementLabelTargetKind::EnhancedFor,
        "switch_expression" | "switch_statement" => StatementLabelTargetKind::Switch,
        _ => StatementLabelTargetKind::Other,
    }
}

pub(crate) fn unwrap_labeled_statement_target(mut node: Node) -> Node {
    while node.kind() == "labeled_statement" {
        let Some(child) = traversal::first_child_of_kind(node, "identifier")
            .and_then(|id| id.next_named_sibling())
        else {
            break;
        };
        node = child;
    }
    node
}

pub fn infer_type_from_initializer(type_node: Node, bytes: &[u8]) -> Option<String> {
    let decl = type_node.parent()?;
    if decl.kind() != "local_variable_declaration" {
        return None;
    }
    let declarator = traversal::first_child_of_kind(decl, "variable_declarator")?;
    let init = declarator.named_child(1)?;
    match init.kind() {
        "object_creation_expression" => {
            let ty_node = init.child_by_field_name("type")?;
            let text = ty_node.utf8_text(bytes).ok()?;
            let simple = text.split('<').next()?.trim();
            if !simple.is_empty() {
                return Some(simple.to_string());
            }
        }
        _ => {
            let text = init.utf8_text(bytes).ok()?;
            if let Some(rest) = text.trim().strip_prefix("new ") {
                let class_name = rest.split('(').next()?.split('<').next()?.trim();
                if !class_name.is_empty() {
                    return Some(class_name.to_string());
                }
            }
        }
    }
    None
}

pub fn is_cursor_in_comment_with_rope(source: &str, _rope: &Rope, offset: usize) -> bool {
    let before = &source[..offset];

    let last_open = before.rfind("/*");
    let last_close = before.rfind("*/");
    if let Some(open) = last_open {
        match last_close {
            None => return true,
            Some(close) if open > close => return true,
            _ => {}
        }
    }

    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    is_in_line_comment(&source[line_start..offset])
}

pub fn is_cursor_in_comment(source: &str, offset: usize) -> bool {
    let rope = Rope::from_str(source);
    is_cursor_in_comment_with_rope(source, &rope, offset)
}

fn is_in_line_comment(line: &str) -> bool {
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' if !in_char => in_string = !in_string,
            '\'' if !in_string => in_char = !in_char,
            '/' if !in_string && !in_char => {
                if chars.peek() == Some(&'/') {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

pub fn java_type_to_internal(ty: &str) -> String {
    ty.trim().replace('.', "/")
}

pub fn find_error_ancestor(node: Node) -> Option<Node> {
    // Includes the node itself (non-strict), then climbs.
    if node.kind() == "ERROR" {
        return Some(node);
    }
    traversal::ancestor_of_kind(node, "ERROR")
}

pub fn error_has_new_keyword(error_node: Node) -> bool {
    traversal::any_child_of_kind(error_node, "new").is_some()
}

pub fn find_identifier_in_error(error_node: Node) -> Option<Node> {
    traversal::first_child_of_kinds(error_node, &["identifier", "type_identifier"])
}

pub fn error_has_trailing_dot(error_node: Node, offset: usize) -> bool {
    let children: Vec<Node> = {
        let mut cursor = error_node.walk();
        error_node.children(&mut cursor).collect()
    };
    let visible: Vec<Node> = children
        .into_iter()
        .filter(|child| child.start_byte() < offset)
        .collect();
    let Some(last) = visible.last() else {
        return false;
    };
    if last.kind() == "." {
        return last.end_byte() <= offset;
    }
    if last.kind() == ";" {
        return visible
            .iter()
            .rev()
            .nth(1)
            .is_some_and(|child| child.kind() == "." && child.end_byte() <= offset);
    }
    false
}

pub fn is_in_type_position(id_node: Node, decl_node: Node) -> bool {
    let mut walker = decl_node.walk();
    for child in decl_node.named_children(&mut walker) {
        if child.kind() == "modifiers" {
            continue;
        }
        return child.id() == id_node.id();
    }
    false
}

pub fn is_in_type_arguments(node: Node) -> bool {
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        if parent.kind() == "type_arguments" {
            return true;
        }
        cur = parent;
    }
    false
}

pub fn is_in_name_position(id_node: Node, decl_node: Node) -> bool {
    let mut wc = decl_node.walk();
    for declarator in decl_node.named_children(&mut wc) {
        if declarator.kind() != "variable_declarator" {
            continue;
        }
        if let Some(name_node) = declarator.child_by_field_name("name")
            && name_node.id() == id_node.id()
        {
            return true;
        }
    }
    false
}

pub(crate) fn find_string_ancestor<'a>(node: Node<'a>) -> Option<Node<'a>> {
    // Includes the node itself (non-strict), then climbs.
    if matches!(node.kind(), "string_literal" | "text_block") {
        return Some(node);
    }
    traversal::ancestor_of_kinds(node, &["string_literal", "text_block"])
}
