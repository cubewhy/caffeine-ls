use super::Language;
use crate::completion::{CompletionContext, context::CursorLocation};
use crate::language::ClassifiedToken;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::language::rope_utils::rope_line_col_to_offset;
use crate::language::ts_utils::find_method_by_offset;
use ropey::Rope;
use smallvec::smallvec;
use tower_lsp::lsp_types::{SemanticTokenModifier, SemanticTokenType};
use tree_sitter::{Node, Parser};

pub mod class_parser;
pub mod injection;
pub mod locals;
pub mod location;
pub mod members;
pub mod scope;
pub mod type_ctx;
pub mod utils;

const SENTINEL: &str = "__KIRO__";

pub fn make_java_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("failed to load java grammar");
    parser
}

#[derive(Debug)]
pub struct JavaLanguage;

impl JavaLanguage {
    fn is_static(&self, node: Node, bytes: &[u8]) -> bool {
        node.child_by_field_name("modifiers")
            .and_then(|m| m.utf8_text(bytes).ok())
            .is_some_and(|text| text.contains("static"))
    }

    fn is_final(&self, node: Node, bytes: &[u8]) -> bool {
        node.child_by_field_name("modifiers")
            .and_then(|m| m.utf8_text(bytes).ok())
            .is_some_and(|text| text.contains("final"))
    }

    fn is_field_declarator_identifier(&self, ident: Node) -> bool {
        // identifier -> variable_declarator -> field_declaration
        ident
            .parent()
            .and_then(|p| p.parent())
            .is_some_and(|gp| gp.kind() == "field_declaration")
    }
}

impl Language for JavaLanguage {
    fn id(&self) -> &'static str {
        "java"
    }

    fn supports(&self, language_id: &str) -> bool {
        language_id == "java"
    }

    fn make_parser(&self) -> Parser {
        make_java_parser()
    }

    fn parse_completion_context(
        &self,
        source: &str,
        line: u32,
        character: u32,
        trigger_char: Option<char>,
    ) -> Option<CompletionContext> {
        let rope = Rope::from_str(source);
        let offset = rope_line_col_to_offset(&rope, line, character)?;
        tracing::debug!(line, character, trigger = ?trigger_char, "java: parsing context");
        let extractor = JavaContextExtractor::with_rope(source.to_string(), offset, rope);
        if extractor.is_in_comment() {
            return Some(CompletionContext::new(
                CursorLocation::Unknown,
                "",
                vec![],
                None,
                None,
                None,
                vec![],
            ));
        }
        let mut parser = make_java_parser();
        let tree = parser.parse(extractor.source_str(), None)?;
        Some(extractor.extract(tree.root_node(), trigger_char))
    }

    fn supports_semantic_tokens(&self) -> bool {
        true
    }

    fn classify_semantic_token<'a>(
        &self,
        node: Node<'a>,
        bytes: &'a [u8],
    ) -> Option<ClassifiedToken> {
        match node.kind() {
            "type_identifier" => {
                if is_annotation_name(node) {
                    Some(ClassifiedToken {
                        ty: SemanticTokenType::DECORATOR,
                        modifiers: smallvec![],
                    })
                } else {
                    Some(ClassifiedToken {
                        ty: SemanticTokenType::CLASS,
                        modifiers: smallvec![],
                    })
                }
            }

            "string_literal" => Some(ClassifiedToken {
                ty: SemanticTokenType::STRING,
                modifiers: smallvec![],
            }),

            "static" => Some(ClassifiedToken {
                ty: SemanticTokenType::MODIFIER,
                modifiers: smallvec![SemanticTokenModifier::STATIC],
            }),
            "final" => Some(ClassifiedToken {
                ty: SemanticTokenType::MODIFIER,
                modifiers: smallvec![SemanticTokenModifier::READONLY],
            }),

            "identifier" => {
                if is_annotation_name(node) {
                    return Some(ClassifiedToken {
                        ty: SemanticTokenType::DECORATOR,
                        modifiers: smallvec![],
                    });
                }

                let parent = node.parent()?;
                match parent.kind() {
                    // Method declaration name
                    "method_declaration" => {
                        let mut mods = smallvec![];
                        if self.is_static(parent, bytes) {
                            mods.push(SemanticTokenModifier::STATIC);
                        }
                        Some(ClassifiedToken {
                            ty: SemanticTokenType::METHOD,
                            modifiers: mods,
                        })
                    }

                    // The name of the method call (Note: in tree-sitter-java, the name in method_invocation is also an identifier)
                    "method_invocation" => {
                        // TODO: method_invocation 的 modifiers 不一定有意义，但如果你想把“静态调用”也标出来，
                        // 得靠语义（index）推断；这里先不做。
                        Some(ClassifiedToken {
                            ty: SemanticTokenType::METHOD,
                            modifiers: smallvec![],
                        })
                    }

                    // The variable declaration (local/field) name is in the name field of variable_declarator.
                    "variable_declarator" => {
                        let is_field = self.is_field_declarator_identifier(node);
                        if is_field {
                            // readonly: You need to look at the modifiers of field_declaration (it is the parent of variable_declarator)
                            let field_decl = parent.parent()?; // field_declaration
                            let mut mods = smallvec![];
                            if self.is_final(field_decl, bytes) {
                                mods.push(SemanticTokenModifier::READONLY);
                            }
                            Some(ClassifiedToken {
                                ty: SemanticTokenType::PROPERTY,
                                modifiers: mods,
                            })
                        } else {
                            Some(ClassifiedToken {
                                ty: SemanticTokenType::VARIABLE,
                                modifiers: smallvec![],
                            })
                        }
                    }

                    // Parameter name: name of formal_parameter
                    "formal_parameter" => Some(ClassifiedToken {
                        ty: SemanticTokenType::PARAMETER,
                        modifiers: smallvec![],
                    }),

                    _ => None,
                }
            }

            _ => None,
        }
    }
}

fn is_annotation_name(node: Node) -> bool {
    node.parent().is_some_and(|p| {
        (p.kind() == "annotation" || p.kind() == "marker_annotation")
            && p.child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id())
    })
}

pub struct JavaContextExtractor {
    source: String,
    pub rope: Rope,
    pub offset: usize,
}

impl JavaContextExtractor {
    pub fn new(source: impl Into<String>, offset: usize) -> Self {
        let source = source.into();
        let rope = Rope::from_str(&source);
        Self {
            source,
            rope,
            offset,
        }
    }

    /// Create a simplified extractor for indexing (no cursor offset needed)
    pub fn for_indexing(source: &str) -> Self {
        Self::new(source, 0)
    }

    pub(crate) fn with_rope(source: String, offset: usize, rope: Rope) -> Self {
        Self {
            source,
            rope,
            offset,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        self.source.as_bytes()
    }
    pub fn source_str(&self) -> &str {
        &self.source
    }

    /// 字节范围切片（tree-sitter 给出的 start/end 直接用）
    pub fn byte_slice(&self, start: usize, end: usize) -> &str {
        &self.source[start..end]
    }

    pub fn node_text(&self, node: Node) -> &str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }

    /// 判断当前 offset 是否在注释中
    pub fn is_in_comment(&self) -> bool {
        utils::is_cursor_in_comment_with_rope(&self.source, &self.rope, self.offset)
    }

    fn extract(self, root: Node, trigger_char: Option<char>) -> CompletionContext {
        let cursor_node = self.find_cursor_node(root);

        if cursor_node
            .map(|n| utils::is_comment_kind(n.kind()))
            .unwrap_or(false)
        {
            return self.empty_context();
        }

        let (location, query) = location::determine_location(&self, cursor_node, trigger_char);

        // 如果 AST 解析失败，走注入路径
        let (location, query) = if matches!(location, CursorLocation::Unknown) {
            injection::inject_and_determine(&self, cursor_node, trigger_char)
                .unwrap_or((location, query))
        } else {
            (location, query)
        };

        let local_variables = locals::extract_locals(&self, root, cursor_node);
        let enclosing_class = scope::extract_enclosing_class(&self, cursor_node)
            .or_else(|| scope::extract_enclosing_class_by_offset(&self, root));
        let enclosing_package = scope::extract_package(&self, root);
        let enclosing_internal_name =
            utils::build_internal_name(&enclosing_package, &enclosing_class);
        let existing_imports = scope::extract_imports(&self, root);
        let type_ctx = SourceTypeCtx::new(
            enclosing_package.clone(),
            existing_imports.clone(),
            None, // completion path 没有 index 快照，None 触发 hardcode java.lang fallback
        );
        let existing_static_imports = scope::extract_static_imports(&self, root);
        let current_class_members = cursor_node
            .and_then(|n| utils::find_ancestor(n, "class_declaration"))
            .and_then(|cls| cls.child_by_field_name("body"))
            .map(|body| members::extract_class_members_from_body(&self, body, &type_ctx))
            .or_else(|| {
                // Fallback: find top-level ERROR node anywhere under program
                let error_node = utils::find_top_error_node(root)?;
                let mut members = Vec::new();
                members::collect_members_from_node(&self, error_node, &type_ctx, &mut members);
                let snapshot = members.clone();
                members.extend(members::parse_partial_methods_from_error(
                    &self, &type_ctx, error_node, &snapshot,
                ));
                Some(members)
            })
            .unwrap_or_default();
        let enclosing_class_member = cursor_node
            .and_then(|n| utils::find_ancestor(n, "method_declaration"))
            .or_else(|| find_method_by_offset(root, self.offset))
            .or_else(|| utils::find_enclosing_method_in_error(root, self.offset))
            .and_then(|m| members::parse_method_node(&self, &type_ctx, m));
        let char_after_cursor = self.source[self.offset..]
            .chars()
            .find(|c| !(c.is_alphanumeric() || *c == '_'));

        CompletionContext::new(
            location,
            query,
            local_variables,
            enclosing_class,
            enclosing_internal_name,
            enclosing_package,
            existing_imports,
        )
        .with_static_imports(existing_static_imports)
        .with_class_members(current_class_members)
        .with_enclosing_member(enclosing_class_member)
        .with_char_after_cursor(char_after_cursor)
    }

    fn find_cursor_node<'tree>(&self, root: Node<'tree>) -> Option<Node<'tree>> {
        if let Some(n) =
            root.named_descendant_for_byte_range(self.offset.saturating_sub(1), self.offset)
            && !utils::is_comment_kind(n.kind())
            && n.end_byte() >= self.offset
        {
            return Some(n);
        }
        if self.offset < self.source.len()
            && let Some(n) = root.named_descendant_for_byte_range(self.offset, self.offset + 1)
            && !utils::is_comment_kind(n.kind())
        {
            return Some(n);
        }
        None
    }

    fn empty_context(&self) -> CompletionContext {
        CompletionContext::new(
            CursorLocation::Unknown,
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        )
    }

    fn make_parser(&self) -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("failed to load java grammar");
        parser
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion::context::{CurrentClassMember, CursorLocation},
        language::{java::injection::build_injected_source, rope_utils::line_col_to_offset},
    };

    fn at(src: &str, line: u32, col: u32) -> CompletionContext {
        JavaLanguage
            .parse_completion_context(src, line, col, None)
            .unwrap()
    }

    fn end_of(src: &str) -> CompletionContext {
        let lines: Vec<&str> = src.lines().collect();
        let line = (lines.len() - 1) as u32;
        let col = lines.last().unwrap().len() as u32;
        at(src, line, col)
    }

    #[test]
    fn test_import() {
        let src = "import com.example.Foo;";
        let ctx = end_of(src);
        assert!(matches!(ctx.location, CursorLocation::Import { .. }));
    }

    #[test]
    fn test_member_access() {
        // "class A { void f() { someList.get; } }\n"
        //  "someList.get" starts at byte 21, "get" at byte 30..33
        //  col = 33 - start_of_line(0) = 33
        let src = "class A { void f() { someList.get; } }\n";
        let ctx = at(src, 0, 33);
        assert!(
            matches!(ctx.location, CursorLocation::MemberAccess { .. }),
            "{:?}",
            ctx.location
        );
        if let CursorLocation::MemberAccess { member_prefix, .. } = &ctx.location {
            assert_eq!(member_prefix, "get");
        }
    }

    #[test]
    fn test_constructor() {
        let src = indoc::indoc! {r#"
            class A {
                void f() { new ArrayList() }
            }
        "#};
        let line = 1u32;
        let col = src.lines().nth(1).unwrap().find("ArrayList").unwrap() as u32 + 9;
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::ConstructorCall { .. }),
            "{:?}",
            ctx.location
        );
    }

    #[test]
    fn test_local_var_extracted() {
        let src = indoc::indoc! {r#"
            class A {
                void f() {
                    List<String> items = new ArrayList<>();
                    items.get
                }
            }
        "#};
        let line = 3u32;
        let col = src.lines().nth(3).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "items"),
            "locals: {:?}",
            ctx.local_variables
        );
    }

    #[test]
    fn test_params_extracted() {
        let src = indoc::indoc! {r#"
            class A {
                void process(String input, int count) {
                    input.length
                }
            }
        "#};
        let line = 2u32;
        let col = src.lines().nth(2).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "input")
        );
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "count")
        );
    }

    #[test]
    fn test_enclosing_class() {
        let src = indoc::indoc! {r#"
            class MyService {
                void foo() { this.bar }
            }
        "#};
        let line = 1u32;
        let col = src.lines().nth(1).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert_eq!(ctx.enclosing_class.as_deref(), Some("MyService"));
    }

    #[test]
    fn test_imports_extracted() {
        let src = indoc::indoc! {r#"
            import java.util.List;
            import java.util.Map;
            class A { void f() {} }
        "#};
        let ctx = at(src, 2, 10);
        assert!(ctx.existing_imports.iter().any(|i| i.contains("List")));
        assert!(ctx.existing_imports.iter().any(|i| i.contains("Map")));
    }

    #[test]
    fn test_this_dot_no_semicolon_member_prefix_empty() {
        // No semicolon or subsequent text follows `this.`, the cursor immediately follows the dot.
        // `member_prefix` should be an empty string, `location` should be `MemberAccess`.
        let src = indoc::indoc! {r#"
        class A {
            private void priFunc() {}
            void fun() {
                this.
            }
        }
    "#};
        // The cursor is after the dot "this."
        let line = 3u32;
        let raw_line = src.lines().nth(3).unwrap();
        let col = raw_line.find("this.").unwrap() as u32 + 5; // after the dot (".")
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { member_prefix, receiver_expr, .. }
                if member_prefix.is_empty() && receiver_expr == "this"
            ),
            "expected MemberAccess{{receiver_expr=this, member_prefix=}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_this_dot_cursor_before_existing_identifier() {
        // `this.|fun()` — cursor after the dot and before fun
        // member_prefix should be an empty string
        let src = indoc::indoc! {r#"
        class A {
            private void priFunc() {}
            void fun() {
                this.fun()
            }
        }
    "#};
        let line = 3u32;
        let raw_line = src.lines().nth(3).unwrap();
        // The cursor is after "this." and before "fun".
        let col = raw_line.find("this.").unwrap() as u32 + 5;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { member_prefix, receiver_expr, .. }
                if member_prefix.is_empty() && receiver_expr == "this"
            ),
            "cursor before 'fun': member_prefix should be empty, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_this_dot_cursor_mid_identifier() {
        // `this.fu|n()` — cursor is in the middle of fun
        // member_prefix should be "fu"
        let src = indoc::indoc! {r#"
        class A {
            void fun() {
                this.fun()
            }
        }
    "#};
        let line = 2u32;
        let raw_line = src.lines().nth(2).unwrap();
        // Starting column of fun
        let fun_col = raw_line.find("fun").unwrap() as u32;
        // The cursor is after fu and before n.
        let col = fun_col + 2;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { member_prefix, receiver_expr, .. }
                if member_prefix == "fu" && receiver_expr == "this"
            ),
            "cursor mid-identifier: member_prefix should be 'fu', got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_this_dot_with_semicolon_member_prefix_empty() {
        // `this.|;` — There is a semicolon, the cursor is after the dot.
        let src = indoc::indoc! {r#"
        class A {
            void fun() {
                this.;
            }
        }
    "#};
        let line = 2u32;
        let raw_line = src.lines().nth(2).unwrap();
        let col = raw_line.find("this.").unwrap() as u32 + 5;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { member_prefix, receiver_expr, .. }
                if member_prefix.is_empty() && receiver_expr == "this"
            ),
            "this.|; should give empty member_prefix, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_expression_ma_finds_main_in_same_package() {
        // `Ma|` should be found in the same package as Main
        // Validate ExpressionProvider without filtering enclosing class
        let src = indoc::indoc! {r#"
        package org.cubewhy.a;
        class Main {
            void func() {
                Ma
            }
        }
    "#};
        let line = 3u32;
        let raw_line = src.lines().nth(3).unwrap();
        let col = raw_line.len() as u32;
        let ctx = at(src, line, col);
        // Verification shows that Expression{prefix="Ma"} has been parsed.
        assert!(
            matches!(&ctx.location, CursorLocation::Expression { prefix } if prefix == "Ma"),
            "expected Expression{{Ma}}, got {:?}",
            ctx.location
        );
        // enclosing_class should be Main
        assert_eq!(ctx.enclosing_class.as_deref(), Some("Main"));
    }

    #[test]
    fn test_argument_prefix_truncated_at_cursor() {
        // When `println(var|)`, the cursor is in the middle of `aVar`, so the prefix should be the part before "var".
        // That is, in `println(aVar)`, the cursor is after "aV", so the prefix should be "aV".
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                String aVar = "test";
                System.out.println(aVar)
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        // Cursor after "aV" and before "ar"
        let col = raw.find("aVar").unwrap() as u32 + 2;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::MethodArgument { prefix } if prefix == "aV"),
            "expected MethodArgument{{aV}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_direct_call_with_paren_is_member_access() {
        // fun|() — No receiver, should be parsed as an Expression, prefix = "fun"
        let src = indoc::indoc! {r#"
        class A {
            void fun() {}
            void test() {
                fun()
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        // Cursor at the end of "fun"
        let col = raw.find("fun").unwrap() as u32 + 3;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::MemberAccess { member_prefix, .. } if member_prefix == "fun"),
            "direct call fun() should give MemberAccess{{fun}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_char_after_cursor_skips_identifier_chars() {
        // fun|() — The cursor is at the end of fun, skipping the first character after the identifier, which is '('.
        let src = indoc::indoc! {r#"
        class A {
            void fun() {}
            void test() {
                fun()
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.find("fun").unwrap() as u32 + 3;
        let ctx = at(src, line, col);
        assert!(
            ctx.has_paren_after_cursor(),
            "char after identifier should be '(', got {:?}",
            ctx.char_after_cursor
        );
    }

    #[test]
    fn test_this_method_paren_already_exists() {
        // this.priFunc() — The cursor is at the end of priFunc, followed by '('
        let src = indoc::indoc! {r#"
        class A {
            void fun() {
                this.priFunc()
            }
            private void priFunc() {}
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("priFunc").unwrap() as u32 + "priFunc".len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.has_paren_after_cursor(),
            "after 'priFunc' should see '(', char_after_cursor={:?}",
            ctx.char_after_cursor
        );
    }

    #[test]
    fn test_empty_argument_list_is_method_argument() {
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                System.out.println()
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find('(').unwrap() as u32 + 1;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::MethodArgument { prefix } if prefix.is_empty()),
            "empty argument list should give MethodArgument{{prefix:\"\"}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_static_method_members_visible_in_static_context() {
        // In the main() static method, static methods of the same class should be recorded as is_static=true in enclosing_class_member.
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                pr
            }
            public static Object pri() { return null; }
        }
    "#};
        let line = 2u32;
        let col = src.lines().nth(2).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.is_in_static_context(),
            "main() should be static context"
        );
        assert!(
            ctx.current_class_members.contains_key("pri"),
            "pri should be in current_class_members"
        );
        let pri = ctx.current_class_members.get("pri").unwrap();
        assert!(pri.is_static(), "pri() should be marked static");
    }

    #[test]
    fn test_var_init_expression_is_expression_location() {
        // `Object a = p|` — The initialization expression should be resolved to Expression{prefix:"p"}
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                Object a = p
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::Expression { prefix } if prefix == "p"),
            "var init expression should give Expression{{p}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_var_init_expression_with_prefix_a() {
        // `String aCopy = a|` — Initialization expression, prefix = "a"
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                String aVar = "test";
                String aCopy = a
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::Expression { prefix } if prefix == "a"),
            "var init expression should give Expression{{a}}, got {:?}",
            ctx.location
        );
        // Simultaneously verify that aVar is in a local variable
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "aVar"),
            "aVar should be in locals: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_empty_line_in_static_method_has_static_context() {
        // The blank line is inside main(), enclosing_class_member should be main (static).
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                
            }
            public static Object pri() { return null; }
        }
    "#};
        let line = 2u32;
        // Blank line, col = 0 or any position within the line
        let col = 4u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.is_in_static_context(),
            "empty line inside main() should have static context, enclosing_member={:?}",
            ctx.enclosing_class_member
        );
        assert!(
            ctx.current_class_members.contains_key("pri"),
            "pri should be indexed: {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_empty_line_static_context_has_pri_member() {
        // Verify the complete chain: blank line + static context + pri is a static member
        let src = indoc::indoc! {r#"
        class A {
            public static void main() {
                
            }
            public static Object pri() { return null; }
        }
    "#};
        let line = 2u32;
        let col = 4u32;
        let ctx = at(src, line, col);

        let pri = ctx.current_class_members.get("pri");
        assert!(pri.is_some(), "pri should be in current_class_members");
        assert!(pri.unwrap().is_static(), "pri() should be marked as static");
    }

    #[test]
    fn test_member_access_before_paren_is_member_access() {
        // cl.|f() — Cursor after '.' and before 'f'
        let src = indoc::indoc! {r#"
        class A {
            void test() {
                RandomClass cl = new RandomClass();
                cl.f()
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.find("cl.").unwrap() as u32 + 3; // After the dot, before f
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { member_prefix, receiver_expr, .. }
                if member_prefix.is_empty() && receiver_expr == "cl"
            ),
            "cl.|f() should give MemberAccess{{receiver=cl, prefix=}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_member_access_no_paren_cursor_after_dot() {
        // cl.| (without parentheses, cursor is placed directly after the dot)
        // fallback_location should be able to handle this
        let src = indoc::indoc! {r#"
        class A {
            void test() {
                RandomClass cl = new RandomClass();
                cl.
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.find("cl.").unwrap() as u32 + 3;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { member_prefix, receiver_expr, .. }
                if member_prefix.is_empty() && receiver_expr == "cl"
            ),
            "cl.| should give MemberAccess, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_constructor_expected_type_string() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                String a = new ;
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        // cursor after "new "
        let col = raw.find("new ").unwrap() as u32 + 4;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::ConstructorCall { expected_type: Some(t), .. }
                if t == "String"
            ),
            "expected_type should be 'String', got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_constructor_no_expected_type_standalone() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new ;
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("new ").unwrap() as u32 + 4;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::ConstructorCall { expected_type: None, class_prefix, .. }
                if class_prefix.is_empty()
            ),
            "standalone 'new' should have ConstructorCall{{prefix:\"\", expected_type:None}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_constructor_expected_type_with_prefix() {
        // RandomClass rc = new RandomClass|
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                RandomClass rc = new RandomClass();
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("RandomClass(").unwrap() as u32 + "RandomClass".len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::ConstructorCall { expected_type: Some(t), .. }
                if t == "RandomClass"
            ),
            "expected_type should be 'RandomClass', got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_new_chain_dot_is_member_access() {
        // new RandomClass().| → MemberAccess{receiver="new RandomClass()", prefix=""}
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new RandomClass().
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col =
            raw.find("new RandomClass().").unwrap() as u32 + "new RandomClass().".len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { receiver_expr, member_prefix, .. }
                if receiver_expr == "new RandomClass()" && member_prefix.is_empty()
            ),
            "new Foo().| should be MemberAccess, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_new_chain_dot_with_prefix_is_member_access() {
        // new RandomClass().fu| → MemberAccess{receiver="new RandomClass()", prefix="fu"}
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new RandomClass().fu
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find(".fu").unwrap() as u32 + 3;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { receiver_expr, member_prefix, .. }
                if receiver_expr == "new RandomClass()" && member_prefix == "fu"
            ),
            "new Foo().fu| should be MemberAccess{{prefix=fu}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_new_without_dot_still_constructor() {
        // new RandomClass| -> ConstructorCall (not affected by fix)
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new RandomClass
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("new RandomClass").unwrap() as u32 + "new RandomClass".len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::ConstructorCall { class_prefix, .. }
                if class_prefix == "RandomClass"
            ),
            "new Foo| should still be ConstructorCall, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_var_with_non_constructor_init_skipped() {
        // var x = someMethod() — cannot infer, should not crash
        let src = indoc::indoc! {r#"
        class A {
            static String helper() { return ""; }
            void f() {
                var x = helper();
                x.
            }
        }
    "#};
        let line = 4u32;
        let raw = src.lines().nth(4).unwrap();
        let col = raw.find("x.").unwrap() as u32 + 2;
        let ctx = at(src, line, col);
        // x should not appear (type unknown), but should not crash
        // The important thing is no panic
        let _ = ctx;
    }

    #[test]
    fn test_bare_method_call_no_semicolon() {
        // getMain2().| without semicolon
        let src = indoc::indoc! {r#"
        class Main {
            public static void main(String[] args) {
                getMain2().
            }
            private static Object getMain2() { return null; }
        }
    "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("getMain2().").map(|c| (i as u32, c as u32 + 11)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { receiver_expr, member_prefix, .. }
                if receiver_expr == "getMain2()" && member_prefix.is_empty()
            ),
            "getMain2().| should be MemberAccess, got {:?}",
            ctx.location
        );
        // enclosing_internal_name must be set for bare method call resolution
        assert!(
            ctx.enclosing_internal_name.is_some(),
            "enclosing_internal_name should be set even without semicolon"
        );
        assert_eq!(ctx.enclosing_class.as_deref(), Some("Main"));
    }

    #[test]
    fn test_cursor_in_line_comment_returns_unknown() {
        let src = "// some random comments\n\npublic class ExampleClass {}";
        // Cursor at the end of the comment line
        let col = "// some random comments".len() as u32;
        let ctx = JavaLanguage
            .parse_completion_context(src, 0, col, None)
            .unwrap();
        assert!(
            matches!(ctx.location, CursorLocation::Unknown),
            "cursor inside line comment should give Unknown, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_cursor_on_empty_line_after_comment_is_expression() {
        let src = "// some random comments\n\npublic class ExampleClass {}";
        // The cursor is on line 1 (empty line).
        let ctx = JavaLanguage
            .parse_completion_context(src, 1, 0, None)
            .unwrap();
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "empty line after comment should not be Unknown, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_empty_line_after_comment_not_comment_context() {
        let src = indoc::indoc! {r#"
        // some random comments
        
        public class ExampleClass {}
    "#};
        let ctx = at(src, 1, 0);
        assert!(
            !matches!(&ctx.location,
            CursorLocation::Expression { prefix } if prefix == "comments"),
            "empty line after comment should not have prefix='comments', got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_cursor_inside_line_comment_is_unknown() {
        let src = "// some random comments";
        let col = src.len() as u32;
        let ctx = at(src, 0, col);
        assert!(
            matches!(ctx.location, CursorLocation::Unknown),
            "cursor inside line comment should be Unknown, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_cursor_at_start_of_line_comment_is_unknown() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                // comment here
            }
        }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.find("//").unwrap() as u32 + 5;
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::Unknown),
            "cursor inside // comment should be Unknown, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_empty_line_after_comment_is_not_unknown() {
        // A blank line immediately following a comment line, with the cursor on the blank line, should not be interpreted as a comment.
        let src = "// some random comments\n\npublic class ExampleClass {}";
        // line=1 is a blank line
        let ctx = at(src, 1, 0);
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "empty line after comment should NOT be Unknown, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_cursor_inside_comment_is_unknown() {
        let src = "// some random comments\n\npublic class ExampleClass {}";
        // line=0, col is in the middle of the comment
        let col = "// some random".len() as u32;
        let ctx = at(src, 0, col);
        assert!(
            matches!(ctx.location, CursorLocation::Unknown),
            "cursor inside comment should be Unknown, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_line_col_to_offset_multibyte() {
        // 含 CJK 字符（UTF-8 3字节，UTF-16 1单元）
        let src = "你好\nworld";
        // line=0, character=2 → 第3个UTF-16单元 = 6字节
        assert_eq!(line_col_to_offset(src, 0, 2), Some(6));
        // line=1, character=3 → "wor" = 3字节
        assert_eq!(line_col_to_offset(src, 1, 3), Some(6 + 1 + 3)); // \n=1, wor=3
    }

    #[test]
    fn test_line_col_to_offset_out_of_bounds() {
        let src = "hello\nworld";
        assert_eq!(line_col_to_offset(src, 5, 0), None);
    }

    #[test]
    fn test_assignment_rhs_is_expression() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                Agent.inst = 
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::Expression { prefix } if prefix.is_empty()),
            "rhs of assignment should be Expression{{prefix: \"\"}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_incomplete_method_argument_is_expression() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                proxy.run(
            }
        }
    "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);
        match &ctx.location {
            CursorLocation::Expression { prefix } | CursorLocation::MethodArgument { prefix } => {
                assert!(prefix.is_empty(), "prefix should be empty");
            }
            _ => panic!(
                "Expected Expression or MethodArgument, got {:?}",
                ctx.location
            ),
        }
    }

    #[test]
    fn test_injection_trailing_dot_member_access() {
        let src = indoc::indoc! {r#"
        class A {
            void test() {
                RandomClass cl = new RandomClass();
                cl.
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("cl.").map(|c| (i as u32, c as u32 + 3)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { member_prefix, receiver_expr, .. }
                if member_prefix.is_empty() && receiver_expr == "cl"
            ),
            "cl.| via injection should give MemberAccess{{receiver=cl}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_injection_new_keyword_constructor() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("new").map(|c| (i as u32, c as u32 + 3)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::ConstructorCall { .. }),
            "new| via injection should give ConstructorCall, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_injection_assignment_rhs_empty() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                Agent.inst =
            }
        }
        "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::Expression { prefix } if prefix.is_empty()),
            "empty assignment rhs should give Expression{{prefix:\"\"}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_injection_chained_call_dot() {
        let src = indoc::indoc! {r#"
        class Main {
            public static void main(String[] args) {
                getMain2().
            }
            private static Object getMain2() { return null; }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("getMain2().").map(|c| (i as u32, c as u32 + 11)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { receiver_expr, member_prefix, .. }
                if receiver_expr == "getMain2()" && member_prefix.is_empty()
            ),
            "getMain2().| via injection should give MemberAccess, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_snapshot_agent_error_case() {
        let src = indoc::indoc! {r#"
    class Agent {
        static Object inst;
        private static Object test() { return null; }
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
    }
    "#};

        // Snapshot the raw AST
        fn node_to_string(node: tree_sitter::Node, src: &str, indent: usize) -> String {
            let pad = " ".repeat(indent * 2);
            let text: String = src[node.start_byte()..node.end_byte()]
                .chars()
                .take(40)
                .collect();
            let mut s = format!(
                "{}{} [{}-{}] {:?}\n",
                pad,
                node.kind(),
                node.start_byte(),
                node.end_byte(),
                text
            );
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                s.push_str(&node_to_string(child, src, indent + 1));
            }
            s
        }

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let ast = node_to_string(tree.root_node(), src, 0);
        insta::assert_snapshot!("agent_error_ast", ast);

        // Snapshot the extraction result
        let line = 7u32;
        let raw = src.lines().nth(7).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);

        let mut members: Vec<String> = ctx
            .current_class_members
            .keys()
            .map(|k| {
                let m = &ctx.current_class_members[k];
                format!(
                    "{} static={} private={} method={}",
                    k,
                    m.is_static(),
                    m.is_private(),
                    m.is_method()
                )
            })
            .collect();
        members.sort();
        insta::assert_snapshot!("agent_error_members", members.join("\n"));
        insta::assert_snapshot!("agent_error_location", format!("{:?}", ctx.location));
        insta::assert_snapshot!(
            "agent_error_enclosing",
            format!("{:?}", ctx.enclosing_class)
        );
    }

    #[test]
    fn test_snapshot_partial_method_extraction() {
        let src = indoc::indoc! {r#"
    class Agent {
        static Object inst;
        private static Object test() { return null; }
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
    }
    "#};

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        // root is program, child(0) is ERROR
        let error_node = root.child(0).unwrap();
        assert_eq!(error_node.kind(), "ERROR");

        let extractor = super::JavaContextExtractor::new(src, 0);

        // Snapshot direct children kinds
        let mut cursor = error_node.walk();
        let children_info: Vec<String> = error_node
            .children(&mut cursor)
            .map(|c| {
                format!(
                    "{} {:?}",
                    c.kind(),
                    &src[c.start_byte()..c.end_byte().min(c.start_byte() + 30)]
                )
            })
            .collect();
        insta::assert_snapshot!("error_children", children_info.join("\n"));

        let type_ctx = SourceTypeCtx::new(None, vec![], None);

        // Snapshot what parse_partial_methods_from_error returns
        let snapshot: Vec<CurrentClassMember> = vec![];
        let partial =
            members::parse_partial_methods_from_error(&extractor, &type_ctx, error_node, &snapshot);
        let mut result: Vec<String> = partial
            .iter()
            .map(|m| {
                format!(
                    "{} static={} private={}",
                    m.name(),
                    m.is_static(),
                    m.is_private()
                )
            })
            .collect();
        result.sort();
        insta::assert_snapshot!("partial_methods", result.join("\n"));
    }

    #[test]
    fn test_snapshot_real_agent_case() {
        let src = indoc::indoc! {r#"
    package org.cubewhy.relx;
    public class Agent {
        private static Object inst;
        public static void premain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            new Object().run();
        }
        private static Object test() { return null; }
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
    }
    "#};

        let line = 13u32;
        let raw = src.lines().nth(13).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);

        let mut members: Vec<String> = ctx
            .current_class_members
            .keys()
            .map(|k| {
                let m = &ctx.current_class_members[k];
                format!(
                    "{} static={} private={} method={}",
                    k,
                    m.is_static(),
                    m.is_private(),
                    m.is_method()
                )
            })
            .collect();
        members.sort();
        insta::assert_snapshot!("real_agent_members", members.join("\n"));
        insta::assert_snapshot!("real_agent_location", format!("{:?}", ctx.location));
        insta::assert_snapshot!("real_agent_enclosing", format!("{:?}", ctx.enclosing_class));
    }

    #[test]
    fn test_snapshot_real_agent_root() {
        let src = indoc::indoc! {r#"
    package org.cubewhy.relx;
    public class Agent {
        private static Object inst;
        public static void premain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            new Object().run();
        }
        private static Object test() { return null; }
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
    }
    "#};

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        // Just snapshot top-level structure (2 levels deep)
        let mut top = String::new();
        let mut rc = root.walk();
        for child in root.children(&mut rc) {
            top.push_str(&format!(
                "{} [{}-{}]\n",
                child.kind(),
                child.start_byte(),
                child.end_byte()
            ));
            let mut cc = child.walk();
            for grandchild in child.children(&mut cc) {
                top.push_str(&format!(
                    "  {} [{}-{}]\n",
                    grandchild.kind(),
                    grandchild.start_byte(),
                    grandchild.end_byte()
                ));
            }
        }
        insta::assert_snapshot!("real_agent_root", top);
    }

    #[test]
    fn test_class_members_visible_when_whole_file_is_error() {
        let src = indoc::indoc! {r#"
    package org.cubewhy.relx;
    public class Agent {
        private static Object inst;
        public static void premain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            new Object().run();
        }
        private static Object test() { return null; }
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
    }
    "#};
        let line = 13u32;
        let raw = src.lines().nth(13).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.current_class_members.contains_key("test"),
            "test() should be visible, members: {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
        assert!(
            ctx.current_class_members.contains_key("agentmain"),
            "agentmain() should be visible, members: {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
        assert!(
            ctx.current_class_members.contains_key("premain"),
            "premain() should be visible, members: {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
        assert_eq!(ctx.enclosing_class.as_deref(), Some("Agent"));
    }

    #[test]
    fn test_snapshot_test_after_agentmain() {
        let src = indoc::indoc! {r#"
    package org.cubewhy.relx;
    public class Agent {
        private static Object inst;
        public static void premain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            new Object().run();
        }
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
        private static Object test() { return null; }
    }
    "#};

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let mut top = String::new();
        let mut rc = root.walk();
        for child in root.children(&mut rc) {
            top.push_str(&format!(
                "{} [{}-{}]\n",
                child.kind(),
                child.start_byte(),
                child.end_byte()
            ));
            let mut cc = child.walk();
            for grandchild in child.children(&mut cc) {
                top.push_str(&format!(
                    "  {} [{}-{}] {:?}\n",
                    grandchild.kind(),
                    grandchild.start_byte(),
                    grandchild.end_byte(),
                    &src[grandchild.start_byte()
                        ..grandchild.end_byte().min(grandchild.start_byte() + 30)]
                ));
            }
        }
        insta::assert_snapshot!(top);

        let line = 12u32;
        let raw = src.lines().nth(12).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);
        let mut members: Vec<String> = ctx
            .current_class_members
            .keys()
            .map(|k| k.to_string())
            .collect();
        members.sort();
        insta::assert_snapshot!(members.join("\n"));
    }

    #[test]
    fn test_snapshot_class_body_structure() {
        let src = indoc::indoc! {r#"
    package org.cubewhy.relx;
    public class Agent {
        private static Object inst;
        public static void premain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            new Object().run();
        }
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
        private static Object test() { return null; }
    }
    "#};

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();

        fn print_node(node: tree_sitter::Node, src: &str, indent: usize, out: &mut String) {
            let pad = " ".repeat(indent * 2);
            let text: String = src[node.start_byte()..node.end_byte()]
                .chars()
                .take(30)
                .collect();
            out.push_str(&format!(
                "{}{} [{}-{}] {:?}\n",
                pad,
                node.kind(),
                node.start_byte(),
                node.end_byte(),
                text
            ));
            if indent < 4 {
                // 只展开4层
                let mut c = node.walk();
                for child in node.children(&mut c) {
                    print_node(child, src, indent + 1, out);
                }
            }
        }

        let mut out = String::new();
        print_node(tree.root_node(), src, 0, &mut out);
        insta::assert_snapshot!(out);
    }

    #[test]
    fn test_snapshot_agentmain_block() {
        let src = indoc::indoc! {r#"
    package org.cubewhy.relx;
    public class Agent {
        private static Object inst;
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
        private static Object test() { return null; }
    }
    "#};

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();

        // Just print everything under agentmain's block
        fn print_flat(node: tree_sitter::Node, src: &str, out: &mut String) {
            let text: String = src[node.start_byte()..node.end_byte()]
                .chars()
                .take(30)
                .collect();
            out.push_str(&format!(
                "{} [{}-{}] {:?}\n",
                node.kind(),
                node.start_byte(),
                node.end_byte(),
                text
            ));
            let mut c = node.walk();
            for child in node.children(&mut c) {
                out.push_str(&format!(
                    "  {} [{}-{}] {:?}\n",
                    child.kind(),
                    child.start_byte(),
                    child.end_byte(),
                    &src[child.start_byte()..child.end_byte().min(child.start_byte() + 25)]
                ));
            }
        }

        // Find agentmain block by offset range [292-453] from previous snapshot
        let block = tree
            .root_node()
            .named_descendant_for_byte_range(292, 293)
            .unwrap();
        // walk up to block
        let mut cur = block;
        while cur.kind() != "block" {
            cur = cur.parent().unwrap();
        }

        let mut out = String::new();
        print_flat(cur, src, &mut out);
        insta::assert_snapshot!(out);
    }

    #[test]
    fn test_class_members_visible_when_error_node_present() {
        let src = indoc::indoc! {r#"
    package org.cubewhy.relx;
    public class Agent {
        private static Object inst;
        public static void agentmain(String args, Object inst) throws Exception {
            Agent.inst = inst;
            var proxy = new Object();
            proxy.run();
            Agent.inst = 
        }
        private static Object test() { return null; }
    }
    "#};
        let line = 7u32;
        let raw = src.lines().nth(7).unwrap();
        let col = raw.len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.current_class_members.contains_key("test"),
            "test() should be extracted even when misread as local_variable_declaration, members: {:?}",
            ctx.current_class_members.keys().collect::<Vec<_>>()
        );
        assert!(
            ctx.current_class_members.get("test").unwrap().is_static(),
            "test() should be marked static"
        );
    }

    #[test]
    fn test_chained_method_call_dot_is_member_access() {
        // RealMain.getInstance().| → MemberAccess, not StaticAccess
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            RealMain.getInstance().
        }
    }
    "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("getInstance().").map(|c| (i as u32, c as u32 + 14)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { receiver_expr, member_prefix, .. }
                if receiver_expr == "RealMain.getInstance()" && member_prefix.is_empty()
            ),
            "RealMain.getInstance().| should be MemberAccess, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_chained_method_call_dot_with_prefix_is_member_access() {
        // RealMain.getInstance().ge| → MemberAccess{prefix="ge"}
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            RealMain.getInstance().ge
        }
    }
    "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("getInstance().ge")
                    .map(|c| (i as u32, c as u32 + 16))
            })
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { receiver_expr, member_prefix, .. }
                if receiver_expr == "RealMain.getInstance()" && member_prefix == "ge"
            ),
            "RealMain.getInstance().ge| should be MemberAccess{{prefix=ge}}, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_uppercase_receiver_method_invocation_is_member_access() {
        // Uppercase.method().| — receiver is method_invocation, not identifier
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            Uppercase.method().
        }
    }
    "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("method().").map(|c| (i as u32, c as u32 + 9)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::MemberAccess { receiver_expr, .. }
                if receiver_expr == "Uppercase.method()"
            ),
            "Uppercase.method().| should be MemberAccess, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_formal_param_type_position_is_type_annotation() {
        let src = indoc::indoc! {r#"
    class A {
        public static void main(String[] args) {}
    }
    "#};
        let line = 1u32;
        let raw = src.lines().nth(1).unwrap();
        let col = raw.find("String").unwrap() as u32 + 3; // cursor mid "String"
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::TypeAnnotation { .. }),
            "cursor on param type should be TypeAnnotation, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_formal_param_name_position_is_variable_name() {
        let src = indoc::indoc! {r#"
    class A {
        public static void main(String[] args) {}
    }
    "#};
        let line = 1u32;
        let raw = src.lines().nth(1).unwrap();
        let args_col = raw.find("args").unwrap() as u32 + 2;
        let ctx = at(src, line, args_col);
        assert!(
            matches!(ctx.location, CursorLocation::VariableName { .. }),
            "cursor on param name should be VariableName, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_formal_param_name_has_type_name() {
        let src = indoc::indoc! {r#"
    class A {
        public static void main(String[] args) {}
    }
    "#};
        let line = 1u32;
        let raw = src.lines().nth(1).unwrap();
        let args_col = raw.find("args").unwrap() as u32 + 2;
        let ctx = at(src, line, args_col);
        assert!(
            matches!(&ctx.location, CursorLocation::VariableName { type_name } if type_name == "String[]"),
            "param name location should carry type 'String[]', got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_var_name_position_is_variable_name() {
        let src = indoc::indoc! {r#"
    class A {
        public static void main() {
            String aCopy
        }
    }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let acopy_start = raw.find("aCopy").unwrap() as u32;
        let col = acopy_start + 2;
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::VariableName { .. }),
            "var name position should be VariableName, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_var_name_position_has_type_name() {
        let src = indoc::indoc! {r#"
    class A {
        public static void main() {
            String aCopy
        }
    }
    "#};
        let line = 2u32;
        let raw = src.lines().nth(2).unwrap();
        let acopy_start = raw.find("aCopy").unwrap() as u32;
        let col = acopy_start + 2;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::VariableName { type_name } if type_name == "String"),
            "var name location should carry type 'String', got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_new_followed_by_comment_is_constructor() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            new // comment
        }
    }
    "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("new ").map(|c| (i as u32, c as u32 + 4)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::ConstructorCall { .. }),
            "new followed by comment should give ConstructorCall, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_new_at_end_of_line_is_constructor() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            new
        }
    }
    "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("new").map(|c| (i as u32, c as u32 + 3)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::ConstructorCall { .. }),
            "new at end of line should give ConstructorCall, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_expression_after_syntax_error_line() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            String str = "hello":
            s
        }
    }
    "#};
        let line = 3u32;
        let col = src.lines().nth(3).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(&ctx.location, CursorLocation::Expression { prefix } if prefix == "s"),
            "expression after syntax error should still parse, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_snapshot_syntax_error_colon_ast() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            String str = "hello":
            var b1 = str;
            b
        }
    }
    "#};
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();

        fn dump(node: tree_sitter::Node, src: &str, indent: usize, out: &mut String) {
            let pad = "  ".repeat(indent);
            let text: String = src[node.start_byte()..node.end_byte()]
                .chars()
                .take(30)
                .collect();
            out.push_str(&format!(
                "{}{} [{}-{}] {:?}\n",
                pad,
                node.kind(),
                node.start_byte(),
                node.end_byte(),
                text
            ));
            let mut c = node.walk();
            for child in node.children(&mut c) {
                dump(child, src, indent + 1, out);
            }
        }

        let mut out = String::new();
        dump(tree.root_node(), src, 0, &mut out);
        insta::assert_snapshot!(out);
    }

    #[test]
    fn test_locals_extracted_after_syntax_error_line() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            String str = "hello":
            var b1 = str;
            b
        }
    }
    "#};
        let line = 4u32;
        let col = src.lines().nth(4).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            ctx.local_variables.iter().any(|v| v.name.as_ref() == "str"),
            "str should be in locals: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(
            ctx.local_variables.iter().any(|v| v.name.as_ref() == "b1"),
            "b1 should be in locals: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| v.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_fqn_type_in_var_decl_triggers_import() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            java.util.A
        }
    }
    "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("java.util.A").map(|c| (i as u32, c as u32 + 11)))
            .unwrap();
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix.contains("java.util")
            ) || matches!(
                &ctx.location,
                CursorLocation::MemberAccess { receiver_expr, .. } if receiver_expr.contains("java.util")
            ),
            "java.util.A should give Import or MemberAccess with java.util receiver, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_snapshot_fqn_type_injection() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            java.util.A
        }
    }
    "#};
        let offset = src.find("java.util.A").unwrap() + 11;
        let extractor = JavaContextExtractor::new(src, offset);

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let cursor_node = extractor.find_cursor_node(tree.root_node());

        let injected = build_injected_source(&extractor, cursor_node);
        insta::assert_snapshot!(injected);
    }

    #[test]
    fn test_snapshot_fqn_type_injection_v2() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            java.util.A
        }
    }
    "#};
        let offset = src.find("java.util.A").unwrap() + 11;
        let extractor = JavaContextExtractor::new(src, offset);

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let cursor_node = extractor.find_cursor_node(tree.root_node());

        let injected = build_injected_source(&extractor, cursor_node);
        insta::assert_snapshot!("injected_v2", injected);

        // 同时 snapshot 注入后的 AST
        let mut parser2 = tree_sitter::Parser::new();
        parser2
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree2 = parser2.parse(&injected, None).unwrap();

        fn dump(node: tree_sitter::Node, src: &str, indent: usize, out: &mut String) {
            let pad = "  ".repeat(indent);
            let text: String = src[node.start_byte()..node.end_byte()]
                .chars()
                .take(40)
                .collect();
            out.push_str(&format!(
                "{}{} [{}-{}] {:?}\n",
                pad,
                node.kind(),
                node.start_byte(),
                node.end_byte(),
                text
            ));
            let mut c = node.walk();
            for child in node.children(&mut c) {
                dump(child, src, indent + 1, out);
            }
        }

        let mut ast = String::new();
        dump(tree2.root_node(), &injected, 0, &mut ast);
        insta::assert_snapshot!("injected_v2_ast", ast);
    }

    #[test]
    fn test_injection_import_without_semicolon() {
        let src = indoc::indoc! {r#"
        import java.l
        class A {}
        "#};
        // 模拟光标在 'java.l' 后面
        let line = 0u32;
        let col = 13u32;
        let ctx = at(src, line, col);

        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "java.l"
            ),
            "__KIRO__ should be stripped from import prefix, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_import_prefix_does_not_include_next_line() {
        let src = indoc::indoc! {r#"
        import org.cubewhy.
        // some comment
        class A {}
    "#};
        let line = 0u32;
        let col = src.lines().next().unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "org.cubewhy."
            ),
            "import prefix should not bleed into next line, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_import_multiline_prefix_flattened() {
        let src = indoc::indoc! {r#"
        import
            org.cubewhy.
        class A {}
    "#};
        // 光标在 org.cubewhy. 末尾
        let line = 1u32;
        let col = src.lines().nth(1).unwrap().len() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "org.cubewhy."
            ),
            "multiline import should be flattened, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_import_with_inline_comment_ignored() {
        let src = indoc::indoc! {r#"
        import org.; // some comment
        class A {}
    "#};
        let line = 0u32;
        let col = src.lines().next().unwrap().find(';').unwrap() as u32;
        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "org."
            ),
            "inline comment should be stripped from import prefix, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_constructor_incomplete_with_newlines_swallowed() {
        // Reproduces the issue where "Ar\n\n System..." is captured as the class prefix
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new Ar
                
                System.out.println("hello");
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("new Ar").map(|c| (i as u32, c as u32 + 6)))
            .unwrap();

        let ctx = at(src, line, col);

        assert!(
            matches!(
                &ctx.location,
                CursorLocation::ConstructorCall { class_prefix, .. }
                if class_prefix == "Ar"
            ),
            "Should extract 'Ar' cleanly without newlines, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_constructor_with_newline_gap_triggers_injection() {
        // Reproduces the issue where "System.out.println" is captured as class_prefix
        // because tree-sitter greedily consumes the next line.
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new 
        
                System.out.println(new ArrayList());
            }
        }
        "#};
        // cursor is after "new "
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("new ").map(|c| (i as u32, c as u32 + 4)))
            .unwrap();

        let ctx = at(src, line, col);

        // The fix should cause handle_constructor to return Unknown,
        // triggering injection of "new __KIRO__()", which results in an empty prefix.
        match &ctx.location {
            CursorLocation::ConstructorCall { class_prefix, .. } => {
                assert!(
                    class_prefix.is_empty(),
                    "Expected empty class_prefix (from injection), but got '{}'",
                    class_prefix
                );
            }
            _ => panic!("Expected ConstructorCall, got {:?}", ctx.location),
        }
    }

    #[test]
    fn test_import_with_whitespace_and_newlines() {
        // 验证 AST 遍历能自动忽略空白字符，将分散的 token 拼合
        let src = indoc::indoc! {r#"
        import java.
            util.
            List;
        class A {}
        "#};
        // 光标在 List 后面
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("List").map(|c| (i as u32, c as u32 + 4)))
            .unwrap();

        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "java.util.List"
            ),
            "Should flatten multiline import ignoring whitespace, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_import_with_block_comments() {
        // 验证 AST 遍历能准确跳过 block_comment 节点
        let src = indoc::indoc! {r#"
        import java./* comment */util.List;
        class A {}
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("List").map(|c| (i as u32, c as u32 + 4)))
            .unwrap();

        let ctx = at(src, line, col);
        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "java.util.List"
            ),
            "Should ignore inline block comments, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_import_incomplete_truncated() {
        // 验证光标截断逻辑：光标在 'u' 之后，应该只捕获到 'u'，忽略后面的 'til'
        let src = "import java.util;";
        // 光标在 'java.u|til'
        let col = "import java.u".len() as u32;
        let ctx = at(src, 0, col);

        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "java.u"
            ),
            "Should truncate text at cursor position, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_import_asterisk() {
        // 验证 * 号被视为有效 token 收集
        let src = "import java.util.*;";
        let col = "import java.util.*".len() as u32;
        let ctx = at(src, 0, col);

        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "java.util.*"
            ),
            "Should include asterisk, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_extractor_new_no_lifetime() {
        // 验证 JavaContextExtractor 可以脱离 source 字符串独立存活
        let ctx = {
            let src = String::from("class A { void f() { int x = 1; } }");
            JavaContextExtractor::new(src, 22) // 离开作用域 src 已 move 进去
        };
        // ctx 仍然有效
        assert_eq!(ctx.source_str().len(), 35);
        assert!(!ctx.is_in_comment());
    }

    #[test]
    fn test_extractor_for_indexing() {
        let ctx = JavaContextExtractor::for_indexing("class A {}");
        assert_eq!(ctx.offset, 0);
        assert_eq!(ctx.source_str(), "class A {}");
    }

    #[test]
    fn test_extractor_byte_slice() {
        let src = "class A { void f() {} }";
        let ctx = JavaContextExtractor::new(src, 0);
        // tree-sitter 给出字节偏移，byte_slice 应正确切片
        assert_eq!(ctx.byte_slice(0, 5), "class");
        assert_eq!(ctx.byte_slice(6, 7), "A");
    }

    #[test]
    fn test_extractor_is_in_comment_true() {
        let src = "class A { // comment\n void f() {} }";
        // offset 在 comment 内部
        let col = src.find("//").unwrap() + 5;
        let ctx = JavaContextExtractor::new(src, col);
        assert!(ctx.is_in_comment());
    }

    #[test]
    fn test_extractor_is_in_comment_false() {
        let src = "class A { void f() {} }";
        let ctx = JavaContextExtractor::new(src, 10);
        assert!(!ctx.is_in_comment());
    }

    #[test]
    fn test_extractor_with_rope_reuses_rope() {
        // with_rope 构造不重复建 Rope（性能路径）
        let src = String::from("package a; class B {}");
        let rope = ropey::Rope::from_str(&src);
        let line_count = rope.len_lines();
        let ctx = JavaContextExtractor::with_rope(src, 0, rope);
        assert_eq!(ctx.rope.len_lines(), line_count);
    }

    #[test]
    fn test_extractor_node_text_multibyte() {
        // 含 CJK 字符的类名，node_text 应正确返回
        let src = "class 测试类 {}";
        let ctx = JavaContextExtractor::new(src, 0);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        // 仅确保不 panic，multibyte 处理正确
        let _ = parser.parse(ctx.source_str(), None);
    }
}
