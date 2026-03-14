use crate::language::java::{
    completion_context::normalize_top_level_generic_base,
    utils::{
        find_ancestor, find_string_ancestor, is_comment_kind, is_in_name_position,
        is_in_type_position,
    },
};
use crate::{
    language::java::{JavaContextExtractor, utils::strip_sentinel},
    semantic::CursorLocation,
    semantic::context::{
        ExpectedTypeSource, FunctionalExprShape, FunctionalMethodCallHint, FunctionalTargetHint,
        MethodRefQualifierKind, StatementLabelCompletionKind,
    },
};
use std::sync::Arc;
use tree_sitter::Node;

// ── public entry points ─────────────────────────────────────────────────────

pub(crate) fn determine_location(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    trigger_char: Option<char>,
) -> (CursorLocation, String) {
    let (loc, query) = determine_location_impl(ctx, cursor_node, trigger_char);
    if location_has_newline(&loc) {
        return (CursorLocation::Unknown, String::new());
    }
    (loc, query)
}

pub(crate) fn infer_functional_target_hint(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
) -> Option<FunctionalTargetHint> {
    let node = cursor_node?;
    let (expected_type_source, mut expected_type_context) = infer_expected_type_source(ctx, node);
    let assignment_lhs_expr = infer_assignment_rhs_lhs_expr(ctx, node);
    if expected_type_context.is_none() && assignment_lhs_expr.is_some() {
        expected_type_context = Some(ExpectedTypeSource::AssignmentRhs);
    }
    let method_call = infer_method_argument_target_hint(ctx, node);
    let expr_shape = infer_functional_expr_shape(ctx, node);
    if expected_type_source.is_none() && assignment_lhs_expr.is_none() && method_call.is_none() {
        return None;
    }
    Some(FunctionalTargetHint {
        expected_type_source,
        expected_type_context,
        assignment_lhs_expr,
        method_call,
        expr_shape,
    })
}

fn determine_location_impl(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    trigger_char: Option<char>,
) -> (CursorLocation, String) {
    let node = match cursor_node {
        Some(n) => n,
        None => return (CursorLocation::Unknown, String::new()),
    };

    // Comments are handled by the caller; if we somehow land here, bail.
    if is_comment_kind(node.kind()) {
        return (CursorLocation::Unknown, String::new());
    }

    // String literals
    if let Some(str_node) = find_string_ancestor(node) {
        let prefix = extract_string_prefix(ctx, str_node);
        return (
            CursorLocation::StringLiteral {
                prefix: prefix.clone(),
            },
            prefix,
        );
    }

    if let Some(result) = detect_statement_label_location(ctx, node) {
        return result;
    }

    // Walk upward through the AST to find a significant ancestor.
    let mut current = node;
    loop {
        match current.kind() {
            // ── statement labels (break / continue) ──────────────────────
            "break_statement" | "continue_statement" => {
                if let Some(result) = handle_jump_statement(ctx, current) {
                    return result;
                }
                // cursor is on the keyword itself – not a label completion site
                break;
            }

            // ── annotations ──────────────────────────────────────────────
            "marker_annotation" | "annotation" => {
                return handle_annotation(ctx, current);
            }

            // ── imports ──────────────────────────────────────────────────
            "import_declaration" => return handle_import(ctx, current),

            // ── member access / method invocation ────────────────────────
            "method_invocation" | "field_access" => {
                return handle_member_access(ctx, current);
            }

            // ── method references ─────────────────────────────────────────
            "method_reference" => return handle_method_reference(ctx, current),

            // ── constructor calls ─────────────────────────────────────────
            "object_creation_expression" => {
                // Check if cursor is inside the type-argument list of the constructor.
                if let Some(type_args) =
                    find_innermost_constructor_type_arguments(current, ctx.offset)
                {
                    let prefix = find_prefix_in_type_arguments_hole(ctx, type_args);
                    return (
                        CursorLocation::TypeAnnotation {
                            prefix: prefix.clone(),
                        },
                        prefix,
                    );
                }
                return handle_constructor(ctx, current);
            }

            // ── argument lists ────────────────────────────────────────────
            "argument_list" => return handle_argument_list(ctx, current),

            // ── identifiers – delegate to the richer handler ─────────────
            "identifier" | "type_identifier" => {
                return handle_identifier(ctx, current, trigger_char);
            }

            "local_variable_declaration" => {
                if let Some(ctor) = find_object_creation_at_cursor(current, ctx.offset) {
                    if let Some(type_args) =
                        find_innermost_constructor_type_arguments(ctor, ctx.offset)
                    {
                        let prefix = find_prefix_in_type_arguments_hole(ctx, type_args);
                        return (
                            CursorLocation::TypeAnnotation {
                                prefix: prefix.clone(),
                            },
                            prefix,
                        );
                    }
                    return handle_constructor(ctx, ctor);
                }
            }

            "ERROR" => {
                if let Some(p) = current.parent() {
                    if p.kind() == "argument_list" {
                        // Check if this ERROR node is a bare dot (trailing dot after an expression).
                        let is_trailing_dot = {
                            let mut wc = current.walk();
                            let children: Vec<_> = current.children(&mut wc).collect();
                            children.len() == 1 && children[0].kind() == "."
                        };
                        if is_trailing_dot {
                            // Find the preceding sibling in argument_list.
                            let receiver = find_preceding_named_sibling(current, p);
                            if let Some(recv) = receiver {
                                let receiver_expr = ctx.node_text(recv).to_string();
                                return (
                                    CursorLocation::MemberAccess {
                                        receiver_semantic_type: None,
                                        receiver_type: None,
                                        member_prefix: String::new(),
                                        receiver_expr,
                                        arguments: None,
                                    },
                                    String::new(),
                                );
                            }
                        }
                        return handle_argument_list(ctx, p);
                    }
                }
                return (CursorLocation::Unknown, String::new());
            }

            // ── stop climbing at scope boundaries ─────────────────────────
            "block" | "class_body" | "program" => break,

            _ => {}
        }

        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    // Last resort: treat the cursor node as a plain expression/identifier.
    if !matches!(node.kind(), "identifier" | "type_identifier") {
        return (CursorLocation::Unknown, String::new());
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

/// Find the named sibling immediately before `node` within `parent`.
fn find_preceding_named_sibling<'a>(node: Node<'a>, parent: Node<'a>) -> Option<Node<'a>> {
    let mut wc = parent.walk();
    let mut prev: Option<Node<'a>> = None;
    for child in parent.named_children(&mut wc) {
        if child.id() == node.id() {
            return prev;
        }
        prev = Some(child);
    }
    None
}

fn detect_statement_label_location(
    ctx: &JavaContextExtractor,
    node: Node,
) -> Option<(CursorLocation, String)> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "break_statement" | "continue_statement" => {
                return handle_jump_statement(ctx, cur);
            }
            "block" | "class_body" | "program" => {
                // Cursor may be in whitespace after an incomplete break/continue.
                let preceding = {
                    let mut wc = cur.walk();
                    let mut last: Option<Node> = None;
                    for child in cur.named_children(&mut wc) {
                        if child.start_byte() < ctx.offset {
                            last = Some(child);
                        } else {
                            break;
                        }
                    }
                    last
                };

                if let Some(stmt) = preceding {
                    if matches!(stmt.kind(), "break_statement" | "continue_statement") {
                        let has_semicolon = {
                            let mut wc2 = stmt.walk();
                            stmt.children(&mut wc2).any(|c| c.kind() == ";")
                        };
                        if !has_semicolon {
                            return handle_jump_statement(ctx, stmt);
                        }
                    }

                    // handle ERROR node containing break/continue keyword
                    if stmt.kind() == "ERROR" {
                        let (jump_kind, keyword_end) = {
                            let mut wc2 = stmt.walk();
                            let mut found = None;
                            for child in stmt.children(&mut wc2) {
                                if child.kind() == "break" {
                                    found = Some((
                                        StatementLabelCompletionKind::Break,
                                        child.end_byte(),
                                    ));
                                    break;
                                }
                                if child.kind() == "continue" {
                                    found = Some((
                                        StatementLabelCompletionKind::Continue,
                                        child.end_byte(),
                                    ));
                                    break;
                                }
                            }
                            match found {
                                Some(f) => f,
                                None => return None, // not a jump statement error
                            }
                        };
                        if ctx.offset > keyword_end {
                            return Some((
                                CursorLocation::StatementLabel {
                                    kind: jump_kind,
                                    prefix: String::new(),
                                },
                                String::new(),
                            ));
                        }
                    }
                }
                break;
            }
            _ => {}
        }
        cur = cur.parent()?;
    }
    None
}

fn find_object_creation_at_cursor(node: Node, offset: usize) -> Option<Node> {
    if node.kind() == "object_creation_expression"
        && node.start_byte() <= offset
        && node.end_byte() >= offset
    {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.start_byte() <= offset
            && child.end_byte() >= offset
            && let Some(found) = find_object_creation_at_cursor(child, offset)
        {
            return Some(found);
        }
    }
    None
}

// ── jump-statement (break / continue) label ──────────────────────────────────

/// Returns Some only when the cursor is inside the *label slot* of the statement.
fn handle_jump_statement(
    ctx: &JavaContextExtractor,
    stmt: Node,
) -> Option<(CursorLocation, String)> {
    let kind = match stmt.kind() {
        "break_statement" => StatementLabelCompletionKind::Break,
        "continue_statement" => StatementLabelCompletionKind::Continue,
        _ => return None,
    };

    // The label identifier is the first named child (if any).
    let label_node = stmt
        .named_children(&mut stmt.walk())
        .find(|c| c.kind() == "identifier");

    if let Some(ident) = label_node {
        // Cursor must be inside (or at the end of) the identifier span.
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
        // Cursor is past the identifier → statement is complete, not a label site.
        return None;
    }

    // No identifier yet: cursor must be in the whitespace between the keyword and `;` / `}`.
    // The keyword ends at the first child node's end byte.
    let keyword_end = stmt
        .child(0)
        .map(|c| c.end_byte())
        .unwrap_or(stmt.start_byte());

    if ctx.offset > keyword_end {
        // Cursor is right after the keyword with no label token → empty prefix.
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

// ── annotation ───────────────────────────────────────────────────────────────

fn handle_annotation(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
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
    let mut cur = node.parent();
    while let Some(n) = cur {
        let et = match n.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "annotation_type_declaration"
            | "record_declaration" => "TYPE",
            "formal_parameter" | "spread_parameter"
                if n.parent().map(|p| p.kind()) == Some("record_declaration") =>
            {
                "RECORD_COMPONENT"
            }
            "method_declaration" => "METHOD",
            "field_declaration" => "FIELD",
            "formal_parameter" | "spread_parameter" => "PARAMETER",
            "constructor_declaration" => "CONSTRUCTOR",
            "local_variable_declaration" => "LOCAL_VARIABLE",
            "ERROR" | "class_body" | "block" | "program" => return None,
            _ => {
                cur = n.parent();
                continue;
            }
        };
        return Some(Arc::from(et));
    }
    None
}

// ── import ────────────────────────────────────────────────────────────────────

fn handle_import(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
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

/// Recursively collect valid text nodes inside an import declaration, stopping at the cursor.
fn collect_import_text(ctx: &JavaContextExtractor, node: Node, out: &mut String) {
    if node.start_byte() >= ctx.offset {
        return;
    }
    if node.child_count() == 0 {
        let kind = node.kind();
        if kind == "import" || kind == "static" || kind == ";" || is_comment_kind(kind) {
            return;
        }
        out.push_str(&cursor_truncated_text(ctx, node));
    } else {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_import_text(ctx, child, out);
        }
    }
}

// ── member access / field access / method invocation ─────────────────────────

fn handle_member_access(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
    let mut walker = node.walk();
    let children: Vec<Node> = node.children(&mut walker).collect();

    let dot_pos = match children.iter().position(|n| n.kind() == ".") {
        Some(p) => p,
        None => {
            // No dot – treat as a plain expression or implicit-this member call.
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

    // Cursor is before (or on) the dot → completing the receiver expression.
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

    // Cursor is after the dot → completing a member name.
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

// ── method reference ──────────────────────────────────────────────────────────

fn handle_method_reference(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
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

// ── constructor ───────────────────────────────────────────────────────────────

fn handle_constructor(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
    let type_node = node.child_by_field_name("type");

    // If the type node starts after the cursor and is separated by a newline, reject.
    if let Some(ty) = type_node
        && ctx.offset < ty.start_byte()
    {
        let gap = &ctx.source[ctx.offset..ty.start_byte()];
        if gap.contains('\n') {
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

// ── argument list ─────────────────────────────────────────────────────────────

fn handle_argument_list(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
    // Check if the cursor is after a dot inside one of the argument expressions.
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

    // Cursor right after `(` or `,` → fresh argument slot.
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

fn detect_member_access_in_arg_list(
    ctx: &JavaContextExtractor,
    arg_list: Node,
) -> Option<(String, String)> {
    let mut walker = arg_list.walk();
    for child in arg_list.named_children(&mut walker) {
        if child.start_byte() >= ctx.offset {
            continue;
        }
        // Take text from child start up to the cursor.
        let child_end = child.end_byte().min(ctx.offset);
        let child_text = &ctx.source[child.start_byte()..child_end];
        let clean = strip_sentinel(child_text);
        let trimmed = clean.trim_end();
        if let Some(dot_pos) = trimmed.rfind('.') {
            let after_dot = trimmed[dot_pos + 1..].trim();
            // after_dot must be a valid identifier prefix (or empty).
            if after_dot
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                let receiver_expr = trimmed[..dot_pos].trim().to_string();
                if !receiver_expr.is_empty() {
                    let member_prefix = after_dot.to_string();
                    return Some((receiver_expr, member_prefix));
                }
            }
        }
    }
    None
}

fn find_prefix_in_argument_list(ctx: &JavaContextExtractor, arg_list: Node) -> String {
    let mut cursor = arg_list.walk();
    for child in arg_list.named_children(&mut cursor) {
        if child.start_byte() <= ctx.offset && child.end_byte() >= ctx.offset.saturating_sub(1) {
            // Try to find a more precise prefix from inside the child subtree.
            let nested = find_identifier_at_cursor(ctx, child);
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

/// Find the identifier text at or nearest the cursor within a subtree, using the AST.
fn find_identifier_at_cursor(ctx: &JavaContextExtractor, node: Node) -> String {
    if node.start_byte() > ctx.offset || node.end_byte() < ctx.offset.saturating_sub(1) {
        return String::new();
    }
    if matches!(node.kind(), "identifier" | "type_identifier") {
        return strip_sentinel(&cursor_truncated_text(ctx, node));
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        if child.start_byte() <= ctx.offset && child.end_byte() >= ctx.offset.saturating_sub(1) {
            let nested = find_identifier_at_cursor(ctx, child);
            if !nested.is_empty() {
                return nested;
            }
        }
    }
    String::new()
}

// ── identifier handler ────────────────────────────────────────────────────────

fn handle_identifier(
    ctx: &JavaContextExtractor,
    node: Node,
    _trigger_char: Option<char>,
) -> (CursorLocation, String) {
    let mut ancestor = node;
    loop {
        ancestor = match ancestor.parent() {
            Some(p) => p,
            None => break,
        };

        match ancestor.kind() {
            "marker_annotation" | "annotation" => {
                return handle_annotation(ctx, ancestor);
            }
            "field_access" | "method_invocation" => {
                return handle_member_access(ctx, ancestor);
            }
            "method_reference" => {
                return handle_method_reference(ctx, ancestor);
            }
            "import_declaration" => return handle_import(ctx, ancestor),

            "object_creation_expression" => {
                if is_in_constructor_type_arguments(node, ancestor) {
                    let text = cursor_truncated_text(ctx, node);
                    return (
                        CursorLocation::TypeAnnotation {
                            prefix: text.clone(),
                        },
                        text,
                    );
                }
                return handle_constructor(ctx, ancestor);
            }

            "local_variable_declaration" => {
                return handle_identifier_in_local_decl(ctx, node, ancestor);
            }

            "formal_parameter" | "spread_parameter" => {
                if is_in_formal_param_name_position(node, ancestor) {
                    let type_name = ancestor
                        .child_by_field_name("type")
                        .map(|n| ctx.node_text(n).trim().to_string())
                        .unwrap_or_default();
                    return (CursorLocation::VariableName { type_name }, String::new());
                }
                let text = cursor_truncated_text(ctx, node);
                return (
                    CursorLocation::TypeAnnotation {
                        prefix: text.clone(),
                    },
                    text,
                );
            }

            "argument_list" => {
                // If cursor is actually before the argument list (e.g., on the method name),
                // hand off to the method_invocation handler instead.
                if let Some(parent) = ancestor.parent()
                    && parent.kind() == "method_invocation"
                    && ctx.offset <= ancestor.start_byte()
                {
                    return handle_member_access(ctx, parent);
                }
                return handle_argument_list(ctx, ancestor);
            }

            "break_statement" | "continue_statement" => {
                if let Some(result) = handle_jump_statement(ctx, ancestor) {
                    return result;
                }
                break;
            }

            "ERROR" => {
                if let Some(p) = ancestor.parent() {
                    if p.kind() == "argument_list" {
                        return handle_argument_list(ctx, p);
                    }
                }
                // Check if this ERROR looks like an incomplete generic type expression.
                // e.g., `Function<String, S` → ERROR containing `<` and `,`
                if error_looks_like_generic_type(ancestor) {
                    let text = cursor_truncated_text(ctx, node);
                    let clean = strip_sentinel(&text);
                    return (
                        CursorLocation::TypeAnnotation {
                            prefix: clean.clone(),
                        },
                        clean,
                    );
                }
                // ERROR: surface Unknown; injection will handle recovery.
                return (CursorLocation::Unknown, String::new());
            }

            "block" | "class_body" | "program" => break,
            _ => {}
        }
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

/// Returns true if an ERROR node looks like an incomplete generic type expression,
/// i.e., it contains a `<` child (opening of type arguments).
fn error_looks_like_generic_type(error_node: Node) -> bool {
    let mut wc = error_node.walk();
    error_node.children(&mut wc).any(|c| c.kind() == "<")
}

fn handle_identifier_in_local_decl(
    ctx: &JavaContextExtractor,
    node: Node,
    decl: Node,
) -> (CursorLocation, String) {
    // Detect misread: `str\nfunc(...)` parsed as local_variable_declaration
    // where the next sibling begins with `(`.
    if let Some(next) = decl.next_sibling() {
        let next_text = &ctx.source[next.start_byte()..next.end_byte()];
        if next_text.trim_start().starts_with('(') {
            let text = cursor_truncated_text(ctx, node);
            let clean = strip_sentinel(&text);
            return (
                CursorLocation::Expression {
                    prefix: clean.clone(),
                },
                clean,
            );
        }
    }

    // Type position.
    if is_in_type_position(node, decl) {
        let text = cursor_truncated_text(ctx, node);
        return (
            CursorLocation::TypeAnnotation {
                prefix: text.clone(),
            },
            text,
        );
    }

    // Variable name position.
    if is_in_name_position(node, decl) {
        let type_name = extract_type_from_decl(ctx, decl);
        return (CursorLocation::VariableName { type_name }, String::new());
    }

    // Inside the type subtree (e.g., generic arguments of the declared type).
    if is_in_type_subtree(node, decl) {
        let text = cursor_truncated_text(ctx, node);
        return (
            CursorLocation::TypeAnnotation {
                prefix: text.clone(),
            },
            text,
        );
    }

    // Initializer expression or anything else.
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
    let Some(type_args) = find_ancestor(id_node, "type_arguments") else {
        return false;
    };
    is_descendant_of(type_args, ty) && is_descendant_of(id_node, type_args)
}

fn find_innermost_constructor_type_arguments(ctor_node: Node, offset: usize) -> Option<Node> {
    let ty = ctor_node.child_by_field_name("type")?;
    let mut best: Option<Node> = None;
    fn visit<'a>(node: Node<'a>, offset: usize, best: &mut Option<Node<'a>>) {
        if node.kind() == "type_arguments"
            && node.start_byte() < offset
            && offset <= node.end_byte().saturating_sub(1)
        {
            if let Some(prev) = best {
                let prev_len = prev.end_byte().saturating_sub(prev.start_byte());
                let cur_len = node.end_byte().saturating_sub(node.start_byte());
                if cur_len < prev_len {
                    *best = Some(node);
                }
            } else {
                *best = Some(node);
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            visit(child, offset, best);
        }
    }
    visit(ty, offset, &mut best);
    best
}

fn find_prefix_in_type_arguments_hole(ctx: &JavaContextExtractor, type_args: Node) -> String {
    let start = type_args.start_byte().saturating_add(1);
    let end = type_args.end_byte().saturating_sub(1).min(ctx.offset);
    if end <= start {
        return String::new();
    }
    let s = &ctx.source[start..end];
    let mut depth = 0i32;
    let mut last_sep = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => last_sep = i + 1,
            _ => {}
        }
    }
    s[last_sep..].trim().to_string()
}

// ── formal parameter helpers ──────────────────────────────────────────────────

fn is_in_formal_param_name_position(id_node: Node, param_node: Node) -> bool {
    param_node
        .child_by_field_name("name")
        .is_some_and(|n| n.id() == id_node.id())
}

// ── local-declaration helpers ─────────────────────────────────────────────────

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

// ── string literal ────────────────────────────────────────────────────────────

fn extract_string_prefix(ctx: &JavaContextExtractor, str_node: Node) -> String {
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

// ── expected-type / functional-hint helpers ───────────────────────────────────

fn infer_expected_type_from_lhs(ctx: &JavaContextExtractor, node: Node) -> Option<String> {
    let decl = find_ancestor(node, "local_variable_declaration")?;
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

fn infer_assignment_rhs_expected_type(ctx: &JavaContextExtractor, node: Node) -> Option<String> {
    let declarator = find_ancestor(node, "variable_declarator")?;
    let value_node = declarator.child_by_field_name("value")?;
    if !is_descendant_of(node, value_node) {
        return None;
    }
    let decl = declarator.parent()?;
    if decl.kind() != "local_variable_declaration" && decl.kind() != "field_declaration" {
        return None;
    }
    Some(extract_type_from_decl(ctx, decl))
}

fn infer_return_expected_type(ctx: &JavaContextExtractor, node: Node) -> Option<String> {
    let return_stmt = find_ancestor(node, "return_statement")?;
    return_stmt.child_by_field_name("value")?;
    let method_decl = find_ancestor(return_stmt, "method_declaration")?;
    let mut walker = method_decl.walk();
    for child in method_decl.named_children(&mut walker) {
        if matches!(child.kind(), "modifiers" | "type_parameters") {
            continue;
        }
        if child.kind() == "identifier" || child.kind() == "formal_parameters" {
            break;
        }
        let ty = ctx.node_text(child).trim();
        if !ty.is_empty() {
            return Some(ty.to_string());
        }
    }
    None
}

fn infer_assignment_rhs_lhs_expr(ctx: &JavaContextExtractor, node: Node) -> Option<String> {
    let assign = find_ancestor(node, "assignment_expression")?;
    let right = assign.child_by_field_name("right")?;
    if !is_descendant_of(node, right) {
        return None;
    }
    let left = assign.child_by_field_name("left")?;
    let lhs = ctx.node_text(left).trim();
    if lhs.is_empty() {
        return None;
    }
    Some(lhs.to_string())
}

fn infer_expected_type_source(
    ctx: &JavaContextExtractor,
    node: Node,
) -> (Option<String>, Option<ExpectedTypeSource>) {
    if let Some(ty) = infer_assignment_rhs_expected_type(ctx, node) {
        return (Some(ty), Some(ExpectedTypeSource::VariableInitializer));
    }
    if let Some(ty) = infer_return_expected_type(ctx, node) {
        return (Some(ty), Some(ExpectedTypeSource::ReturnExpr));
    }
    (None, None)
}

fn infer_method_argument_target_hint(
    ctx: &JavaContextExtractor,
    node: Node,
) -> Option<FunctionalMethodCallHint> {
    let arg_list = find_ancestor(node, "argument_list")?;
    let invocation = arg_list.parent()?;
    if invocation.kind() != "method_invocation" {
        return None;
    }
    let receiver_expr = invocation
        .child_by_field_name("object")
        .map(|n| ctx.node_text(n).to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "this".to_string());
    let method_name = invocation
        .child_by_field_name("name")
        .map(|n| ctx.node_text(n).to_string())
        .unwrap_or_default();
    if method_name.is_empty() {
        return None;
    }
    let arg_index = argument_index_at_cursor(ctx, arg_list);
    let arg_texts = split_argument_texts(ctx, arg_list);
    Some(FunctionalMethodCallHint {
        receiver_expr,
        method_name,
        arg_index,
        arg_texts,
    })
}

// ── functional expression shape ───────────────────────────────────────────────

fn infer_functional_expr_shape(
    ctx: &JavaContextExtractor,
    node: Node,
) -> Option<FunctionalExprShape> {
    if let Some(method_ref) = find_ancestor(node, "method_reference") {
        return infer_method_reference_shape(ctx, method_ref);
    }
    if let Some(lambda) = find_ancestor(node, "lambda_expression") {
        return infer_lambda_shape(ctx, lambda);
    }
    None
}

fn infer_method_reference_shape(
    ctx: &JavaContextExtractor,
    method_ref: Node,
) -> Option<FunctionalExprShape> {
    let raw = strip_sentinel(ctx.node_text(method_ref).trim());
    let sep_idx = raw.find("::")?;
    let qualifier_expr = raw[..sep_idx].trim().to_string();
    if qualifier_expr.is_empty() {
        return None;
    }
    let rhs_raw = raw[sep_idx + 2..].trim_start();
    let rhs_no_type_args = strip_leading_type_arguments(rhs_raw);
    let member_name = strip_sentinel(rhs_no_type_args.trim()).to_string();
    if member_name.is_empty() {
        return None;
    }
    let is_constructor = member_name == "new" || member_name.starts_with("new");
    let qualifier_kind = infer_method_ref_qualifier_kind(&qualifier_expr);
    Some(FunctionalExprShape::MethodReference {
        qualifier_expr,
        member_name,
        is_constructor,
        qualifier_kind,
    })
}

fn infer_method_ref_qualifier_kind(qualifier_expr: &str) -> MethodRefQualifierKind {
    let trimmed = qualifier_expr.trim();
    if trimmed == "this" || trimmed == "super" {
        return MethodRefQualifierKind::Expr;
    }
    if trimmed
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        return MethodRefQualifierKind::Type;
    }
    MethodRefQualifierKind::Unknown
}

fn infer_lambda_shape(ctx: &JavaContextExtractor, lambda: Node) -> Option<FunctionalExprShape> {
    let raw = strip_sentinel(ctx.node_text(lambda).trim());
    let arrow = raw.find("->")?;
    let params_raw = raw[..arrow].trim();
    let body_raw = raw[arrow + 2..].trim();
    let param_count = parse_lambda_param_count(params_raw)?;
    let expression_body = if body_raw.starts_with('{') || body_raw.is_empty() {
        None
    } else {
        Some(body_raw.to_string())
    };
    Some(FunctionalExprShape::Lambda {
        param_count,
        expression_body,
    })
}

fn parse_lambda_param_count(params_raw: &str) -> Option<usize> {
    let p = params_raw.trim();
    if p.is_empty() {
        return None;
    }
    if let Some(inner) = p.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        let inner = inner.trim();
        if inner.is_empty() {
            return Some(0);
        }
        return Some(split_top_level_args(inner).len());
    }
    if p.contains(',') {
        return None;
    }
    Some(1)
}

// ── argument index / split ────────────────────────────────────────────────────

fn argument_index_at_cursor(ctx: &JavaContextExtractor, arg_list: Node) -> usize {
    let start = arg_list.start_byte().saturating_add(1);
    let end = arg_list.end_byte().saturating_sub(1).min(ctx.offset);
    if end <= start {
        return 0;
    }
    count_top_level_commas(&ctx.source[start..end])
}

fn split_argument_texts(ctx: &JavaContextExtractor, arg_list: Node) -> Vec<String> {
    let start = arg_list.start_byte().saturating_add(1);
    let end = arg_list.end_byte().saturating_sub(1);
    if end <= start {
        return vec![];
    }
    split_top_level_args(&ctx.source[start..end])
}

fn count_top_level_commas(s: &str) -> usize {
    let mut depth = 0i32;
    let mut prev = None;
    let mut commas = 0usize;
    for c in s.chars() {
        match c {
            '(' | '<' | '[' | '{' => depth += 1,
            '>' if prev == Some('-') => {}
            ')' | '>' | ']' | '}' => depth = (depth - 1).max(0),
            ',' if depth == 0 => commas += 1,
            _ => {}
        }
        prev = Some(c);
    }
    commas
}

fn split_top_level_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut prev = None;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '<' | '[' | '{' => depth += 1,
            '>' if prev == Some('-') => {}
            ')' | '>' | ']' | '}' => depth = (depth - 1).max(0),
            ',' if depth == 0 => {
                let part = s[start..i].trim();
                if !part.is_empty() {
                    out.push(part.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
        prev = Some(c);
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

// ── shared utilities ──────────────────────────────────────────────────────────

fn cursor_truncated_text(ctx: &JavaContextExtractor, node: Node) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(ctx.offset);
    if end <= start {
        return String::new();
    }
    ctx.byte_slice(start, end).to_string()
}

fn is_descendant_of(node: Node, ancestor: Node) -> bool {
    let mut cur = node;
    loop {
        if cur.id() == ancestor.id() {
            return true;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

fn location_has_newline(loc: &CursorLocation) -> bool {
    match loc {
        CursorLocation::ConstructorCall { class_prefix, .. } => class_prefix.contains('\n'),
        CursorLocation::MemberAccess { member_prefix, .. } => member_prefix.contains('\n'),
        CursorLocation::Expression { prefix } => prefix.contains('\n'),
        CursorLocation::MethodArgument { prefix } => prefix.contains('\n'),
        CursorLocation::TypeAnnotation { prefix } => prefix.contains('\n'),
        CursorLocation::MethodReference {
            qualifier_expr,
            member_prefix,
            ..
        } => qualifier_expr.contains('\n') || member_prefix.contains('\n'),
        CursorLocation::Annotation { prefix, .. } => prefix.contains('\n'),
        CursorLocation::Import { prefix } => prefix.contains('\n'),
        CursorLocation::StringLiteral { prefix } => prefix.contains('\n'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use tree_sitter::Parser;

    use crate::{
        language::java::{
            JavaContextExtractor,
            location::{
                count_top_level_commas, determine_location, infer_annotation_target,
                split_top_level_args,
            },
        },
        semantic::{
            CursorLocation,
            context::{FunctionalTargetHint, StatementLabelCompletionKind},
        },
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
    fn test_misread_type_as_expression() {
        // Missing semicolon can make tree-sitter misread `str\nfunc(...)` as a declaration.
        // Cursor at `str` must still be treated as Expression, not TypeAnnotation.
        let src = indoc::indoc! {r#"
    class A {
        public static String str = "1234";
        public static void func() {
            str
            func(func("1234", 5678));
        }
    }

    "#};
        // Cursor sits at the end of method-body `str`.
        let marker = "func() {\n        str";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::Expression { .. }),
            "Expected Expression (str is an expression, not a type annotation), got {:?}",
            loc
        );
        assert_eq!(query, "str");
    }

    #[test]
    fn test_break_routes_to_statement_label_location() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        outer: {
            break 
        }
    }
}
"#};
        let marker = "break ";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(
                loc,
                CursorLocation::StatementLabel {
                    kind: StatementLabelCompletionKind::Break,
                    ref prefix
                } if prefix.is_empty()
            ),
            "Expected StatementLabel(Break), got {:?}",
            loc
        );
        assert!(
            query.is_empty(),
            "expected empty break label query, got {query:?}"
        );
    }

    #[test]
    fn test_continue_partial_prefix_routes_to_statement_label_location() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        outer:
        while (true) {
            continue out
        }
    }
}
"#};
        let marker = "continue out";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(
                loc,
                CursorLocation::StatementLabel {
                    kind: StatementLabelCompletionKind::Continue,
                    ref prefix
                } if prefix == "out"
            ),
            "Expected StatementLabel(Continue, \"out\"), got {:?}",
            loc
        );
        assert_eq!(query, "out");
    }

    #[test]
    fn test_break_after_semicolon_is_not_statement_label_location() {
        let src = indoc::indoc! {r#"
class T {
    void m() {
        outer: {
            break outer;
        }
    }
}
"#};
        let marker = "break outer;";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);
        assert!(
            !matches!(loc, CursorLocation::StatementLabel { .. }),
            "completed break statement must not stay in StatementLabel, got {:?}",
            loc
        );
    }

    #[test]
    fn test_continue_after_semicolon_is_not_statement_label_location() {
        let src = indoc::indoc! {r#"
class T {
    void m() {
        loop: while (true) {
            continue loop;
        }
    }
}
"#};
        let marker = "continue loop;";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);
        assert!(
            !matches!(loc, CursorLocation::StatementLabel { .. }),
            "completed continue statement must not stay in StatementLabel, got {:?}",
            loc
        );
    }

    #[test]
    fn test_split_top_level_args_keeps_lambda_arrow_from_hiding_commas() {
        let args = split_top_level_args(r##"s -> s.subs, "hello""##);
        assert_eq!(args, vec![r#"s -> s.subs"#, r#""hello""#]);
        assert_eq!(count_top_level_commas(r##"s -> s.subs, "hello""##), 1);
    }

    #[test]
    fn test_genuine_type_annotation_in_local_decl() {
        // Genuine local declaration: type-position completion must stay TypeAnnotation.
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            HashM map = null;
        }
    }
    "#};
        let offset = src.find("HashM").unwrap() + 5; // Cursor right after `HashM`.
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation for genuine type position, got {:?}",
            loc
        );
        assert_eq!(query, "HashM");
    }

    #[test]
    fn test_generic_type_argument_position_is_type_annotation() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            List<Bo> nums = null;
        }
    }
    "#};
        let offset = src.find("Bo").unwrap() + 2;
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation in generic type argument, got {:?}",
            loc
        );
        assert_eq!(query, "Bo");
    }

    #[test]
    fn test_nested_generic_type_argument_position_is_type_annotation() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            Map<String, In> m = null;
        }
    }
    "#};
        let offset = src.find("In").unwrap() + 2;
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation in nested generic arg, got {:?}",
            loc
        );
        assert_eq!(query, "In");
    }

    #[test]
    fn test_wildcard_bound_type_argument_position_is_type_annotation() {
        let src = indoc::indoc! {r#"
    class A<T> {
        void f() {
            Function<? super T, ? extends Nu> fn = null;
        }
    }
    "#};
        let offset = src.find("Nu").unwrap() + 2;
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation in wildcard bound arg, got {:?}",
            loc
        );
        assert_eq!(query, "Nu");
    }

    #[test]
    fn test_misread_not_triggered_when_next_sibling_is_normal_statement() {
        // Misread guard should not trigger for a normal following statement.
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            ArrayL list = new ArrayList<>();
            int x = 1;
        }
    }
    "#};
        let offset = src.find("ArrayL").unwrap() + 6;
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation, got {:?}",
            loc
        );
    }

    #[test]
    fn test_cursor_inside_receiver_identifier() {
        // Cursor inside receiver identifier should remain Expression, not MemberAccess.
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            List<String> strings = new ArrayList<>();
            strings.addAll();
        }
    }
    "#};
        let marker = "strings.addAll";
        let offset = src.find(marker).unwrap() + 4; // `stri|ngs`
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::Expression { .. }),
            "Expected Expression because cursor is inside the receiver, got {:?}",
            loc
        );
        assert_eq!(query, "stri");
    }

    #[test]
    fn test_cursor_exactly_before_dot_in_member_access() {
        // Cursor right before dot is still receiver completion (Expression).
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            List<String> strings = new ArrayList<>();
            strings.addAll();
        }
    }
    "#};
        let marker = "strings.addAll";
        let offset = src.find(marker).unwrap() + 7; // `strings|.addAll`
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::Expression { .. }),
            "Expected Expression when cursor is exactly before the dot, got {:?}",
            loc
        );
        assert_eq!(query, "strings");
    }

    #[test]
    fn test_member_access_after_array_access_in_arg_list() {
        let src = indoc::indoc! {r#"
class A {
    void f(String[][] matrix) {
        System.out.println(matrix[0][1].);
    }
}
"#};
        // Cursor is after `.` and before `)`.
        let marker = "matrix[0][1].";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(
                loc,
                CursorLocation::MemberAccess { ref receiver_expr, ref member_prefix, .. }
                if receiver_expr == "matrix[0][1]" && member_prefix.is_empty()
            ),
            "Expected MemberAccess with receiver_expr=matrix[0][1], got {:?}",
            loc
        );
        assert_eq!(query, "");
    }

    #[test]
    fn test_misread_local_decl_dotted_tail_routes_to_member_access() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        var a = new HashMap<String, String>();
        a.put;
    }
}
"#};
        let marker = "a.put";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(
                loc,
                CursorLocation::MemberAccess { ref receiver_expr, ref member_prefix, .. }
                if receiver_expr == "a" && member_prefix == "put"
            ),
            "Expected MemberAccess for dotted tail misread, got {:?}",
            loc
        );
        assert_eq!(query, "put");
    }

    #[test]
    fn test_member_access_with_partial_member_in_arg_list() {
        // Partial member already typed; ensure existing field_access path still wins.
        let src = indoc::indoc! {r#"
class A {
    void f(String[][] matrix) {
        System.out.println(matrix[0][1].toS);
    }
}
"#};
        let marker = "matrix[0][1].toS";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::MemberAccess { .. }),
            "Expected MemberAccess, got {:?}",
            loc
        );
        assert_eq!(query, "toS");
    }

    #[test]
    fn test_normal_method_argument_not_affected() {
        // New logic must not regress ordinary method-argument completion.
        let src = indoc::indoc! {r#"
class A {
    void f(String[][] matrix) {
        System.out.println(matr);
    }
}
"#};
        let marker = "println(matr";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::MethodArgument { .. }),
            "Expected MethodArgument, got {:?}",
            loc
        );
        assert_eq!(query, "matr");
    }

    #[test]
    fn test_method_argument_concat_uses_identifier_local_prefix() {
        let src = indoc::indoc! {r#"
class A {
    void f(int intValue) {
        System.out.println("intValue = " + intVa);
    }
}
"#};
        let marker = "intVa";
        let offset = src.rfind(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(&loc, CursorLocation::MethodArgument { prefix } if prefix == "intVa"),
            "Expected MethodArgument{{prefix:\"intVa\"}}, got {:?}",
            loc
        );
        assert_eq!(query, "intVa");
    }

    #[test]
    fn test_method_argument_concat_empty_rhs_is_empty_prefix() {
        let src = indoc::indoc! {r#"
class A {
    void f(int testValue) {
        System.out.println("test = " + );
    }
}
"#};
        let marker = "\"test = \" + ";
        let offset = src.rfind(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(&loc, CursorLocation::MethodArgument { prefix } if prefix.is_empty()),
            "Expected MethodArgument with empty prefix, got {:?}",
            loc
        );
        assert!(query.is_empty(), "query should be empty, got {query:?}");
    }

    #[test]
    fn test_annotation_string_literal_location() {
        let src = r#"class A {
        @SuppressWarnings("not parsed as string lol")
        void f() {}
    }"#;

        // Cursor inside a string literal should stay StringLiteral location.
        let offset = src.find("not parsed").unwrap() + "not parsed".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::StringLiteral { .. }),
            "Expected StringLiteral, got {:?}",
            loc
        );
        assert_eq!(query, "not parsed");
    }

    #[test]
    fn test_annotation_partial_name_is_annotation_not_expression() {
        let src = indoc::indoc! {r#"
class A {
    @SuppressW
    void f() {}
}
"#};
        let offset = src.find("SuppressW").unwrap() + "SuppressW".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::Annotation { .. }),
            "Expected Annotation, got {:?}",
            loc
        );
        assert_eq!(query, "SuppressW");
    }

    #[test]
    fn test_annotation_partial_name_target_method() {
        let src = indoc::indoc! {r#"
class A {
    @Overri
    void f() {}
}
"#};
        let offset = src.find("Overri").unwrap() + "Overri".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, _) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(
                loc,
                CursorLocation::Annotation { ref target_element_type, .. }
                if target_element_type.as_deref() == Some("METHOD")
            ),
            "Expected Annotation with METHOD target, got {:?}",
            loc
        );
    }

    #[test]
    fn test_empty_arg_after_comma_should_not_return_comma_prefix() {
        let src = indoc::indoc! {r#"
class A {
    void f(String s, int a) {
        s.charAt(a, );
    }
}
"#};
        let marker = "charAt(a, ";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::MethodArgument { ref prefix } if prefix.is_empty()),
            "Expected MethodArgument with empty prefix, got {:?}",
            loc
        );
        assert_eq!(query, "");
    }

    #[test]
    fn test_constructor_prefix_normalizes_top_level_generic_suffix() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        new ArrayList<String>()
    }
}
"#};
        let marker = "ArrayList<String>";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(
                loc,
                CursorLocation::ConstructorCall { ref class_prefix, .. } if class_prefix == "ArrayList"
            ),
            "Expected normalized constructor class_prefix=ArrayList, got {:?}",
            loc
        );
        assert_eq!(query, "ArrayList");
    }

    #[test]
    fn test_constructor_prefix_non_generic_unchanged() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        new ArrayList()
    }
}
"#};
        let marker = "ArrayList";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(
                loc,
                CursorLocation::ConstructorCall { ref class_prefix, .. } if class_prefix == "ArrayList"
            ),
            "Expected unchanged constructor class_prefix=ArrayList, got {:?}",
            loc
        );
        assert_eq!(query, "ArrayList");
    }

    #[test]
    fn test_constructor_type_argument_identifier_is_type_annotation() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        new Box<In>(1);
    }
}
"#};
        let marker = "In";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation inside constructor generic arg, got {:?}",
            loc
        );
        assert_eq!(query, "In");
    }

    #[test]
    fn test_constructor_nested_type_argument_identifier_is_type_annotation() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        new Box<Map<String, In>>(1);
    }
}
"#};
        let marker = "In";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation in nested constructor generic arg, got {:?}",
            loc
        );
        assert_eq!(query, "In");
    }

    #[test]
    fn test_constructor_empty_type_argument_hole_is_type_annotation() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        new Box<>(1);
    }
}
"#};
        let marker = "<>";
        let offset = src.find(marker).unwrap() + 1; // cursor in <|>
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation inside empty constructor generic hole, got {:?}",
            loc
        );
        assert_eq!(query, "");
    }

    #[test]
    fn test_constructor_second_type_argument_hole_is_type_annotation() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        new HashMap<String, >(1);
    }
}
"#};
        let marker = "String, >";
        let offset = src.find(marker).unwrap() + "String, ".len(); // cursor in second arg hole
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation in second constructor generic hole, got {:?}",
            loc
        );
        assert_eq!(query, "");
    }

    #[test]
    fn test_method_reference_type_method_classification() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        List::size
    }
}
"#};
        let marker = "List::size";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(
                loc,
                CursorLocation::MethodReference {
                    ref qualifier_expr,
                    ref member_prefix,
                    is_constructor: false
                } if qualifier_expr == "List" && member_prefix == "size"
            ),
            "Expected MethodReference for List::size, got {:?}",
            loc
        );
        assert_eq!(query, "size");
    }

    #[test]
    fn test_method_reference_expr_method_classification() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        this::toString
    }
}
"#};
        let marker = "this::toString";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(
                loc,
                CursorLocation::MethodReference {
                    ref qualifier_expr,
                    ref member_prefix,
                    is_constructor: false
                } if qualifier_expr == "this" && member_prefix == "toString"
            ),
            "Expected MethodReference for this::toString, got {:?}",
            loc
        );
        assert_eq!(query, "toString");
    }

    #[test]
    fn test_method_reference_constructor_classification() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        ArrayList::new
    }
}
"#};
        let marker = "ArrayList::new";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(
                loc,
                CursorLocation::MethodReference {
                    ref qualifier_expr,
                    ref member_prefix,
                    is_constructor: true
                } if qualifier_expr == "ArrayList" && member_prefix == "new"
            ),
            "Expected constructor MethodReference for ArrayList::new, got {:?}",
            loc
        );
        assert_eq!(query, "ArrayList");
    }

    #[test]
    fn test_functional_target_hint_assignment_rhs() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        Function<String, Integer> fn = String::length;
    }
}
"#};
        let marker = "String::length";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let hint = super::infer_functional_target_hint(&ctx, cursor_node);

        assert!(
            matches!(
                hint.as_ref(),
                Some(FunctionalTargetHint {
                    expected_type_source: Some(ty),
                    ..
                }) if ty == "Function<String, Integer>"
            ),
            "Expected assignment RHS target type hint, got {:?}",
            hint
        );
        assert!(matches!(
            hint.as_ref().and_then(|h| h.expr_shape.clone()),
            Some(crate::semantic::context::FunctionalExprShape::MethodReference {
                qualifier_expr,
                member_name,
                is_constructor: false,
                qualifier_kind: crate::semantic::context::MethodRefQualifierKind::Type,
            }) if qualifier_expr == "String" && member_name == "length"
        ));
        assert!(matches!(
            hint.as_ref().and_then(|h| h.expected_type_context.clone()),
            Some(crate::semantic::context::ExpectedTypeSource::VariableInitializer)
        ));
    }

    #[test]
    fn test_functional_target_hint_assignment_rhs_lhs_expr() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        int x = 0;
        x = fo;
    }
}
"#};
        let marker = "fo";
        let offset = src.rfind(marker).unwrap() + 1;
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let hint = super::infer_functional_target_hint(&ctx, cursor_node);

        assert!(matches!(
            hint.as_ref().and_then(|h| h.assignment_lhs_expr.as_deref()),
            Some("x")
        ));
        assert!(matches!(
            hint.as_ref().and_then(|h| h.expected_type_context.clone()),
            Some(crate::semantic::context::ExpectedTypeSource::AssignmentRhs)
        ));
    }

    #[test]
    fn test_functional_target_hint_method_argument() {
        let src = indoc::indoc! {r#"
class A {
    void f(Stream<String> stream) {
        stream.map(String::trim);
    }
}
"#};
        let marker = "String::trim";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let hint = super::infer_functional_target_hint(&ctx, cursor_node);

        assert!(
            matches!(
                hint.as_ref(),
                Some(FunctionalTargetHint {
                    method_call: Some(m),
                    ..
                }) if m.method_name == "map" && m.arg_index == 0 && m.receiver_expr == "stream"
            ),
            "Expected method-argument functional target hint, got {:?}",
            hint
        );
        assert!(matches!(
            hint.as_ref().and_then(|h| h.expr_shape.clone()),
            Some(crate::semantic::context::FunctionalExprShape::MethodReference {
                qualifier_expr,
                member_name,
                is_constructor: false,
                qualifier_kind: crate::semantic::context::MethodRefQualifierKind::Type,
            }) if qualifier_expr == "String" && member_name == "trim"
        ));
    }

    #[test]
    fn test_functional_target_hint_lambda_shape() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        Function<Integer, Integer> fn = x -> x + 1;
    }
}
"#};
        let marker = "x + 1";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let hint = super::infer_functional_target_hint(&ctx, cursor_node);

        assert!(matches!(
            hint.and_then(|h| h.expr_shape),
            Some(crate::semantic::context::FunctionalExprShape::Lambda {
                param_count: 1,
                expression_body: Some(ref body),
            }) if body == "x + 1"
        ));
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
}
