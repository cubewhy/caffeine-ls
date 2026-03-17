use crate::language::java::JavaContextExtractor;
use crate::language::java::location::heuristics::{
    detect_member_tail_in_misread_local_decl, detect_new_keyword_before_cursor,
    detect_trailing_dot_in_text, detect_variable_name_position_in_error, handle_import_from_text,
    is_import_context, is_misread_expression_in_local_decl, is_variable_name_after_complete_type,
    is_variable_name_after_previous_declaration,
};
use crate::language::java::utils::strip_sentinel;
use crate::language::java::{
    completion_context::normalize_top_level_generic_base,
    utils::{is_comment_kind, is_in_name_position, is_in_type_position},
};
use crate::semantic::CursorLocation;
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
    let type_node = node.child_by_field_name("type");

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
        },
        class_prefix,
    )
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
    if matches!(node.kind(), "identifier" | "type_identifier") {
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
    // Heuristic: an ERROR node that only contains a type with trailing whitespace
    // indicates we're waiting for a variable name.
    if let Some(err) = ancestor_of_kind(node, "ERROR")
        && let Some(type_name) = detect_variable_name_position_in_error(ctx, err)
    {
        return (CursorLocation::VariableName { type_name }, String::new());
    }
    // Dispatch each ancestor kind to its handler using a combinator chain.
    // Context carries (extractor, original_identifier_node).
    // `.climb(stop_kinds)` walks parent() until a handler returns Some or a
    // stop-kind is reached.
    type Ctx<'a> = (&'a JavaContextExtractor, Node<'a>);

    let id_handler = handler_fn(|inp: Input<Ctx<'_>>| handle_annotation(inp.ctx.0, inp.node))
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
            let before = &ctx.source[..ctx.offset.min(ctx.source.len())];
            if is_import_context(before) {
                return Some(handle_import_from_text(ctx, before));
            }
            if let Some((receiver_expr, member_prefix)) = detect_trailing_dot_in_text(before) {
                return Some((
                    CursorLocation::MemberAccess {
                        receiver_semantic_type: None,
                        receiver_type: None,
                        member_prefix: member_prefix.clone(),
                        receiver_expr,
                        arguments: None,
                    },
                    member_prefix,
                ));
            }
            if let Some((class_prefix, expected_type)) = detect_new_keyword_before_cursor(before) {
                return Some((
                    CursorLocation::ConstructorCall {
                        class_prefix: class_prefix.clone(),
                        expected_type,
                    },
                    class_prefix,
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
    use tree_sitter::Parser;

    use crate::language::java::{
        JavaContextExtractor, location::handlers::infer_annotation_target,
    };

    fn setup_with(source: &str, offset: usize) -> (JavaContextExtractor, tree_sitter::Tree) {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("failed to load java grammar");
        let tree = parser.parse(source, None).unwrap();
        let ctx = JavaContextExtractor::new(source, offset, None);
        (ctx, tree)
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
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

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
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (location, _) = super::handle_identifier(&ctx, cursor_node.unwrap(), None);
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
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (location, _) = super::handle_identifier(&ctx, cursor_node.unwrap(), None);
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
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (location, _) = super::handle_identifier(&ctx, cursor_node.unwrap(), None);
        assert!(
            matches!(location, crate::semantic::CursorLocation::Expression { .. }),
            "Expected Expression for `ArrayList ;`, got {:?}",
            location
        );
    }
}
