use crate::language::java::SENTINEL;
use crate::semantic::context::{CursorLocation, StatementLabelTargetKind};
use ropey::Rope;
use rust_asm::constants::{
    ACC_ABSTRACT, ACC_FINAL, ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC, ACC_STATIC,
};
use std::sync::Arc;
use tree_sitter::Node;

pub(crate) fn find_top_error_node(root: Node) -> Option<Node> {
    let mut cursor = root.walk();
    root.children(&mut cursor)
        .find(|&child| child.kind() == "ERROR")
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
    let tp_node = node.child_by_field_name("type_parameters").or_else(|| {
        node.children(&mut node.walk())
            .find(|n| n.kind() == "type_parameters")
    })?;

    let mut sig = String::from("<");
    let mut has_params = false;
    let mut walker = tp_node.walk();

    for child in tp_node.named_children(&mut walker) {
        if child.kind() == "type_parameter" {
            // Java 是 identifier，Kotlin 是 type_identifier
            if let Some(id_node) = child
                .children(&mut child.walk())
                .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
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

pub(crate) fn find_ancestor<'a>(mut node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    loop {
        node = node.parent()?;
        if node.kind() == kind {
            return Some(node);
        }
    }
}

/// Remove SENTINEL from the string (the prefix in the injection path may contain it).
pub(crate) fn strip_sentinel(s: &str) -> String {
    s.replace(SENTINEL, "")
}

/// Remove any remaining SENTINEL from CursorLocation
pub(crate) fn strip_sentinel_from_location(loc: CursorLocation) -> CursorLocation {
    match loc {
        CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            member_prefix,
            receiver_expr,
            arguments,
        } => CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_type,
            member_prefix: strip_sentinel(&member_prefix),
            receiver_expr: strip_sentinel(&receiver_expr),
            arguments: arguments.map(|a| strip_sentinel(&a)),
        },
        CursorLocation::ConstructorCall {
            class_prefix,
            expected_type,
        } => CursorLocation::ConstructorCall {
            class_prefix: strip_sentinel(&class_prefix),
            expected_type,
        },
        CursorLocation::Expression { prefix } => CursorLocation::Expression {
            prefix: strip_sentinel(&prefix),
        },
        CursorLocation::MethodArgument { prefix } => CursorLocation::MethodArgument {
            prefix: strip_sentinel(&prefix),
        },
        CursorLocation::Annotation {
            prefix,
            target_element_type,
        } => CursorLocation::Annotation {
            prefix: strip_sentinel(&prefix),
            target_element_type,
        },
        CursorLocation::TypeAnnotation { prefix } => CursorLocation::TypeAnnotation {
            prefix: strip_sentinel(&prefix),
        },
        CursorLocation::MethodReference {
            qualifier_expr,
            member_prefix,
            is_constructor,
        } => CursorLocation::MethodReference {
            qualifier_expr: strip_sentinel(&qualifier_expr),
            member_prefix: strip_sentinel(&member_prefix),
            is_constructor,
        },
        CursorLocation::StringLiteral { prefix } => CursorLocation::StringLiteral {
            prefix: strip_sentinel(&prefix),
        },
        CursorLocation::StatementLabel { kind, prefix } => CursorLocation::StatementLabel {
            kind,
            prefix: strip_sentinel(&prefix),
        },
        CursorLocation::VariableName { type_name } => CursorLocation::VariableName { type_name },
        CursorLocation::Import { prefix } => CursorLocation::Import {
            prefix: strip_sentinel(&prefix),
        },
        other => other,
    }
}

/// Count unmatched `{` and `(` in src, return closing string to append.
pub(crate) fn close_open_brackets(src: &str) -> String {
    let mut braces: i32 = 0;
    let mut parens: i32 = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    let mut it = src.chars().peekable();
    while let Some(c) = it.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' if !in_char => in_string = !in_string,
            '\'' if !in_string => in_char = !in_char,
            _ if in_string || in_char => {}
            '/' if it.peek() == Some(&'/') => {
                // skip to end of line
                for nc in it.by_ref() {
                    if nc == '\n' {
                        break;
                    }
                }
            }
            '/' if it.peek() == Some(&'*') => {
                // skip block comment
                it.next(); // consume '*'
                let mut prev = ' ';
                for nc in it.by_ref() {
                    if prev == '*' && nc == '/' {
                        break;
                    }
                    prev = nc;
                }
            }
            '{' => braces += 1,
            '}' => {
                if braces > 0 {
                    braces -= 1;
                }
            }
            '(' => parens += 1,
            ')' => {
                if parens > 0 {
                    parens -= 1;
                }
            }
            _ => {}
        }
    }
    let mut tail = String::new();
    for _ in 0..parens {
        tail.push(')');
    }
    tail.push(';');
    for _ in 0..braces {
        tail.push('}');
    }
    tail
}

pub(crate) fn get_initializer_text(type_node: Node, bytes: &[u8]) -> Option<String> {
    let decl = type_node.parent()?;
    if decl.kind() != "local_variable_declaration" {
        return None;
    }
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let init = child.named_child(1)?;
        return init.utf8_text(bytes).ok().map(|s| s.to_string());
    }
    None
}

pub(crate) fn find_enclosing_method_in_error(root: Node, offset: usize) -> Option<Node> {
    // When the file is a top-level ERROR, method_declaration nodes may still
    // exist as direct children of the ERROR node
    fn dfs<'a>(node: Node<'a>, offset: usize, result: &mut Option<Node<'a>>) {
        if node.start_byte() > offset {
            return;
        }
        if node.kind() == "method_declaration"
            && node.start_byte() <= offset
            && node.end_byte() >= offset
        {
            *result = Some(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            dfs(child, offset, result);
        }
    }
    let mut result = None;
    dfs(root, offset, &mut result);
    result
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
        let mut cursor = node.walk();
        let Some(child) = node
            .named_children(&mut cursor)
            .find(|child| child.kind() != "identifier")
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
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let init = child.named_child(1)?;
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
    }
    None
}

pub fn is_cursor_in_comment_with_rope(source: &str, rope: &Rope, offset: usize) -> bool {
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
    let line_idx = rope.byte_to_line(offset.min(source.len().saturating_sub(1)));
    let line_byte_start = rope.line_to_byte(line_idx);
    let safe_offset = offset.max(line_byte_start).min(source.len());
    is_in_line_comment(&source[line_byte_start..safe_offset])
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

pub fn find_error_ancestor(mut node: Node) -> Option<Node> {
    loop {
        if node.kind() == "ERROR" {
            return Some(node);
        }
        node = node.parent()?;
    }
}

pub fn error_has_new_keyword(error_node: Node) -> bool {
    let mut cursor = error_node.walk();
    error_node.children(&mut cursor).any(|c| c.kind() == "new")
}

pub fn find_identifier_in_error(error_node: Node) -> Option<Node> {
    let mut cursor = error_node.walk();
    error_node
        .children(&mut cursor)
        .find(|c| c.kind() == "identifier" || c.kind() == "type_identifier")
}

pub fn error_has_trailing_dot(error_node: Node, offset: usize) -> bool {
    let mut cursor = error_node.walk();
    let children: Vec<Node> = error_node.children(&mut cursor).collect();
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

pub(crate) fn find_string_ancestor<'a>(mut node: Node<'a>) -> Option<Node<'a>> {
    loop {
        match node.kind() {
            "string_literal" | "text_block" => return Some(node),
            _ => {}
        }
        node = node.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use crate::language::java::utils::close_open_brackets;

    #[test]
    fn test_close_open_brackets_unit() {
        assert_eq!(close_open_brackets("class A { void f() {"), ";}}");
        assert_eq!(close_open_brackets("foo(a, b"), ");");
        assert_eq!(close_open_brackets("class A { void f() { foo("), ");}}");
        assert_eq!(close_open_brackets("class A {}"), ";");
        assert_eq!(close_open_brackets("}}}}"), ";");
    }
}
