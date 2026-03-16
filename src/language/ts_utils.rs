use tree_sitter::Node;

pub use tree_sitter_utils::query::{capture_text, run_query};

/// Find the innermost `method_declaration` containing `offset`.
///
/// Thin wrapper around [`tree_sitter_utils::traversal::find_node_by_offset`]
/// that fixes the target kind to `"method_declaration"`.
#[inline]
pub fn find_method_by_offset(root: Node, offset: usize) -> Option<Node> {
    tree_sitter_utils::traversal::find_node_by_offset(root, "method_declaration", offset)
}

#[cfg(test)]
mod tests {
    use crate::language::rope_utils::line_col_to_offset;

    use super::*;
    use tree_sitter::Parser;

    fn parse_java(src: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        parser.parse(src, None).unwrap()
    }

    fn offset_of(src: &str, line: u32, col: u32) -> usize {
        line_col_to_offset(src, line, col).unwrap()
    }

    /// Returns (start_line, end_line) of the found method, or None
    fn method_lines(src: &str, line: u32, col: u32) -> Option<(usize, usize)> {
        let tree = parse_java(src);
        let offset = offset_of(src, line, col);
        find_method_by_offset(tree.root_node(), offset)
            .map(|n| (n.start_position().row, n.end_position().row))
    }

    #[test]
    fn test_finds_method_on_normal_line() {
        let src = indoc::indoc! {r#"
            class A {
                void foo() {
                    int x = 1;
                }
            }
        "#};
        // line 2: "    int x = 1;"
        assert!(method_lines(src, 2, 4).is_some());
    }

    #[test]
    fn test_finds_method_on_indented_blank_line() {
        let src = "class A {\n    void foo() {\n        \n        int x = 1;\n    }\n}\n";
        // line 2 is "        " (blank with spaces), col 4
        let result = method_lines(src, 2, 4);
        assert!(
            result.is_some(),
            "blank line with indent inside method should find method_declaration"
        );
    }

    #[test]
    fn test_finds_method_on_col0_blank_line_inside_method() {
        // col-0 blank line inside method body
        let src = "class A {\n    void foo() {\n\n        int x = 1;\n    }\n}\n";
        // line 2 is "\n" (completely empty), col 0
        let result = method_lines(src, 2, 0);
        assert!(
            result.is_some(),
            "col-0 blank line inside method should find method_declaration, got None"
        );
    }

    #[test]
    fn test_does_not_find_method_outside_class() {
        let src = "class A {\n    void foo() {\n    }\n}\n";
        // line 3: "}" — outside method body (after closing brace)
        // actually line 3 is "}" of class, completely outside method
        let result = method_lines(src, 3, 0);
        assert!(
            result.is_none(),
            "line outside method should not find method_declaration"
        );
    }

    #[test]
    fn test_finds_correct_method_when_multiple_methods() {
        let src = indoc::indoc! {r#"
            class A {
                void foo() {
                    int x = 1;
                }
                void bar() {
                    int y = 2;
                }
            }
        "#};
        // line 5: "    int y = 2;" — inside bar()
        let r = method_lines(src, 5, 8);
        assert!(r.is_some());
        // bar() starts at line 4
        assert_eq!(r.unwrap().0, 4, "should find bar(), not foo()");
    }

    #[test]
    fn test_col0_blank_between_methods_returns_none() {
        // blank line BETWEEN two methods (not inside any method body)
        // tree-sitter: the blank line between methods is inside class_body, not method_declaration
        let src = "class A {\n    void foo() {\n    }\n\n    void bar() {\n    }\n}\n";
        // line 3 is completely blank, between foo() and bar()
        let result = method_lines(src, 3, 0);
        assert!(
            result.is_none(),
            "blank line between methods should NOT find any method_declaration"
        );
    }
}
