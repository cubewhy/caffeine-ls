mod block;
mod error;
mod handlers;
mod heuristics;
mod hints;
mod utils;

pub(crate) use hints::infer_functional_target_hint;

use crate::language::java::JavaContextExtractor;
use crate::language::java::location::utils::is_member_part_of_scoped_type;
use crate::language::java::utils::{find_string_ancestor, is_comment_kind};
use crate::semantic::CursorLocation;
use tree_sitter::Node;
use tree_sitter_utils::Handler;
use tree_sitter_utils::{HandlerExt, Input, handler_fn};

pub(crate) fn determine_location(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    trigger_char: Option<char>,
) -> (CursorLocation, String) {
    let (loc, query) = determine_location_impl(ctx, cursor_node, trigger_char);
    if utils::location_has_newline(&loc) {
        return (CursorLocation::Unknown, String::new());
    }
    (loc, query)
}

fn determine_location_impl(
    ctx: &JavaContextExtractor,
    cursor_node: Option<Node>,
    trigger_char: Option<char>,
) -> (CursorLocation, String) {
    let Some(node) = cursor_node else {
        // cursor is in blank area (e.g., after comment, at end of file)
        if let Some(type_name) = heuristics::detect_variable_name_after_type_text(ctx) {
            return (CursorLocation::VariableName { type_name }, String::new());
        }
        return (
            CursorLocation::Expression {
                prefix: String::new(),
            },
            String::new(),
        );
    };

    if is_comment_kind(node.kind()) {
        return (CursorLocation::Unknown, String::new());
    }

    if let Some(str_node) = find_string_ancestor(node) {
        let prefix = handlers::extract_string_prefix(ctx, str_node);
        return (
            CursorLocation::StringLiteral {
                prefix: prefix.clone(),
            },
            prefix,
        );
    }

    if let Some(result) = hints::detect_statement_label_location(ctx, node) {
        return result;
    }

    // Build a combinator chain that dispatches on node kind and climbs.
    // Each handler arm returns `Option<(CursorLocation, String)>`; `.or()`
    // tries the next arm on `None`; `.climb(stop_kinds)` walks to the parent
    // when the whole chain returns `None` for the current node.
    //
    // Stop kinds are the container boundaries where climbing must halt.
    let location_handler =
        // jump statements: only fire when the handler succeeds; on failure the
        // whole climbing stops (break semantics reproduced by returning None
        // AND listing the kinds in stop_kinds below).
        (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
            let (ctx, _trigger_char, _orig) = inp.ctx;
            handlers::handle_jump_statement(ctx, inp.node)
        })
        .for_kinds(&["break_statement", "continue_statement"])
        .or(
            handler_fn(|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                handlers::handle_annotation(ctx, inp.node)
            })
            .for_kinds(&["marker_annotation", "annotation"]),
        )
        .or(
            handler_fn(|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                handlers::handle_import(ctx, inp.node)
            })
            .for_kinds(&["import_declaration"]),
        )
        .or(
            handler_fn(|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                handlers::handle_member_access(ctx, inp.node)
            })
            .for_kinds(&["method_invocation", "field_access"]),
        )
        .or(
            handler_fn(|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                handlers::handle_method_reference(ctx, inp.node)
            })
            .for_kinds(&["method_reference"]),
        )
        .or(
            (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                if let Some(type_args) =
                    utils::find_innermost_constructor_type_arguments(inp.node, ctx.offset)
                {
                    let prefix = utils::find_prefix_in_type_arguments_hole(ctx, type_args);
                    return Some((CursorLocation::TypeAnnotation { prefix: prefix.clone() }, prefix));
                }
                Some(handlers::handle_constructor(ctx, inp.node))
            })
            .for_kinds(&["object_creation_expression"]),
        )
        .or(
            handler_fn(|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                handlers::handle_argument_list(ctx, inp.node)
            })
            .for_kinds(&["argument_list"]),
        )
        .or(
            (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, trigger_char, _) = inp.ctx;
                if is_member_part_of_scoped_type(inp.node) {
                    return None;
                }
                Some(handlers::handle_identifier(ctx, inp.node, trigger_char))
            })
            .for_kinds(&["identifier", "type_identifier"]),
        )
        .or(
            (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                heuristics::detect_member_access_in_scoped_type(ctx, inp.node)
            })
            .for_kinds(&["scoped_type_identifier"]),
        )
        .or(
            (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                if let Some(ctor) = utils::find_object_creation_at_cursor(inp.node, ctx.offset) {
                    if let Some(type_args) =
                        utils::find_innermost_constructor_type_arguments(ctor, ctx.offset)
                    {
                        let prefix = utils::find_prefix_in_type_arguments_hole(ctx, type_args);
                        return Some((
                            CursorLocation::TypeAnnotation { prefix: prefix.clone() },
                            prefix,
                        ));
                    }
                    return Some(handlers::handle_constructor(ctx, ctor));
                }
                if let Some(r) = heuristics::detect_member_access_in_local_decl(ctx, inp.node) {
                    return Some(r);
                }
                heuristics::detect_constructor_in_local_decl_error(ctx, inp.node)
            })
            .for_kinds(&["local_variable_declaration"]),
        )
        .or(
            (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, _, _) = inp.ctx;
                handlers::handle_expression_statement(ctx, inp.node)
            })
            .for_kinds(&["expression_statement"]),
        )
        .or(
            handler_fn(|_inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                (CursorLocation::Expression { prefix: String::new() }, String::new())
            })
            .for_kinds(&["dimensions"]),
        )
        .or(
            (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (ctx, trigger_char, orig) = inp.ctx;
                Some(error::handle_error(ctx, inp.node, orig, trigger_char))
            })
            .for_kinds(&["ERROR"]),
        )
        .or(
            (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| {
                let (_, _, orig) = inp.ctx;
                if inp.node.id() == orig.id() {
                    Some(block::handle_block_as_cursor(inp.ctx.0, inp.node))
                } else {
                    None
                }
            })
            .for_kinds(&["block"]),
        )
        // class_body / program / block (non-cursor): stop climbing (return None)
        .or(
            (|inp: Input<(&JavaContextExtractor, Option<char>, Node)>| -> Option<(CursorLocation, String)> {
                let (_, _, orig) = inp.ctx;
                if inp.node.kind() == "class_body" && inp.node.id() == orig.id() {
                    Some((CursorLocation::Expression { prefix: String::new() }, String::new()))
                } else {
                    // Returning None here causes climbing to stop because
                    // these kinds are listed in stop_kinds below.
                    None
                }
            })
            .for_kinds(&["block", "class_body", "program"]),
        )
        .climb(&["block", "class_body", "program",
                 "break_statement", "continue_statement"]);

    let ctx_tuple: (&JavaContextExtractor, Option<char>, Node) = (ctx, trigger_char, node);
    if let Some(result) = location_handler.handle(Input::new(node, ctx_tuple, trigger_char)) {
        return result;
    }

    if matches!(node.kind(), "identifier" | "type_identifier") {
        let text = utils::cursor_truncated_text(ctx, node);
        let clean = crate::language::java::utils::strip_sentinel(&text);
        return (
            CursorLocation::Expression {
                prefix: clean.clone(),
            },
            clean,
        );
    }

    if let Some(type_name) = heuristics::detect_variable_name_after_type_text(ctx) {
        return (CursorLocation::VariableName { type_name }, String::new());
    }

    (
        CursorLocation::Expression {
            prefix: String::new(),
        },
        String::new(),
    )
}

#[cfg(test)]
mod tests {
    use tree_sitter::Parser;

    use crate::{
        language::java::{JavaContextExtractor, location::determine_location},
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
    fn test_type_with_trailing_space_is_variable_name() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            ArrayList 
        }
    }
    "#};
        let marker = "ArrayList ";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::VariableName { .. }),
            "Expected VariableName for `ArrayList `, got {:?}",
            loc
        );
    }

    #[test]
    fn test_type_with_trailing_space_after_previous_statement_is_variable_name() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            Object[] o = new Object[]{};
            ArrayList 
        }
    }
    "#};
        let marker = "ArrayList ";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::VariableName { .. }),
            "Expected VariableName for `ArrayList ` after previous statement, got {:?}",
            loc
        );
    }

    #[test]
    fn test_type_with_trailing_space_after_invalid_array_syntax_is_variable_name() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            Object[] o = new Object[];
            ArrayList 
        }
    }
    "#};
        let marker = "ArrayList ";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::VariableName { .. }),
            "Expected VariableName for `ArrayList ` after invalid array syntax, got {:?}",
            loc
        );
    }

    #[test]
    fn test_type_without_trailing_space_is_expression_in_block() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            ArrayList|
        }
    }
    "#};
        let marker = "|";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::Expression { .. }),
            "Expected Expression for `ArrayList` in block, got {:?}",
            loc
        );
    }

    #[test]
    fn test_type_without_trailing_space_after_previous_statement_is_expression() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            Object[] o = new Object[]{};
            ArrayList|
        }
    }
    "#};
        let marker = "|";
        let offset = src.find(marker).unwrap() + marker.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::Expression { .. }),
            "Expected Expression for `ArrayList` after previous statement, got {:?}",
            loc
        );
    }

    #[test]
    fn test_type_with_trailing_space_in_empty_source_is_variable_name() {
        let src = "ArrayList ";
        let offset = src.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::VariableName { .. }),
            "Expected VariableName for `ArrayList ` in empty source, got {:?}",
            loc
        );
    }

    #[test]
    fn test_type_without_trailing_space_in_empty_source_is_expression() {
        let src = "ArrayList";
        let offset = src.len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());

        let (loc, _query) = determine_location(&ctx, cursor_node, None);

        assert!(
            matches!(loc, CursorLocation::Expression { .. }),
            "Expected Expression for `ArrayList` in empty source, got {:?}",
            loc
        );
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
    fn test_plain_type_awaiting_variable_name() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        String 
    }
}
"#};
        let offset = src.find("String ").unwrap() + "String ".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, _) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::VariableName { ref type_name } if type_name == "String"),
            "Expected VariableName{{String}}, got {:?}",
            loc
        );
    }

    #[test]
    fn test_array_type_awaiting_variable_name() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        String[] 
    }
}
"#};
        let offset = src.find("String[] ").unwrap() + "String[] ".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, _) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::VariableName { ref type_name } if type_name == "String[]"),
            "Expected VariableName{{String[]}}, got {:?}",
            loc
        );
    }

    #[test]
    fn test_primitive_type_awaiting_variable_name() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        int 
    }
}
"#};
        let offset = src.find("int ").unwrap() + "int ".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, _) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::VariableName { ref type_name } if type_name == "int"),
            "Expected VariableName{{int}}, got {:?}",
            loc
        );
    }

    #[test]
    fn test_generic_type_awaiting_variable_name() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        List<String> 
    }
}
"#};
        let offset = src.find("List<String> ").unwrap() + "List<String> ".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, _) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(loc, CursorLocation::VariableName { ref type_name } if type_name == "List<String>"),
            "Expected VariableName{{List<String>}}, got {:?}",
            loc
        );
    }

    #[test]
    fn test_keyword_identifier_not_variable_name() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        return 
    }
}
"#};
        let offset = src.find("return ").unwrap() + "return ".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, _) = determine_location(&ctx, cursor_node, None);
        assert!(
            !matches!(loc, CursorLocation::VariableName { .. }),
            "return should NOT be VariableName, got {:?}",
            loc
        );
    }

    #[test]
    fn test_array_creation_dimension_is_expression() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        new Object[]
    }
}
"#};
        let offset = src.find('[').unwrap() + 1; // cursor inside `[|]`
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, _) = determine_location(&ctx, cursor_node, None);
        assert!(
            !matches!(loc, CursorLocation::Unknown),
            "new Object[|] should not be Unknown, got {:?}",
            loc
        );
    }

    #[test]
    fn test_scoped_type_in_error_is_member_access() {
        let src = indoc::indoc! {r#"
class A {
    void f() {
        value.appe
    }
}
"#};
        let offset = src.find("appe").unwrap() + "appe".len();
        let (ctx, tree) = setup_with(src, offset);
        let cursor_node = ctx.find_cursor_node(tree.root_node());
        let (loc, query) = determine_location(&ctx, cursor_node, None);
        assert!(
            matches!(
                loc,
                CursorLocation::MemberAccess { ref receiver_expr, ref member_prefix, .. }
                if receiver_expr == "value" && member_prefix == "appe"
            ),
            "Expected MemberAccess{{value, appe}}, got {:?}",
            loc
        );
        assert_eq!(query, "appe");
    }
}
