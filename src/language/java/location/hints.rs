use crate::language::java::JavaContextExtractor;
use crate::semantic::CursorLocation;
use crate::semantic::context::{
    ExpectedTypeSource, FunctionalExprShape, FunctionalMethodCallHint, FunctionalTargetHint,
    MethodRefQualifierKind, StatementLabelCompletionKind,
};
use tree_sitter::Node;
use tree_sitter_utils::traversal::{ancestor_of_kind, first_child_of_kind, is_descendant_of};
use tree_sitter_utils::{Handler, HandlerExt, Input, handler_fn};

use super::utils::cursor_truncated_text;

pub(crate) fn infer_functional_target_hint(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
) -> Option<FunctionalTargetHint> {
    let node = cursor_node?;
    let (mut expected_type_source, mut expected_type_context) =
        infer_expected_type_source(ctx, node);
    let assignment_lhs_expr = infer_assignment_rhs_lhs_expr(ctx, node);
    if expected_type_context.is_none() && assignment_lhs_expr.is_some() {
        expected_type_context = Some(ExpectedTypeSource::AssignmentRhs);
    }
    let method_call = infer_method_argument_target_hint(ctx, node);
    let mut expr_shape = infer_functional_expr_shape(ctx, node);
    if (expected_type_source.is_none() || expr_shape.is_none())
        && let Some(recovered) = infer_functional_target_hint_from_error_arrow(ctx, node)
    {
        if expected_type_source.is_none() {
            expected_type_source = recovered.expected_type_source;
        }
        if expected_type_context.is_none() {
            expected_type_context = recovered.expected_type_context;
        }
        if expr_shape.is_none() {
            expr_shape = recovered.expr_shape;
        }
    }
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

/// Fallback: recover functional target hint from ERROR node containing `->`.
///
/// When tree-sitter produces a top-level ERROR for incomplete lambda syntax like
///   `Function<String, Integer> f = s -> s.subs`
/// we scan the ERROR's children for `->` and extract the type from the preceding
/// `Type name =` pattern.
fn infer_functional_target_hint_from_error_arrow(
    ctx: &JavaContextExtractor,
    node: Node,
) -> Option<FunctionalTargetHint> {
    // Walk up to find an ERROR (or program-level ERROR) containing `->`
    let mut container = None;
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "ERROR" || n.kind() == "program" {
            let mut wc = n.walk();
            let has_arrow = n
                .children(&mut wc)
                .any(|c| c.kind() == "->" && c.end_byte() <= ctx.offset);
            if has_arrow {
                container = Some(n);
                break;
            }
        }
        current = n.parent();
    }
    let container = container?;

    let mut wc = container.walk();
    let children: Vec<Node> = container.children(&mut wc).collect();

    // Find the `->` before cursor
    let arrow_idx = children
        .iter()
        .rposition(|n| n.kind() == "->" && n.end_byte() <= ctx.offset)?;

    // Look for pattern: ... Type name = params -> ...
    // We need `=` before params, and a type node before `name`.
    // Walk backwards from arrow: params_node is at arrow_idx-1, then `=`, then `name`, then `Type`
    if arrow_idx < 3 {
        return None;
    }

    let params_idx = (0..arrow_idx).rev().find(|&i| {
        matches!(
            children[i].kind(),
            "identifier" | "inferred_parameters" | "formal_parameters"
        )
    })?;
    let params_node = &children[params_idx];
    // Validate params_node looks like lambda parameters
    // Find `=` before params
    let eq_idx = (0..params_idx).rev().find(|&i| children[i].kind() == "=")?;

    let name_idx = (0..eq_idx)
        .rev()
        .find(|&i| matches!(children[i].kind(), "identifier" | "type_identifier"))?;

    let type_node = (0..name_idx).rev().map(|i| &children[i]).find(|node| {
        matches!(
            node.kind(),
            "generic_type"
                | "type_identifier"
                | "scoped_type_identifier"
                | "array_type"
                | "integral_type"
                | "floating_point_type"
                | "boolean_type"
                | "void_type"
        )
    })?;

    let expected_type = normalize_recovered_expected_type_text(ctx.node_text(*type_node));
    if expected_type.is_empty() {
        return None;
    }

    // Infer lambda param count from params_node
    let param_count = match params_node.kind() {
        "identifier" => Some(1usize),
        "inferred_parameters" => {
            let mut wc2 = params_node.walk();
            Some(
                params_node
                    .named_children(&mut wc2)
                    .filter(|n| n.kind() == "identifier")
                    .count(),
            )
        }
        "formal_parameters" => {
            let mut wc2 = params_node.walk();
            Some(
                params_node
                    .named_children(&mut wc2)
                    .filter(|n| matches!(n.kind(), "formal_parameter" | "spread_parameter"))
                    .count(),
            )
        }
        _ => None,
    };

    // Build expression body (text between `->` and cursor, if not a block)
    let arrow_end = children[arrow_idx].end_byte();
    let expression_body = if ctx.offset > arrow_end {
        let body_text = ctx.source[arrow_end..ctx.offset].trim();
        if body_text.is_empty() || body_text.starts_with('{') {
            None
        } else {
            Some(body_text.to_string())
        }
    } else {
        None
    };

    let expr_shape = param_count.map(|pc| FunctionalExprShape::Lambda {
        param_count: pc,
        expression_body,
    });

    Some(FunctionalTargetHint {
        expected_type_source: Some(expected_type),
        expected_type_context: Some(ExpectedTypeSource::VariableInitializer),
        assignment_lhs_expr: None,
        method_call: None,
        expr_shape,
    })
}

fn normalize_recovered_expected_type_text(raw: &str) -> String {
    let tail = raw
        .lines()
        .rev()
        .map(strip_trailing_line_comment)
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| raw.trim());
    tail.rsplit(';').next().unwrap_or(tail).trim().to_string()
}

fn strip_trailing_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        match b {
            b'\\' if in_string || in_char => {
                escaped = true;
            }
            b'"' if !in_char => in_string = !in_string,
            b'\'' if !in_string => in_char = !in_char,
            b'/' if !in_string && !in_char && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                return &line[..i];
            }
            _ => {}
        }
        i += 1;
    }
    line
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

fn infer_assignment_rhs_expected_type(ctx: &JavaContextExtractor, node: Node) -> Option<String> {
    let declarator = ancestor_of_kind(node, "variable_declarator")?;
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
    let return_stmt = ancestor_of_kind(node, "return_statement")?;
    return_stmt.child_by_field_name("value")?;
    let method_decl = ancestor_of_kind(return_stmt, "method_declaration")?;
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
    let assign = ancestor_of_kind(node, "assignment_expression")?;
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

fn infer_method_argument_target_hint(
    ctx: &JavaContextExtractor,
    node: Node,
) -> Option<FunctionalMethodCallHint> {
    let arg_list = ancestor_of_kind(node, "argument_list")?;
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

pub(super) fn count_top_level_commas(s: &str) -> usize {
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

pub(super) fn split_top_level_args(s: &str) -> Vec<String> {
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

fn infer_functional_expr_shape(
    ctx: &JavaContextExtractor,
    node: Node,
) -> Option<FunctionalExprShape> {
    if let Some(method_ref) = ancestor_of_kind(node, "method_reference") {
        return infer_method_reference_shape(ctx, method_ref);
    }
    if let Some(lambda) = ancestor_of_kind(node, "lambda_expression") {
        return infer_lambda_shape(ctx, lambda);
    }
    None
}

fn infer_method_reference_shape(
    ctx: &JavaContextExtractor,
    method_ref: Node,
) -> Option<FunctionalExprShape> {
    let raw = ctx.node_text(method_ref).trim().to_string();
    let sep_idx = raw.find("::")?;
    let qualifier_expr = raw[..sep_idx].trim().to_string();
    if qualifier_expr.is_empty() {
        return None;
    }
    let rhs_raw = raw[sep_idx + 2..].trim_start();
    let rhs_no_type_args = strip_leading_type_arguments(rhs_raw);
    let member_name = rhs_no_type_args.trim().to_string();
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
    let raw = ctx.node_text(lambda).trim().to_string();
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

fn extract_type_from_decl(ctx: &JavaContextExtractor, decl_node: Node) -> String {
    let mut walker = decl_node.walk();
    for child in decl_node.named_children(&mut walker) {
        if child.kind() == "modifiers" {
            continue;
        }
        if child.kind() == "variable_declarator" {
            break;
        }
        if is_decl_type_node(child.kind()) {
            return normalize_recovered_expected_type_text(ctx.node_text(child));
        }
        if child.kind() == "ERROR"
            && let Some(type_node) = find_first_type_like_descendant(child)
        {
            return normalize_recovered_expected_type_text(ctx.node_text(type_node));
        }
    }
    String::new()
}

fn is_decl_type_node(kind: &str) -> bool {
    matches!(
        kind,
        "generic_type"
            | "type_identifier"
            | "scoped_type_identifier"
            | "array_type"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "void_type"
    )
}

fn find_first_type_like_descendant(node: Node) -> Option<Node> {
    if is_decl_type_node(node.kind()) {
        return Some(node);
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        if let Some(found) = find_first_type_like_descendant(child) {
            return Some(found);
        }
    }
    None
}

// ── statement label detection ─────────────────────────────────────────────────

pub(super) fn detect_statement_label_location(
    ctx: &JavaContextExtractor,
    node: Node,
) -> Option<(CursorLocation, String)> {
    // Check the node itself first (inclusive), then walk ancestors
    let statement = if matches!(node.kind(), "break_statement" | "continue_statement") {
        Some(node)
    } else {
        tree_sitter_utils::traversal::ancestor_of_kinds(
            node,
            &["break_statement", "continue_statement"],
        )
    };
    if let Some(statement) = statement {
        return jump_statement_location(ctx, statement);
    }
    detect_partial_jump_label_location(ctx, node)
}

fn jump_statement_location(
    ctx: &JavaContextExtractor,
    stmt: Node,
) -> Option<(CursorLocation, String)> {
    let kind = match stmt.kind() {
        "break_statement" => StatementLabelCompletionKind::Break,
        "continue_statement" => StatementLabelCompletionKind::Continue,
        _ => return None,
    };
    if !is_active_jump_label_slot(ctx, stmt) {
        return None;
    }
    let prefix = first_child_of_kind(stmt, "identifier")
        .map(|ident| cursor_truncated_text(ctx, ident))
        .unwrap_or_else(|| extract_jump_label_prefix_from_text(ctx, stmt.start_byte()));
    Some((
        CursorLocation::StatementLabel {
            kind,
            prefix: prefix.clone(),
        },
        prefix,
    ))
}

fn extract_jump_label_prefix_from_text(ctx: &JavaContextExtractor, stmt_start: usize) -> String {
    let raw = ctx.byte_slice(stmt_start, ctx.offset).to_string();
    let trimmed = raw.trim_end();
    if let Some(rest) = trimmed.strip_prefix("break") {
        return rest.trim().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("continue") {
        return rest.trim().to_string();
    }
    String::new()
}

fn is_active_jump_label_slot(ctx: &JavaContextExtractor, stmt: Node) -> bool {
    let identifier = first_child_of_kind(stmt, "identifier");
    if let Some(ident) = identifier {
        return ctx.offset >= ident.start_byte() && ctx.offset <= ident.end_byte();
    }
    let keyword_end = stmt.start_byte()
        + match stmt.kind() {
            "break_statement" => "break".len(),
            "continue_statement" => "continue".len(),
            _ => return false,
        };
    ctx.offset >= keyword_end && ctx.offset < stmt.end_byte()
}

fn detect_partial_jump_label_location(
    ctx: &JavaContextExtractor,
    node: Node,
) -> Option<(CursorLocation, String)> {
    let lower_bound = handler_fn(|inp: Input<&JavaContextExtractor>| inp.node.start_byte())
        .for_kinds(&[
            "block",
            "switch_block_statement_group",
            "switch_rule",
            "program",
            "method_declaration",
            "constructor_declaration",
            "compact_constructor_declaration",
            "lambda_expression",
            "class_body",
        ])
        .climb(&[])
        .handle(Input::new(node, ctx, None))
        .unwrap_or(0);
    let before = ctx.byte_slice(lower_bound, ctx.offset).to_string();
    let trimmed = before.trim_end();

    let (kind, prefix) = if let Some(rest) = trimmed.strip_suffix("break") {
        if rest
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        (StatementLabelCompletionKind::Break, String::new())
    } else if let Some(rest) = trimmed.strip_suffix("continue") {
        if rest
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        (StatementLabelCompletionKind::Continue, String::new())
    } else {
        let prefix = extract_identifier_prefix_near_cursor(ctx, lower_bound);
        if prefix.is_empty() {
            return None;
        }
        let ident_start = ctx.offset.saturating_sub(prefix.len());
        let head = ctx
            .byte_slice(lower_bound, ident_start)
            .trim_end()
            .to_string();
        let trimmed_head = head.trim_end();
        if let Some(rest) = trimmed_head.strip_suffix("break") {
            if rest
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return None;
            }
            (StatementLabelCompletionKind::Break, prefix)
        } else if let Some(rest) = trimmed_head.strip_suffix("continue") {
            if rest
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return None;
            }
            (StatementLabelCompletionKind::Continue, prefix)
        } else {
            return None;
        }
    };

    Some((
        CursorLocation::StatementLabel {
            kind,
            prefix: prefix.clone(),
        },
        prefix,
    ))
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
    ctx.source[i..ctx.offset].to_string()
}

#[cfg(test)]
mod tests {

    use crate::language::java::location::hints::{count_top_level_commas, split_top_level_args};

    #[test]
    fn test_split_top_level_args_keeps_lambda_arrow_from_hiding_commas() {
        let args = split_top_level_args(r##"s -> s.subs, "hello""##);
        assert_eq!(args, vec![r#"s -> s.subs"#, r#""hello""#]);
        assert_eq!(count_top_level_commas(r##"s -> s.subs, "hello""##), 1);
    }
}
