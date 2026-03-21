//! # Java Location Heuristics
//!
//! This module centralizes all heuristic rules used by Java cursor-location
//! detection. These rules exist because tree-sitter operates on incomplete or
//! invalid code while the user is typing, and Java syntax can be ambiguous
//! without full context.
//!
//! ## Guiding Principles
//! - Keep heuristics explicit and testable.
//! - Prefer AST-based signals when possible; fall back to source-text analysis
//!   only when the AST is missing or misleading.
//! - Document why each heuristic exists and what trade-offs it introduces.
//! - Keep the call sites in `location/*` orchestration code thin; the logic
//!   lives here.

use crate::language::java::JavaContextExtractor;
use crate::language::java::location::utils::cursor_truncated_text;
use crate::language::java::members::is_java_keyword;
use crate::language::java::utils::strip_sentinel;
use crate::semantic::CursorLocation;
use tree_sitter::Node;
use tree_sitter_utils::traversal::is_descendant_of;

// === Text-only heuristics =====================================================

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Detect trailing-dot pattern in source text before the cursor.
///
/// What it does:
/// - Parses the last line of text and looks for `<expr>.<prefix>`.
/// - Returns `(receiver_expr, member_prefix)` when the pattern is found.
///
/// Why it exists:
/// - In ERROR contexts, the AST may not include a `field_access` or
///   `method_invocation` node yet, but users still expect member completion.
///
/// Effect on behavior:
/// - Routes completion into `MemberAccess` even when the AST is missing the
///   dot node.
///
/// Trade-offs:
/// - Text-only parsing can be fooled by complex line prefixes; we limit this
///   to the last line to reduce false positives.
pub(super) fn detect_trailing_dot_in_text(before_cursor: &str) -> Option<(String, String)> {
    let s = before_cursor.trim_end();
    if s.is_empty() {
        return None;
    }
    let last_line = s.rsplit('\n').next().unwrap_or(s);
    let bytes = last_line.as_bytes();
    let len = bytes.len();

    let mut i = len;
    while i > 0 && is_ident_char(bytes[i - 1]) {
        i -= 1;
    }
    let member_prefix = last_line[i..].to_string();

    if i == 0 || bytes[i - 1] != b'.' {
        return None;
    }
    let dot_pos = i - 1;

    if !member_prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }

    let before_dot = &last_line[..dot_pos];
    let receiver_expr = extract_last_expr_from_text(before_dot)?;

    if receiver_expr.is_empty() {
        return None;
    }

    Some((receiver_expr, member_prefix))
}

/// Extract the last complete expression from the end of a string.
///
/// What it does:
/// - Walks backwards, respecting parentheses and braces, and returns the last
///   expression chunk on the line (or after `;`, `=`, or `->`).
///
/// Why it exists:
/// - Needed to recover the receiver expression for member access when the AST
///   is incomplete.
///
/// Trade-offs:
/// - This is a heuristic; it does not fully parse Java and can be wrong for
///   extremely complex line prefixes.
pub(super) fn extract_last_expr_from_text(s: &str) -> Option<String> {
    let s = s.trim_end();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let end = bytes.len();
    let mut paren_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut i = end;

    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => paren_depth += 1,
            b'(' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                } else {
                    return non_empty(s[i + 1..end].trim());
                }
            }
            b'}' => brace_depth += 1,
            b'{' => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                } else {
                    return non_empty(s[i + 1..end].trim());
                }
            }
            b';' if paren_depth == 0 && brace_depth == 0 => {
                return non_empty(s[i + 1..end].trim());
            }
            b'\n' if paren_depth == 0 && brace_depth == 0 => {
                return non_empty(s[i + 1..end].trim());
            }
            // lambda arrow `->`
            b'>' if paren_depth == 0 && brace_depth == 0 && i > 0 && bytes[i - 1] == b'-' => {
                return non_empty(s[i + 1..end].trim());
            }
            // assignment `=` (not `==`, `!=`, `<=`, `>=`)
            b'=' if paren_depth == 0 && brace_depth == 0 => {
                let prev = if i > 0 { bytes[i - 1] } else { 0 };
                let next = if i + 1 < end { bytes[i + 1] } else { 0 };
                if !matches!(prev, b'=' | b'!' | b'<' | b'>') && next != b'=' {
                    return non_empty(s[i + 1..end].trim());
                }
            }
            _ => {}
        }
    }

    non_empty(s[..end].trim())
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Detect `new ClassName` pattern before the cursor.
///
/// What it does:
/// - Scans the last line for a standalone `new` token and captures the
///   following identifier prefix.
///
/// Why it exists:
/// - `object_creation_expression` is often missing or malformed in error
///   recovery states; this allows constructor completion to still trigger.
///
/// Trade-offs:
/// - Only inspects the last line; multi-line `new` expressions are treated
///   conservatively.
pub(super) fn detect_new_keyword_before_cursor(
    before_cursor: &str,
) -> Option<(String, Option<String>)> {
    let current_line = before_cursor.rsplit('\n').next().unwrap_or(before_cursor);
    let current_line = current_line.trim_end();
    if current_line.is_empty() {
        return None;
    }

    let last_line = current_line.trim_start();

    let new_start = find_new_token_pos(last_line)?;
    let after_new = last_line[new_start + 3..].trim_start();

    if !after_new.is_empty() {
        let first = after_new.chars().next().unwrap();
        if !first.is_ascii_alphabetic() && first != '_' && first != '$' {
            return None;
        }
    }

    let class_prefix: String = after_new
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$' || *c == '.')
        .collect();

    Some((class_prefix, None))
}

/// Find position of a standalone `new` token in a string.
fn find_new_token_pos(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"new" {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok = i + 3 >= bytes.len() || !is_ident_char(bytes[i + 3]);
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Determine if text before the cursor looks like an `import` statement.
///
/// What it does:
/// - Looks at the last statement-like segment and checks for `import`.
///
/// Why it exists:
/// - In error recovery, the AST may not have an `import_declaration` node yet.
///
/// Trade-offs:
/// - Pure text scan; can be fooled by embedded `import` tokens in comments.
pub(super) fn is_import_context(before_cursor: &str) -> bool {
    let last_stmt = before_cursor
        .rsplit([';', '{', '}'])
        .next()
        .unwrap_or(before_cursor)
        .trim_start();
    last_stmt.starts_with("import")
}

/// Build an import location from text-only context.
///
/// What it does:
/// - Extracts the typed import prefix and returns the appropriate
///   `CursorLocation` (static vs non-static).
///
/// Why it exists:
/// - Enables import completion even when the parser failed to create an AST
///   node for the import statement.
///
/// Trade-offs:
/// - Relies on string parsing; does not validate the full statement.
pub(super) fn handle_import_from_text(
    _ctx: &JavaContextExtractor,
    before: &str,
) -> (CursorLocation, String) {
    let last_stmt = before
        .rsplit([';', '{', '}'])
        .next()
        .unwrap_or(before)
        .trim_start();

    let after_import = last_stmt.strip_prefix("import").unwrap_or("").trim_start();
    let is_static = after_import.starts_with("static ");
    let prefix_raw = if is_static {
        after_import
            .strip_prefix("static")
            .unwrap_or("")
            .trim_start()
    } else {
        after_import
    };
    let prefix = strip_sentinel(prefix_raw);
    let query = prefix.rsplit('.').next().unwrap_or("").to_string();
    let location = if is_static {
        CursorLocation::ImportStatic { prefix }
    } else {
        CursorLocation::Import { prefix }
    };
    (location, query)
}

// === AST recovery heuristics ==================================================

/// Detects if an identifier in a local_variable_declaration is actually a
/// misread expression.
///
/// What it does:
/// - Checks if a `local_variable_declaration` is immediately followed by a
///   sibling that begins with `(`.
///
/// Why it exists:
/// - Tree-sitter may misparse `str\nfunc(...)` as a declaration (`str` type,
///   `func` variable name), but it's actually two statements.
///
/// Effect on behavior:
/// - Forces the cursor location to `Expression` to avoid type completions.
///
/// Trade-offs:
/// - Only detects the specific "next token is '('" pattern.
pub(super) fn is_misread_expression_in_local_decl(
    _node: Node,
    decl_node: Node,
    ctx: &JavaContextExtractor,
) -> bool {
    if decl_node.kind() != "local_variable_declaration" {
        return false;
    }

    if let Some(next) = decl_node.next_sibling() {
        let next_text = &ctx.source[next.start_byte()..next.end_byte()];
        if next_text.trim_start().starts_with('(') {
            return true;
        }
    }

    false
}

/// Detects member access tail misread as local variable declaration.
///
/// What it does:
/// - Looks for an `ERROR` subtree inside a `local_variable_declaration` and
///   extracts `<receiver>.<member_prefix>` from the raw text.
///
/// Why it exists:
/// - Incomplete member access like `a.put` can be misparsed as a declaration
///   (`a` type, `put` variable name).
///
/// Effect on behavior:
/// - Routes completion to `MemberAccess` instead of type/variable name.
///
/// Trade-offs:
/// - Assumes the last dot before the cursor represents member access.
pub(super) fn detect_member_tail_in_misread_local_decl(
    ctx: &JavaContextExtractor,
    node: Node,
    decl_node: Node,
) -> Option<(String, String)> {
    // Must be in ERROR context
    let err = crate::language::java::utils::find_ancestor(node, "ERROR")?;
    if !is_descendant_of(err, decl_node) {
        return None;
    }

    let start = decl_node.start_byte();
    if ctx.offset <= start {
        return None;
    }

    let raw = &ctx.source[start..ctx.offset];
    let clean = strip_sentinel(raw).trim_end().to_string();

    // Look for dot indicating member access
    let dot = clean.rfind('.')?;
    let receiver_expr = clean[..dot].trim().to_string();
    let member_prefix = clean[dot + 1..].trim().to_string();

    if receiver_expr.is_empty() {
        return None;
    }

    Some((receiver_expr, member_prefix))
}

/// Detects if the cursor is in a variable-name position after a *previous*
/// complete declaration, specifically for array-like declarations.
///
/// What it does:
/// - If a complete *array* declaration appears before the current statement and
///   a newline separates them, treat the current identifier as a *type* for a
///   new declaration **only when** the cursor has moved past the type token and
///   at least one whitespace character is present (e.g., `ArrayList |`).
///
/// Why it exists:
/// - After a valid (or near-valid) declaration, tree-sitter often parses the
///   next line as a bare expression. Users typically intend to start a new
///   declaration, so variable-name completion should trigger.
///
/// Effect on behavior:
/// - Returns `Some(type_name)` so the caller can emit `VariableName` location.
///
/// Trade-offs:
/// - We intentionally require the previous declaration to look array-like
///   (`[]`) and the current line to be a simple identifier. This avoids
///   misclassifying bare identifier expressions on new lines, but means we
///   won't trigger for non-array declarations in malformed code.
/// - We also require a whitespace gap after the type token to avoid returning
///   `VariableName` for `ArrayList|` (still typing the type).
pub(super) fn is_variable_name_after_previous_declaration(
    id_node: Node,
    ctx: &JavaContextExtractor,
) -> Option<String> {
    if !line_prefix_is_simple_identifier(ctx) {
        return None;
    }
    if !cursor_after_identifier_with_space(id_node, ctx) {
        return None;
    }

    if let Some((container, containing_stmt)) = find_container_and_stmt(id_node) {
        if has_semicolon_after_cursor_in_stmt(containing_stmt, ctx.offset) {
            return None;
        }
        if let Some(prev_decl) = find_last_complete_decl_before(container, containing_stmt)
            .or_else(|| find_last_complete_decl_within_error(containing_stmt, id_node))
        {
            let prev_text = ctx.node_text(prev_decl);
            if !looks_like_array_declaration(prev_text) {
                return None;
            }
            let gap_start = prev_decl.end_byte().min(ctx.source.len());
            let gap_end = id_node.start_byte().min(ctx.source.len());
            if gap_start < gap_end {
                let gap = &ctx.source[gap_start..gap_end];
                if gap.contains('\n')
                    && matches!(
                        containing_stmt.kind(),
                        "local_variable_declaration"
                            | "field_declaration"
                            | "ERROR"
                            | "expression_statement"
                    )
                {
                    if let Some(type_name) = extract_type_from_decl_like(ctx, containing_stmt) {
                        return Some(type_name);
                    }

                    let id_text = strip_sentinel(ctx.node_text(id_node).trim());
                    if !id_text.is_empty() && !is_java_keyword(&id_text) {
                        return Some(id_text);
                    }
                }
            }
        }
    }

    None
}

fn find_container_and_stmt(id_node: Node) -> Option<(Node, Node)> {
    let mut current = id_node;
    while let Some(parent) = current.parent() {
        if matches!(parent.kind(), "block" | "class_body" | "program") {
            return Some((parent, current));
        }
        current = parent;
    }
    None
}

fn find_last_complete_decl_before<'a>(container: Node<'a>, stmt: Node<'a>) -> Option<Node<'a>> {
    let mut walker = container.walk();
    let mut prev: Option<Node> = None;
    for child in container.named_children(&mut walker) {
        if child.start_byte() >= stmt.start_byte() {
            break;
        }
        if let Some(found) = find_last_complete_decl_in(child) {
            prev = Some(found);
        }
    }
    prev
}

fn find_last_complete_decl_within_error<'a>(
    containing_stmt: Node<'a>,
    id_node: Node<'a>,
) -> Option<Node<'a>> {
    if containing_stmt.kind() != "ERROR" {
        return None;
    }
    let limit = id_node.start_byte();
    let semi_limit = find_last_semicolon_before(containing_stmt, limit)?;
    find_last_complete_decl_before_in(containing_stmt, semi_limit)
}

fn find_last_complete_decl_in(node: Node) -> Option<Node> {
    let mut last = None;
    if is_complete_variable_declaration(node) {
        last = Some(node);
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        if let Some(found) = find_last_complete_decl_in(child) {
            last = Some(found);
        }
    }
    last
}

fn find_last_complete_decl_before_in(node: Node, limit: usize) -> Option<Node> {
    let mut last = None;
    if is_complete_variable_declaration(node) && node.end_byte() <= limit {
        last = Some(node);
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        if child.start_byte() > limit {
            break;
        }
        if let Some(found) = find_last_complete_decl_before_in(child, limit) {
            last = Some(found);
        }
    }
    last
}

fn line_prefix_is_simple_identifier(ctx: &JavaContextExtractor) -> bool {
    let before = &ctx.source[..ctx.offset.min(ctx.source.len())];
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_prefix = &before[line_start..];
    let trimmed = line_prefix.trim();
    if trimmed.is_empty() {
        if ctx.offset < ctx.source.len() {
            return is_ident_char(ctx.source.as_bytes()[ctx.offset]);
        }
        return false;
    }
    trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn looks_like_array_declaration(text: &str) -> bool {
    text.contains("[]")
}

fn cursor_after_identifier_with_space(id_node: Node, ctx: &JavaContextExtractor) -> bool {
    let id_end = id_node.end_byte();
    if ctx.offset <= id_end || id_end > ctx.source.len() {
        return false;
    }
    let gap_end = ctx.offset.min(ctx.source.len());
    let gap = &ctx.source[id_end..gap_end];
    gap.chars().any(|c| c.is_whitespace())
}

fn has_semicolon_after_cursor_in_stmt(stmt: Node, offset: usize) -> bool {
    if stmt.kind() == "ERROR" {
        return collect_semicolon_positions(stmt)
            .iter()
            .any(|span| span.start >= offset);
    }
    let mut wc = stmt.walk();
    for child in stmt.children(&mut wc) {
        if child.kind() == ";" && child.start_byte() >= offset {
            return true;
        }
    }
    false
}

fn find_last_semicolon_before(error_node: Node, limit: usize) -> Option<usize> {
    let semicolons = collect_semicolon_positions(error_node);
    semicolons
        .iter()
        .filter(|span| span.end <= limit)
        .map(|span| span.end)
        .max()
}

fn find_next_semicolon_after(error_node: Node, limit: usize) -> Option<usize> {
    let semicolons = collect_semicolon_positions(error_node);
    semicolons
        .iter()
        .filter(|span| span.start >= limit)
        .map(|span| span.start)
        .min()
}

struct SemicolonSpan {
    start: usize,
    end: usize,
}

fn collect_semicolon_positions(error_node: Node) -> Vec<SemicolonSpan> {
    let mut semicolons = Vec::new();
    let mut stack = vec![error_node];
    while let Some(node) = stack.pop() {
        let mut wc = node.walk();
        for child in node.children(&mut wc) {
            if child.kind() == ";" {
                if !semicolon_in_for_header(child, error_node) {
                    semicolons.push(SemicolonSpan {
                        start: child.start_byte(),
                        end: child.end_byte(),
                    });
                }
            } else if child.child_count() > 0 {
                stack.push(child);
            }
        }
    }
    semicolons.sort_by_key(|span| span.start);
    semicolons
}

fn semicolon_in_for_header(mut node: Node, error_node: Node) -> bool {
    while let Some(parent) = node.parent() {
        if parent.id() == error_node.id() {
            return false;
        }
        if parent.kind() == "for_statement" {
            return true;
        }
        node = parent;
    }
    false
}

fn is_complete_variable_declaration(node: Node) -> bool {
    matches!(
        node.kind(),
        "local_variable_declaration" | "field_declaration"
    ) && has_variable_declarator(node)
}

fn has_variable_declarator(node: Node) -> bool {
    let mut wc = node.walk();
    node.named_children(&mut wc)
        .any(|child| child.kind() == "variable_declarator")
}

fn extract_type_from_decl_like(ctx: &JavaContextExtractor, decl_node: Node) -> Option<String> {
    if !matches!(
        decl_node.kind(),
        "local_variable_declaration" | "field_declaration"
    ) {
        return None;
    }
    let mut walker = decl_node.walk();
    let type_node = decl_node
        .named_children(&mut walker)
        .find(|child| child.kind() != "modifiers")?;
    let type_text = strip_sentinel(ctx.node_text(type_node).trim());
    if type_text.is_empty() || is_java_keyword(&type_text) {
        return None;
    }
    Some(type_text)
}

/// Detects if the cursor is in variable name position after a type declaration.
///
/// What it does:
/// - For a `local_variable_declaration` that has no `variable_declarator`,
///   checks whether there is whitespace between the type and the cursor.
///
/// Why it exists:
/// - Distinguishes `String|` (still typing type) from `String |` (type complete,
///   variable name expected).
///
/// Effect on behavior:
/// - Enables variable-name suggestions in the latter case.
///
/// Trade-offs:
/// - Only applies to local variable declarations, not fields.
pub(super) fn is_variable_name_after_complete_type(
    _id_node: Node,
    decl_node: Node,
    ctx: &JavaContextExtractor,
) -> bool {
    // Must be in local_variable_declaration
    if decl_node.kind() != "local_variable_declaration" {
        return false;
    }

    // Must have no variable_declarator (incomplete declaration)
    let mut walker = decl_node.walk();
    let has_declarator = decl_node
        .named_children(&mut walker)
        .any(|child| child.kind() == "variable_declarator");

    if has_declarator {
        return false;
    }

    // Find the type node (first non-modifiers child)
    let type_node = {
        let mut walker = decl_node.walk();
        decl_node
            .named_children(&mut walker)
            .find(|child| child.kind() != "modifiers")
    };

    let type_node = match type_node {
        Some(n) => n,
        None => return false,
    };

    // Check if there's whitespace after the type node and before cursor
    let type_end = type_node.end_byte();
    if ctx.offset <= type_end {
        return false;
    }

    let gap = &ctx.source[type_end..ctx.offset];

    // Must have at least one whitespace character
    if !gap.chars().any(|c| c.is_whitespace()) {
        return false;
    }

    // The type text should not be a Java keyword
    let type_text = ctx.node_text(type_node).trim();
    if is_java_keyword(type_text) {
        return false;
    }

    // Check if type looks complete
    matches!(
        type_node.kind(),
        "type_identifier"
            | "generic_type"
            | "array_type"
            | "scoped_type_identifier"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
    )
}

/// Detect variable name position inside an ERROR node containing only a type.
///
/// What it does:
/// - Accepts `ERROR` nodes that wrap a single type-like child and no assignment
///   or semicolon.
///
/// Why it exists:
/// - Tree-sitter emits ERROR for incomplete declarations like `List<String> |`.
///
/// Effect on behavior:
/// - Converts such positions into `VariableName` completions.
///
/// Trade-offs:
/// - If the ERROR node actually represents another construct, we may suggest
///   variable names too eagerly. We mitigate this by requiring a single
///   type-like child and no `=` or `;` within the *current* statement segment.
/// - We split the ERROR node at semicolons using tree-sitter token children so
///   a previous statement (e.g., `Object[] o = ...;`) does not block variable
///   name completion for the next statement.
pub(super) fn detect_variable_name_position_in_error(
    ctx: &JavaContextExtractor,
    error_node: Node,
) -> Option<String> {
    if ctx.offset < error_node.end_byte() {
        return None;
    }
    let segment_start = find_last_semicolon_before(error_node, ctx.offset)
        .unwrap_or_else(|| error_node.start_byte());
    let segment_end =
        find_next_semicolon_after(error_node, ctx.offset).unwrap_or_else(|| error_node.end_byte());
    let mut wc = error_node.walk();
    let named_children: Vec<Node> = error_node
        .named_children(&mut wc)
        .filter(|child| child.start_byte() >= segment_start && child.end_byte() <= segment_end)
        .collect();
    if named_children.len() != 1 {
        return None;
    }
    let inner = named_children[0];
    if !is_type_like_node_kind(inner.kind()) {
        return None;
    }
    let type_end = inner.end_byte();
    if ctx.offset <= type_end {
        return None;
    }
    if segment_end < error_node.end_byte() {
        return None;
    }
    let gap_end = ctx.offset.min(ctx.source.len());
    let gap_start = type_end.min(gap_end);
    let gap = &ctx.source[gap_start..gap_end];
    if !gap.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    if inner.kind() == "identifier" {
        let text = ctx.node_text(inner).trim();
        if is_java_keyword(text) {
            return None;
        }
    }
    let mut wc2 = error_node.walk();
    let has_assignment_or_semi = error_node.children(&mut wc2).any(|c| {
        c.start_byte() >= segment_start
            && c.end_byte() <= segment_end
            && matches!(c.kind(), "=" | ";")
    });
    if has_assignment_or_semi {
        return None;
    }
    let type_name = strip_sentinel(ctx.node_text(inner).trim());
    if type_name.is_empty() {
        return None;
    }
    Some(type_name)
}

/// Detect variable name position when the last block child is an ERROR node.
///
/// This is a convenience wrapper for `detect_variable_name_position_in_error`.
pub(super) fn detect_variable_name_position(
    ctx: &JavaContextExtractor,
    block: Node,
) -> Option<(CursorLocation, String)> {
    let preceding = {
        let mut wc = block.walk();
        let mut last: Option<Node> = None;
        for child in block.named_children(&mut wc) {
            if child.end_byte() <= ctx.offset {
                last = Some(child);
            } else {
                break;
            }
        }
        last
    }?;
    if preceding.kind() != "ERROR" {
        return None;
    }

    let type_name = detect_variable_name_position_in_error(ctx, preceding)?;
    Some((CursorLocation::VariableName { type_name }, String::new()))
}

/// Recognize type-like AST node kinds used in variable-name heuristics.
fn is_type_like_node_kind(kind: &str) -> bool {
    matches!(
        kind,
        "type_identifier"
            | "identifier"
            | "generic_type"
            | "array_type"
            | "scoped_type_identifier"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "void_type"
            | "annotated_type"
    )
}

/// Detect variable name position from source text when the AST is missing or unhelpful.
///
/// What it does:
/// - Checks the current line for a type-like token followed by whitespace.
/// - Ensures the line contains only characters that can appear in a type
///   (identifiers, generics, arrays, dots, and whitespace).
///
/// Why it exists:
/// - When the cursor is on trailing whitespace, `find_cursor_node` may not
///   return a useful AST node (often `Unknown`), but users still expect
///   variable-name suggestions after a completed type.
///
/// Trade-offs:
/// - Pure text validation can be fooled by unusual formatting. We keep it
///   conservative to avoid misclassifying expressions.
pub(super) fn detect_variable_name_after_type_text(ctx: &JavaContextExtractor) -> Option<String> {
    let before = &ctx.source[..ctx.offset.min(ctx.source.len())];
    let last_byte = before.as_bytes().last().copied()?;
    if !(last_byte as char).is_whitespace() {
        return None;
    }

    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &before[line_start..];
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains(';') {
        return None;
    }
    if !trimmed.chars().all(is_type_text_char) {
        return None;
    }

    let first_token = first_identifier_token(trimmed)?;
    if is_java_keyword(&first_token) {
        return None;
    }
    let last_token = last_identifier_token(trimmed)?;
    if is_java_keyword(&last_token) {
        return None;
    }

    Some(trimmed.trim().to_string())
}

fn is_type_text_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '_' | '$' | '.' | '<' | '>' | ',' | '?' | '[' | ']' | ' ' | '&'
        )
}

fn first_identifier_token(s: &str) -> Option<String> {
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            start = Some(i);
            break;
        }
    }
    let start = start?;
    let mut end = start;
    for (i, c) in s[start..].char_indices() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '$') {
            break;
        }
        end = start + i + c.len_utf8();
    }
    if end <= start {
        return None;
    }
    Some(s[start..end].to_string())
}

fn last_identifier_token(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 && !(bytes[i - 1] as char).is_ascii_alphanumeric() && bytes[i - 1] != b'_' {
        if bytes[i - 1] != b'$' {
            i -= 1;
            continue;
        }
        break;
    }
    let end = i;
    while i > 0 {
        let b = bytes[i - 1];
        if (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$' {
            i -= 1;
        } else {
            break;
        }
    }
    if i >= end {
        return None;
    }
    Some(s[i..end].to_string())
}

/// Convert a misparsed `scoped_type_identifier` into member access.
///
/// What it does:
/// - Treats `a.b` parsed as a scoped type as `receiver=a` and `member=b`.
///
/// Why it exists:
/// - Error recovery often promotes `a.b` into a type when it is actually
///   member access.
///
/// Effect on behavior:
/// - Routes completion to `MemberAccess` instead of type completion.
pub(super) fn scoped_type_to_member_access(
    ctx: &JavaContextExtractor,
    scoped: Node,
) -> Option<(CursorLocation, String)> {
    let mut wc = scoped.walk();
    let parts: Vec<Node> = scoped.named_children(&mut wc).collect();
    if parts.len() < 2 {
        return None;
    }
    let member_node = *parts.last()?;
    // dot is one byte before member_node
    let receiver_end = member_node.start_byte().saturating_sub(1);
    let receiver_start = scoped.start_byte();
    if receiver_end <= receiver_start {
        return None;
    }
    let receiver_expr = ctx.source[receiver_start..receiver_end].trim().to_string();
    if receiver_expr.is_empty() || receiver_expr.contains(' ') || receiver_expr.contains('\t') {
        return None;
    }

    let member_prefix = strip_sentinel(&cursor_truncated_text(ctx, member_node));

    if receiver_expr.is_empty() {
        return None;
    }

    Some((
        CursorLocation::MemberAccess {
            receiver_semantic_type: None,
            receiver_type: None,
            member_prefix: member_prefix.clone(),
            receiver_expr,
            arguments: None,
        },
        member_prefix,
    ))
}

/// Detect member access when a scoped type is actually a member access.
///
/// What it does:
/// - If the cursor is within a `scoped_type_identifier` and the text looks like
///   `a.b`, return `MemberAccess`.
///
/// Why it exists:
/// - Tree-sitter can misclassify member access as a scoped type during recovery.
///
/// Trade-offs:
/// - Requires a simple `receiver.member` shape; ignores complex receivers.
pub(super) fn detect_member_access_in_scoped_type(
    ctx: &JavaContextExtractor,
    scoped_type: Node,
) -> Option<(CursorLocation, String)> {
    if scoped_type.kind() != "scoped_type_identifier" {
        return None;
    }
    // Only trigger when cursor is within this node
    if ctx.offset < scoped_type.start_byte() || ctx.offset > scoped_type.end_byte() {
        return None;
    }
    let mut wc = scoped_type.walk();
    let parts: Vec<Node> = scoped_type.named_children(&mut wc).collect();
    if parts.len() < 2 {
        return None;
    }
    let member_node = *parts.last()?;
    let receiver_start = scoped_type.start_byte();
    let dot_pos = member_node.start_byte().checked_sub(1)?;
    let receiver_end = dot_pos;
    if receiver_end <= receiver_start {
        return None;
    }
    let receiver_expr = ctx.source[receiver_start..receiver_end].trim().to_string();
    if receiver_expr.is_empty() || receiver_expr.contains(' ') {
        return None;
    }
    let member_prefix = strip_sentinel(&cursor_truncated_text(ctx, member_node));
    Some((
        CursorLocation::MemberAccess {
            receiver_semantic_type: None,
            receiver_type: None,
            member_prefix: member_prefix.clone(),
            receiver_expr,
            arguments: None,
        },
        member_prefix,
    ))
}

/// Detect member access misread as a local variable declaration.
///
/// What it does:
/// - If the `type` node of a `local_variable_declaration` is a
///   `scoped_type_identifier`, interpret it as `receiver.member`.
///
/// Why it exists:
/// - Error recovery can interpret `a.b` as a type path.
///
/// Effect on behavior:
/// - Routes completion to `MemberAccess` with the extracted prefix.
pub(super) fn detect_member_access_in_local_decl(
    ctx: &JavaContextExtractor,
    decl_node: Node,
) -> Option<(CursorLocation, String)> {
    // If type is scoped_type_identifier and cursor is at its end, treat as member access.
    let type_node = {
        let mut wc = decl_node.walk();
        decl_node
            .named_children(&mut wc)
            .find(|n| n.kind() != "variable_declarator" && n.kind() == "scoped_type_identifier")
    }?;

    // Cursor must be within type_node.
    if ctx.offset < type_node.start_byte() || ctx.offset > type_node.end_byte() {
        return None;
    }

    let mut wc = type_node.walk();
    let parts: Vec<Node> = type_node.named_children(&mut wc).collect();
    if parts.len() < 2 {
        return None;
    }

    let member_node = *parts.last()?;
    let dot_before = member_node.start_byte().saturating_sub(1);
    let receiver_end = dot_before;
    let receiver_start = type_node.start_byte();
    if receiver_end <= receiver_start {
        return None;
    }

    let receiver_expr = ctx.source[receiver_start..receiver_end].trim().to_string();
    if receiver_expr.contains(' ') || receiver_expr.is_empty() {
        return None;
    }

    let member_prefix = strip_sentinel(&cursor_truncated_text(ctx, member_node));

    Some((
        CursorLocation::MemberAccess {
            receiver_semantic_type: None,
            receiver_type: None,
            member_prefix: member_prefix.clone(),
            receiver_expr,
            arguments: None,
        },
        member_prefix,
    ))
}

/// Detect constructor completion inside a local declaration ERROR subtree.
///
/// What it does:
/// - Looks for `new` within an ERROR child of a local declaration and extracts
///   the class prefix via text scanning.
///
/// Why it exists:
/// - In incomplete declarations, the constructor node may not be present.
///
/// Effect on behavior:
/// - Forces `ConstructorCall` completion with an expected type hint.
///
/// Trade-offs:
/// - Depends on text parsing for the class name prefix.
pub(super) fn detect_constructor_in_local_decl_error(
    ctx: &JavaContextExtractor,
    decl_node: Node,
) -> Option<(CursorLocation, String)> {
    let mut wc = decl_node.walk();
    let error_child = decl_node
        .children(&mut wc)
        .find(|n| n.kind() == "ERROR" && n.start_byte() <= ctx.offset)?;

    let mut wc2 = error_child.walk();
    let has_new = error_child.children(&mut wc2).any(|n| n.kind() == "new");
    if !has_new {
        return None;
    }

    let before = &ctx.source[..ctx.offset.min(ctx.source.len())];
    let (class_prefix, _) = detect_new_keyword_before_cursor(before)?;

    let expected_type = {
        let mut wc3 = decl_node.walk();
        decl_node
            .named_children(&mut wc3)
            .find(|n| n.kind() != "variable_declarator" && !n.is_error())
            .map(|n| ctx.node_text(n).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    Some((
        CursorLocation::ConstructorCall {
            class_prefix: class_prefix.clone(),
            expected_type,
        },
        class_prefix,
    ))
}

/// Detect `expr .` pattern inside an ERROR node and turn it into member access.
///
/// What it does:
/// - Finds an expression child that ends before the cursor and checks if the
///   following text begins with `.`.
///
/// Why it exists:
/// - Error recovery may split `expr.` into an ERROR node rather than a proper
///   `field_access`.
///
/// Effect on behavior:
/// - Routes completion to `MemberAccess` with the extracted receiver/prefix.
///
/// Trade-offs:
/// - Only handles simple expression receivers.
pub(super) fn detect_dot_after_expression_child(
    ctx: &JavaContextExtractor,
    error_node: Node,
) -> Option<(CursorLocation, String)> {
    let mut wc = error_node.walk();
    let children: Vec<Node> = error_node.children(&mut wc).collect();

    let expr_child = children.iter().rev().find(|n| {
        !n.is_extra()
            && n.end_byte() <= ctx.offset
            && matches!(
                n.kind(),
                "method_invocation"
                    | "object_creation_expression"
                    | "field_access"
                    | "identifier"
                    | "this"
            )
    })?;

    let child_end = expr_child.end_byte();
    if child_end > ctx.source.len() {
        return None;
    }
    let after = &ctx.source[child_end..ctx.offset.min(ctx.source.len())];
    let after_trimmed = after.trim_start();

    if !after_trimmed.starts_with('.') {
        return None;
    }

    let receiver_expr = ctx.node_text(*expr_child).trim().to_string();
    if receiver_expr.is_empty() {
        return None;
    }

    let after_dot = after_trimmed[1..].trim_start();
    let member_prefix: String = after_dot
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();

    Some((
        CursorLocation::MemberAccess {
            receiver_semantic_type: None,
            receiver_type: None,
            member_prefix: member_prefix.clone(),
            receiver_expr,
            arguments: None,
        },
        member_prefix,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    use crate::language::test_helpers::completion_context_from_source;
    use crate::semantic::CursorLocation;

    fn location_at_offset(source: &str, offset: usize) -> CursorLocation {
        let rope = Rope::from_str(source);
        let line = rope.byte_to_line(offset) as u32;
        let col = (offset - rope.line_to_byte(line as usize)) as u32;
        completion_context_from_source("java", source, line, col, None).location
    }

    #[test]
    fn test_variable_name_after_simple_type_with_space() {
        let src = "String ";
        let offset = src.len();
        assert!(
            matches!(
                location_at_offset(src, offset),
                CursorLocation::VariableName { .. }
            ),
            "Should detect variable name position after 'String '"
        );
    }

    #[test]
    fn test_variable_name_after_generic_type_with_space() {
        let src = "List<String> ";
        let offset = src.len();
        assert!(
            matches!(
                location_at_offset(src, offset),
                CursorLocation::VariableName { .. }
            ),
            "Should detect variable name position after 'List<String> '"
        );
    }

    #[test]
    fn test_not_variable_name_without_space() {
        let src = "String";
        let offset = src.len();
        assert!(
            !matches!(
                location_at_offset(src, offset),
                CursorLocation::VariableName { .. }
            ),
            "Should NOT detect variable name position without space after 'String'"
        );
    }

    #[test]
    fn test_not_variable_name_after_array_initializer_without_space() {
        let src = "Object[] o = new Object[]{};\nArrayList";
        let offset = src.find("ArrayList").unwrap() + "ArrayList".len();
        assert!(
            matches!(
                location_at_offset(src, offset),
                CursorLocation::Expression { .. }
            ),
            "Should NOT detect variable name position without trailing space"
        );
    }

    #[test]
    fn test_not_variable_name_after_invalid_array_syntax_without_space() {
        let src = "Object[] o = new Object[];\nArrayList";
        let offset = src.find("ArrayList").unwrap() + "ArrayList".len();
        assert!(
            matches!(
                location_at_offset(src, offset),
                CursorLocation::Expression { .. }
            ),
            "Should NOT detect variable name position without trailing space"
        );
    }

    #[test]
    fn test_variable_name_after_type_with_space_in_error() {
        let src = "ArrayList ";
        let offset = src.len();
        assert!(
            matches!(
                location_at_offset(src, offset),
                CursorLocation::VariableName { .. }
            ),
            "Should detect variable name position after 'ArrayList ' in error context"
        );
    }

    #[test]
    fn test_not_variable_name_after_type_without_space_in_error() {
        let src = "ArrayList";
        let offset = src.len();
        assert!(
            !matches!(
                location_at_offset(src, offset),
                CursorLocation::VariableName { .. }
            ),
            "Should NOT detect variable name position without whitespace in error context"
        );
    }

    #[test]
    fn test_not_variable_name_when_semicolon_follows_identifier() {
        let src = "Object[] o = new Object[]{};\nArrayList ;";
        let offset = src.find("ArrayList").unwrap() + "ArrayList".len();
        assert!(
            matches!(
                location_at_offset(src, offset),
                CursorLocation::Expression { .. }
            ),
            "Should not treat `ArrayList ;` as variable name position"
        );
    }

    #[test]
    fn test_extract_last_expr_simple_identifier() {
        assert_eq!(
            extract_last_expr_from_text("class A { void f() { this"),
            Some("this".to_string())
        );
    }

    #[test]
    fn test_extract_last_expr_method_call() {
        assert_eq!(
            extract_last_expr_from_text("{ RealMain.getInstance()"),
            Some("RealMain.getInstance()".to_string())
        );
    }

    #[test]
    fn test_extract_last_expr_after_semicolon() {
        assert_eq!(
            extract_last_expr_from_text("int x = 1; obj"),
            Some("obj".to_string())
        );
    }

    #[test]
    fn test_detect_trailing_dot_simple() {
        assert_eq!(
            detect_trailing_dot_in_text("class A { void f() { cl."),
            Some(("cl".to_string(), String::new()))
        );
    }

    #[test]
    fn test_detect_trailing_dot_with_prefix() {
        assert_eq!(
            detect_trailing_dot_in_text("class A { void f() { a.p"),
            Some(("a".to_string(), "p".to_string()))
        );
    }

    #[test]
    fn test_detect_trailing_dot_with_this() {
        assert_eq!(
            detect_trailing_dot_in_text("class A { void f() { this."),
            Some(("this".to_string(), String::new()))
        );
    }

    #[test]
    fn test_detect_trailing_dot_with_call() {
        assert_eq!(
            detect_trailing_dot_in_text("class A { void f() { RealMain.getInstance()."),
            Some(("RealMain.getInstance()".to_string(), String::new()))
        );
    }

    #[test]
    fn test_detect_new_keyword_before_cursor_with_prefix() {
        assert_eq!(
            detect_new_keyword_before_cursor("class A { void f() { new RandomCla"),
            Some(("RandomCla".to_string(), None))
        );
    }

    #[test]
    fn test_detect_new_keyword_before_cursor_no_prefix() {
        assert_eq!(
            detect_new_keyword_before_cursor("class A { void f() { new "),
            Some((String::new(), None))
        );
    }

    #[test]
    fn test_detect_new_keyword_before_cursor_multiline() {
        assert_eq!(
            detect_new_keyword_before_cursor("class A { void f() { new\nFoo"),
            None
        );
    }
}
