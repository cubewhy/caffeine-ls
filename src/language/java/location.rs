use std::sync::Arc;

use crate::language::java::utils::{
    find_ancestor, find_string_ancestor, is_comment_kind, is_in_name_position, is_in_type_position,
};
use tree_sitter::Node;

use crate::{
    language::java::{JavaContextExtractor, utils::strip_sentinel},
    semantic::CursorLocation,
};

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

fn determine_location_impl(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    trigger_char: Option<char>,
) -> (CursorLocation, String) {
    let node = match cursor_node {
        Some(n) => n,
        None => return (CursorLocation::Unknown, String::new()),
    };
    let mut current = node;
    if let Some(str_node) = find_string_ancestor(node) {
        let prefix = extract_string_prefix(ctx, str_node);
        return (
            CursorLocation::StringLiteral {
                prefix: prefix.clone(),
            },
            prefix,
        );
    }
    loop {
        match current.kind() {
            "marker_annotation" | "annotation" => {
                let name_node = current.child_by_field_name("name");
                let prefix = name_node
                    .map(|n| cursor_truncated_text(ctx, n))
                    .unwrap_or_default();
                return (
                    CursorLocation::Annotation {
                        prefix: prefix.clone(),
                        target_element_type: infer_annotation_target(current),
                    },
                    prefix,
                );
            }
            "import_declaration" => return handle_import(ctx, current),
            "method_invocation" => return handle_member_access(ctx, current),
            "field_access" => return handle_member_access(ctx, current),
            "method_reference" => return handle_method_reference(ctx, current),
            "object_creation_expression" => return handle_constructor(ctx, current),
            "argument_list" => return handle_argument_list(ctx, current),
            "identifier" | "type_identifier" => {
                return handle_identifier(ctx, current, trigger_char);
            }
            _ => {}
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }
    (CursorLocation::Unknown, String::new())
}

fn infer_annotation_target(node: Node) -> Option<Arc<str>> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        let et = match n.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "annotation_type_declaration" => "TYPE",
            "record_declaration" => "TYPE",
            // record_component = record header 里的参数
            "formal_parameter" if n.parent().map(|p| p.kind()) == Some("record_declaration") => {
                "RECORD_COMPONENT"
            }
            "method_declaration" => "METHOD",
            "field_declaration" => "FIELD",
            "formal_parameter" => "PARAMETER",
            "constructor_declaration" => "CONSTRUCTOR",
            "local_variable_declaration" => "LOCAL_VARIABLE",
            _ => {
                cur = n.parent();
                continue;
            }
        };
        return Some(Arc::from(et));
    }
    None
}

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
            // raw looks like: "\"abc" or "\"abc\""
            // want: "abc" truncated at cursor, without quotes
            // find first quote in raw, then take after it
            if let Some(pos) = raw.find('"') {
                let after = &raw[pos + 1..];
                // 如果 cursor 已经过了 closing quote（少见但可能，比如点到末尾），剥掉末尾 quote
                return after.strip_suffix('"').unwrap_or(after).to_string();
            }
            String::new()
        }
        "text_block" => {
            // raw looks like: "\"\"\" ...", want content without the leading """
            // 这里先做简单处理：去掉开头的 """（以及可能的首个换行）
            let s = raw.to_string();
            if let Some(rest) = s.strip_prefix("\"\"\"") {
                let mut rest = rest;
                // Java text block 通常允许紧跟一个换行
                if rest.starts_with('\n') {
                    rest = &rest[1..];
                }
                return rest.to_string();
            }
            // fallback
            s
        }
        _ => raw.to_string(),
    }
}

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
    // collect_import_text has automatically skipped the "import" and "static" keywords.
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

/// Recursively collect valid text (identifiers, dots, asterisks) in import declarations
/// Skip import keywords, static keywords, semicolons, and all types of comments
fn collect_import_text(ctx: &JavaContextExtractor, node: Node, out: &mut String) {
    // If the node's starting position is already after the cursor, skip it (because we need the part before the cursor).
    if node.start_byte() >= ctx.offset {
        return;
    }

    // If there are no child nodes, it means it is a leaf node (Token)
    if node.child_count() == 0 {
        let kind = node.kind();
        if kind == "import" || kind == "static" || kind == ";" || is_comment_kind(kind) {
            return;
        }

        // Get the text truncated to the cursor position
        // Note: cursor_truncated_text internally processes node.end_byte().min(ctx.offset)
        let text = cursor_truncated_text(ctx, node);
        out.push_str(&text);
    } else {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_import_text(ctx, child, out);
        }
    }
}

fn handle_member_access(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
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
                        receiver_expr: String::new(), // 空字符串代表隐式 this
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
        let receiver_node = if dot_pos > 0 {
            Some(children[dot_pos - 1])
        } else {
            None
        };
        let prefix = receiver_node
            .map(|n| {
                let text = cursor_truncated_text(ctx, n);
                strip_sentinel(&text)
            })
            .unwrap_or_default();
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
        // This path won't be reached if the path is injected; in non-injection paths, there's nothing after `dot`.
        None => String::new(),
    };

    let receiver_node = if dot_pos > 0 {
        Some(children[dot_pos - 1])
    } else {
        None
    };
    let receiver_expr = receiver_node
        .map(|n| ctx.node_text(n).to_string())
        .unwrap_or_default();

    // Check if this member access is part of a method invocation to extract arguments
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

fn handle_constructor(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
    let type_node = node.child_by_field_name("type");

    // If the type node starts after the cursor and is separated by a newline,
    // it means tree-sitter consumed the next line as the type.
    // We reject this and return Unknown to trigger the injection path.
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
            // Use cursor_truncated_text so we don't capture text *after* the cursor
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

pub(crate) fn normalize_top_level_generic_base(raw: &str) -> &str {
    let trimmed = raw.trim();
    let mut angle_depth = 0i32;
    for (i, c) in trimmed.char_indices() {
        match c {
            '<' if angle_depth == 0 => return trimmed[..i].trim_end(),
            '<' => angle_depth += 1,
            '>' => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                }
            }
            _ => {}
        }
    }
    trimmed
}

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
                let name_node = ancestor.child_by_field_name("name");
                let prefix = name_node
                    .map(|n| cursor_truncated_text(ctx, n))
                    .unwrap_or_default();
                return (
                    CursorLocation::Annotation {
                        prefix: prefix.clone(),
                        target_element_type: infer_annotation_target(ancestor),
                    },
                    prefix,
                );
            }
            "field_access" | "method_invocation" => {
                return handle_member_access(ctx, ancestor);
            }
            "method_reference" => {
                return handle_method_reference(ctx, ancestor);
            }
            "import_declaration" => return handle_import(ctx, ancestor),
            "object_creation_expression" => return handle_constructor(ctx, ancestor),
            "local_variable_declaration" => {
                // Detect misreading: If the next sibling begins with `(`,
                // this indicates that `str\nfunc(...)` was misread as local_variable_declaration,
                // the "type" of the cursor is actually an expression.
                if let Some(next) = ancestor.next_sibling() {
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

                if is_in_type_position(node, ancestor) {
                    let text = cursor_truncated_text(ctx, node);
                    return (
                        CursorLocation::TypeAnnotation {
                            prefix: text.clone(),
                        },
                        text,
                    );
                }

                if is_in_name_position(node, ancestor) {
                    let type_name = extract_type_from_decl(ctx, ancestor);
                    return (CursorLocation::VariableName { type_name }, String::new());
                }

                if is_in_type_subtree(node, ancestor) {
                    // 取光标前的文本作为 prefix，但只取最后一段（点后面的部分）
                    // let text = cursor_truncated_text(ctx, node);
                    // let clean = strip_sentinel(&text);
                    // 触发注入路径来正确识别
                    return (CursorLocation::Unknown, String::new());
                }
                let text = cursor_truncated_text(ctx, node);
                let clean = strip_sentinel(&text);
                return (
                    CursorLocation::Expression {
                        prefix: clean.clone(),
                    },
                    clean,
                );
            }
            "formal_parameter" => {
                if is_in_formal_param_name_position(node, ancestor) {
                    let type_name = ancestor
                        .child_by_field_name("type")
                        .map(|n| ctx.node_text(n).trim().to_string())
                        .unwrap_or_default();
                    return (CursorLocation::VariableName { type_name }, String::new());
                }

                // type position
                let text = cursor_truncated_text(ctx, node);
                return (
                    CursorLocation::TypeAnnotation {
                        prefix: text.clone(),
                    },
                    text,
                );
            }
            "argument_list" => {
                if let Some(parent) = node.parent()
                    && parent.kind() == "method_invocation"
                    && ctx.offset <= ancestor.start_byte()
                {
                    return handle_member_access(ctx, parent);
                }
                return handle_argument_list(ctx, ancestor);
            }
            // If inside ERROR node, return Unknown to trigger injection path
            "ERROR" => return (CursorLocation::Unknown, String::new()),
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

fn is_in_formal_param_name_position(id_node: Node, param_node: Node) -> bool {
    param_node
        .child_by_field_name("name")
        .is_some_and(|n| n.id() == id_node.id())
}

fn handle_argument_list(ctx: &JavaContextExtractor, node: Node) -> (CursorLocation, String) {
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

    // func(a, |)
    if is_fresh_argument_position(ctx, node) {
        return (
            CursorLocation::MethodArgument {
                prefix: String::new(),
            },
            String::new(),
        );
    }

    // Find the nearest identifier node before the cursor in the argument_list as the prefix
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
        let child_end = child.end_byte();
        // The child must end before the cursor.
        if child_end > ctx.offset {
            continue;
        }
        // Check if the text between the end of child and the cursor (after removing sentinel) begins with a '.'.
        let gap = &ctx.source[child_end..ctx.offset];
        let gap_clean = strip_sentinel(gap);
        let gap_trimmed = gap_clean.trim_start();
        if let Some(stripped) = gap_trimmed.strip_prefix('.') {
            let receiver_expr = ctx.source[child.start_byte()..child_end].to_string();
            // The member name may have been partially typed after the '.'
            let after_dot = stripped.trim_start();
            let member_prefix = strip_sentinel(after_dot).trim_end().to_string();
            return Some((receiver_expr, member_prefix));
        }
    }
    None
}

fn find_prefix_in_argument_list(ctx: &JavaContextExtractor, arg_list: Node) -> String {
    let mut cursor = arg_list.walk();
    for child in arg_list.named_children(&mut cursor) {
        if child.start_byte() <= ctx.offset && child.end_byte() >= ctx.offset.saturating_sub(1) {
            let text = cursor_truncated_text(ctx, child);
            let clean = strip_sentinel(&text);

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

fn cursor_truncated_text(ctx: &JavaContextExtractor, node: Node) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(ctx.offset);
    if end <= start {
        return String::new();
    }
    ctx.byte_slice(start, end).to_string()
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
    // Find the type child node of decl_node and check if id_node is in its descendants.
    let mut walker = decl_node.walk();
    for child in decl_node.named_children(&mut walker) {
        if child.kind() == "modifiers" {
            continue;
        }
        // The first non-modifiers child node is a type node
        return is_descendant_of(id_node, child);
    }
    false
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

#[cfg(test)]
mod tests {
    use tree_sitter::Parser;

    use crate::{
        language::java::{JavaContextExtractor, location::determine_location},
        semantic::CursorLocation,
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
        // str 缺分号，TS 把 `str\nfunc(...)` 误读为
        // local_variable_declaration(type=str, declarator=func)
        // cursor 在 str 末尾时应该得到 Expression 而非 TypeAnnotation
        let src = indoc::indoc! {r#"
    class A {
        public static String str = "1234";
        public static void func() {
            str
            func(func("1234", 5678));
        }
    }
    "#};
        // offset 紧贴 str 末尾（方法体内那个 str，不是字段声明里的）
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
    fn test_genuine_type_annotation_in_local_decl() {
        // 正常的 local_variable_declaration，cursor 在 type 上，不能被误读检测误伤
        // 用 `HashM` 作为类型前缀，后面跟 `map =` 确保 TS 解析为 local_variable_declaration
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            HashM map = null;
        }
    }
    "#};
        let offset = src.find("HashM").unwrap() + 5; // 紧贴 HashM 末尾
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
    fn test_misread_not_triggered_when_next_sibling_is_normal_statement() {
        // next sibling 不以 `(` 开头时，误读检测不应触发
        // 保证普通 local_variable_declaration 里 type 位置的补全正常
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
        // 当光标在 receiver 的内部时（例如 stri|ngs.addAll();）
        // 解析器不应该返回 MemberAccess，而应将其视为普通的 Expression
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            List<String> strings = new ArrayList<>();
            strings.addAll();
        }
    }
    "#};
        // 计算光标在 "stri|ngs" 中间的位置
        let marker = "strings.addAll";
        let offset = src.find(marker).unwrap() + 4; // 长度正好到 "stri"
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
        // 当光标紧贴在点之前（例如 strings|.addAll();）
        // 用户依然是在完成 receiver，应该视作 Expression
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            List<String> strings = new ArrayList<>();
            strings.addAll();
        }
    }
    "#};
        let marker = "strings.addAll";
        let offset = src.find(marker).unwrap() + 7; // 光标在 "strings" 后，'.' 的前面
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
        // 光标在 '.' 之后，')' 之前
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
    fn test_member_access_with_partial_member_in_arg_list() {
        // println(matrix[0][1].toS|) — 已打了部分 member 名称
        // 此时 tree-sitter 大概率能生成 field_access，走已有路径；
        // 此测试确保结果仍然正确，不被新逻辑干扰
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
        // 普通方法参数补全不应被新逻辑误伤
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
    fn test_annotation_string_literal_location() {
        let src = r#"class A {
        @SuppressWarnings("not parsed as string lol")
        void f() {}
    }"#;

        // cursor 放在 string 中间，比如 "not parsed| ..."
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
}
