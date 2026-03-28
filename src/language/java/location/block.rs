use crate::language::java::JavaContextExtractor;
use crate::language::java::location::heuristics::{
    detect_new_keyword_before_cursor, detect_trailing_dot_in_text,
    detect_variable_name_after_type_text, detect_variable_name_position,
    recover_error_location_ast_first,
};
use crate::semantic::CursorLocation;
use tree_sitter::Node;

/// cursor_node is the block itself (cases 4, 9, 10, 11, 13, 14)
pub(super) fn handle_block_as_cursor(
    ctx: &JavaContextExtractor,
    block: Node,
) -> (CursorLocation, String) {
    // Find last named child ending at or before cursor
    let last_child = {
        let mut wc = block.walk();
        let mut last: Option<Node> = None;
        for child in block.named_children(&mut wc) {
            if child.end_byte() <= ctx.offset {
                last = Some(child);
            }
        }
        last
    };

    if let Some(child) = last_child
        && child.kind() == "ERROR"
    {
        return handle_error_as_last_block_child(ctx, child);
    }

    // No ERROR: check for variable name position (e.g. `List<String> |`)
    if let Some(var_loc) = detect_variable_name_position(ctx, block) {
        return var_loc;
    }

    // Text fallback: trailing whitespace after a type-like token in a block.
    if let Some(type_name) = detect_variable_name_after_type_text(ctx) {
        return (CursorLocation::VariableName { type_name }, String::new());
    }

    let before = &ctx.source[..ctx.offset.min(ctx.source.len())];
    if let Some((receiver_expr, member_prefix)) = detect_trailing_dot_in_text(before) {
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

    // Empty block or cursor after complete statement → Expression
    (
        CursorLocation::Expression {
            prefix: String::new(),
        },
        String::new(),
    )
}

/// ERROR is the last child of block before cursor (cases 9, 11, 12, 13)
fn handle_error_as_last_block_child(
    ctx: &JavaContextExtractor,
    error_node: Node,
) -> (CursorLocation, String) {
    if let Some(result) = recover_error_location_ast_first(ctx, error_node) {
        return result;
    }

    let before = &ctx.source[..ctx.offset.min(ctx.source.len())];
    // Case 12/cases with `new`: detect new keyword
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
    // Case 11/13: incomplete assignment `int x =`) → Expression
    (
        CursorLocation::Expression {
            prefix: String::new(),
        },
        String::new(),
    )
}
