use crate::language::java::type_ctx::SourceTypeCtx;
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

pub(crate) fn strip_leading_java_type_modifiers(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("final ") {
            s = rest;
            continue;
        }
        if s.starts_with('@')
            && let Some(space) = s.find(' ')
        {
            s = &s[space + 1..];
            continue;
        }
        break;
    }
    s.trim()
}

pub(crate) fn split_java_generic_base(ty: &str) -> Option<(&str, Option<&str>)> {
    if let Some(start) = ty.find('<') {
        let mut depth = 0i32;
        for (i, c) in ty.char_indices().skip(start) {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        let base = ty[..start].trim();
                        let args = ty[start + 1..i].trim();
                        return Some((base, Some(args)));
                    }
                }
                _ => {}
            }
        }
        None
    } else {
        Some((ty.trim(), None))
    }
}

pub(crate) fn split_java_generic_args(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                result.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        result.push(s[start..].trim());
    }
    result.into_iter().filter(|x| !x.is_empty()).collect()
}

pub(crate) fn split_top_level_java_intersection_bounds(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            '&' if depth == 0 => {
                let bound = s[start..i].trim();
                if !bound.is_empty() {
                    result.push(bound);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        result.push(tail);
    }
    result
}

pub(crate) fn source_type_to_signature(type_ctx: &SourceTypeCtx, ty: &str) -> String {
    let mut s = strip_leading_java_type_modifiers(ty.trim());
    if s == "?" {
        return "*".to_string();
    }
    if let Some(bound) = s.strip_prefix("? extends ") {
        return format!("+{}", source_type_to_signature(type_ctx, bound));
    }
    if let Some(bound) = s.strip_prefix("? super ") {
        return format!("-{}", source_type_to_signature(type_ctx, bound));
    }

    let mut dims = 0usize;
    if let Some(stripped) = s.strip_suffix("...") {
        s = stripped.trim();
        dims += 1;
    }
    while let Some(stripped) = s.strip_suffix("[]") {
        s = stripped.trim();
        dims += 1;
    }

    let bounds = split_top_level_java_intersection_bounds(s);
    if bounds.len() > 1 {
        return source_type_to_signature(type_ctx, bounds[0]);
    }

    let (base, args) = split_java_generic_base(s).unwrap_or((s, None));
    let mut out = match base {
        "void" => "V".to_string(),
        "boolean" => "Z".to_string(),
        "byte" => "B".to_string(),
        "char" => "C".to_string(),
        "short" => "S".to_string(),
        "int" => "I".to_string(),
        "long" => "J".to_string(),
        "float" => "F".to_string(),
        "double" => "D".to_string(),
        other => {
            let resolved = type_ctx.resolve_simple(other);
            let internal = resolved.replace('.', "/");
            let is_type_var =
                !internal.contains('/') && internal.chars().all(|c| c.is_ascii_uppercase());
            if is_type_var {
                format!("T{};", internal)
            } else if let Some(args_text) = args {
                let rendered_args = split_java_generic_args(args_text)
                    .into_iter()
                    .map(|a| source_type_to_signature(type_ctx, a))
                    .collect::<Vec<_>>()
                    .join("");
                format!("L{}<{}>;", internal, rendered_args)
            } else {
                format!("L{};", internal)
            }
        }
    };

    while dims > 0 {
        out = format!("[{out}");
        dims -= 1;
    }
    out
}

/// Extracts generic parameters from a class or method and constructs them into a generic signature according to the JVM specification.
/// For example, extracts `<T:Ljava/lang/Object;E:Ljava/lang/Object;>Ljava/lang/Object;` from `class List<T, E>`.
pub fn extract_generic_signature(
    node: Node,
    bytes: &[u8],
    suffix: &str,
    type_ctx: Option<&SourceTypeCtx>,
) -> Option<Arc<str>> {
    let mut sig = extract_type_parameters_prefix(node, bytes, type_ctx)?;
    sig.push_str(suffix);
    Some(Arc::from(sig))
}

/// Extract only the `<...>` type-parameter prefix from class/method declarations.
pub fn extract_type_parameters_prefix(
    node: Node,
    bytes: &[u8],
    type_ctx: Option<&SourceTypeCtx>,
) -> Option<String> {
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
                    if let Some(bound_node) = traversal::first_child_of_kind(child, "type_bound") {
                        let bound_text = node_text(bound_node, bytes).trim();
                        let bounds = bound_text
                            .strip_prefix("extends")
                            .map(str::trim)
                            .map(split_top_level_java_intersection_bounds)
                            .unwrap_or_default();
                        if bounds.is_empty() {
                            sig.push_str(":Ljava/lang/Object;");
                        } else if let Some(type_ctx) = type_ctx {
                            for bound in bounds {
                                sig.push(':');
                                sig.push_str(&source_type_to_signature(type_ctx, bound));
                            }
                        } else {
                            sig.push_str(":Ljava/lang/Object;");
                        }
                    } else {
                        sig.push_str(":Ljava/lang/Object;");
                    }
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

pub(crate) fn strip_sentinel(s: &str) -> String {
    s.to_string()
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
