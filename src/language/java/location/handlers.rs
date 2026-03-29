use crate::language::java::JavaContextExtractor;
use crate::language::java::location::heuristics::{
    detect_member_tail_in_misread_local_decl, detect_new_keyword_before_cursor,
    detect_trailing_dot_in_text, handle_import_from_text, is_import_context,
    is_misread_expression_in_local_decl, is_variable_name_after_complete_type,
    is_variable_name_after_previous_declaration, recover_error_location_ast_first,
};
use crate::language::java::utils::strip_sentinel;
use crate::language::java::{
    completion_context::normalize_top_level_generic_base,
    utils::{is_comment_kind, is_in_name_position, is_in_type_position},
};
use crate::semantic::CursorLocation;
use std::collections::HashSet;
use std::sync::Arc;
use tree_sitter::Node;
use tree_sitter_utils::traversal::is_descendant_of;
use tree_sitter_utils::{
    Handler, HandlerExt, Input, handler_fn, has_parent_kind, traversal::ancestor_of_kind,
};

use super::utils::cursor_truncated_text;

pub(super) fn extract_string_prefix(ctx: &JavaContextExtractor, str_node: Node) -> String {
    let start = str_node.start_byte();
    let end = str_node.end_byte();
    let cut = ctx.offset.min(end);
    if cut <= start {
        return String::new();
    }
    let raw = &ctx.source[start..cut];
    match str_node.kind() {
        "string_literal" => {
            if let Some(pos) = raw.find('"') {
                let after = &raw[pos + 1..];
                return after.strip_suffix('"').unwrap_or(after).to_string();
            }
            String::new()
        }
        "text_block" => {
            let s = raw.to_string();
            if let Some(rest) = s.strip_prefix("\"\"\"") {
                let mut rest = rest;
                if rest.starts_with('\n') {
                    rest = &rest[1..];
                }
                return rest.to_string();
            }
            s
        }
        _ => raw.to_string(),
    }
}

pub(super) fn handle_jump_statement(
    ctx: &JavaContextExtractor,
    stmt: Node,
) -> Option<(CursorLocation, String)> {
    use crate::semantic::context::StatementLabelCompletionKind;

    let kind = match stmt.kind() {
        "break_statement" => StatementLabelCompletionKind::Break,
        "continue_statement" => StatementLabelCompletionKind::Continue,
        _ => return None,
    };

    let label_node = stmt
        .named_children(&mut stmt.walk())
        .find(|c| c.kind() == "identifier");

    if let Some(ident) = label_node {
        if ctx.offset >= ident.start_byte() && ctx.offset <= ident.end_byte() {
            let prefix = strip_sentinel(&cursor_truncated_text(ctx, ident));
            return Some((
                CursorLocation::StatementLabel {
                    kind,
                    prefix: prefix.clone(),
                },
                prefix,
            ));
        }
        return None;
    }

    let keyword_end = stmt
        .child(0)
        .map(|c| c.end_byte())
        .unwrap_or(stmt.start_byte());
    if ctx.offset > keyword_end {
        return Some((
            CursorLocation::StatementLabel {
                kind,
                prefix: String::new(),
            },
            String::new(),
        ));
    }
    None
}

pub(super) fn handle_annotation(
    ctx: &JavaContextExtractor,
    node: Node,
) -> (CursorLocation, String) {
    let name_node = node.child_by_field_name("name");
    let prefix = name_node
        .map(|n| cursor_truncated_text(ctx, n))
        .unwrap_or_default();
    let target_element_type = infer_annotation_target(node);
    (
        CursorLocation::Annotation {
            prefix: prefix.clone(),
            target_element_type,
        },
        prefix,
    )
}

pub(super) fn detect_annotation_param_location(
    ctx: &JavaContextExtractor,
    arg_list: Node,
    orig: Option<Node>,
) -> Option<(CursorLocation, String)> {
    if arg_list.kind() != "annotation_argument_list" {
        return None;
    }

    let annotation = arg_list.parent()?;
    if annotation.kind() != "annotation" {
        return None;
    }

    if let Some(orig) = orig {
        if let Some(pair) = ancestor_of_kind(orig, "element_value_pair").filter(|pair| {
            pair.parent()
                .is_some_and(|parent| parent.id() == arg_list.id())
        }) {
            let key = pair.child_by_field_name("key")?;
            if orig.id() == key.id() || is_descendant_of(orig, key) {
                let prefix = strip_sentinel(&cursor_truncated_text(ctx, key));
                let used_keys = collect_annotation_param_keys(ctx, arg_list, Some(pair.id()));
                return Some(annotation_param_location(
                    ctx, annotation, prefix, used_keys, false,
                ));
            }
            return None;
        }

        if !annotation_argument_list_has_pairs(arg_list) {
            return None;
        }

        let prefix = strip_sentinel(&cursor_truncated_text(ctx, orig));
        if !is_annotation_param_prefix(&prefix) {
            return None;
        }

        let used_keys = collect_annotation_param_keys(ctx, arg_list, None);
        return Some(annotation_param_location(
            ctx, annotation, prefix, used_keys, true,
        ));
    }

    if is_fresh_annotation_param_position(ctx, arg_list) {
        let used_keys = collect_annotation_param_keys(ctx, arg_list, None);
        return Some(annotation_param_location(
            ctx,
            annotation,
            String::new(),
            used_keys,
            true,
        ));
    }

    let mut walker = arg_list.walk();
    for child in arg_list.named_children(&mut walker) {
        if !node_contains_cursor(ctx, child) {
            continue;
        }
        if child.kind() != "element_value_pair" {
            return None;
        }
        let key = child.child_by_field_name("key")?;
        if ctx.offset > key.end_byte() {
            return None;
        }
        let prefix = strip_sentinel(&cursor_truncated_text(ctx, key));
        let used_keys = collect_annotation_param_keys(ctx, arg_list, Some(child.id()));
        return Some(annotation_param_location(
            ctx, annotation, prefix, used_keys, false,
        ));
    }

    None
}

pub(super) fn handle_annotation_argument_list(
    ctx: &JavaContextExtractor,
    node: Node,
) -> (CursorLocation, String) {
    detect_annotation_param_location(ctx, node, None).unwrap_or_else(|| {
        (
            CursorLocation::Expression {
                prefix: String::new(),
            },
            String::new(),
        )
    })
}

fn annotation_param_location(
    ctx: &JavaContextExtractor,
    annotation: Node,
    prefix: String,
    used_keys: Vec<Arc<str>>,
    fresh_slot: bool,
) -> (CursorLocation, String) {
    let annotation_name = annotation
        .child_by_field_name("name")
        .map(|name| Arc::from(ctx.node_text(name)));
    (
        CursorLocation::AnnotationParam {
            prefix: prefix.clone(),
            annotation_name,
            used_keys,
            fresh_slot,
        },
        prefix,
    )
}

fn collect_annotation_param_keys(
    ctx: &JavaContextExtractor,
    arg_list: Node,
    skip_pair_id: Option<usize>,
) -> Vec<Arc<str>> {
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    let mut walker = arg_list.walk();
    for child in arg_list.named_children(&mut walker) {
        if child.kind() != "element_value_pair" {
            continue;
        }
        if skip_pair_id.is_some_and(|id| id == child.id()) {
            continue;
        }
        let Some(key) = child.child_by_field_name("key") else {
            continue;
        };
        let text = ctx.node_text(key).trim();
        if text.is_empty() || !seen.insert(text.to_string()) {
            continue;
        }
        keys.push(Arc::from(text));
    }
    keys
}

fn annotation_argument_list_has_pairs(arg_list: Node) -> bool {
    let mut walker = arg_list.walk();
    arg_list
        .named_children(&mut walker)
        .any(|child| child.kind() == "element_value_pair")
}

fn is_fresh_annotation_param_position(ctx: &JavaContextExtractor, arg_list: Node) -> bool {
    let start = arg_list.start_byte();
    if ctx.offset <= start {
        return false;
    }
    let raw = &ctx.source[start..ctx.offset];
    let clean = strip_sentinel(raw);
    let last_non_ws = clean.chars().rev().find(|c| !c.is_whitespace());
    matches!(last_non_ws, Some(',') | Some('('))
}

fn node_contains_cursor(ctx: &JavaContextExtractor, node: Node) -> bool {
    node.start_byte() <= ctx.offset && ctx.offset <= node.end_byte().saturating_add(1)
}

fn is_annotation_param_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn infer_annotation_target(node: Node) -> Option<Arc<str>> {
    // Each handler arm maps a node kind to the corresponding annotation
    // target string.  The chain climbs ancestors until one matches or a
    // stop-kind is reached.
    //
    // The "RECORD_COMPONENT" arm is guarded with `has_parent_kind` so it
    // only fires for formal/spread parameters that are record components.
    let handler = handler_fn(|_: Input<()>| Arc::from("TYPE"))
        .for_kinds(&[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "annotation_type_declaration",
            "record_declaration",
        ])
        .or(handler_fn(|_: Input<()>| Arc::from("RECORD_COMPONENT"))
            .for_kinds(&["formal_parameter", "spread_parameter"])
            .when(has_parent_kind("record_declaration")))
        .or(handler_fn(|_: Input<()>| Arc::from("METHOD")).for_kinds(&["method_declaration"]))
        .or(handler_fn(|_: Input<()>| Arc::from("FIELD")).for_kinds(&["field_declaration"]))
        .or(handler_fn(|_: Input<()>| Arc::from("PARAMETER"))
            .for_kinds(&["formal_parameter", "spread_parameter"]))
        .or(handler_fn(|_: Input<()>| Arc::from("CONSTRUCTOR"))
            .for_kinds(&["constructor_declaration", "compact_constructor_declaration"]))
        .or(handler_fn(|_: Input<()>| Arc::from("LOCAL_VARIABLE"))
            .for_kinds(&["local_variable_declaration"]))
        // Climb past unrecognised kinds; stop at scope boundaries.
        .climb(&["ERROR", "class_body", "block", "program"]);

    // The annotation node itself is excluded — we start from its parent.
    let start = node.parent()?;
    handler.handle(Input::new(start, (), None))
}

pub(super) fn handle_import(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
    let mut is_static = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "static" {
            is_static = true;
            break;
        }
    }
    let mut raw_prefix = String::new();
    collect_import_text(ctx, node, &mut raw_prefix);
    let prefix = strip_sentinel(&raw_prefix);
    let query = prefix.rsplit('.').next().unwrap_or("").to_string();
    let location = if is_static {
        CursorLocation::ImportStatic { prefix }
    } else {
        CursorLocation::Import { prefix }
    };
    (location, query)
}

fn collect_import_text(ctx: &JavaContextExtractor, node: Node, out: &mut String) {
    if node.start_byte() >= ctx.offset {
        return;
    }
    if node.child_count() == 0 {
        let kind = node.kind();
        if kind == "import" || kind == "static" || kind == ";" || is_comment_kind(kind) {
            return;
        }
        let text = cursor_truncated_text(ctx, node);
        out.push_str(&text);
    } else {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_import_text(ctx, child, out);
        }
    }
}

pub(super) fn handle_member_access(
    ctx: &JavaContextExtractor,
    node: Node,
) -> (CursorLocation, String) {
    let mut walker = node.walk();
    let children: Vec<Node> = node.children(&mut walker).collect();

    let dot_pos = match children.iter().position(|n| n.kind() == ".") {
        Some(p) => p,
        None => {
            let name_node = children
                .iter()
                .find(|n| n.kind() == "identifier" || n.kind() == "type_identifier");
            let text = name_node
                .map(|n| cursor_truncated_text(ctx, *n))
                .unwrap_or_default();
            let clean = strip_sentinel(&text);
            let arguments = if node.kind() == "method_invocation" {
                node.child_by_field_name("arguments")
                    .map(|n| ctx.node_text(n).to_string())
            } else {
                None
            };
            if arguments.is_some() {
                return (
                    CursorLocation::MemberAccess {
                        receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                        receiver_semantic_type: None,
                        receiver_type: None,
                        member_prefix: clean.clone(),
                        receiver_expr: String::new(),
                        arguments,
                    },
                    clean,
                );
            }
            return (
                CursorLocation::Expression {
                    prefix: clean.clone(),
                },
                clean,
            );
        }
    };

    let dot_node = children[dot_pos];

    if ctx.offset <= dot_node.start_byte() {
        let prefix = if dot_pos > 0 {
            let raw = cursor_truncated_text(ctx, children[dot_pos - 1]);
            strip_sentinel(&raw)
        } else {
            String::new()
        };
        return (
            CursorLocation::Expression {
                prefix: prefix.clone(),
            },
            prefix,
        );
    }

    let member_node = children[dot_pos + 1..]
        .iter()
        .find(|n| n.kind() == "identifier" || n.kind() == "type_identifier");

    let member_prefix = match member_node {
        Some(mn) => {
            let s = mn.start_byte();
            let e = mn.end_byte();
            let raw = if ctx.offset <= s {
                String::new()
            } else if ctx.offset < e {
                ctx.source[s..ctx.offset].to_string()
            } else {
                ctx.node_text(*mn).to_string()
            };
            strip_sentinel(&raw)
        }
        None => String::new(),
    };

    let receiver_expr = if dot_pos > 0 {
        ctx.node_text(children[dot_pos - 1]).to_string()
    } else {
        String::new()
    };

    let arguments = if node.kind() == "method_invocation" {
        node.child_by_field_name("arguments")
            .map(|n| ctx.node_text(n).to_string())
    } else {
        None
    };

    (
        CursorLocation::MemberAccess {
            receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
            receiver_semantic_type: None,
            receiver_type: None,
            member_prefix: member_prefix.clone(),
            receiver_expr,
            arguments,
        },
        member_prefix,
    )
}

pub(super) fn handle_method_reference(
    ctx: &JavaContextExtractor,
    node: Node,
) -> (CursorLocation, String) {
    let start = node.start_byte();
    let end = node.end_byte().min(ctx.offset);
    if end <= start {
        return (CursorLocation::Unknown, String::new());
    }
    let raw = &ctx.source[start..end];
    let Some(sep_idx) = raw.find("::") else {
        let prefix = strip_sentinel(raw.trim());
        return (
            CursorLocation::Expression {
                prefix: prefix.clone(),
            },
            prefix,
        );
    };
    let qualifier_expr = strip_sentinel(raw[..sep_idx].trim());
    let rhs_raw = raw[sep_idx + 2..].trim_start();
    let rhs_no_type_args = strip_leading_type_arguments(rhs_raw);
    let member_prefix = strip_sentinel(rhs_no_type_args.trim());
    let is_constructor = member_prefix == "new" || member_prefix.starts_with("new");
    let query = if is_constructor {
        qualifier_expr.clone()
    } else {
        member_prefix.clone()
    };
    (
        CursorLocation::MethodReference {
            qualifier_expr,
            member_prefix,
            is_constructor,
        },
        query,
    )
}

fn strip_leading_type_arguments(s: &str) -> &str {
    let trimmed = s.trim_start();
    if !trimmed.starts_with('<') {
        return trimmed;
    }
    let mut depth = 0i32;
    for (i, c) in trimmed.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return trimmed[i + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    trimmed
}

pub(super) fn handle_constructor(
    ctx: &JavaContextExtractor,
    node: Node,
) -> (CursorLocation, String) {
    if let Some(relative_semicolon) =
        ctx.source[node.start_byte()..ctx.offset.min(ctx.source.len())].rfind(';')
    {
        let semicolon_offset = node.start_byte() + relative_semicolon;
        let trailing = &ctx.source[semicolon_offset + 1..ctx.offset.min(ctx.source.len())];
        if trailing.chars().any(|ch| !ch.is_whitespace()) {
            let prefix = extract_identifier_prefix_near_cursor(ctx, semicolon_offset + 1);
            return (
                CursorLocation::Expression {
                    prefix: prefix.clone(),
                },
                prefix,
            );
        }
    }

    let type_node = node.child_by_field_name("type");
    let qualifier_expr = constructor_qualifier_expr(ctx, node, type_node);

    if let Some(new_node) = tree_sitter_utils::traversal::any_child_of_kind(node, "new")
        && ctx.offset <= new_node.start_byte()
    {
        if let Some((receiver_expr, member_prefix)) =
            qualified_constructor_member_access_before_new(ctx, node, new_node, type_node)
        {
            return (
                CursorLocation::MemberAccess {
                    receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                    receiver_semantic_type: None,
                    receiver_type: None,
                    member_prefix: member_prefix.clone(),
                    receiver_expr,
                    arguments: None,
                },
                member_prefix,
            );
        }

        let prefix = extract_identifier_prefix_near_cursor(ctx, node.start_byte());
        return (
            CursorLocation::Expression {
                prefix: prefix.clone(),
            },
            prefix,
        );
    }

    if let Some(ty) = type_node
        && ctx.offset < ty.start_byte()
    {
        let gap = &ctx.source[ctx.offset..ty.start_byte()];
        if gap.contains('\n') {
            // Find `new` anonymous node
            let new_node = tree_sitter_utils::traversal::any_child_of_kind(node, "new");
            if let Some(new_n) = new_node
                && ctx.offset >= new_n.end_byte()
            {
                // cursor is after `new` but before type on next line
                let expected_type = infer_expected_type_from_lhs(ctx, node);
                return (
                    CursorLocation::ConstructorCall {
                        class_prefix: String::new(),
                        expected_type,
                        qualifier_expr,
                        qualifier_owner_internal: None,
                    },
                    String::new(),
                );
            }
            return (CursorLocation::Unknown, String::new());
        }
    }

    let class_prefix = type_node
        .map(|n| {
            let raw = cursor_truncated_text(ctx, n);
            let clean = strip_sentinel(&raw);
            normalize_top_level_generic_base(&clean).to_string()
        })
        .unwrap_or_default();
    let expected_type = infer_expected_type_from_lhs(ctx, node);
    (
        CursorLocation::ConstructorCall {
            class_prefix: class_prefix.clone(),
            expected_type,
            qualifier_expr,
            qualifier_owner_internal: None,
        },
        class_prefix,
    )
}

fn constructor_qualifier_expr(
    ctx: &JavaContextExtractor,
    node: Node,
    type_node: Option<Node>,
) -> Option<String> {
    let type_start = type_node?.start_byte();
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        if child.end_byte() > type_start {
            continue;
        }
        if matches!(
            child.kind(),
            "_simple_type" | "type_identifier" | "generic_type" | "scoped_type_identifier"
        ) {
            continue;
        }
        let text = ctx.node_text(child).trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn qualified_constructor_member_access_before_new(
    ctx: &JavaContextExtractor,
    node: Node,
    new_node: Node,
    type_node: Option<Node>,
) -> Option<(String, String)> {
    let receiver_expr = constructor_qualifier_expr(ctx, node, type_node)?;
    let before_new = &ctx.source[node.start_byte()..new_node.start_byte()];
    let separator_offset = node.start_byte() + before_new.rfind('.')?;
    if ctx.offset < separator_offset.saturating_add(1) {
        return None;
    }

    let member_prefix = extract_identifier_prefix_near_cursor(ctx, separator_offset + 1);
    Some((receiver_expr, member_prefix))
}

fn infer_expected_type_from_lhs(ctx: &JavaContextExtractor, node: Node) -> Option<String> {
    let decl = ancestor_of_kind(node, "local_variable_declaration")?;
    let mut walker = decl.walk();
    for child in decl.named_children(&mut walker) {
        if child.kind() == "modifiers" {
            continue;
        }
        let ty = ctx.node_text(child);
        if !ty.is_empty() {
            return Some(ty.to_string());
        }
    }
    None
}

pub(super) fn handle_argument_list(
    ctx: &JavaContextExtractor,
    node: Node,
) -> (CursorLocation, String) {
    if let Some((receiver_expr, member_prefix)) = detect_member_access_in_arg_list(ctx, node) {
        return (
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: member_prefix.clone(),
                receiver_expr,
                arguments: None,
            },
            member_prefix,
        );
    }
    if is_fresh_argument_position(ctx, node) {
        return (
            CursorLocation::MethodArgument {
                prefix: String::new(),
            },
            String::new(),
        );
    }
    let prefix = find_prefix_in_argument_list(ctx, node);
    (
        CursorLocation::MethodArgument {
            prefix: prefix.clone(),
        },
        prefix,
    )
}

fn detect_member_access_in_arg_list(
    ctx: &JavaContextExtractor,
    arg_list: Node,
) -> Option<(String, String)> {
    let mut walker = arg_list.walk();
    for child in arg_list.named_children(&mut walker) {
        let child_end = child.end_byte();
        if child_end > ctx.offset {
            continue;
        }
        let gap = &ctx.source[child_end..ctx.offset];
        let gap_clean = strip_sentinel(gap);
        let gap_trimmed = gap_clean.trim_start();
        if let Some(stripped) = gap_trimmed.strip_prefix('.') {
            let receiver_expr = ctx.source[child.start_byte()..child_end].to_string();
            let after_dot = stripped.trim_start();
            let member_prefix = strip_sentinel(after_dot).trim_end().to_string();
            return Some((receiver_expr, member_prefix));
        }
    }
    None
}

fn is_fresh_argument_position(ctx: &JavaContextExtractor, arg_list: Node) -> bool {
    let start = arg_list.start_byte();
    if ctx.offset <= start {
        return false;
    }
    let raw = &ctx.source[start..ctx.offset];
    let clean = strip_sentinel(raw);
    let last_non_ws = clean.chars().rev().find(|c| !c.is_whitespace());
    matches!(last_non_ws, Some(',') | Some('('))
}

fn find_prefix_in_argument_list(ctx: &JavaContextExtractor, arg_list: Node) -> String {
    let mut cursor = arg_list.walk();
    for child in arg_list.named_children(&mut cursor) {
        if child.start_byte() <= ctx.offset && child.end_byte() >= ctx.offset.saturating_sub(1) {
            let nested = find_prefix_in_expr_subtree(ctx, child);
            if !nested.is_empty() {
                return nested;
            }
            let clean = strip_sentinel(&cursor_truncated_text(ctx, child));
            if is_empty_expression_site_after_operator(&clean) {
                return String::new();
            }
            if child.kind() == "ERROR" {
                let last_non_ws = clean.chars().rev().find(|c| !c.is_whitespace());
                if matches!(last_non_ws, Some(',') | Some('(')) {
                    return String::new();
                }
            }
            if clean.trim() == "," {
                return String::new();
            }
            return clean;
        }
    }
    String::new()
}

fn is_empty_expression_site_after_operator(prefix: &str) -> bool {
    let trimmed = prefix.trim_end();
    let Some(last) = trimmed.chars().next_back() else {
        return true;
    };
    matches!(
        last,
        '+' | '-'
            | '*'
            | '/'
            | '%'
            | '&'
            | '|'
            | '^'
            | '<'
            | '>'
            | '='
            | '!'
            | '?'
            | ':'
            | '('
            | '['
            | '{'
            | ','
    )
}

fn find_prefix_in_expr_subtree(ctx: &JavaContextExtractor, node: Node) -> String {
    if node.start_byte() > ctx.offset || node.end_byte() < ctx.offset.saturating_sub(1) {
        return String::new();
    }
    if matches!(
        node.kind(),
        "identifier" | "type_identifier" | "this" | "super"
    ) {
        return strip_sentinel(&cursor_truncated_text(ctx, node));
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        if child.start_byte() <= ctx.offset && child.end_byte() >= ctx.offset.saturating_sub(1) {
            let nested = find_prefix_in_expr_subtree(ctx, child);
            if !nested.is_empty() {
                return nested;
            }
        }
    }
    extract_identifier_prefix_near_cursor(ctx, node.start_byte())
}

fn extract_identifier_prefix_near_cursor(ctx: &JavaContextExtractor, lower_bound: usize) -> String {
    let mut i = ctx.offset.min(ctx.source.len());
    while i > lower_bound {
        let ch = ctx.source.as_bytes()[i - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i >= ctx.offset {
        return String::new();
    }
    strip_sentinel(&ctx.source[i..ctx.offset])
}

/// Case 7: expression_statement contains [expr, ERROR(".")]
pub(super) fn handle_expression_statement(
    ctx: &JavaContextExtractor,
    stmt: Node,
) -> Option<(CursorLocation, String)> {
    let mut wc = stmt.walk();
    let named: Vec<Node> = stmt.named_children(&mut wc).collect();

    // Case 7: ERROR child containing only "."
    let error_child = named.iter().find(|n| {
        if n.kind() != "ERROR" {
            return false;
        }
        let mut wc2 = n.walk();
        let ch: Vec<_> = n.children(&mut wc2).collect();
        ch.len() == 1 && ch[0].kind() == "."
    });

    if let Some(error_child) = error_child {
        let receiver_node = named
            .iter()
            .rev()
            .find(|n| n.kind() != "ERROR" && n.end_byte() <= error_child.start_byte())?;
        let receiver_expr = ctx.node_text(*receiver_node).trim().to_string();
        if receiver_expr.is_empty() {
            return None;
        }
        let dot_end = error_child.start_byte() + 1;
        let member_prefix = if ctx.offset > dot_end && dot_end <= ctx.source.len() {
            ctx.source[dot_end..ctx.offset.min(ctx.source.len())]
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect()
        } else {
            String::new()
        };
        return Some((
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: member_prefix.clone(),
                receiver_expr,
                arguments: None,
            },
            member_prefix,
        ));
    }

    // Case D: binary/ternary expression with MISSING right operand
    // `testValue + /*cursor*/` → Expression{ prefix: "" }
    for child in &named {
        if matches!(
            child.kind(),
            "binary_expression" | "ternary_expression" | "assignment_expression"
        ) && let Some(right) = child
            .child_by_field_name("right")
            .or_else(|| child.child_by_field_name("value"))
            && right.is_missing()
            && ctx.offset >= child.start_byte()
        {
            return Some((
                CursorLocation::Expression {
                    prefix: String::new(),
                },
                String::new(),
            ));
        }
    }

    None
}

pub(super) fn handle_identifier(
    ctx: &JavaContextExtractor,
    node: Node,
    _trigger_char: Option<char>,
) -> (CursorLocation, String) {
    // Heuristic: if a complete declaration ends on a previous line, treat the
    // current identifier as a type starting a new declaration.
    if let Some(type_name) = is_variable_name_after_previous_declaration(node, ctx) {
        return (CursorLocation::VariableName { type_name }, String::new());
    }
    if let Some(err) = ancestor_of_kind(node, "ERROR")
        && let Some(result) = recover_error_location_ast_first(ctx, err)
    {
        return result;
    }
    // Dispatch each ancestor kind to its handler using a combinator chain.
    // Context carries (extractor, original_identifier_node).
    // `.climb(stop_kinds)` walks parent() until a handler returns Some or a
    // stop-kind is reached.
    type Ctx<'a> = (&'a JavaContextExtractor, Node<'a>);

    let id_handler = (|inp: Input<Ctx<'_>>| {
        let (ctx, orig): (&JavaContextExtractor, Node) = inp.ctx;
        let name = inp.node.child_by_field_name("name")?;
        if orig.id() == name.id() || is_descendant_of(orig, name) {
            return Some(handle_annotation(ctx, inp.node));
        }
        None
    })
    .for_kinds(&["marker_annotation", "annotation"])
    .or(
        handler_fn(|inp: Input<Ctx<'_>>| handle_member_access(inp.ctx.0, inp.node))
            .for_kinds(&["field_access", "method_invocation"]),
    )
    .or(
        handler_fn(|inp: Input<Ctx<'_>>| handle_method_reference(inp.ctx.0, inp.node))
            .for_kinds(&["method_reference"]),
    )
    .or(
        handler_fn(|inp: Input<Ctx<'_>>| handle_import(inp.ctx.0, inp.node))
            .for_kinds(&["import_declaration"]),
    )
    .or((|inp: Input<Ctx<'_>>| {
        let (ctx, orig): (&JavaContextExtractor, Node) = inp.ctx;
        if is_in_constructor_type_arguments(orig, inp.node) {
            let text = cursor_truncated_text(ctx, orig);
            return Some((
                CursorLocation::TypeAnnotation {
                    prefix: text.clone(),
                },
                text,
            ));
        }
        Some(handle_constructor(ctx, inp.node))
    })
    .for_kinds(&["object_creation_expression"]))
    .or((|inp: Input<Ctx<'_>>| {
        let (ctx, orig): (&JavaContextExtractor, Node) = inp.ctx;
        if is_misread_expression_in_local_decl(orig, inp.node, ctx) {
            let text = cursor_truncated_text(ctx, orig);
            let clean = strip_sentinel(&text);
            return Some((
                CursorLocation::Expression {
                    prefix: clean.clone(),
                },
                clean,
            ));
        }
        if is_variable_name_after_complete_type(orig, inp.node, ctx) {
            let type_name = extract_type_from_decl(ctx, inp.node);
            return Some((CursorLocation::VariableName { type_name }, String::new()));
        }
        if let Some((receiver_expr, member_prefix)) =
            detect_member_tail_in_misread_local_decl(ctx, orig, inp.node)
        {
            return Some((
                CursorLocation::MemberAccess {
                    receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                    receiver_semantic_type: None,
                    receiver_type: None,
                    member_prefix: member_prefix.clone(),
                    receiver_expr,
                    arguments: None,
                },
                member_prefix,
            ));
        }
        if is_in_type_position(orig, inp.node) {
            let text = cursor_truncated_text(ctx, orig);
            return Some((
                CursorLocation::TypeAnnotation {
                    prefix: text.clone(),
                },
                text,
            ));
        }
        if is_in_name_position(orig, inp.node) {
            let type_name = extract_type_from_decl(ctx, inp.node);
            return Some((CursorLocation::VariableName { type_name }, String::new()));
        }
        if is_in_type_subtree(orig, inp.node) {
            let text = cursor_truncated_text(ctx, orig);
            return Some((
                CursorLocation::TypeAnnotation {
                    prefix: text.clone(),
                },
                text,
            ));
        }
        let text = cursor_truncated_text(ctx, orig);
        let clean = strip_sentinel(&text);
        Some((
            CursorLocation::Expression {
                prefix: clean.clone(),
            },
            clean,
        ))
    })
    .for_kinds(&["local_variable_declaration"]))
    .or((|inp: Input<Ctx<'_>>| {
        let (ctx, orig): (&JavaContextExtractor, Node) = inp.ctx;
        if is_in_formal_param_name_position(orig, inp.node) {
            let type_name = inp
                .node
                .child_by_field_name("type")
                .map(|n| ctx.node_text(n).trim().to_string())
                .unwrap_or_default();
            return Some((CursorLocation::VariableName { type_name }, String::new()));
        }
        let text = cursor_truncated_text(ctx, orig);
        Some((
            CursorLocation::TypeAnnotation {
                prefix: text.clone(),
            },
            text,
        ))
    })
    .for_kinds(&["formal_parameter", "spread_parameter"]))
    .or((|inp: Input<Ctx<'_>>| {
        let (ctx, orig): (&JavaContextExtractor, Node) = inp.ctx;
        detect_annotation_param_location(ctx, inp.node, Some(orig))
    })
    .for_kinds(&["annotation_argument_list"]))
    .or((|inp: Input<Ctx<'_>>| {
        let (ctx, orig): (&JavaContextExtractor, Node) = inp.ctx;
        if let Some(parent) = orig.parent()
            && parent.kind() == "method_invocation"
            && ctx.offset <= inp.node.start_byte()
        {
            return Some(handle_member_access(ctx, parent));
        }
        Some(handle_argument_list(ctx, inp.node))
    })
    .for_kinds(&["argument_list"]))
    .or((|inp: Input<Ctx<'_>>| {
        let (ctx, orig): (&JavaContextExtractor, Node) = inp.ctx;
        if let Some(result) = recover_error_location_ast_first(ctx, inp.node) {
            return Some(result);
        }
        let before = &ctx.source[..ctx.offset.min(ctx.source.len())];
        if is_import_context(before) {
            return Some(handle_import_from_text(ctx, before));
        }
        if let Some((receiver_expr, member_prefix)) = detect_trailing_dot_in_text(before) {
            return Some((
                CursorLocation::MemberAccess {
                    receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                    receiver_semantic_type: None,
                    receiver_type: None,
                    member_prefix: member_prefix.clone(),
                    receiver_expr,
                    arguments: None,
                },
                member_prefix,
            ));
        }
        if let Some(detected) = detect_new_keyword_before_cursor(before) {
            return Some((
                CursorLocation::ConstructorCall {
                    class_prefix: detected.class_prefix.clone(),
                    expected_type: None,
                    qualifier_expr: detected.qualifier_expr,
                    qualifier_owner_internal: None,
                },
                detected.class_prefix,
            ));
        }
        let text = cursor_truncated_text(ctx, orig);
        let clean = strip_sentinel(&text);
        Some((
            CursorLocation::Expression {
                prefix: clean.clone(),
            },
            clean,
        ))
    })
    .for_kinds(&["ERROR"]))
    .or((|inp: Input<Ctx<'_>>| {
        let (ctx, orig): (&JavaContextExtractor, Node) = inp.ctx;
        if inp.node.kind() == "class_body" {
            let text = cursor_truncated_text(ctx, orig);
            let clean = strip_sentinel(&text);
            return Some((
                CursorLocation::Expression {
                    prefix: clean.clone(),
                },
                clean,
            ));
        }
        // block/program: stop climbing (return None)
        None
    })
    .for_kinds(&["block", "class_body", "program"]))
    .climb(&["block", "class_body", "program"]);

    // combinator from the parent node to preserve identical semantics.
    let ctx_pair: Ctx<'_> = (ctx, node);
    if let Some(parent) = node.parent()
        && let Some(result) = id_handler.handle(Input::new(parent, ctx_pair, None))
    {
        return result;
    }

    let text = cursor_truncated_text(ctx, node);
    let clean = strip_sentinel(&text);
    (
        CursorLocation::Expression {
            prefix: clean.clone(),
        },
        clean,
    )
}

fn is_in_constructor_type_arguments(id_node: Node, ctor_node: Node) -> bool {
    let Some(ty) = ctor_node.child_by_field_name("type") else {
        return false;
    };
    let Some(type_args) = ancestor_of_kind(id_node, "type_arguments") else {
        return false;
    };
    is_descendant_of(type_args, ty) && is_descendant_of(id_node, type_args)
}

fn extract_type_from_decl(ctx: &JavaContextExtractor, decl_node: Node) -> String {
    let mut walker = decl_node.walk();
    for child in decl_node.named_children(&mut walker) {
        if child.kind() == "modifiers" {
            continue;
        }
        return ctx.node_text(child).trim().to_string();
    }
    String::new()
}

fn is_in_type_subtree(id_node: Node, decl_node: Node) -> bool {
    let mut walker = decl_node.walk();
    for child in decl_node.named_children(&mut walker) {
        if child.kind() == "modifiers" {
            continue;
        }
        return is_descendant_of(id_node, child);
    }
    false
}

fn is_in_formal_param_name_position(id_node: Node, param_node: Node) -> bool {
    param_node
        .child_by_field_name("name")
        .is_some_and(|n| n.id() == id_node.id())
}

#[cfg(test)]
mod tests {
    use ropey::Rope;
    use tree_sitter::Parser;

    use crate::language::java::location::handlers::infer_annotation_target;
    use crate::language::test_helpers::completion_context_from_source;
    use crate::semantic::SemanticContext;

    fn semantic_context_at_offset(source: &str, offset: usize) -> SemanticContext {
        let rope = Rope::from_str(source);
        let line = rope.byte_to_line(offset) as u32;
        let col = (offset - rope.line_to_byte(line as usize)) as u32;
        completion_context_from_source("java", source, line, col, None)
    }

    #[test]
    fn test_dangling_annotation_in_error_context_returns_none() {
        let src = indoc::indoc! {r#"
        class A {
            @Overr
        }
        "#};
        // Position cursor at the end of @Overr
        let offset = src.find("@Overr").unwrap() + 6;
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("failed to load java grammar");
        let tree = parser.parse(src, None).unwrap();
        let cursor_node = tree
            .root_node()
            .named_descendant_for_byte_range(offset.saturating_sub(1), offset)
            .or_else(|| {
                tree.root_node()
                    .named_descendant_for_byte_range(offset, offset + 1)
            });

        // Traverse up to find the annotation node
        let mut n = cursor_node.expect("Node should exist");
        while n.kind() != "marker_annotation" && n.kind() != "annotation" {
            if let Some(parent) = n.parent() {
                n = parent;
            } else {
                break;
            }
        }

        let target = infer_annotation_target(n);

        // Assert that we get None instead of "TYPE"
        assert!(
            target.is_none(),
            "Expected None for a dangling annotation inside an ERROR/class_body, but got {:?}",
            target
        );
    }

    #[test]
    fn test_identifier_after_empty_array_initializer_is_expression_without_space() {
        let src = "Object[] o = new Object[]{};\nArrayList";
        let offset = src.find("ArrayList").unwrap();
        let ctx = semantic_context_at_offset(src, offset);
        let location = ctx.location;
        assert!(
            matches!(location, crate::semantic::CursorLocation::Expression { .. }),
            "Expected Expression without trailing space after complete array initializer, got {:?}",
            location
        );
    }

    #[test]
    fn test_identifier_after_invalid_array_syntax_is_expression_without_space() {
        let src = "Object[] o = new Object[];\nArrayList";
        let offset = src.find("ArrayList").unwrap();
        let ctx = semantic_context_at_offset(src, offset);
        let location = ctx.location;
        assert!(
            matches!(location, crate::semantic::CursorLocation::Expression { .. }),
            "Expected Expression without trailing space after invalid array syntax, got {:?}",
            location
        );
    }

    #[test]
    fn test_identifier_with_semicolon_after_type_is_expression() {
        let src = "Object[] o = new Object[]{};\nArrayList ;";
        let offset = src.find("ArrayList").unwrap() + "ArrayList".len();
        let ctx = semantic_context_at_offset(src, offset);
        let location = ctx.location;
        assert!(
            matches!(location, crate::semantic::CursorLocation::Expression { .. }),
            "Expected Expression for `ArrayList ;`, got {:?}",
            location
        );
    }
}
