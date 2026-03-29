use ropey::Rope;
use std::sync::Arc;
use tracing::debug;
use tree_sitter::{Node, Parser, Query};

use super::Language;
use super::ts_utils::{capture_text, run_query};
use crate::completion::CompletionCandidate;
use crate::completion::provider::CompletionProvider;
use crate::index::{ClassMetadata, ClassOrigin, IndexView, NameTable};
use crate::semantic::{CursorLocation, LocalVar, SemanticContext, types::type_name::TypeName};

#[derive(Debug)]
pub struct KotlinLanguage;

static KOTLIN_COMPLETION_PROVIDERS: [&dyn CompletionProvider; 0] = [];

impl Language for KotlinLanguage {
    fn id(&self) -> &'static str {
        "kotlin"
    }

    fn supports(&self, language_id: &str) -> bool {
        language_id == "kotlin"
    }

    fn make_parser(&self) -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_kotlin::LANGUAGE.into())
            .expect("failed to load kotlin grammar");
        parser
    }

    fn file_extensions(&self) -> &[&str] {
        &["kt", "kts"]
    }

    fn top_level_type_kinds(&self) -> &[&str] {
        &["class_declaration", "object_declaration"]
    }

    fn completion_providers(&self) -> &[&'static dyn CompletionProvider] {
        &KOTLIN_COMPLETION_PROVIDERS
    }

    fn post_process_candidates(
        &self,
        mut candidates: Vec<CompletionCandidate>,
        _ctx: &SemanticContext,
    ) -> Vec<CompletionCandidate> {
        for c in &mut candidates {
            let name = c.label.as_ref();
            if name.len() > 3
                && (name.starts_with("get") || name.starts_with("set"))
                && name.chars().nth(3).is_some_and(|c| c.is_uppercase())
            {
                c.score -= 5.0;
            }
        }
        candidates.sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates
    }

    fn extract_package_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
    ) -> Option<Arc<str>> {
        crate::salsa_queries::kotlin::extract_kotlin_package(db, file)
    }

    fn extract_imports_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
    ) -> Vec<Arc<str>> {
        crate::salsa_queries::kotlin::extract_kotlin_imports(db, file)
    }

    fn extract_classes_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
    ) -> Vec<ClassMetadata> {
        crate::salsa_queries::kotlin::parse_kotlin_classes(db, file)
    }

    fn extract_classes_with_index_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        _origin: &ClassOrigin,
        _name_table: Option<Arc<NameTable>>,
        _view: Option<&IndexView>,
    ) -> Vec<ClassMetadata> {
        self.extract_classes_salsa(db, file)
    }

    fn discover_internal_names(
        &self,
        source: &str,
        _tree: Option<&tree_sitter::Tree>,
    ) -> Vec<Arc<str>> {
        crate::index::source::discover_kotlin_names(source)
    }

    fn extract_classes_from_source(
        &self,
        source: &str,
        origin: &ClassOrigin,
        _tree: Option<&tree_sitter::Tree>,
        _name_table: Option<Arc<NameTable>>,
        _view: Option<&IndexView>,
    ) -> Vec<ClassMetadata> {
        crate::index::source::parse_kotlin_source(source, origin.clone())
    }

    // ========================================================================
    // Salsa-based methods for incremental computation
    // ========================================================================

    fn extract_completion_context_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        line: u32,
        character: u32,
        trigger_char: Option<char>,
    ) -> Option<Arc<crate::salsa_queries::CompletionContextData>> {
        Some(
            crate::salsa_queries::kotlin::extract_kotlin_completion_context(
                db,
                file,
                line,
                character,
                trigger_char,
            ),
        )
    }

    fn resolve_symbol_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        line: u32,
        character: u32,
    ) -> Option<Arc<crate::salsa_queries::ResolvedSymbolData>> {
        crate::salsa_queries::kotlin::resolve_kotlin_symbol(db, file, line, character)
    }

    fn compute_inlay_hints_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        range: tower_lsp::lsp_types::Range,
    ) -> Option<Arc<Vec<crate::salsa_queries::InlayHintData>>> {
        Some(crate::salsa_queries::kotlin::compute_kotlin_inlay_hints(
            db,
            file,
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
        ))
    }

    fn is_local_variable_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        symbol_name: Arc<str>,
        offset: usize,
    ) -> bool {
        crate::salsa_queries::kotlin::is_kotlin_local_variable(db, file, symbol_name, offset)
    }

    fn infer_variable_type_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        decl_offset: usize,
    ) -> Option<Arc<str>> {
        crate::salsa_queries::kotlin::infer_kotlin_variable_type(db, file, decl_offset)
    }
}

pub(crate) fn extract_kotlin_semantic_context_for_test(
    source: &str,
    line: u32,
    character: u32,
    trigger_char: Option<char>,
) -> Option<SemanticContext> {
    let rope = Rope::from_str(source);
    let offset = crate::language::rope_utils::rope_line_col_to_offset(&rope, line, character)?;
    let mut parser = KotlinLanguage.make_parser();
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    Some(KotlinContextExtractor::new_with_rope(source, offset, rope).extract(root, trigger_char))
}

struct KotlinContextExtractor<'s> {
    source: &'s str,
    bytes: &'s [u8],
    offset: usize,
    rope: Rope,
}

impl<'s> KotlinContextExtractor<'s> {
    fn new_with_rope(source: &'s str, offset: usize, rope: Rope) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            offset,
            rope,
        }
    }

    fn extract(self, root: Node, trigger_char: Option<char>) -> SemanticContext {
        let cursor_node =
            root.named_descendant_for_byte_range(self.offset.saturating_sub(1), self.offset);

        let (location, query) = self.determine_location(cursor_node, trigger_char);
        let local_variables = self.extract_locals(root, cursor_node);
        let enclosing_class = self.extract_enclosing_class(cursor_node);
        let enclosing_package = self.extract_package(root);
        let enclosing_internal_name = match (&enclosing_package, &enclosing_class) {
            (Some(pkg), Some(cls)) => Some(Arc::from(format!("{}/{}", pkg, cls).as_str())),
            (None, Some(cls)) => Some(Arc::clone(cls)),
            _ => None,
        };
        let existing_imports = self.extract_imports(root);

        SemanticContext::new(
            location,
            query,
            local_variables,
            enclosing_class,
            enclosing_internal_name,
            enclosing_package,
            existing_imports,
        )
    }

    fn determine_location(
        &self,
        cursor_node: Option<Node>,
        trigger_char: Option<char>,
    ) -> (CursorLocation, String) {
        let node = match cursor_node {
            Some(n) => n,
            None => return self.fallback_location(trigger_char),
        };

        let mut current = node;
        loop {
            match current.kind() {
                "import_header" => return self.handle_import(current),
                "navigation_expression" => return self.handle_navigation(current),
                "navigation_suffix" => {
                    if let Some(nav) = current.parent()
                        && nav.kind() == "navigation_expression"
                    {
                        return self.handle_navigation(nav);
                    }
                }

                "simple_identifier" | "identifier" => {
                    // Traverse upwards through all ancestors until a semantically meaningful node is found.
                    let mut ancestor = current;
                    loop {
                        ancestor = match ancestor.parent() {
                            Some(p) => p,
                            None => break,
                        };
                        match ancestor.kind() {
                            // import_list > import_header > identifier > simple_identifier
                            // The cursor can be at any depth; search upwards for import_header.
                            "import_header" => return self.handle_import(ancestor),
                            "navigation_expression" => return self.handle_navigation(ancestor),
                            "navigation_suffix" => {
                                if let Some(nav) = ancestor.parent()
                                    && nav.kind() == "navigation_expression"
                                {
                                    return self.handle_navigation(nav);
                                }
                            }
                            // Stop when encountering a statement/function body
                            "statements"
                            | "function_body"
                            | "class_body"
                            | "source_file"
                            | "function_declaration" => break,
                            _ => {}
                        }
                    }
                    let text = self.node_text(current).to_string();
                    return (
                        CursorLocation::Expression {
                            prefix: text.clone(),
                        },
                        text,
                    );
                }

                _ => {}
            }

            match current.parent() {
                Some(p) => current = p,
                None => break,
            }
        }

        self.fallback_location(trigger_char)
    }

    fn handle_import(&self, node: Node) -> (CursorLocation, String) {
        // The text in the import_header is "import org.example.Foo".
        let text = self.node_text(node);
        let prefix = text.trim_start_matches("import").trim().to_string();
        let query = prefix.rsplit('.').next().unwrap_or("").to_string();
        (CursorLocation::Import { prefix }, query)
    }

    fn handle_navigation(&self, node: Node) -> (CursorLocation, String) {
        // navigation_expression:
        //   child[0] = receiver (simple_identifier / this_expression / another nav_expr)
        //   child[1] = navigation_suffix
        //                child[0] = "." or "?."
        //                child[1] = simple_identifier (member name)

        // Retrieve member names from navigation_suffix
        let suffix = node
            .named_children(&mut node.walk())
            .find(|n| n.kind() == "navigation_suffix");

        let member_prefix = match suffix {
            Some(s) => {
                // Child nodes of navigation_suffix: skip "." / "?.", take the first named child node
                // Note: get/set keywords are unnamed nodes here, use child() to retrieve them
                let mut walker = s.walk();
                let member_node = s
                    .children(&mut walker)
                    .find(|n| n.kind() != "." && n.kind() != "?.");
                member_node
                    .map(|n| self.node_text(n).to_string())
                    .unwrap_or_default()
            }
            None => String::new(),
        };

        // Retrieve the receiver (the first named child node)
        let receiver_node = node.named_child(0);
        let receiver_expr = receiver_node
            .map(|n| self.node_text(n).to_string())
            .unwrap_or_default();

        if let Some(recv) = receiver_node {
            let recv_text = self.node_text(recv);
            // Uppercase start → Class name → Static access
            if recv_text.chars().next().is_some_and(|c| c.is_uppercase()) {
                let internal = recv_text.replace('.', "/");
                return (
                    CursorLocation::MemberAccess {
                        receiver_kind: crate::semantic::AccessReceiverKind::Type {
                            class_internal_name: Arc::from(internal.as_str()),
                        },
                        receiver_semantic_type: None,
                        receiver_type: Some(Arc::from(internal.as_str())),
                        member_prefix: member_prefix.clone(),
                        receiver_expr: recv_text.to_string(),
                        arguments: None,
                    },
                    member_prefix,
                );
            }
        }

        (
            CursorLocation::MemberAccess {
                receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: member_prefix.clone(),
                receiver_expr,
                arguments: None, // TODO: extract kotlin method argumants
            },
            member_prefix,
        )
    }

    fn fallback_location(&self, _trigger_char: Option<char>) -> (CursorLocation, String) {
        let line_idx = self
            .rope
            .byte_to_line(self.offset.min(self.source.len().saturating_sub(1)));
        let line_byte_start = self.rope.line_to_byte(line_idx);
        let safe_offset = self.offset.max(line_byte_start).min(self.source.len());
        let last_line = self.source[line_byte_start..safe_offset].trim();

        let normalized = last_line.replace("?.", ".");
        if let Some(dot_pos) = last_meaningful_dot(&normalized) {
            let receiver = normalized[..dot_pos].trim();
            let member_prefix = normalized[dot_pos + 1..].to_string();
            let is_type = receiver.chars().next().is_some_and(|c| c.is_uppercase());
            return if is_type {
                (
                    CursorLocation::MemberAccess {
                        receiver_kind: crate::semantic::AccessReceiverKind::Type {
                            class_internal_name: Arc::from(receiver.replace('.', "/").as_str()),
                        },
                        receiver_semantic_type: None,
                        receiver_type: Some(Arc::from(receiver.replace('.', "/").as_str())),
                        member_prefix: member_prefix.clone(),
                        receiver_expr: receiver.to_string(),
                        arguments: None,
                    },
                    member_prefix,
                )
            } else {
                (
                    CursorLocation::MemberAccess {
                        receiver_kind: crate::semantic::AccessReceiverKind::Unknown,
                        receiver_semantic_type: None,
                        receiver_type: None,
                        member_prefix: member_prefix.clone(),
                        receiver_expr: receiver.to_string(),
                        arguments: None,
                    },
                    member_prefix,
                )
            };
        }

        let query = last_line
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next_back()
            .unwrap_or("")
            .to_string();
        (
            CursorLocation::Expression {
                prefix: query.clone(),
            },
            query,
        )
    }

    fn extract_locals(&self, root: Node, cursor_node: Option<Node>) -> Vec<LocalVar> {
        let search_root = cursor_node
            .and_then(|n| {
                find_ancestor_any(
                    n,
                    &[
                        "function_declaration",
                        "function_literal",
                        "anonymous_function",
                    ],
                )
            })
            .unwrap_or(root);

        let mut vars = Vec::new();

        // AST structure:
        // property_declaration
        //   binding_pattern_kind (val/var)
        //   variable_declaration
        //     simple_identifier   <- variable name
        //     ":"
        //     user_type
        //       type_identifier   <- type name
        let decl_query = r#"
            (property_declaration
                (variable_declaration
                    (simple_identifier) @name
                    (user_type
                        (type_identifier) @type)))
        "#;

        if let Ok(q) = Query::new(&tree_sitter_kotlin::LANGUAGE.into(), decl_query) {
            let name_idx = q.capture_index_for_name("name").unwrap();
            let type_idx = q.capture_index_for_name("type").unwrap();

            for captures in run_query(&q, search_root, self.bytes, Some(0..self.offset)) {
                let name = capture_text(&captures, name_idx, self.bytes);
                let ty = capture_text(&captures, type_idx, self.bytes);
                if let (Some(name), Some(ty)) = (name, ty) {
                    vars.push(LocalVar {
                        name: Arc::from(name),
                        type_internal: TypeName::new(kotlin_type_to_internal(ty)),
                        decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                        init_expr: None,
                    });
                }
            }
        }

        // foreach variable
        let for_query = r#"
            (for_statement
                (variable_declaration
                    (simple_identifier) @name))
        "#;
        if let Ok(q) = Query::new(&tree_sitter_kotlin::LANGUAGE.into(), for_query) {
            let name_idx = q.capture_index_for_name("name").unwrap();
            for captures in run_query(&q, search_root, self.bytes, Some(0..self.offset)) {
                if let Some(name) = capture_text(&captures, name_idx, self.bytes) {
                    vars.push(LocalVar {
                        name: Arc::from(name),
                        type_internal: TypeName::new("java/lang/Object"),
                        decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                        init_expr: None,
                    });
                }
            }
        }

        vars.extend(self.extract_params(cursor_node));
        vars
    }

    fn extract_params(&self, cursor_node: Option<Node>) -> Vec<LocalVar> {
        let func = match cursor_node.and_then(|n| {
            find_ancestor_any(
                n,
                &[
                    "function_declaration",
                    "function_literal",
                    "anonymous_function",
                ],
            )
        }) {
            Some(f) => f,
            None => return vec![],
        };

        // AST structure
        // function_value_parameters
        //   parameter
        //     simple_identifier   <- parameter name
        //     ":"
        //     user_type
        //       type_identifier   <- parameter type
        let param_query = r#"
            (function_value_parameters
                (parameter
                    (simple_identifier) @name
                    (user_type
                        (type_identifier) @type)))
        "#;

        let q = match Query::new(&tree_sitter_kotlin::LANGUAGE.into(), param_query) {
            Ok(q) => q,
            Err(e) => {
                debug!("kotlin param query error: {}", e);
                return vec![];
            }
        };

        let name_idx = q.capture_index_for_name("name").unwrap();
        let type_idx = q.capture_index_for_name("type").unwrap();

        run_query(&q, func, self.bytes, None)
            .into_iter()
            .filter_map(|captures| {
                let name = capture_text(&captures, name_idx, self.bytes)?;
                let ty = capture_text(&captures, type_idx, self.bytes)?;
                Some(LocalVar {
                    name: Arc::from(name),
                    type_internal: TypeName::new(kotlin_type_to_internal(ty)),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                })
            })
            .collect()
    }

    fn extract_enclosing_class(&self, cursor_node: Option<Node>) -> Option<Arc<str>> {
        // Actual AST structure (classes and objects are the same):
        // class_declaration / object_declaration
        //   "class" / "object"
        //   type_identifier   <- class name
        //   class_body
        let class_node = cursor_node.and_then(|n| {
            find_ancestor_any(
                n,
                &[
                    "class_declaration",
                    "object_declaration",
                    "companion_object",
                ],
            )
        })?;

        // Find the first direct child node of type_identifier
        let mut walker = class_node.walk();
        class_node
            .children(&mut walker)
            .find(|n| n.kind() == "type_identifier")
            .map(|n| Arc::from(self.node_text(n)))
    }

    fn extract_imports(&self, root: Node) -> Vec<Arc<str>> {
        // AST structure:
        // import_list
        //   import_header
        //     "import"
        //     identifier  ("org.example.Foo")
        let query_src = r#"(import_header) @import"#;
        let q = match Query::new(&tree_sitter_kotlin::LANGUAGE.into(), query_src) {
            Ok(q) => q,
            Err(_) => return vec![],
        };
        let idx = q.capture_index_for_name("import").unwrap();

        run_query(&q, root, self.bytes, None)
            .into_iter()
            .filter_map(|captures| {
                let text = capture_text(&captures, idx, self.bytes)?;
                let cleaned: Arc<str> = text.trim_start_matches("import").trim().into();
                if cleaned.as_ref().is_empty() {
                    None
                } else {
                    Some(cleaned)
                }
            })
            .collect()
    }

    fn extract_package(&self, root: Node) -> Option<Arc<str>> {
        let q_src = r#"(package_header (identifier) @pkg)"#;
        let q = Query::new(&tree_sitter_kotlin::LANGUAGE.into(), q_src).ok()?;
        let idx = q.capture_index_for_name("pkg")?;
        let results = run_query(&q, root, self.bytes, None);
        let pkg = results
            .first()
            .and_then(|caps| capture_text(caps, idx, self.bytes))?;
        Some(Arc::from(pkg.replace('.', "/").as_str()))
    }

    fn node_text(&self, node: Node) -> &str {
        node.utf8_text(self.bytes).unwrap_or("")
    }
}

fn find_ancestor_any<'a>(mut node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    loop {
        node = node.parent()?;
        if kinds.contains(&node.kind()) {
            return Some(node);
        }
    }
}

fn last_meaningful_dot(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut last = None;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '.' if depth == 0 => last = Some(i),
            _ => {}
        }
    }
    last
}

pub fn kotlin_type_to_internal(ty: &str) -> &str {
    let ty = ty.trim_end_matches('?');
    match ty {
        "String" => "java/lang/String",
        "Int" => "int",
        "Long" => "long",
        "Double" => "double",
        "Float" => "float",
        "Boolean" => "boolean",
        "Byte" => "byte",
        "Short" => "short",
        "Char" => "char",
        "Unit" => "void",
        "Any" => "java/lang/Object",
        "Number" => "java/lang/Number",
        "List" => "java/util/List",
        "MutableList" => "java/util/ArrayList",
        "Map" => "java/util/Map",
        "MutableMap" => "java/util/HashMap",
        "Set" => "java/util/Set",
        "MutableSet" => "java/util/HashSet",
        "Collection" => "java/util/Collection",
        "Iterable" => "java/lang/Iterable",
        "Sequence" => "kotlin/sequences/Sequence",
        "Pair" => "kotlin/Pair",
        "Triple" => "kotlin/Triple",
        "File" => "java/io/File",
        "Path" => "java/nio/file/Path",
        "Exception" => "java/lang/Exception",
        "RuntimeException" => "java/lang/RuntimeException",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::test_helpers::completion_context_from_source;
    use crate::semantic::context::CursorLocation;

    fn at(src: &str, line: u32, col: u32) -> SemanticContext {
        completion_context_from_source("kotlin", src, line, col, None)
    }

    #[test]
    fn test_import() {
        let src = "import org.example.Foo\n";
        let ctx = at(src, 0, 22);
        assert!(
            matches!(ctx.location, CursorLocation::Import { .. }),
            "{:?}",
            ctx.location
        );
    }

    #[test]
    fn test_member_access_basic() {
        let src = "fun main() {\n    someList.get\n}\n";
        let ctx = at(src, 1, 16);
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
    fn test_member_access_safe_call() {
        let src = "fun main() {\n    nullable?.length\n}\n";
        let ctx = at(src, 1, 20);
        assert!(
            matches!(ctx.location, CursorLocation::MemberAccess { .. }),
            "{:?}",
            ctx.location
        );
        if let CursorLocation::MemberAccess { member_prefix, .. } = &ctx.location {
            assert_eq!(member_prefix, "length");
        }
    }

    #[test]
    fn test_val_local_extracted() {
        let src = "fun main() {\n    val items: List<String> = listOf()\n    items.get\n}\n";
        let ctx = at(src, 2, 13);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "items"),
            "locals: {:?}",
            ctx.local_variables
        );
        let ty = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "items")
            .map(|v| v.type_internal.erased_internal());
        assert_eq!(ty, Some("java/util/List"));
    }

    #[test]
    fn test_var_local_extracted() {
        let src = "fun main() {\n    var count: Int = 0\n    count.\n}\n";
        let ctx = at(src, 2, 11);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "count"),
            "locals: {:?}",
            ctx.local_variables
        );
    }

    #[test]
    fn test_function_params_extracted() {
        let src = "fun process(input: String, count: Int) {\n    input.length\n}\n";
        let ctx = at(src, 1, 17);
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "input"),
            "locals: {:?}",
            ctx.local_variables
        );
        assert!(
            ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "count"),
            "locals: {:?}",
            ctx.local_variables
        );
        let ty = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == "input")
            .map(|v| v.type_internal.erased_internal());
        assert_eq!(ty, Some("java/lang/String"));
    }

    #[test]
    fn test_enclosing_class() {
        let src = "class MyViewModel {\n    fun foo() {\n        this.bar\n    }\n}\n";
        let ctx = at(src, 2, 16);
        assert_eq!(ctx.enclosing_class.as_deref(), Some("MyViewModel"));
    }

    #[test]
    fn test_enclosing_class_internal_name() {
        // package + class → internal name
        let src = indoc::indoc! {r#"
            package com.example
            class MyViewModel {
                fun foo() {
                    this.bar
                }
            }
        "#};
        let ctx = at(src, 3, 16);
        assert_eq!(
            ctx.enclosing_internal_name.as_deref(),
            Some("com/example/MyViewModel")
        );
    }

    #[test]
    fn test_enclosing_object() {
        let src = "object Singleton {\n    fun foo() {\n        this.bar\n    }\n}\n";
        let ctx = at(src, 2, 16);
        assert_eq!(ctx.enclosing_class.as_deref(), Some("Singleton"));
    }

    #[test]
    fn test_enclosing_object_internal_name() {
        let src = indoc::indoc! {r#"
            package org.app
            object Singleton {
                fun foo() {
                    this.bar
                }
            }
        "#};
        let ctx = at(src, 3, 16);
        assert_eq!(
            ctx.enclosing_internal_name.as_deref(),
            Some("org/app/Singleton")
        );
    }

    #[test]
    fn test_enclosing_package() {
        let src = indoc::indoc! {r#"
            package org.cubewhy.a
            class Main {
                fun foo() {
                    this.bar
                }
            }
        "#};
        let ctx = at(src, 3, 16);
        assert_eq!(ctx.enclosing_package.as_deref(), Some("org/cubewhy/a"));
    }

    #[test]
    fn test_no_package_internal_name_is_simple() {
        // 没有 package 声明时，internal_name = 简单类名
        let src = "class Bare {\n    fun foo() {\n        this.x\n    }\n}\n";
        let ctx = at(src, 2, 14);
        assert_eq!(ctx.enclosing_internal_name.as_deref(), Some("Bare"));
    }

    #[test]
    fn test_imports_extracted() {
        let src = "import java.util.List\nimport java.io.File\nfun main() {}\n";
        let ctx = at(src, 2, 5);
        assert!(ctx.existing_imports.iter().any(|i| i.contains("List")));
        assert!(ctx.existing_imports.iter().any(|i| i.contains("File")));
    }

    #[test]
    fn test_nullable_type_stripped() {
        assert_eq!(kotlin_type_to_internal("String?"), "java/lang/String");
        assert_eq!(kotlin_type_to_internal("Int?"), "int");
    }

    #[test]
    fn test_kotlin_type_mapping() {
        assert_eq!(
            kotlin_type_to_internal("MutableList"),
            "java/util/ArrayList"
        );
        assert_eq!(kotlin_type_to_internal("MutableMap"), "java/util/HashMap");
        assert_eq!(kotlin_type_to_internal("Any"), "java/lang/Object");
        assert_eq!(kotlin_type_to_internal("Unit"), "void");
    }

    #[test]
    fn test_post_process_lowers_getter_score() {
        use crate::completion::candidate::{CandidateKind, CompletionCandidate};

        let ctx = SemanticContext::new(
            CursorLocation::Expression { prefix: "".into() },
            "",
            vec![],
            None,
            None, // enclosing_internal_name ← 新增
            None, // enclosing_package
            vec![],
        );

        let getter = CompletionCandidate::new(
            Arc::from("getName"),
            "getName(".to_string(),
            CandidateKind::Method {
                descriptor: Arc::from("()Ljava/lang/String;"),
                defining_class: Arc::from("Foo"),
            },
            "test",
        );
        let getter_score_before = getter.score;

        let normal = CompletionCandidate::new(
            Arc::from("process"),
            "process(".to_string(),
            CandidateKind::Method {
                descriptor: Arc::from("()V"),
                defining_class: Arc::from("Foo"),
            },
            "test",
        );
        let normal_score_before = normal.score;

        let result = KotlinLanguage.post_process_candidates(vec![getter, normal], &ctx);

        let getter_after = result
            .iter()
            .find(|c| c.label.as_ref() == "getName")
            .unwrap();
        let normal_after = result
            .iter()
            .find(|c| c.label.as_ref() == "process")
            .unwrap();

        assert!(
            getter_after.score < getter_score_before,
            "getter score should decrease"
        );
        assert_eq!(normal_after.score, normal_score_before, "normal unchanged");
    }
}
