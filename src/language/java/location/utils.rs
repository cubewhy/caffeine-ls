use crate::language::java::JavaContextExtractor;
use crate::semantic::CursorLocation;
use tree_sitter::Node;

pub(super) fn cursor_truncated_text(ctx: &JavaContextExtractor, node: Node) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(ctx.offset);
    if end <= start {
        return String::new();
    }
    ctx.byte_slice(start, end).to_string()
}

pub(super) fn find_innermost_constructor_type_arguments(
    ctor_node: Node,
    offset: usize,
) -> Option<Node> {
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

pub(super) fn find_prefix_in_type_arguments_hole(
    ctx: &JavaContextExtractor,
    type_args: Node,
) -> String {
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

pub(super) fn find_object_creation_at_cursor(node: Node, offset: usize) -> Option<Node> {
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

pub(super) fn location_has_newline(loc: &CursorLocation) -> bool {
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

pub(crate) fn is_member_part_of_scoped_type(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "scoped_type_identifier" {
        return false;
    }
    let mut wc = parent.walk();
    let first_named = parent.named_children(&mut wc).next();
    first_named.is_some_and(|first| first.id() != node.id())
}
