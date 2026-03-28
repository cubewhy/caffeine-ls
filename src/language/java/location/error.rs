use crate::language::java::JavaContextExtractor;
use crate::semantic::CursorLocation;
use tree_sitter::Node;
use tree_sitter_utils::traversal;

use super::handlers;
use super::heuristics::{
    detect_new_keyword_before_cursor, detect_trailing_dot_in_text, handle_import_from_text,
    is_import_context, recover_error_location_ast_first,
};
use super::utils::cursor_truncated_text;

/// cursor_node is ERROR (cases 1, 2, 3, 5, 6, 8, 12)
pub(super) fn handle_error(
    ctx: &JavaContextExtractor,
    error_node: Node,
    cursor_node: Node,
    trigger_char: Option<char>,
) -> (CursorLocation, String) {
    let parent_kind = error_node.parent().map(|p| p.kind()).unwrap_or("");

    // ERROR inside argument_list
    if parent_kind == "argument_list" {
        let p = error_node.parent().unwrap();
        let is_trailing_dot = {
            let mut wc = error_node.walk();
            let ch: Vec<_> = error_node.children(&mut wc).collect();
            ch.len() == 1 && ch[0].kind() == "."
        };
        if is_trailing_dot && let Some(recv) = traversal::preceding_named_sibling(error_node, p) {
            let receiver_expr = ctx.node_text(recv).to_string();
            return (
                CursorLocation::MemberAccess {
                    receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                    receiver_semantic_type: None,
                    receiver_type: None,
                    member_prefix: String::new(),
                    receiver_expr,
                    arguments: None,
                },
                String::new(),
            );
        }
        return handlers::handle_argument_list(ctx, p);
    }

    if parent_kind == "annotation_argument_list" {
        let p = error_node.parent().unwrap();
        if let Some(result) = handlers::detect_annotation_param_location(ctx, p, None) {
            return result;
        }
    }

    // ERROR inside class_body (case 5: `prote`)
    if parent_kind == "class_body" {
        return handle_error_in_class_body(ctx, error_node, cursor_node);
    }

    // Source text analysis for ERROR in block or at program level
    let before = &ctx.source[..ctx.offset.min(ctx.source.len())];

    if let Some(result) = recover_error_location_ast_first(ctx, error_node) {
        return result;
    }

    if is_import_context(before) {
        return handle_import_from_text(ctx, before);
    }

    // Priority 1: trailing dot → MemberAccess
    if let Some((receiver, member_prefix)) = detect_trailing_dot_in_text(before) {
        return (
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: member_prefix.clone(),
                receiver_expr: receiver,
                arguments: None,
            },
            member_prefix,
        );
    }

    // Priority 2: `new ClassName` → ConstructorCall
    if let Some(detected) = detect_new_keyword_before_cursor(before) {
        return (
            CursorLocation::ConstructorCall {
                class_prefix: detected.class_prefix.clone(),
                expected_type: None,
                qualifier_expr: detected.qualifier_expr,
                qualifier_owner_internal: None,
            },
            detected.class_prefix,
        );
    }

    // Priority 3: cursor is on identifier → Expression
    if matches!(cursor_node.kind(), "identifier" | "type_identifier") {
        return handlers::handle_identifier(ctx, cursor_node, trigger_char);
    }

    (
        CursorLocation::Expression {
            prefix: String::new(),
        },
        String::new(),
    )
}

/// Case 5: ERROR in class_body
pub(super) fn handle_error_in_class_body(
    ctx: &JavaContextExtractor,
    error_node: Node,
    cursor_node: Node,
) -> (CursorLocation, String) {
    let ident = if matches!(cursor_node.kind(), "identifier" | "type_identifier") {
        Some(cursor_node)
    } else {
        let mut wc = error_node.walk();
        error_node
            .named_children(&mut wc)
            .find(|n| matches!(n.kind(), "identifier" | "type_identifier"))
    };

    if let Some(id) = ident {
        let text = cursor_truncated_text(ctx, id);
        return (
            CursorLocation::Expression {
                prefix: text.clone(),
            },
            text,
        );
    }

    (
        CursorLocation::Expression {
            prefix: String::new(),
        },
        String::new(),
    )
}
