use tree_sitter::Node;

use crate::{
    language::java::{
        JavaContextExtractor, SENTINEL,
        location::determine_location,
        scope::is_cursor_in_class_member_position,
        utils::{
            close_open_brackets, error_has_new_keyword, error_has_trailing_dot,
            find_error_ancestor, strip_sentinel, strip_sentinel_from_location,
        },
    },
    semantic::CursorLocation,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForceInjectionPredicates {
    pub location_is_type_like: bool,
    pub has_cursor_node: bool,
    pub in_error_context: bool,
    pub in_statement_context: bool,
    pub in_genuine_type_arguments: bool,
    pub dotted_member_tail: bool,
}

pub(crate) fn force_injection_predicates(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    location: &CursorLocation,
) -> ForceInjectionPredicates {
    let location_is_type_like = matches!(
        location,
        CursorLocation::TypeAnnotation { .. } | CursorLocation::VariableName { .. }
    );
    let has_cursor_node = cursor_node.is_some();
    let in_error_context = cursor_node
        .map(|n| n.kind() == "ERROR" || find_error_ancestor(n).is_some())
        .unwrap_or(false);
    let in_statement_context = cursor_node
        .map(is_in_statement_or_expression_context)
        .unwrap_or(false);
    let in_genuine_type_arguments = cursor_node
        .map(is_in_genuine_type_argument_context)
        .unwrap_or(false);
    let dotted_member_tail = has_dotted_member_like_tail(ctx);

    ForceInjectionPredicates {
        location_is_type_like,
        has_cursor_node,
        in_error_context,
        in_statement_context,
        in_genuine_type_arguments,
        dotted_member_tail,
    }
}

pub(crate) fn should_force_injection(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    location: &CursorLocation,
) -> bool {
    let p = force_injection_predicates(ctx, cursor_node, location);
    if !p.location_is_type_like {
        return false;
    }

    if !p.has_cursor_node {
        return false;
    }
    if !p.in_error_context && !p.in_statement_context {
        return false;
    }
    if p.in_genuine_type_arguments {
        return false;
    }
    p.dotted_member_tail
}

fn is_in_statement_or_expression_context(node: Node) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if matches!(
            n.kind(),
            "expression_statement"
                | "local_variable_declaration"
                | "method_invocation"
                | "field_access"
                | "assignment_expression"
                | "return_statement"
                | "block"
        ) {
            return true;
        }
        if matches!(
            n.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "annotation_type_declaration"
                | "program"
        ) {
            return false;
        }
        cur = n.parent();
    }
    false
}

fn is_in_genuine_type_argument_context(node: Node) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.kind() == "type_arguments" {
            return true;
        }
        if matches!(
            n.kind(),
            "method_declaration" | "class_declaration" | "program"
        ) {
            break;
        }
        cur = n.parent();
    }
    false
}

fn has_dotted_member_like_tail(ctx: &JavaContextExtractor) -> bool {
    if ctx.offset == 0 {
        return false;
    }
    let before = ctx.byte_slice(0, ctx.offset);
    let trimmed = strip_sentinel(before).trim_end().to_string();
    if trimmed.is_empty() {
        return false;
    }

    let tail = trimmed
        .rsplit(['\n', ';', '{', '}'])
        .next()
        .unwrap_or(trimmed.as_str())
        .trim();
    let dot = match tail.rfind('.') {
        Some(i) => i,
        None => return false,
    };
    let recv = tail[..dot].trim();
    let member = tail[dot + 1..].trim();
    if recv.is_empty() {
        return false;
    }
    if !member.is_empty()
        && !member
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return false;
    }
    recv.chars().all(|c| {
        c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == ')' || c == ']' || c == '('
    })
}

/// Construct the injected source: Replace the token at the cursor with SENTINEL (or insert it directly)
pub fn build_injected_source(
    extractor: &JavaContextExtractor,
    cursor_node: Option<Node>,
) -> String {
    let class_member_position = is_cursor_in_class_member_position(cursor_node);
    let before = extractor.byte_slice(0, extractor.offset);
    let trimmed = before.trim_end();

    // Case 1: cursor right after `new` keyword (bare `new` or `new `)
    if trimmed.ends_with("new") {
        let immediate_suffix = &extractor.source_str()[extractor.offset..];
        // Check if there are newlines before the next token
        let separated_by_newline = immediate_suffix
            .chars()
            .take_while(|c| c.is_whitespace())
            .any(|c| c == '\n' || c == '\r');

        let after = immediate_suffix.trim_start();
        let next_meaningful = after.chars().next();
        let is_continuation = next_meaningful.is_some_and(|c| c.is_alphanumeric() || c == '_');

        // If separated by newline, we assume the user stopped typing 'new ...'
        // and the next line is unrelated.
        if separated_by_newline || !is_continuation {
            return inject_at(
                extractor,
                extractor.offset,
                extractor.offset,
                &format!(" {SENTINEL}()"),
            );
        }
    }

    // Case 2/3: cursor inside an ERROR node
    let error_node = cursor_node.and_then(|n| {
        if n.kind() == "ERROR" {
            Some(n)
        } else {
            find_error_ancestor(n)
        }
    });

    if let Some(err) = error_node {
        let cursor_local_anchor = cursor_node.and_then(|n| {
            if !(n.kind() == "identifier" || n.kind() == "type_identifier") {
                return None;
            }
            if n.start_byte() >= extractor.offset {
                return None;
            }
            if !is_descendant_of(n, err) {
                return None;
            }
            Some(n)
        });

        // Case 2: constructor recovery in ERROR context.
        // Accept either an explicit constructor AST context, or a cursor-local `new ...`
        // token before the cursor inside this ERROR span.
        if error_has_new_keyword(err)
            && (is_cursor_in_constructor_context(cursor_node)
                || error_has_new_before_cursor(extractor, err))
        {
            if let Some(id_node) = cursor_local_anchor
                .or_else(|| find_identifier_in_error_before_offset(err, extractor.offset))
            {
                let start = id_node.start_byte();
                if start > extractor.offset {
                    return inject_at(
                        extractor,
                        extractor.offset,
                        extractor.offset,
                        &format!(" {SENTINEL}()"),
                    );
                }
                let end = extractor.offset.min(id_node.end_byte());
                if end > start {
                    let prefix = &extractor.source[start..end];
                    return inject_at(
                        extractor,
                        start,
                        id_node.end_byte(),
                        &format!("{prefix}{SENTINEL}()"),
                    );
                }
            }
            return inject_at(
                extractor,
                extractor.offset,
                extractor.offset,
                &format!(" {SENTINEL}()"),
            );
        }

        // Case 3: ERROR has trailing dot (e.g. `cl.`)
        if error_has_trailing_dot(err, extractor.offset) {
            let terminator = if requires_semicolon(cursor_node) {
                ";"
            } else {
                ""
            };
            return inject_at(
                extractor,
                extractor.offset,
                extractor.offset,
                &format!("{SENTINEL}{terminator}"),
            );
        }
    }

    // Case 4: normal identifier/type_identifier — append sentinel
    let (replace_start, replace_end) = match cursor_node {
        Some(n)
            if (n.kind() == "identifier" || n.kind() == "type_identifier")
                && n.start_byte() < extractor.offset =>
        {
            (n.start_byte(), extractor.offset.min(n.end_byte()))
        }
        _ => (extractor.offset, extractor.offset),
    };

    let terminator = if requires_semicolon(cursor_node) {
        ";"
    } else {
        ""
    };

    if replace_start == replace_end {
        if class_member_position {
            // In class/interface/enum member position, injecting a bare identifier statement
            // (`__KIRO__;`) produces ERROR and collapses location to Unknown.
            // Inject a syntactically valid member declaration to keep AST classification stable.
            return inject_at(
                extractor,
                extractor.offset,
                extractor.offset,
                &format!("void {SENTINEL}() {{}}"),
            );
        }
        // Nothing at cursor, insert sentinel
        inject_at(
            extractor,
            extractor.offset,
            extractor.offset,
            &format!("{SENTINEL}{terminator}"),
        )
    } else {
        let prefix = &extractor.source[replace_start..replace_end];
        if class_member_position {
            // In member declaration positions, keep injection as declaration-shaped,
            // not expression/method-call shaped, otherwise parser recovers to ERROR.
            return inject_at(
                extractor,
                replace_start,
                replace_end,
                &format!("void {prefix}{SENTINEL}() {{}}"),
            );
        }
        // cursor in scoped_type_identifier (e.g., java.util.A) -> add semicolon to turn into expression statement
        // let in_scoped_type = cursor_node.is_some_and(|n| {
        //     find_ancestor(n, "scoped_type_identifier").is_some()
        //         || n.kind() == "scoped_type_identifier"
        // });

        // Check if we are inside a `new` expression to inject parentheses
        // This helps tree-sitter terminate the object_creation_expression correctly
        // instead of swallowing the next lines into the type node.
        let is_constructor_ctx = cursor_node.is_some_and(|n| {
            let mut curr = Some(n);
            while let Some(node) = curr {
                if node.kind() == "object_creation_expression" {
                    return true;
                }
                if node.kind() == "method_declaration"
                    || node.kind() == "class_declaration"
                    || node.kind() == "block"
                {
                    break;
                }
                curr = node.parent();
            }
            false
        });

        let suffix = if is_constructor_ctx || token_before_offset_is_new(extractor, replace_start) {
            "()"
        } else {
            ""
        };

        inject_at(
            extractor,
            replace_start,
            replace_end,
            &format!("{prefix}{SENTINEL}{suffix}{terminator}"),
        )
    }
}

fn is_cursor_in_constructor_context(cursor_node: Option<Node>) -> bool {
    let Some(n) = cursor_node else {
        return false;
    };
    let mut cur = Some(n);
    while let Some(node) = cur {
        if node.kind() == "object_creation_expression" {
            return true;
        }
        if matches!(
            node.kind(),
            "method_declaration" | "class_declaration" | "program"
        ) {
            break;
        }
        cur = node.parent();
    }
    false
}

fn error_has_new_before_cursor(extractor: &JavaContextExtractor, err: Node) -> bool {
    let start = err.start_byte();
    let end = err.end_byte().min(extractor.offset);
    if end <= start {
        return false;
    }
    contains_new_token(&extractor.source_str()[start..end])
}

fn token_before_offset_is_new(extractor: &JavaContextExtractor, offset: usize) -> bool {
    if offset == 0 {
        return false;
    }
    let prefix = extractor.byte_slice(0, offset);
    contains_new_token(prefix)
}

fn contains_new_token(s: &str) -> bool {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|tok| tok == "new")
}

fn find_identifier_in_error_before_offset<'a>(err: Node<'a>, offset: usize) -> Option<Node<'a>> {
    fn visit<'a>(node: Node<'a>, offset: usize, best: &mut Option<Node<'a>>) {
        if (node.kind() == "identifier" || node.kind() == "type_identifier")
            && node.start_byte() <= offset
        {
            if let Some(prev) = best {
                if node.start_byte() > prev.start_byte() {
                    *best = Some(node);
                }
            } else {
                *best = Some(node);
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit(child, offset, best);
        }
    }

    let mut best = None;
    visit(err, offset, &mut best);
    best
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

fn inject_at(
    extractor: &JavaContextExtractor,
    replace_start: usize,
    replace_end: usize,
    replacement: &str,
) -> String {
    let mut s = String::with_capacity(extractor.source.len() + replacement.len() + 16);
    s.push_str(extractor.byte_slice(0, replace_start));
    s.push_str(replacement);
    s.push_str(&extractor.source_str()[replace_end..]);
    let tail = close_open_brackets(&s);
    s.push_str(&tail);
    s
}

/// Inject SENTINEL, reparse, and run determine_location on the new tree.
pub fn inject_and_determine(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    trigger_char: Option<char>,
) -> Option<(CursorLocation, String)> {
    let injected_source = build_injected_source(ctx, cursor_node);
    let sentinel_offset = injected_source.find(SENTINEL)?;
    let sentinel_end = sentinel_offset + SENTINEL.len();

    let mut parser = ctx.make_parser();
    let new_tree = parser.parse(&injected_source, None)?;
    let new_root = new_tree.root_node();

    let sentinel_node = new_root.named_descendant_for_byte_range(sentinel_offset, sentinel_end)?;

    tracing::debug!(
        injected_source = injected_source,
        sentinel_node_kind = sentinel_node.kind(),
        "inject_and_determine"
    );

    if sentinel_node.kind() == "identifier" || sentinel_node.kind() == "type_identifier" {
        let tmp = JavaContextExtractor::new(
            injected_source.clone(),
            sentinel_end,
            ctx.name_table.clone(),
        );
        let (loc, q) = determine_location(&tmp, Some(sentinel_node), trigger_char);
        let clean_q = if q == SENTINEL {
            String::new()
        } else {
            strip_sentinel(&q)
        };
        let clean_loc = strip_sentinel_from_location(loc);
        return Some((clean_loc, clean_q));
    }

    let mut cur = sentinel_node;
    loop {
        if cur.kind() == "identifier" || cur.kind() == "type_identifier" {
            let tmp = JavaContextExtractor::new(
                injected_source.clone(),
                sentinel_end,
                ctx.name_table.clone(),
            );
            let (loc, q) = determine_location(&tmp, Some(cur), trigger_char);
            let clean_q = strip_sentinel(&q);
            let clean_loc = strip_sentinel_from_location(loc);
            return Some((clean_loc, clean_q));
        }
        cur = cur.parent()?;
        if cur.kind() == "method_declaration" || cur.kind() == "program" {
            break;
        }
    }

    None
}

fn requires_semicolon(cursor_node: Option<Node>) -> bool {
    let Some(n) = cursor_node else {
        return true;
    };
    let mut curr = Some(n);
    while let Some(node) = curr {
        let kind = node.kind();
        if matches!(
            kind,
            "formal_parameters"
                | "argument_list"
                | "type_arguments"
                | "condition"
                | "for_statement"
                | "while_statement"
                | "catch_clause"
                | "parenthesized_expression"
                | "array_access"
                | "array_initializer"
        ) {
            return false;
        }

        if matches!(kind, "block" | "class_body" | "program") {
            break;
        }
        curr = node.parent();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn setup_ctx(source: &str, offset: usize) -> (JavaContextExtractor, tree_sitter::Tree) {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("failed to load java grammar");
        let tree = parser.parse(source, None).unwrap();

        let ctx = JavaContextExtractor::new(source, offset, None);
        (ctx, tree)
    }

    #[test]
    fn test_build_injected_source_trailing_dot() {
        // Case 3: ERROR node with trailing dot
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                cl.
            }
        }
        "#};
        // Find the end of `cl.`
        let offset = src.find("cl.").unwrap() + 3;
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let injected = build_injected_source(&ctx, cursor_node);

        // Expected behavior: Inject __KIRO__ after the dot and add a semicolon to close the statement,
        // and close_open_brackets will automatically complete the missing `}`.
        assert!(
            injected.contains(&format!("cl.{SENTINEL};")),
            "Injected source should contain sentinel and semicolon, got:\n{}",
            injected
        );
    }

    #[test]
    fn test_build_injected_source_new_keyword_with_newline() {
        // Case 1: A newline after the `new` keyword causes the AST to swallow the next line.
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new 
                
                System.out.println("hello");
            }
        }
        "#};
        let offset = src.find("new ").unwrap() + 4;
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let injected = build_injected_source(&ctx, cursor_node);

        // Expectation: Because a newline character follows `new`, it's inferred that the user hasn't finished writing the code, so `__KIRO__()` is injected directly as an empty constructor.
        assert!(
            injected.contains(&format!("new  {SENTINEL}()")),
            "Injected source should treat separated new as complete call, got:\n{}",
            injected
        );
    }

    #[test]
    fn test_build_injected_source_normal_identifier() {
        // Case 4: Ordinary Identifiers
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                myVa
            }
        }
        "#};
        let offset = src.find("myVa").unwrap() + 4;
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let injected = build_injected_source(&ctx, cursor_node);

        // Expectation: Since there is no suffix, treat it as an expression statement and inject a semicolon.
        assert!(
            injected.contains(&format!("myVa{SENTINEL}")),
            "Injected source should append sentinel to identifier, got:\n{}",
            injected
        );
    }

    #[test]
    fn test_build_injected_source_scoped_type() {
        // Case 4: scoped_type_identifier (FQN)
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                java.util.A
            }
        }
        "#};
        let offset = src.find("java.util.A").unwrap() + 11;
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let injected = build_injected_source(&ctx, cursor_node);

        // Expectation: When injecting package name types, add a semicolon to convert it to a valid MemberAccess.
        assert!(
            injected.contains(&format!("java.util.A{SENTINEL};")),
            "Injected source should append semicolon for scoped types, got:\n{}",
            injected
        );
    }

    #[test]
    /// Verify whether the CursorLocation can actually be resolved after injection.
    fn test_inject_and_determine_member_access() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                cl.
            }
        }
        "#};
        let offset = src.find("cl.").unwrap() + 3;
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let result = inject_and_determine(&ctx, cursor_node, None);
        assert!(result.is_some(), "inject_and_determine should succeed");

        let (location, query) = result.unwrap();

        // Expected result: MemberAccess, receiver_expr is "cl", and query is empty.
        assert!(
            matches!(
                &location,
                CursorLocation::MemberAccess { member_prefix, receiver_expr, .. }
                if member_prefix.is_empty() && receiver_expr == "cl"
            ),
            "Expected MemberAccess for cl., got {:?}",
            location
        );
        assert_eq!(query, "", "Query should be empty");
    }

    #[test]
    fn test_inject_and_determine_constructor() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new RandomCla
            }
        }
        "#};
        let offset = src.find("RandomCla").unwrap() + 9;
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let result = inject_and_determine(&ctx, cursor_node, None);
        assert!(result.is_some(), "inject_and_determine should succeed");

        let (location, query) = result.unwrap();

        assert!(
            matches!(
                &location,
                CursorLocation::ConstructorCall { class_prefix, .. }
                if class_prefix == "RandomCla"
            ),
            "Expected ConstructorCall with prefix RandomCla, got {:?}",
            location
        );
        assert_eq!(query, "RandomCla", "Query should match the prefix");
    }

    #[test]
    fn test_inject_and_determine_plain_identifier() {
        let src = indoc::indoc! {r#"
    class A {
        public static void func() {}
        void f() {
            func
        }
    }
    "#};
        let offset = src.find("func\n").unwrap() + 4; // 指向方法体内的 func
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let result = inject_and_determine(&ctx, cursor_node, None);
        assert!(
            result.is_some(),
            "inject_and_determine should succeed for plain identifier"
        );

        let (location, query) = result.unwrap();
        assert_eq!(query, "func");
        // 应当解析为某种可补全的上下文（如 Identifier / MethodCall）
        println!("location: {:?}", location);
    }

    #[test]
    fn test_build_injected_source_in_parameters() {
        let src = indoc::indoc! {r#"
        class A {
            public static void func(Str) {
            }
        }
        "#};
        let offset = src.find("Str").unwrap() + 3;
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let injected = build_injected_source(&ctx, cursor_node);

        // 预期：位于 parameters 中，不应该注入分号导致 AST 解析错误
        assert!(!injected.contains(&format!("Str{SENTINEL};")));
        assert!(injected.contains(&format!("Str{SENTINEL}")));
    }

    #[test]
    fn test_inject_and_determine_in_parameters() {
        let src = indoc::indoc! {r#"
        class A {
            public static void func(Str) {
            }
        }
        "#};
        let offset = src.find("Str").unwrap() + 3;
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let result = inject_and_determine(&ctx, cursor_node, None);
        assert!(
            result.is_some(),
            "inject_and_determine should succeed in parameters"
        );

        let (location, query) = result.unwrap();

        // 预期：被正确识别为参数列表中的类型注解补全
        assert!(
            matches!(location, CursorLocation::TypeAnnotation { .. }),
            "Expected TypeAnnotation, got {:?}",
            location
        );
        assert_eq!(query, "Str");
    }

    #[test]
    fn test_inject_and_determine_class_body_slot_after_nested_class_is_not_unknown() {
        let src_with_cursor = indoc::indoc! {r#"
        public class VarargsExample {
            public static class Test implements Runnable {
                @Override
                public void run() {
                    throw new RuntimeException("Not implemented yet");
                }
            }

            // cursor here
            |
        }
        "#};
        let offset = src_with_cursor.find('|').expect("cursor marker");
        let src = src_with_cursor.replacen('|', "", 1);
        let (ctx, tree) = setup_ctx(&src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let injected = build_injected_source(&ctx, cursor_node);

        assert!(
            injected.contains(&format!("void {SENTINEL}() {{}}")),
            "class-body injection should use valid member declaration, got:\n{}",
            injected
        );

        let result = inject_and_determine(&ctx, cursor_node, None)
            .expect("inject_and_determine should succeed in class member slot");
        let (location, query) = result;

        assert!(
            !matches!(location, CursorLocation::Unknown),
            "class-body slot after nested class must not be Unknown: {:?}",
            location
        );
        assert_eq!(query, "");
    }

    #[test]
    fn test_snapshot_injection_class_body_slot_after_nested_class() {
        let src_with_cursor = indoc::indoc! {r#"
        public class VarargsExample {
            public static class Test implements Runnable {
                @Override
                public void run() {
                    throw new RuntimeException("Not implemented yet");
                }
            }

            // cursor here
            |
        }
        "#};
        let offset = src_with_cursor.find('|').expect("cursor marker");
        let src = src_with_cursor.replacen('|', "", 1);
        let (ctx, tree) = setup_ctx(&src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let injected = build_injected_source(&ctx, cursor_node);
        let result = inject_and_determine(&ctx, cursor_node, None);
        let out = format!(
            "cursor_node_kind={:?}\ninjected_source=\n{}\nresult={:?}\n",
            cursor_node.map(|n| n.kind().to_string()),
            injected,
            result
        );
        insta::assert_snapshot!("injection_class_body_slot_after_nested_class", out);
    }

    #[test]
    fn test_inject_and_determine_class_body_partial_member_prefix_not_unknown() {
        let src_with_cursor = indoc::indoc! {r#"
        public class A {
            prote|
        }
        "#};
        let offset = src_with_cursor.find('|').expect("cursor marker");
        let src = src_with_cursor.replacen('|', "", 1);
        let (ctx, tree) = setup_ctx(&src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let injected = build_injected_source(&ctx, cursor_node);

        assert!(
            injected.contains(&format!("void prote{SENTINEL}() {{}}")),
            "class-body partial declaration should use declaration-shaped injection, got:\n{}",
            injected
        );
        assert!(
            !injected.contains(&format!("prote{SENTINEL}();")),
            "must not inject expression-shaped call in class body: {}",
            injected
        );

        let (loc, query) = inject_and_determine(&ctx, cursor_node, None)
            .expect("inject_and_determine should succeed in class member prefix");
        assert!(!matches!(loc, CursorLocation::Unknown), "got {:?}", loc);
        assert_eq!(query, "prote");
    }

    #[test]
    fn test_snapshot_injection_class_body_partial_prefix_after_nested_class() {
        let src_with_cursor = indoc::indoc! {r#"
        public class A {
            class B {}
            prote|
        }
        "#};
        let offset = src_with_cursor.find('|').expect("cursor marker");
        let src = src_with_cursor.replacen('|', "", 1);
        let (ctx, tree) = setup_ctx(&src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let injected = build_injected_source(&ctx, cursor_node);
        let result = inject_and_determine(&ctx, cursor_node, None);
        let out = format!(
            "cursor_node_kind={:?}\ninjected_source=\n{}\nresult={:?}\n",
            cursor_node.map(|n| n.kind().to_string()),
            injected,
            result
        );
        insta::assert_snapshot!(
            "injection_class_body_partial_prefix_after_nested_class",
            out
        );
    }

    #[test]
    fn test_snapshot_injection_anchor_drift_for_member_tail_with_later_generics() {
        let src = indoc::indoc! {r#"
        class Demo {
            void f() {
                var a = new HashMap<String, String>();
                a.p
                List<Box<? extends Number>> nums = List.of(
                    new Box<>(1),
                    new Box<>(2.5)
                );
                nums.add();
            }
        }
        "#};
        let marker = "a.p";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_ctx(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let cursor_info = cursor_node.map(|n| {
            format!(
                "{}:[{}..{}]:'{}'",
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                ctx.node_text(n)
            )
        });

        let err = cursor_node.and_then(|n| {
            if n.kind() == "ERROR" {
                Some(n)
            } else {
                find_error_ancestor(n)
            }
        });
        let err_info = err.map(|n| {
            format!(
                "{}:[{}..{}]:'{}'",
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                ctx.node_text(n)
            )
        });
        let err_has_new = err.map(error_has_new_keyword).unwrap_or(false);
        let chosen_anchor = err.and_then(|n| find_identifier_in_error_before_offset(n, offset));
        assert!(
            chosen_anchor
                .map(|n| n.start_byte() <= offset)
                .unwrap_or(true),
            "chosen anchor must not start after cursor offset"
        );
        let anchor_info = chosen_anchor.map(|n| {
            format!(
                "{}:[{}..{}]:'{}'",
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                ctx.node_text(n)
            )
        });

        let injected = build_injected_source(&ctx, cursor_node);
        let out = format!(
            "offset={offset}\ncursor_node={cursor_info:?}\nerror_node={err_info:?}\nerror_has_new={err_has_new}\nchosen_anchor={anchor_info:?}\ninjected_source=\n{injected}\n"
        );
        insta::assert_snapshot!(
            "injection_anchor_drift_member_tail_with_later_generics",
            out
        );
    }
}
