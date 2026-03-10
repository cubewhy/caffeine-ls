use std::sync::Arc;

use super::Language;
use crate::completion::provider::CompletionProvider;
use crate::index::{IndexScope, IndexView, NameTable};
use crate::language::java::completion::providers::{
    annotation::AnnotationProvider, constructor::ConstructorProvider,
    expression::ExpressionProvider, import::ImportProvider, import_static::ImportStaticProvider,
    keyword::KeywordProvider, local_var::LocalVarProvider, member::MemberProvider,
    name_suggestion::NameSuggestionProvider, override_member::OverrideProvider,
    package::PackageProvider, snippet::SnippetProvider,
    static_import_member::StaticImportMemberProvider, static_member::StaticMemberProvider,
    this_member::ThisMemberProvider,
};
use crate::language::java::symbols::collect_java_symbols;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::language::rope_utils::rope_line_col_to_offset;
use crate::language::ts_utils::find_method_by_offset;
use crate::language::{ClassifiedToken, ParseEnv};
use crate::semantic::{CursorLocation, SemanticContext};
use ropey::Rope;
use smallvec::smallvec;
use tower_lsp::lsp_types::{SemanticTokenModifier, SemanticTokenType};
use tree_sitter::{Node, Parser};

pub mod class_parser;
pub mod completion;
pub mod completion_context;
pub mod expression_typing;
pub mod flow;
pub mod injection;
pub mod locals;
pub mod location;
pub mod members;
pub mod render;
pub mod scope;
pub mod symbols;
pub mod type_ctx;
pub mod utils;

const SENTINEL: &str = "__KIRO__";

static JAVA_COMPLETION_PROVIDERS: [&dyn CompletionProvider; 15] = [
    &LocalVarProvider,
    &ThisMemberProvider,
    &MemberProvider,
    &StaticMemberProvider,
    &ConstructorProvider,
    &PackageProvider,
    &ExpressionProvider,
    &ImportProvider,
    &ImportStaticProvider,
    &StaticImportMemberProvider,
    &OverrideProvider,
    &KeywordProvider,
    &AnnotationProvider,
    &SnippetProvider,
    &NameSuggestionProvider,
];

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

    fn parse_completion_context_with_tree(
        &self,
        source: &str,
        rope: &Rope,
        root: Node,
        line: u32,
        character: u32,
        trigger_char: Option<char>,
        env: &ParseEnv,
    ) -> Option<SemanticContext> {
        let offset = rope_line_col_to_offset(rope, line, character)?;
        tracing::debug!(line, character, trigger = ?trigger_char, "java: parsing context (cached tree)");
        let extractor = JavaContextExtractor::with_rope(
            source.to_string(),
            offset,
            rope.clone(),
            env.name_table.clone(),
        );
        if extractor.is_in_comment() {
            return Some(SemanticContext::new(
                CursorLocation::Unknown,
                "",
                vec![],
                None,
                None,
                None,
                vec![],
            ));
        }
        Some(extractor.extract(root, trigger_char))
    }

    fn completion_providers(&self) -> &[&'static dyn CompletionProvider] {
        &JAVA_COMPLETION_PROVIDERS
    }

    fn enrich_completion_context(
        &self,
        ctx: &mut SemanticContext,
        _scope: IndexScope,
        index: &IndexView,
    ) {
        completion_context::ContextEnricher::new(index).enrich(ctx);
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

    fn supports_collecting_symbols(&self) -> bool {
        true
    }

    fn collect_symbols<'a>(
        &self,
        node: tree_sitter::Node<'a>,
        bytes: &'a [u8],
    ) -> Option<Vec<tower_lsp::lsp_types::DocumentSymbol>> {
        Some(collect_java_symbols(node, bytes))
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
    name_table: Option<Arc<NameTable>>,
}

impl JavaContextExtractor {
    pub fn new(
        source: impl Into<String>,
        offset: usize,
        name_table: Option<Arc<NameTable>>,
    ) -> Self {
        let source = source.into();
        let rope = Rope::from_str(&source);
        Self {
            source,
            rope,
            offset,
            name_table,
        }
    }

    /// Create a simplified extractor for indexing (no cursor offset needed)
    pub fn for_indexing(source: &str, name_table: Option<Arc<NameTable>>) -> Self {
        Self::new(source, 0, name_table)
    }

    pub(crate) fn with_rope(
        source: String,
        offset: usize,
        rope: Rope,
        name_table: Option<Arc<NameTable>>,
    ) -> Self {
        Self {
            source,
            rope,
            offset,
            name_table,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        self.source.as_bytes()
    }
    pub fn source_str(&self) -> &str {
        &self.source
    }

    pub fn byte_slice(&self, start: usize, end: usize) -> &str {
        &self.source[start..end]
    }

    pub fn node_text(&self, node: Node) -> &str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }

    pub fn is_in_comment(&self) -> bool {
        utils::is_cursor_in_comment_with_rope(&self.source, &self.rope, self.offset)
    }

    fn extract(self, root: Node, trigger_char: Option<char>) -> SemanticContext {
        let cursor_node = self.find_cursor_node(root);

        if cursor_node
            .map(|n| utils::is_comment_kind(n.kind()))
            .unwrap_or(false)
        {
            return self.empty_context();
        }

        let (location, query) = location::determine_location(&self, cursor_node, trigger_char);

        // If AST parsing fails, proceed with the injection path.
        let (location, query) = if matches!(location, CursorLocation::Unknown)
            || injection::should_force_injection(&self, cursor_node, &location)
        {
            injection::inject_and_determine(&self, cursor_node, trigger_char)
                .unwrap_or((location, query))
        } else {
            (location, query)
        };
        let functional_target_hint = location::infer_functional_target_hint(&self, cursor_node);

        let enclosing_class = scope::extract_enclosing_class(&self, cursor_node)
            .or_else(|| scope::extract_enclosing_class_by_offset(&self, root));
        let enclosing_package = scope::extract_package(&self, root);
        let enclosing_internal_name =
            scope::extract_enclosing_internal_name(&self, cursor_node, enclosing_package.as_ref())
                .or_else(|| utils::build_internal_name(&enclosing_package, &enclosing_class));
        let existing_imports = scope::extract_imports(&self, root);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            enclosing_package.clone(),
            existing_imports.clone(),
            self.name_table.clone(),
        ));
        let local_variables =
            locals::extract_locals_with_type_ctx(&self, root, cursor_node, Some(&type_ctx));
        let flow_type_overrides = flow::extract_instanceof_true_branch_overrides(
            &self,
            cursor_node,
            &type_ctx,
            &local_variables,
        );
        let existing_static_imports = scope::extract_static_imports(&self, root);
        let is_class_member_position = scope::is_cursor_in_class_member_position(cursor_node);
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

        SemanticContext::new(
            location,
            query,
            local_variables,
            enclosing_class,
            enclosing_internal_name,
            enclosing_package,
            existing_imports,
        )
        .with_functional_target_hint(functional_target_hint)
        .with_flow_type_overrides(flow_type_overrides)
        .with_class_member_position(is_class_member_position)
        .with_static_imports(existing_static_imports)
        .with_class_members(current_class_members)
        .with_enclosing_member(enclosing_class_member)
        .with_char_after_cursor(char_after_cursor)
        .with_extension(type_ctx)
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

    fn empty_context(&self) -> SemanticContext {
        SemanticContext::new(
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
        completion::engine::CompletionEngine,
        completion::{CandidateKind, CompletionCandidate},
        index::{
            ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, ModuleId,
            WorkspaceIndex,
        },
        language::{
            java::{class_parser::parse_java_source, injection::build_injected_source},
            rope_utils::line_col_to_offset,
        },
        semantic::LocalVar,
        semantic::context::{CurrentClassMember, CursorLocation},
        semantic::types::type_name::TypeName,
    };
    use rust_asm::constants::ACC_PUBLIC;
    use std::sync::Arc;

    fn at(src: &str, line: u32, col: u32) -> SemanticContext {
        at_with_trigger(src, line, col, None)
    }

    fn at_with_trigger(src: &str, line: u32, col: u32, trigger: Option<char>) -> SemanticContext {
        let rope = ropey::Rope::from_str(src);

        let mut parser = super::make_java_parser();
        let tree = parser.parse(src, None).expect("failed to parse java");

        super::JavaLanguage
            .parse_completion_context_with_tree(
                src,
                &rope,
                tree.root_node(),
                line,
                col,
                trigger,
                &ParseEnv { name_table: None },
            )
            .expect("parse_completion_context_with_tree returned None")
    }

    fn end_of(src: &str) -> SemanticContext {
        let lines: Vec<&str> = src.lines().collect();
        let line = (lines.len().saturating_sub(1)) as u32;
        let col = lines.last().map(|l| l.len()).unwrap_or(0) as u32;
        at(src, line, col)
    }

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_class(pkg: &str, name: &str) -> ClassMetadata {
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(format!("{}/{}", pkg, name)),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: None,
            origin: ClassOrigin::Unknown,
        }
    }

    fn ctx_and_labels_from_marked_source(
        src_with_cursor: &str,
        view: &IndexView,
    ) -> (SemanticContext, Vec<String>) {
        let (ctx, candidates) = ctx_and_candidates_from_marked_source(src_with_cursor, view);
        let mut labels: Vec<String> = candidates
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();
        labels.sort();
        (ctx, labels)
    }

    fn ctx_and_candidates_from_marked_source(
        src_with_cursor: &str,
        view: &IndexView,
    ) -> (SemanticContext, Vec<CompletionCandidate>) {
        let (src, cursor_byte) = if let Some(idx) = src_with_cursor.find("/*caret*/") {
            (src_with_cursor.replacen("/*caret*/", "", 1), idx)
        } else {
            let idx = src_with_cursor
                .find('|')
                .expect("expected | or /*caret*/ cursor marker in source");
            (src_with_cursor.replacen('|', "", 1), idx)
        };
        let rope = ropey::Rope::from_str(&src);
        let cursor_char = rope.byte_to_char(cursor_byte);
        let line = rope.char_to_line(cursor_char) as u32;
        let col = (cursor_char - rope.line_to_char(line as usize)) as u32;

        let mut parser = super::make_java_parser();
        let tree = parser.parse(&src, None).expect("failed to parse java");
        let ctx = super::JavaLanguage
            .parse_completion_context_with_tree(
                &src,
                &rope,
                tree.root_node(),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                },
            )
            .expect("parse_completion_context_with_tree returned None");
        let engine = CompletionEngine::new();
        let candidates = engine.complete(root_scope(), ctx.clone(), &JavaLanguage, view);
        (ctx, candidates)
    }

    fn make_functional_chain_index() -> WorkspaceIndex {
        let src = indoc::indoc! {r#"
            package org.cubewhy;

            import java.util.function.Function;

            class Box<T> {
                Box(T v) {}
                T get() { return null; }
                <R> Box<R> map(Function<? super T, ? extends R> fn) { return null; }
            }
        "#};
        let parsed = parse_java_source(src, ClassOrigin::Unknown, None);

        let idx = WorkspaceIndex::new();
        idx.add_classes(parsed);
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("trim"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/lang/String;")),
                    },
                    MethodSummary {
                        name: Arc::from("substring"),
                        params: MethodParams::from([("I", "begin")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/lang/String;")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Number"),
                internal_name: Arc::from("java/lang/Number"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("byteValue"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("B")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("ArrayList"),
                internal_name: Arc::from("java/util/ArrayList"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("add"),
                    params: MethodParams::from([("Ljava/lang/Object;", "e")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TE;)Z")),
                    return_type: Some(Arc::from("Z")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("List"),
                internal_name: Arc::from("java/util/List"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("get"),
                    params: MethodParams::from([("I", "index")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(I)TE;")),
                    return_type: Some(Arc::from("Ljava/lang/Object;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("Function"),
                internal_name: Arc::from("java/util/function/Function"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("apply"),
                    params: MethodParams::from([("Ljava/lang/Object;", "t")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;)TR;")),
                    return_type: Some(Arc::from("Ljava/lang/Object;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from(
                    "<T:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                origin: ClassOrigin::Unknown,
            },
        ]);
        idx
    }

    fn make_instanceof_narrowing_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("toString"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("Ljava/lang/String;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("StringBuilder"),
                internal_name: Arc::from("java/lang/StringBuilder"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("append"),
                    params: MethodParams::from([("Ljava/lang/String;", "str")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: Some(Arc::from("Ljava/lang/StringBuilder;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("substring"),
                        params: MethodParams::from([("I", "beginIndex")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/lang/String;")),
                    },
                    MethodSummary {
                        name: Arc::from("isEmpty"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Z")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        idx
    }

    fn make_lambda_scope_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "String"),
            make_class("java/lang", "Integer"),
            make_class("java/lang", "System"),
            make_class("java/util/function", "Function"),
            make_class("java/util/function", "BiFunction"),
            make_class("java/util/function", "Runnable"),
        ]);
        idx
    }

    #[test]
    fn test_lambda_single_param_is_visible_in_local_completion() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class Demo {
                void f() {
                    Function<String, Integer> fn = s -> s|;
                }
            }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);

        assert!(
            ctx.local_variables.iter().any(|lv| lv.name.as_ref() == "s"),
            "lambda param should be injected into local scope: {:?}",
            ctx.local_variables
                .iter()
                .map(|lv| lv.name.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(labels.iter().any(|l| l == "s"), "{labels:?}");
    }

    #[test]
    fn test_lambda_multi_param_names_are_visible_in_scope() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.BiFunction;
            class Demo {
                void f() {
                    BiFunction<String, String, Integer> fn = (left, right) -> /*caret*/left;
                }
            }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);

        assert!(
            ctx.local_variables.iter().any(|lv| lv.name.as_ref() == "left"),
            "expected left in local scope: {:?}",
            ctx.local_variables
                .iter()
                .map(|lv| lv.name.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(
            ctx.local_variables
                .iter()
                .any(|lv| lv.name.as_ref() == "right"),
            "expected right in local scope: {:?}",
            ctx.local_variables
                .iter()
                .map(|lv| lv.name.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(labels.iter().any(|l| l == "left"), "{labels:?}");
        assert!(labels.iter().any(|l| l == "right"), "{labels:?}");
    }

    #[test]
    fn test_zero_arg_lambda_keeps_generic_completion_working() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Runnable;
            class Demo {
                void f() {
                    Runnable r = () -> {
                        Sys|
                    };
                }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|l| l == "System"), "{labels:?}");
    }

    #[test]
    fn test_import() {
        let src = "import com.example.Foo;";
        let ctx = end_of(src);
        assert!(matches!(ctx.location, CursorLocation::Import { .. }));
    }

    #[test]
    fn test_instanceof_true_branch_exposes_narrowed_members() {
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    Object sb = new StringBuilder();
                    if (sb instanceof StringBuilder) {
                        sb.appe|
                    }
                }
            }
        "#};
        let idx = make_instanceof_narrowing_index();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        assert!(
            candidates.iter().any(|c| c.label.as_ref() == "append"),
            "append should be available via flow narrowing in true branch"
        );
    }

    #[test]
    fn test_instanceof_narrowed_member_insert_rewrites_receiver_with_cast() {
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    Object sb = new StringBuilder();
                    if (sb instanceof StringBuilder) {
                        sb.appe|
                    }
                }
            }
        "#};
        let idx = make_instanceof_narrowing_index();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let append = candidates
            .iter()
            .find(|c| c.label.as_ref() == "append")
            .expect("append candidate should exist in narrowed branch");
        assert!(
            append.insert_text.starts_with("append("),
            "primary insert text should remain selector-local, got: {}",
            append.insert_text
        );
        let rewrite = append
            .insertion
            .member_access_rewrite
            .as_ref()
            .expect("narrowed append should carry cast rewrite metadata");
        assert_eq!(rewrite.receiver_expr, "sb");
        assert_eq!(rewrite.cast_type, "java.lang.StringBuilder");
        assert!(
            append.insertion.filter_text.is_none(),
            "cast rewrite should not force custom filter_text here"
        );
    }

    #[test]
    fn test_typed_receiver_member_insert_stays_plain_without_cast() {
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    StringBuilder sb = new StringBuilder();
                    sb.appe|
                }
            }
        "#};
        let idx = make_instanceof_narrowing_index();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let append = candidates
            .iter()
            .find(|c| c.label.as_ref() == "append")
            .expect("append candidate should exist for typed receiver");
        assert!(
            append.insert_text.starts_with("append("),
            "typed receiver insertion should not include cast, got: {}",
            append.insert_text
        );
        assert!(
            !append
                .insert_text
                .contains("((java.lang.StringBuilder) sb)"),
            "typed receiver insertion should not cast"
        );
        assert!(
            append.insertion.member_access_rewrite.is_none(),
            "typed receiver should not carry cast rewrite metadata"
        );
    }

    #[test]
    fn test_instanceof_narrowing_does_not_leak_outside_true_branch() {
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    Object sb = new StringBuilder();
                    if (sb instanceof StringBuilder) {
                    }
                    sb.appe|
                }
            }
        "#};
        let idx = make_instanceof_narrowing_index();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        assert!(
            !candidates.iter().any(|c| c.label.as_ref() == "append"),
            "append should not be available outside instanceof true branch"
        );
    }

    #[test]
    fn test_instanceof_and_true_branch_narrows_multiple_symbols() {
        let src_a = indoc::indoc! {r#"
            class T {
                void m() {
                    Object a = new StringBuilder();
                    Object b = "x";
                    if (a instanceof StringBuilder && b instanceof String) {
                        a.appe|
                    }
                }
            }
        "#};
        let src_b = indoc::indoc! {r#"
            class T {
                void m() {
                    Object a = new StringBuilder();
                    Object b = "x";
                    if (a instanceof StringBuilder && b instanceof String) {
                        b.subs|
                    }
                }
            }
        "#};
        let idx = make_instanceof_narrowing_index();
        let view = idx.view(root_scope());

        let (_ctx_a, candidates_a) = ctx_and_candidates_from_marked_source(src_a, &view);
        assert!(
            candidates_a.iter().any(|c| c.label.as_ref() == "append"),
            "a should narrow to StringBuilder in && true branch"
        );
        let (_ctx_b, candidates_b) = ctx_and_candidates_from_marked_source(src_b, &view);
        assert!(
            candidates_b.iter().any(|c| c.label.as_ref() == "substring"),
            "b should narrow to String in && true branch"
        );
    }

    #[test]
    fn test_instanceof_and_rhs_short_circuit_narrowing() {
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    Object a = new StringBuilder();
                    if (a instanceof StringBuilder && a.appe|) {
                    }
                }
            }
        "#};
        let idx = make_instanceof_narrowing_index();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        assert!(
            candidates.iter().any(|c| c.label.as_ref() == "append"),
            "RHS of && should see lhs true-facts"
        );
    }

    #[test]
    fn test_instanceof_or_rhs_negated_lhs_short_circuit_narrowing() {
        let idx = make_instanceof_narrowing_index();
        let view = idx.view(root_scope());
        let mut flow = std::collections::HashMap::new();
        flow.insert(Arc::from("x"), TypeName::new("java/lang/String"));
        let ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "subs".to_string(),
                receiver_expr: "x".to_string(),
                arguments: None,
            },
            "subs",
            vec![LocalVar {
                name: Arc::from("x"),
                type_internal: TypeName::new("java/lang/Object"),
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        )
        .with_flow_type_overrides(flow)
        .with_extension(Arc::new(SourceTypeCtx::new(
            None,
            vec![],
            Some(view.build_name_table()),
        )));
        let engine = CompletionEngine::new();
        let candidates = engine.complete(root_scope(), ctx, &JavaLanguage, &view);
        assert!(
            candidates.iter().any(|c| c.label.as_ref() == "substring"),
            "RHS of || should narrow from false case of !(x instanceof String)"
        );
    }

    #[test]
    fn test_instanceof_or_true_branch_is_not_over_narrowed() {
        let src_a = indoc::indoc! {r#"
            class T {
                void m(Object a, Object b) {
                    if (a instanceof String || b instanceof StringBuilder) {
                        a.appe/*caret*/
                    }
                }
            }
        "#};
        let src_b = indoc::indoc! {r#"
            class T {
                void m(Object a, Object b) {
                    if (a instanceof String || b instanceof StringBuilder) {
                        b.appe/*caret*/
                    }
                }
            }
        "#};
        let idx = make_instanceof_narrowing_index();
        let view = idx.view(root_scope());

        let (_ctx_a, candidates_a) = ctx_and_candidates_from_marked_source(src_a, &view);
        assert!(
            !candidates_a.iter().any(|c| c.label.as_ref() == "append"),
            "a should not be narrowed in true branch of general ||"
        );
        let (_ctx_b, candidates_b) = ctx_and_candidates_from_marked_source(src_b, &view);
        assert!(
            !candidates_b.iter().any(|c| c.label.as_ref() == "append"),
            "b should not be narrowed in true branch of general ||"
        );
    }

    #[test]
    fn test_generic_type_argument_position_completes_type_candidates() {
        let src = indoc::indoc! {r#"
        import java.util.*;
        class A {
            void f() {
                List<Bo> nums = new ArrayList<>();
            }
        }
        "#};
        let line = 3u32;
        let raw = src.lines().nth(3).unwrap();
        let col = raw.find("Bo").unwrap() as u32 + 2;
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::TypeAnnotation { .. }),
            "generic type argument should route to TypeAnnotation, got {:?}",
            ctx.location
        );

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/util", "List"),
            make_class("java/util", "Map"),
            make_class("java/util", "ArrayList"),
            make_class("org/example", "Box"),
            make_class("java/lang", "String"),
        ]);
        let view = idx.view(root_scope());
        let engine = CompletionEngine::new();
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();

        assert!(
            !labels.is_empty() && labels.contains(&"Box"),
            "type candidates should be available in generic arg position, got {:?}",
            labels
        );
    }

    #[test]
    fn test_var_member_tail_not_type_annotation() {
        let src = indoc::indoc! {r#"
        import java.util.*;
        class A {
            void f() {
                var a = new HashMap<String, String>();
                a.put
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.put")
                    .map(|c| (i as u32, c as u32 + "a.put".len() as u32))
            })
            .expect("expected a.put marker");
        let ctx = at(src, line, col);
        assert!(
            !matches!(ctx.location, CursorLocation::TypeAnnotation { .. }),
            "a.put should not route to TypeAnnotation, got {:?}",
            ctx.location
        );
        assert!(
            matches!(ctx.location, CursorLocation::MemberAccess { .. }),
            "a.put should route to MemberAccess, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_var_member_tail_recovery_a_put_without_semicolon_routes_member_access() {
        let src = indoc::indoc! {r#"
        import java.util.*;
        class A {
            void f() {
                var a = new HashMap<String, String>();
                a.put
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.put")
                    .map(|c| (i as u32, c as u32 + "a.put".len() as u32))
            })
            .expect("expected a.put marker");
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::MemberAccess { .. }),
            "a.put in recovery context should route to MemberAccess, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_var_member_tail_recovery_a_a_without_semicolon_routes_member_access() {
        let src = indoc::indoc! {r#"
        import java.util.*;
        class A {
            void f() {
                var a = new HashMap<String, String>();
                a.a
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.a")
                    .map(|c| (i as u32, c as u32 + "a.a".len() as u32))
            })
            .expect("expected a.a marker");
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::MemberAccess { .. }),
            "a.a in recovery context should route to MemberAccess, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_var_member_tail_completion_offers_hashmap_put() {
        let src = indoc::indoc! {r#"
        import java.util.*;
        class A {
            void f() {
                var a = new HashMap<String, String>();
                a.put;
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.put")
                    .map(|c| (i as u32, c as u32 + "a.put".len() as u32))
            })
            .expect("expected a.put marker");

        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "String"),
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("HashMap"),
                internal_name: Arc::from("java/util/HashMap"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("put"),
                    params: MethodParams::from([
                        ("Ljava/lang/Object;", "key"),
                        ("Ljava/lang/Object;", "value"),
                    ]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TK;TV;)TV;")),
                    return_type: Some(Arc::from("Ljava/lang/Object;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from(
                    "<K:Ljava/lang/Object;V:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(root_scope());
        let name_table = view.build_name_table();
        let rope = ropey::Rope::from_str(src);
        let mut parser = super::make_java_parser();
        let tree = parser.parse(src, None).expect("failed to parse java");
        let ctx = super::JavaLanguage
            .parse_completion_context_with_tree(
                src,
                &rope,
                tree.root_node(),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(name_table),
                },
            )
            .expect("parse_completion_context_with_tree returned None");
        let engine = CompletionEngine::new();
        let results = engine.complete(root_scope(), ctx, &JavaLanguage, &view);
        let labels: Vec<&str> = results.iter().map(|c| c.label.as_ref()).collect();
        assert!(
            labels.contains(&"put"),
            "member completion should include put for a.put, got {:?}",
            labels
        );
    }

    #[test]
    fn test_functional_chain_completion_trim_constructor_and_wildcard_labels() {
        let idx = make_functional_chain_index();
        let view = idx.view(root_scope());

        let src_trim = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> strBox = new Box<>(" hello ");
                strBox.map(String::trim).get().subs|
            }
        }
        "#};
        let (trim_ctx, trim_labels) = ctx_and_labels_from_marked_source(src_trim, &view);
        assert!(
            trim_labels.iter().any(|l| l == "substring"),
            "strBox.map(String::trim).get().subs should include substring, got location={:?} labels={:?}",
            trim_ctx.location,
            trim_labels
        );

        let src_ctor = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> s = new Box<>("x");
                s.map(ArrayList::new).get().ad|
            }
        }
        "#};
        let (ctor_ctx, ctor_labels) = ctx_and_labels_from_marked_source(src_ctor, &view);
        assert!(
            ctor_labels.iter().any(|l| l == "add"),
            "s.map(ArrayList::new).get().ad should include add, got location={:?} labels={:?}",
            ctor_ctx.location,
            ctor_labels
        );

        let src_wild = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                List<Box<? extends Number>> nums = List.of();
                nums.get(0).get().byteV|
            }
        }
        "#};
        let (wild_ctx, wild_labels) = ctx_and_labels_from_marked_source(src_wild, &view);
        assert!(
            wild_labels.iter().any(|l| l == "byteValue"),
            "nums.get(0).get().byteV should include byteValue, got location={:?} labels={:?}",
            wild_ctx.location,
            wild_labels
        );
    }

    #[test]
    fn test_functional_chain_var_local_materializes_box_type() {
        let idx = make_functional_chain_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> strBox = new Box<>(" hello ");
                var a = strBox.map(String::trim);
                a|
            }
        }
        "#};
        let cursor_byte = src.find('|').expect("expected |");
        let src_no_cursor = src.replacen('|', "", 1);
        let rope = ropey::Rope::from_str(&src_no_cursor);
        let cursor_char = rope.byte_to_char(cursor_byte);
        let line = rope.char_to_line(cursor_char) as u32;
        let col = (cursor_char - rope.line_to_char(line as usize)) as u32;
        let mut parser = super::make_java_parser();
        let tree = parser.parse(&src_no_cursor, None).expect("failed to parse");
        let ctx = super::JavaLanguage
            .parse_completion_context_with_tree(
                &src_no_cursor,
                &rope,
                tree.root_node(),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                },
            )
            .expect("parse_completion_context_with_tree returned None");
        let results = CompletionEngine::new().complete(root_scope(), ctx, &JavaLanguage, &view);
        let a = results
            .iter()
            .find(|c| c.label.as_ref() == "a")
            .expect("expected local candidate a");
        match &a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(
                    type_descriptor.as_ref(),
                    "org/cubewhy/Box<Ljava/lang/String;>",
                    "var local candidate should materialize map(String::trim) as Box<String>"
                );
            }
            other => panic!("expected local variable candidate for a, got {other:?}"),
        }
    }

    #[test]
    fn test_functional_direct_chain_member_completion_after_method_ref() {
        let idx = make_functional_chain_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> strBox = new Box<>(" hello ");
                strBox.map(String::trim).g|
            }
        }
        "#};
        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            labels.iter().any(|l| l == "get"),
            "direct chain receiver should remain Box<String> and expose get, got location={:?} labels={:?}",
            ctx.location,
            labels
        );
    }

    #[test]
    fn test_functional_chain_member_completion_parity_var_vs_direct() {
        let idx = make_functional_chain_index();
        let view = idx.view(root_scope());

        let src_var = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> strBox = new Box<>(" hello ");
                var a = strBox.map(String::trim);
                a.g|
            }
        }
        "#};
        let (_ctx_var, labels_var) = ctx_and_labels_from_marked_source(src_var, &view);

        let src_direct = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> strBox = new Box<>(" hello ");
                strBox.map(String::trim).g|
            }
        }
        "#};
        let (_ctx_direct, labels_direct) = ctx_and_labels_from_marked_source(src_direct, &view);

        let var_has_get = labels_var.iter().any(|l| l == "get");
        let direct_has_get = labels_direct.iter().any(|l| l == "get");
        assert_eq!(
            direct_has_get, var_has_get,
            "direct chain completion should match var-materialized chain completion for get()"
        );
        assert!(direct_has_get, "expected get() in both parity paths");
    }

    #[test]
    fn test_functional_constructor_chain_var_local_materializes_type() {
        let idx = make_functional_chain_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> s = new Box<>("x");
                var b = s.map(ArrayList::new).get();
                b|
            }
        }
        "#};
        let cursor_byte = src.find('|').expect("expected |");
        let src_no_cursor = src.replacen('|', "", 1);
        let rope = ropey::Rope::from_str(&src_no_cursor);
        let cursor_char = rope.byte_to_char(cursor_byte);
        let line = rope.char_to_line(cursor_char) as u32;
        let col = (cursor_char - rope.line_to_char(line as usize)) as u32;
        let mut parser = super::make_java_parser();
        let tree = parser.parse(&src_no_cursor, None).expect("failed to parse");
        let ctx = super::JavaLanguage
            .parse_completion_context_with_tree(
                &src_no_cursor,
                &rope,
                tree.root_node(),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                },
            )
            .expect("parse_completion_context_with_tree returned None");
        let results = CompletionEngine::new().complete(root_scope(), ctx, &JavaLanguage, &view);
        let b = results
            .iter()
            .find(|c| c.label.as_ref() == "b")
            .expect("expected local candidate b");
        match &b.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert!(
                    type_descriptor.as_ref().starts_with("java/util/ArrayList"),
                    "constructor reference local should materialize b as ArrayList, got {}",
                    type_descriptor
                );
            }
            other => panic!("expected local variable candidate for b, got {other:?}"),
        }
    }

    #[test]
    fn test_functional_constructor_chain_var_local_continues_members() {
        let idx = make_functional_chain_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> s = new Box<>("x");
                var b = s.map(ArrayList::new).get();
                b.ad|
            }
        }
        "#};
        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            labels.iter().any(|l| l == "add"),
            "constructor reference chain materialized through var should expose ArrayList members, got location={:?} labels={:?}",
            ctx.location,
            labels
        );
    }

    #[test]
    fn test_varargs_parameter_local_symbol_is_array_and_var_materializes_array() {
        let src = indoc::indoc! {r#"
        package org.cubewhy;

        public class VarargsExample {
            public static void printNumbers(int... numbers) {
                var a = numbers;
                a|
            }
        }
        "#};
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        idx.add_classes(vec![make_class("java/lang", "Object")]);
        let view = idx.view(root_scope());
        let (ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);

        let nums = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "numbers")
            .expect("numbers local should exist");
        assert_eq!(nums.type_internal.to_internal_with_generics(), "int[]");

        let a = candidates
            .iter()
            .find(|c| c.label.as_ref() == "a")
            .expect("expected local variable completion for a");
        match &a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "int[]");
            }
            other => panic!("expected local variable candidate, got {other:?}"),
        }
    }

    #[test]
    fn test_binary_expression_var_materialization_surfaces_int_in_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_class("java/lang", "Object")]);
        let view = idx.view(root_scope());

        let src_a = indoc::indoc! {r#"
        class Demo {
            void f() {
                int i = 1;
                var a = 1 + 1;
                var b = i + 1;
                a|
            }
        }
        "#};
        let (_ctx_a, candidates_a) = ctx_and_candidates_from_marked_source(src_a, &view);
        let cand_a = candidates_a
            .iter()
            .find(|c| c.label.as_ref() == "a")
            .expect("expected local candidate a");
        match &cand_a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "int");
            }
            other => panic!("expected local variable candidate for a, got {other:?}"),
        }

        let src_b = indoc::indoc! {r#"
        class Demo {
            void f() {
                int i = 1;
                var a = 1 + 1;
                var b = i + 1;
                b|
            }
        }
        "#};
        let (_ctx_b, candidates_b) = ctx_and_candidates_from_marked_source(src_b, &view);

        let cand_b = candidates_b
            .iter()
            .find(|c| c.label.as_ref() == "b")
            .expect("expected local candidate b");
        match &cand_b.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "int");
            }
            other => panic!("expected local variable candidate for b, got {other:?}"),
        }
    }

    #[test]
    fn test_mixed_expression_var_materialization_surfaces_double_and_string_in_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_class("java/lang", "Object")]);
        let view = idx.view(root_scope());

        let src_a = indoc::indoc! {r#"
        class Demo {
            void f() {
                double i = 1;
                var a = i + 1.0;
                var b = i + "random str";
                a|
            }
        }
        "#};
        let (_ctx_a, candidates_a) = ctx_and_candidates_from_marked_source(src_a, &view);
        let cand_a = candidates_a
            .iter()
            .find(|c| c.label.as_ref() == "a")
            .expect("expected local candidate a");
        match &cand_a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "double");
            }
            other => panic!("expected local variable candidate for a, got {other:?}"),
        }

        let src_b = indoc::indoc! {r#"
        class Demo {
            void f() {
                double i = 1;
                var a = i + 1.0;
                var b = i + "random str";
                b|
            }
        }
        "#};
        let (_ctx_b, candidates_b) = ctx_and_candidates_from_marked_source(src_b, &view);
        let cand_b = candidates_b
            .iter()
            .find(|c| c.label.as_ref() == "b")
            .expect("expected local candidate b");
        match &cand_b.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "java/lang/String");
            }
            other => panic!("expected local variable candidate for b, got {other:?}"),
        }
    }

    #[test]
    fn test_wrapper_arithmetic_var_materialization_surfaces_double_in_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![make_class("java/lang", "Object")]);
        let view = idx.view(root_scope());

        let src = indoc::indoc! {r#"
        class Demo {
            void f() {
                Integer i = 1;
                var a = i + 1 + 1 * 100d;
                a|
            }
        }
        "#};
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let cand_a = candidates
            .iter()
            .find(|c| c.label.as_ref() == "a")
            .expect("expected local candidate a");
        match &cand_a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "double");
            }
            other => panic!("expected local variable candidate for a, got {other:?}"),
        }
    }

    #[test]
    fn test_method_call_arithmetic_var_materialization_surfaces_double_in_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(
            indoc::indoc! {r#"
            class Demo {
                int getInt() { return 1; }
                void f() {}
            }
            "#},
            ClassOrigin::Unknown,
            None,
        ));
        idx.add_classes(vec![make_class("java/lang", "Object")]);
        let view = idx.view(root_scope());

        let src = indoc::indoc! {r#"
        class Demo {
            int getInt() { return 1; }
            void f() {
                var a = getInt() + 1 + 1 * 100d;
                a|
            }
        }
        "#};
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let cand_a = candidates
            .iter()
            .find(|c| c.label.as_ref() == "a")
            .expect("expected local candidate a");
        match &cand_a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "double");
            }
            other => panic!("expected local variable candidate for a, got {other:?}"),
        }
    }

    #[test]
    fn test_bitwise_expression_var_materialization_surfaces_integral_in_completion() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(
            indoc::indoc! {r#"
            class Demo {
                int getInt() { return 1; }
                void f() {}
            }
            "#},
            ClassOrigin::Unknown,
            None,
        ));
        idx.add_classes(vec![make_class("java/lang", "Object")]);
        let view = idx.view(root_scope());

        let src = indoc::indoc! {r#"
        class Demo {
            int getInt() { return 1; }
            void f() {
                var b = getInt() + getInt() ^ 1;
                b|
            }
        }
        "#};
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let cand_b = candidates
            .iter()
            .find(|c| c.label.as_ref() == "b")
            .expect("expected local candidate b");
        match &cand_b.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(type_descriptor.as_ref(), "int");
            }
            other => panic!("expected local variable candidate for b, got {other:?}"),
        }
    }

    #[test]
    fn test_later_declared_method_visible_in_earlier_method_completion() {
        let src = indoc::indoc! {r#"
        package org.cubewhy;

        public class VarargsExample {
            public static void main(String[] args) {
                jo|
            }

            public static String join(String separator, String... parts) {
                return "";
            }
        }
        "#};
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "String"),
        ]);
        let view = idx.view(root_scope());
        let (_, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let join = candidates
            .iter()
            .find(|c| c.label.as_ref() == "join")
            .expect("join should be completable regardless of declaration order");
        match &join.kind {
            crate::completion::CandidateKind::Method { descriptor, .. }
            | crate::completion::CandidateKind::StaticMethod { descriptor, .. } => {
                assert!(
                    descriptor.as_ref().contains("[Ljava/lang/String;"),
                    "join method should preserve varargs array descriptor, got {descriptor}"
                );
            }
            other => panic!("join should be method candidate, got {other:?}"),
        }
    }

    #[test]
    fn test_varargs_string_parts_bind_as_array_in_method_body() {
        let src = indoc::indoc! {r#"
        package org.cubewhy;

        public class VarargsExample {
            public static String join(String separator, String... parts) {
                var p = parts;
                p|
                return "";
            }
        }
        "#};
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "String"),
        ]);
        let view = idx.view(root_scope());
        let (ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);

        let parts = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "parts")
            .expect("parts local should exist");
        assert!(
            parts
                .type_internal
                .to_internal_with_generics()
                .ends_with("String[]"),
            "parts should be array-typed, got {}",
            parts.type_internal.to_internal_with_generics()
        );

        let p = candidates
            .iter()
            .find(|c| c.label.as_ref() == "p")
            .expect("expected local variable completion for p");
        match &p.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert!(
                    type_descriptor.as_ref().ends_with("String[]"),
                    "expected String[] local type descriptor, got {}",
                    type_descriptor
                );
            }
            other => panic!("expected local variable candidate, got {other:?}"),
        }
    }

    #[test]
    fn test_snapshot_varargs_join_callsite_context_members_and_candidates() {
        let src = indoc::indoc! {r#"
        public class VarargsExample {
            public static void main(String[] args) {
                String result = join|("-", "java", "lsp", "test");
                System.out.println(result);
            }

            public static void printNumbers(int... numbers) {}

            public static String join(String separator, String... parts) {
                return "";
            }
        }
        "#};
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "String"),
            make_class("java/lang", "System"),
        ]);
        let view = idx.view(root_scope());
        let (ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);

        let mut locals: Vec<String> = ctx
            .local_variables
            .iter()
            .map(|lv| {
                format!(
                    "{}:{}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics()
                )
            })
            .collect();
        locals.sort();

        let mut members: Vec<String> = ctx
            .current_class_members
            .values()
            .map(|m| {
                format!(
                    "{}|{}|{}|{}",
                    m.name(),
                    if m.is_method() { "method" } else { "field" },
                    m.descriptor(),
                    if m.is_static() { "static" } else { "instance" }
                )
            })
            .collect();
        members.sort();

        let mut out: Vec<String> = candidates
            .iter()
            .map(|c| match &c.kind {
                crate::completion::CandidateKind::Method { descriptor, .. } => {
                    format!(
                        "{}@{}|method|desc={}",
                        c.label.as_ref(),
                        c.source,
                        descriptor.as_ref()
                    )
                }
                crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                    format!(
                        "{}@{}|local|ty={}",
                        c.label.as_ref(),
                        c.source,
                        type_descriptor.as_ref()
                    )
                }
                other => format!("{}@{}|{:?}", c.label.as_ref(), c.source, other),
            })
            .collect();
        out.sort();

        let snapshot = format!(
            "location={:?}\nenclosing={:?}\nenclosing_internal={:?}\nlocals=\n{}\nclass_members=\n{}\ncandidates=\n{}",
            ctx.location,
            ctx.enclosing_class,
            ctx.enclosing_internal_name,
            locals.join("\n"),
            members.join("\n"),
            out.join("\n"),
        );
        insta::assert_snapshot!(
            "varargs_join_callsite_context_members_and_candidates",
            snapshot
        );
    }

    #[test]
    fn test_snapshot_varargs_parameter_metadata_and_body_locals() {
        let src = indoc::indoc! {r#"
        public class VarargsExample {
            public static void printNumbers(int... numbers) {
                var a = numbers;
                a|;
            }

            public static String join(String separator, String... parts) {
                return "";
            }
        }
        "#};
        let parsed = parse_java_source(src, ClassOrigin::Unknown, None);
        let cls = parsed
            .iter()
            .find(|c| c.name.as_ref() == "VarargsExample")
            .expect("VarargsExample class");
        let mut method_rows: Vec<String> = cls
            .methods
            .iter()
            .map(|m| {
                format!(
                    "{}|flags={}|params={:?}|param_names={:?}",
                    m.name,
                    m.access_flags,
                    m.params
                        .items
                        .iter()
                        .map(|p| p.descriptor.as_ref())
                        .collect::<Vec<_>>(),
                    m.params
                        .items
                        .iter()
                        .map(|p| p.name.as_ref())
                        .collect::<Vec<_>>()
                )
            })
            .collect();
        method_rows.sort();

        let idx = WorkspaceIndex::new();
        idx.add_classes(parsed);
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "String"),
        ]);
        let view = idx.view(root_scope());
        let (ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);

        let mut locals: Vec<String> = ctx
            .local_variables
            .iter()
            .map(|lv| {
                format!(
                    "{}:{}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics()
                )
            })
            .collect();
        locals.sort();
        let mut local_candidates: Vec<String> = candidates
            .iter()
            .filter_map(|c| match &c.kind {
                crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                    Some(format!(
                        "{}|descriptor={}|detail_has_array={}",
                        c.label,
                        type_descriptor,
                        c.detail.as_deref().is_some_and(|d| d.contains("[]"))
                    ))
                }
                _ => None,
            })
            .collect();
        local_candidates.sort();

        let snapshot = format!(
            "methods=\n{}\nlocals=\n{}\nlocal_candidates=\n{}",
            method_rows.join("\n"),
            locals.join("\n"),
            local_candidates.join("\n"),
        );
        insta::assert_snapshot!("varargs_parameter_metadata_and_body_locals", snapshot);
    }

    #[test]
    fn test_snapshot_plain_array_parameter_and_var_materialization() {
        let src = indoc::indoc! {r#"
        public class ArrayExample {
            public static void printNumbers(int[] numbers) {
                var a = numbers;
                a|;
            }
        }
        "#};
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        idx.add_classes(vec![make_class("java/lang", "Object")]);
        let view = idx.view(root_scope());
        let (ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);

        let mut locals: Vec<String> = ctx
            .local_variables
            .iter()
            .map(|lv| {
                format!(
                    "{}:{}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics()
                )
            })
            .collect();
        locals.sort();

        let mut local_candidates: Vec<String> = candidates
            .iter()
            .filter_map(|c| match &c.kind {
                crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                    Some(format!(
                        "{}|descriptor={}|detail_has_array={}",
                        c.label,
                        type_descriptor,
                        c.detail.as_deref().is_some_and(|d| d.contains("[]"))
                    ))
                }
                _ => None,
            })
            .collect();
        local_candidates.sort();

        let snapshot = format!(
            "location={:?}\nlocals=\n{}\nlocal_candidates=\n{}",
            ctx.location,
            locals.join("\n"),
            local_candidates.join("\n"),
        );
        insta::assert_snapshot!("plain_array_parameter_and_var_materialization", snapshot);
    }

    #[test]
    fn test_inner_class_constructor_reference_var_materializes_b() {
        let src = indoc::indoc! {r#"
        package org.cubewhy;

        import java.util.*;
        import java.util.function.*;

        public class ChainCheck {
            class Box<T> {
                private final T value;
                Box(T value) { this.value = value; }
                T get() { return value; }
                <R> Box<R> map(Function<? super T, ? extends R> fn) {
                    return new Box<>(fn.apply(value));
                }
            }
            void test() {
                Box<String> s = new Box<>("x");
                var b = s.map(ArrayList::new).get();
                b|
            }
        }
        "#};
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "String"),
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("Function"),
                internal_name: Arc::from("java/util/function/Function"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("apply"),
                    params: MethodParams::from([("Ljava/lang/Object;", "t")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;)TR;")),
                    return_type: Some(Arc::from("Ljava/lang/Object;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from(
                    "<T:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("ArrayList"),
                internal_name: Arc::from("java/util/ArrayList"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("add"),
                    params: MethodParams::from([("Ljava/lang/Object;", "e")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TE;)Z")),
                    return_type: Some(Arc::from("Z")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(root_scope());
        let cursor_byte = src.find('|').expect("expected |");
        let src_no_cursor = src.replacen('|', "", 1);
        let rope = ropey::Rope::from_str(&src_no_cursor);
        let cursor_char = rope.byte_to_char(cursor_byte);
        let line = rope.char_to_line(cursor_char) as u32;
        let col = (cursor_char - rope.line_to_char(line as usize)) as u32;
        let mut parser = super::make_java_parser();
        let tree = parser.parse(&src_no_cursor, None).expect("failed to parse");
        let mut ctx = super::JavaLanguage
            .parse_completion_context_with_tree(
                &src_no_cursor,
                &rope,
                tree.root_node(),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                },
            )
            .expect("parse_completion_context_with_tree returned None");
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let type_ctx = ctx
            .extension::<crate::language::java::type_ctx::SourceTypeCtx>()
            .expect("type ctx");
        let resolver = crate::semantic::types::TypeResolver::new(&view);
        let init_expr = "s.map(ArrayList::new).get()";
        let chain = crate::completion::parser::parse_chain_from_expr(init_expr);
        let seg_eval: Vec<String> = (0..chain.len())
            .map(|i| {
                crate::language::java::expression_typing::evaluate_chain(
                    &chain[..=i],
                    &ctx.local_variables,
                    ctx.enclosing_internal_name.as_ref(),
                    &resolver,
                    type_ctx,
                    &view,
                )
                .as_ref()
                .map(crate::semantic::types::type_name::TypeName::to_internal_with_generics)
                .unwrap_or_else(|| "<none>".to_string())
            })
            .collect();
        let s_ty = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .map(|lv| lv.type_internal.to_internal_with_generics())
            .unwrap_or_default();
        let direct_map = resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
            &s_ty,
            "map",
            1,
            &[],
            &["ArrayList::new".to_string()],
            &ctx.local_variables,
            ctx.enclosing_internal_name.as_ref(),
            Some(&|q| type_ctx.resolve_type_name_strict(q)),
        );
        let direct_get = direct_map.as_ref().and_then(|m| {
            resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &m.to_internal_with_generics(),
                "get",
                0,
                &[],
                &[],
                &ctx.local_variables,
                ctx.enclosing_internal_name.as_ref(),
                Some(&|q| type_ctx.resolve_type_name_strict(q)),
            )
        });
        let b = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "b")
            .expect("expected local b");
        assert_ne!(
            b.type_internal.erased_internal(),
            "var",
            "inner-class constructor-reference chain should materialize b to concrete type; locals={:?} chain={:?} seg_eval={:?} direct_map={:?} direct_get={:?}",
            ctx.local_variables
                .iter()
                .map(|lv| (
                    lv.name.to_string(),
                    lv.type_internal.to_internal_with_generics(),
                    lv.init_expr.clone()
                ))
                .collect::<Vec<_>>(),
            chain,
            seg_eval,
            direct_map
                .as_ref()
                .map(crate::semantic::types::type_name::TypeName::to_internal_with_generics),
            direct_get
                .as_ref()
                .map(crate::semantic::types::type_name::TypeName::to_internal_with_generics)
        );
    }

    #[test]
    fn test_functional_chain_conservative_when_method_ref_unresolved() {
        let idx = make_functional_chain_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> strBox = new Box<>(" hello ");
                strBox.map(Unknown::trim).get().subs|
            }
        }
        "#};
        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            !labels.iter().any(|l| l == "substring"),
            "unresolved method reference should stay conservative and avoid String-only members, got location={:?} labels={:?}",
            ctx.location,
            labels
        );
    }

    #[test]
    fn test_snapshot_functional_chain_completion_labels() {
        let idx = make_functional_chain_index();
        let view = idx.view(root_scope());

        let src_trim = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> strBox = new Box<>(" hello ");
                strBox.map(String::trim).get().subs|
            }
        }
        "#};
        let (trim_ctx, trim_labels) = ctx_and_labels_from_marked_source(src_trim, &view);

        let src_ctor = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> s = new Box<>("x");
                s.map(ArrayList::new).get().ad|
            }
        }
        "#};
        let (ctor_ctx, ctor_labels) = ctx_and_labels_from_marked_source(src_ctor, &view);

        let src_neg = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        class Demo {
            void f() {
                Box<String> strBox = new Box<>(" hello ");
                strBox.map(Unknown::trim).get().subs|
            }
        }
        "#};
        let (neg_ctx, neg_labels) = ctx_and_labels_from_marked_source(src_neg, &view);

        insta::assert_snapshot!(
            "functional_chain_completion_labels",
            format!(
                "trim_location={:?}\ntrim_has_substring={}\ntrim_first10={:?}\n\nctor_location={:?}\nctor_has_add={}\nctor_first10={:?}\n\nneg_location={:?}\nneg_has_substring={}\nneg_first10={:?}\n",
                trim_ctx.location,
                trim_labels.iter().any(|l| l == "substring"),
                trim_labels.iter().take(10).collect::<Vec<_>>(),
                ctor_ctx.location,
                ctor_labels.iter().any(|l| l == "add"),
                ctor_labels.iter().take(10).collect::<Vec<_>>(),
                neg_ctx.location,
                neg_labels.iter().any(|l| l == "substring"),
                neg_labels.iter().take(10).collect::<Vec<_>>(),
            )
        );
    }

    #[test]
    fn test_snapshot_location_precedence_member_vs_type_positions() {
        let src_member = indoc::indoc! {r#"
        import java.util.*;
        class A {
            void f() {
                var a = new HashMap<String, String>();
                a.put
            }
        }
        "#};
        let (member_line, member_col) = src_member
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.put")
                    .map(|c| (i as u32, c as u32 + "a.put".len() as u32))
            })
            .expect("expected a.put marker");
        let member_ctx = at(src_member, member_line, member_col);

        let src_member_alt = indoc::indoc! {r#"
        import java.util.*;
        class A {
            void f() {
                var a = new HashMap<String, String>();
                a.a
            }
        }
        "#};
        let (member_alt_line, member_alt_col) = src_member_alt
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.a")
                    .map(|c| (i as u32, c as u32 + "a.a".len() as u32))
            })
            .expect("expected a.a marker");
        let member_alt_ctx = at(src_member_alt, member_alt_line, member_alt_col);

        let src_generic = indoc::indoc! {r#"
        class A {
            void f() {
                List<Bo> nums = new ArrayList<>();
            }
        }
        "#};
        let (generic_line, generic_col) = src_generic
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("Bo").map(|c| (i as u32, c as u32 + 2)))
            .expect("expected Bo marker");
        let generic_ctx = at(src_generic, generic_line, generic_col);

        let src_ctor_generic = indoc::indoc! {r#"
        class A {
            void f() {
                new Box<In>(1);
            }
        }
        "#};
        let (ctor_line, ctor_col) = src_ctor_generic
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("In").map(|c| (i as u32, c as u32 + 2)))
            .expect("expected In marker");
        let ctor_ctx = at(src_ctor_generic, ctor_line, ctor_col);

        let out = format!(
            "member={:?}\nmember_alt={:?}\ngeneric={:?}\nctor_generic={:?}\n",
            member_ctx.location, member_alt_ctx.location, generic_ctx.location, ctor_ctx.location
        );
        insta::assert_snapshot!("location_precedence_member_vs_type_positions", out);
    }

    #[test]
    fn test_snapshot_force_injection_anchor_for_member_tail_with_following_generics() {
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
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.p")
                    .map(|c| (i as u32, c as u32 + "a.p".len() as u32))
            })
            .expect("expected a.p marker");
        let offset = line_col_to_offset(src, line, col).expect("offset");
        let mut parser = super::make_java_parser();
        let tree = parser.parse(src, None).expect("failed to parse java");
        let extractor = JavaContextExtractor::new(src, offset, None);
        let cursor_node = extractor.find_cursor_node(tree.root_node());
        let cursor_info = cursor_node.map(|n| {
            format!(
                "{}:[{}..{}]:'{}'",
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                extractor.node_text(n)
            )
        });

        let (raw_loc, raw_query) = location::determine_location(&extractor, cursor_node, None);
        let predicates = injection::force_injection_predicates(&extractor, cursor_node, &raw_loc);
        let force = injection::should_force_injection(&extractor, cursor_node, &raw_loc);
        let injected = build_injected_source(&extractor, cursor_node);
        let inject_result = injection::inject_and_determine(&extractor, cursor_node, None);

        let out = format!(
            "offset={offset}\ncursor_node={cursor_info:?}\nraw_location={raw_loc:?}\nraw_query={raw_query:?}\npredicates={predicates:?}\nforce_injection={force}\ninjected_source=\n{injected}\ninject_result={inject_result:?}\n"
        );
        insta::assert_snapshot!(
            "force_injection_anchor_member_tail_with_following_generics",
            out
        );
    }

    #[test]
    fn test_snapshot_force_injection_predicates_member_tail_vs_generic_type_arg() {
        let src_member = indoc::indoc! {r#"
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
        let (m_line, m_col) = src_member
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.p")
                    .map(|c| (i as u32, c as u32 + "a.p".len() as u32))
            })
            .expect("member marker");
        let m_off = line_col_to_offset(src_member, m_line, m_col).expect("member offset");
        let mut parser = super::make_java_parser();
        let m_tree = parser.parse(src_member, None).expect("member parse");
        let m_extractor = JavaContextExtractor::new(src_member, m_off, None);
        let m_cursor = m_extractor.find_cursor_node(m_tree.root_node());
        let (m_loc, _) = location::determine_location(&m_extractor, m_cursor, None);
        let m_pred = injection::force_injection_predicates(&m_extractor, m_cursor, &m_loc);
        let m_force = injection::should_force_injection(&m_extractor, m_cursor, &m_loc);

        let src_member_alt = indoc::indoc! {r#"
        class Demo {
            void f() {
                var a = new HashMap<String, String>();
                a.a
                List<Box<? extends Number>> nums = List.of(
                    new Box<>(1),
                    new Box<>(2.5)
                );
                nums.add();
            }
        }
        "#};
        let (ma_line, ma_col) = src_member_alt
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("a.a")
                    .map(|c| (i as u32, c as u32 + "a.a".len() as u32))
            })
            .expect("member-alt marker");
        let ma_off =
            line_col_to_offset(src_member_alt, ma_line, ma_col).expect("member-alt offset");
        let ma_tree = parser
            .parse(src_member_alt, None)
            .expect("member-alt parse");
        let ma_extractor = JavaContextExtractor::new(src_member_alt, ma_off, None);
        let ma_cursor = ma_extractor.find_cursor_node(ma_tree.root_node());
        let (ma_loc, _) = location::determine_location(&ma_extractor, ma_cursor, None);
        let ma_pred = injection::force_injection_predicates(&ma_extractor, ma_cursor, &ma_loc);
        let ma_force = injection::should_force_injection(&ma_extractor, ma_cursor, &ma_loc);

        let src_generic = indoc::indoc! {r#"
        class A {
            void f() {
                List<Bo> nums = new ArrayList<>();
            }
        }
        "#};
        let (g_line, g_col) = src_generic
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("Bo").map(|c| (i as u32, c as u32 + 2)))
            .expect("generic marker");
        let g_off = line_col_to_offset(src_generic, g_line, g_col).expect("generic offset");
        let g_tree = parser.parse(src_generic, None).expect("generic parse");
        let g_extractor = JavaContextExtractor::new(src_generic, g_off, None);
        let g_cursor = g_extractor.find_cursor_node(g_tree.root_node());
        let (g_loc, _) = location::determine_location(&g_extractor, g_cursor, None);
        let g_pred = injection::force_injection_predicates(&g_extractor, g_cursor, &g_loc);
        let g_force = injection::should_force_injection(&g_extractor, g_cursor, &g_loc);

        let src_ctor_generic = indoc::indoc! {r#"
        class A {
            void f() {
                new Box<In>(1);
            }
        }
        "#};
        let (c_line, c_col) = src_ctor_generic
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("In").map(|c| (i as u32, c as u32 + 2)))
            .expect("ctor-generic marker");
        let c_off = line_col_to_offset(src_ctor_generic, c_line, c_col).expect("ctor offset");
        let c_tree = parser.parse(src_ctor_generic, None).expect("ctor parse");
        let c_extractor = JavaContextExtractor::new(src_ctor_generic, c_off, None);
        let c_cursor = c_extractor.find_cursor_node(c_tree.root_node());
        let (c_loc, _) = location::determine_location(&c_extractor, c_cursor, None);
        let c_pred = injection::force_injection_predicates(&c_extractor, c_cursor, &c_loc);
        let c_force = injection::should_force_injection(&c_extractor, c_cursor, &c_loc);

        let m_cursor_info = m_cursor.map(|n| {
            format!(
                "{}:[{}..{}]:'{}'",
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                m_extractor.node_text(n)
            )
        });
        let g_cursor_info = g_cursor.map(|n| {
            format!(
                "{}:[{}..{}]:'{}'",
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                g_extractor.node_text(n)
            )
        });
        let ma_cursor_info = ma_cursor.map(|n| {
            format!(
                "{}:[{}..{}]:'{}'",
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                ma_extractor.node_text(n)
            )
        });
        let c_cursor_info = c_cursor.map(|n| {
            format!(
                "{}:[{}..{}]:'{}'",
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                c_extractor.node_text(n)
            )
        });

        assert!(
            m_force,
            "a.p should force injection in misread member-tail case"
        );
        assert!(
            ma_force,
            "a.a should force injection in misread member-tail case"
        );
        assert!(!g_force, "List<Bo> should not force injection");
        assert!(!c_force, "new Box<In>(1) should not force injection");

        let out = format!(
            "member_case:\noffset={m_off}\ncursor={m_cursor_info:?}\nlocation={m_loc:?}\npredicates={m_pred:?}\nforce={m_force}\n\nmember_alt_case:\noffset={ma_off}\ncursor={ma_cursor_info:?}\nlocation={ma_loc:?}\npredicates={ma_pred:?}\nforce={ma_force}\n\ngeneric_case:\noffset={g_off}\ncursor={g_cursor_info:?}\nlocation={g_loc:?}\npredicates={g_pred:?}\nforce={g_force}\n\nctor_generic_case:\noffset={c_off}\ncursor={c_cursor_info:?}\nlocation={c_loc:?}\npredicates={c_pred:?}\nforce={c_force}\n"
        );
        insta::assert_snapshot!(
            "force_injection_predicates_member_tail_vs_generic_type_arg",
            out
        );
    }

    #[test]
    fn test_snapshot_inner_class_box_visibility_pipeline_provenance() {
        let src_base = indoc::indoc! {r#"
        package org.cubewhy;

        import java.util.*;

        public class ClassWithGenerics<T> {
            class Box<T> {}

            public T get() {
                return null;
            }
        }
        "#};

        let parsed = parse_java_source(src_base, ClassOrigin::Unknown, None);
        let mut parsed_classes: Vec<String> = parsed
            .iter()
            .map(|c| {
                format!(
                    "name={} internal={} inner_of={:?}",
                    c.name, c.internal_name, c.inner_class_of
                )
            })
            .collect();
        parsed_classes.sort();

        let idx = WorkspaceIndex::new();
        idx.add_classes(parsed);
        let view = idx.view(root_scope());

        let mut index_box_hits: Vec<String> = view
            .get_classes_by_simple_name("Box")
            .into_iter()
            .map(|c| {
                format!(
                    "name={} internal={} inner_of={:?}",
                    c.name, c.internal_name, c.inner_class_of
                )
            })
            .collect();
        index_box_hits.sort();

        let engine = CompletionEngine::new();

        let src_type = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        public class ClassWithGenerics<T> {
            class Box<T> {}
            public T get() {
                List<Bo> nums = new ArrayList<>();
                return null;
            }
        }
        "#};
        let (type_line, type_col) = src_type
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("Bo>").map(|c| (i as u32, c as u32 + 2)))
            .expect("List<Bo> marker");
        let type_ctx = {
            let rope = ropey::Rope::from_str(src_type);
            let mut parser = super::make_java_parser();
            let tree = parser.parse(src_type, None).expect("parse");
            super::JavaLanguage
                .parse_completion_context_with_tree(
                    src_type,
                    &rope,
                    tree.root_node(),
                    type_line,
                    type_col,
                    None,
                    &ParseEnv {
                        name_table: Some(view.build_name_table()),
                    },
                )
                .expect("type ctx")
        };
        let type_location = format!("{:?}", type_ctx.location);
        let mut type_labels: Vec<String> = engine
            .complete(root_scope(), type_ctx, &JavaLanguage, &view)
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();
        type_labels.sort();

        let src_ctor = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        public class ClassWithGenerics<T> {
            class Box<T> {}
            public T get() {
                new Bo
                return null;
            }
        }
        "#};
        let (ctor_line, ctor_col) = src_ctor
            .lines()
            .enumerate()
            .find_map(|(i, l)| {
                l.find("new Bo")
                    .map(|c| (i as u32, c as u32 + "new Bo".len() as u32))
            })
            .expect("new Bo marker");
        let ctor_ctx = {
            let rope = ropey::Rope::from_str(src_ctor);
            let mut parser = super::make_java_parser();
            let tree = parser.parse(src_ctor, None).expect("parse");
            super::JavaLanguage
                .parse_completion_context_with_tree(
                    src_ctor,
                    &rope,
                    tree.root_node(),
                    ctor_line,
                    ctor_col,
                    None,
                    &ParseEnv {
                        name_table: Some(view.build_name_table()),
                    },
                )
                .expect("ctor ctx")
        };
        let ctor_location = format!("{:?}", ctor_ctx.location);
        let mut ctor_labels: Vec<String> = engine
            .complete(root_scope(), ctor_ctx, &JavaLanguage, &view)
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();
        ctor_labels.sort();

        let src_decl = indoc::indoc! {r#"
        package org.cubewhy;
        import java.util.*;
        public class ClassWithGenerics<T> {
            class Box<T> {}
            public T get() {
                Box<String> x = null;
                return null;
            }
        }
        "#};
        let (decl_line, decl_col) = src_decl
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("Box<String>").map(|c| (i as u32, c as u32 + 2)))
            .expect("Box<String> marker");
        let decl_ctx = {
            let rope = ropey::Rope::from_str(src_decl);
            let mut parser = super::make_java_parser();
            let tree = parser.parse(src_decl, None).expect("parse");
            super::JavaLanguage
                .parse_completion_context_with_tree(
                    src_decl,
                    &rope,
                    tree.root_node(),
                    decl_line,
                    decl_col,
                    None,
                    &ParseEnv {
                        name_table: Some(view.build_name_table()),
                    },
                )
                .expect("decl ctx")
        };
        let decl_location = format!("{:?}", decl_ctx.location);
        let mut decl_labels: Vec<String> = engine
            .complete(root_scope(), decl_ctx, &JavaLanguage, &view)
            .into_iter()
            .map(|c| c.label.to_string())
            .collect();
        decl_labels.sort();

        let out = format!(
            "parsed_classes:\n{}\n\nindex_lookup_box:\n{}\n\ntype_location={}\ntype_annotation_list_bo_candidates_contains_box={}\nfirst_20={:?}\n\nctor_location={}\nconstructor_new_bo_candidates_contains_box={}\nfirst_20={:?}\n\ndecl_location={}\ndeclaration_box_string_candidates_contains_box={}\nfirst_20={:?}\n",
            parsed_classes.join("\n"),
            index_box_hits.join("\n"),
            type_location,
            type_labels.iter().any(|l| l == "Box"),
            type_labels.iter().take(20).collect::<Vec<_>>(),
            ctor_location,
            ctor_labels.iter().any(|l| l == "Box"),
            ctor_labels.iter().take(20).collect::<Vec<_>>(),
            decl_location,
            decl_labels.iter().any(|l| l == "Box"),
            decl_labels.iter().take(20).collect::<Vec<_>>(),
        );
        insta::assert_snapshot!("inner_class_box_visibility_pipeline_provenance", out);
    }

    #[test]
    fn test_constructor_generic_type_argument_routes_to_type_annotation() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new Box<In>(1);
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("In").map(|c| (i as u32, c as u32 + 2)))
            .expect("expected In marker");
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::TypeAnnotation { .. }),
            "constructor generic arg should route to TypeAnnotation, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_constructor_empty_generic_hole_routes_to_type_annotation() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new Box<>(1);
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("<>").map(|c| (i as u32, c as u32 + 1)))
            .expect("expected <> marker");
        let ctx = at(src, line, col);
        assert!(
            matches!(ctx.location, CursorLocation::TypeAnnotation { .. }),
            "constructor empty generic hole should route to TypeAnnotation, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_constructor_argument_list_hole_is_not_type_annotation() {
        let src = indoc::indoc! {r#"
        class A {
            void f() {
                new ArrayList(
            }
        }
        "#};
        let (line, col) = src
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("new ArrayList(").map(|c| (i as u32, c as u32 + 14)))
            .expect("expected constructor call");
        let ctx = at(src, line, col);
        assert!(
            !matches!(ctx.location, CursorLocation::TypeAnnotation { .. }),
            "constructor argument-list hole must not be TypeAnnotation, got {:?}",
            ctx.location
        );
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
    fn test_completion_local_var_inside_method_argument_concatenation() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void m() {
                int intValue = 1;
                System.out.println("intValue = " + intVa|);
            }
        }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            matches!(&ctx.location, CursorLocation::MethodArgument { prefix } if prefix == "intVa"),
            "expected MethodArgument{{intVa}}, got {:?}",
            ctx.location
        );
        assert!(
            labels.iter().any(|l| l == "intValue"),
            "intValue should be offered inside arg subexpression, labels={labels:?}"
        );
    }

    #[test]
    fn test_completion_local_var_inside_empty_method_argument_expression_hole() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void m() {
                int testValue = 1;
                System.out.println("test = " + |);
            }
        }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            matches!(&ctx.location, CursorLocation::MethodArgument { prefix } if prefix.is_empty()),
            "expected MethodArgument with empty prefix, got {:?}",
            ctx.location
        );
        assert!(
            labels.iter().any(|l| l == "testValue"),
            "testValue should be offered for empty expression hole in method argument, labels={labels:?}"
        );
    }

    #[test]
    fn test_completion_local_var_inside_plain_method_argument() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void sink(String s) {}
            void m() {
                String value = "x";
                sink(val|);
            }
        }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            matches!(&ctx.location, CursorLocation::MethodArgument { prefix } if prefix == "val"),
            "expected MethodArgument{{val}}, got {:?}",
            ctx.location
        );
        assert!(
            labels.iter().any(|l| l == "value"),
            "value should be offered in method-argument completion, labels={labels:?}"
        );
    }

    #[test]
    fn test_completion_local_var_outside_method_argument_regression() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void m() {
                int intValue = 1;
                intVa|;
            }
        }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            labels.iter().any(|l| l == "intValue"),
            "intValue should still be offered in ordinary expression completion, labels={labels:?}"
        );
    }

    #[test]
    fn test_completion_local_var_inside_empty_non_argument_expression_hole_regression() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void m() {
                int testValue = 1;
                testValue + |;
            }
        }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            labels.iter().any(|l| l == "testValue"),
            "testValue should be offered in non-argument empty expression hole, labels={labels:?}"
        );
    }

    #[test]
    fn test_member_completion_inside_method_argument_regression() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("java/lang")),
            name: Arc::from("String"),
            internal_name: Arc::from("java/lang/String"),
            super_name: Some(Arc::from("java/lang/Object")),
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("toString"),
                params: MethodParams::empty(),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: Some(Arc::from("Ljava/lang/String;")),
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: None,
            origin: ClassOrigin::Unknown,
        }]);
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void m() {
                String value = "x";
                System.out.println(value.toStr|);
            }
        }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            matches!(&ctx.location, CursorLocation::MemberAccess { member_prefix, .. } if member_prefix == "toStr"),
            "expected MemberAccess{{toStr}}, got {:?}",
            ctx.location
        );
        assert!(
            labels.iter().any(|l| l == "toString"),
            "member completion should still work in method arguments, labels={labels:?}"
        );
    }

    #[test]
    fn test_method_completion_inserts_parentheses() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void foo() {}
            void m() {
                fo|;
            }
        }
        "#};

        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let foo = candidates
            .iter()
            .find(|c| c.label.as_ref() == "foo" && matches!(c.kind, CandidateKind::Method { .. }))
            .expect("expected method candidate foo");
        assert_eq!(foo.insert_text, "foo()");
    }

    #[test]
    fn test_method_completion_inserts_parameter_slots() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void print(String s, int n) {}
            void m() {
                pri|;
            }
        }
        "#};

        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let print = candidates
            .iter()
            .find(|c| c.label.as_ref() == "print" && matches!(c.kind, CandidateKind::Method { .. }))
            .expect("expected method candidate print");
        assert_eq!(print.insert_text, "print(${1:s}, ${2:n})$0");
    }

    #[test]
    fn test_method_completion_avoids_duplicate_parentheses() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        class T {
            void foo(int x) {}
            void m() {
                this.fo|();
            }
        }
        "#};

        let (ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        assert!(
            ctx.has_paren_after_cursor(),
            "expected parser to detect existing '(' after cursor"
        );
        let foo = candidates
            .iter()
            .find(|c| c.label.as_ref() == "foo" && matches!(c.kind, CandidateKind::Method { .. }))
            .expect("expected method candidate foo");
        assert_eq!(
            foo.insert_text, "foo",
            "must avoid adding duplicate parentheses when '(' already exists"
        );
    }

    #[test]
    fn test_constructor_completion_inserts_parameter_slots() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![ClassMetadata {
            package: Some(Arc::from("org/cubewhy")),
            name: Arc::from("Printer"),
            internal_name: Arc::from("org/cubewhy/Printer"),
            super_name: Some(Arc::from("java/lang/Object")),
            interfaces: vec![],
            annotations: vec![],
            methods: vec![MethodSummary {
                name: Arc::from("<init>"),
                params: MethodParams::from([("Ljava/lang/String;", "message"), ("I", "count")]),
                annotations: vec![],
                access_flags: ACC_PUBLIC,
                is_synthetic: false,
                generic_signature: None,
                return_type: None,
            }],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: None,
            origin: ClassOrigin::Unknown,
        }]);
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
        package org.cubewhy;
        class T {
            void m() {
                new Pri|;
            }
        }
        "#};

        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let ctor = candidates
            .iter()
            .find(|c| {
                c.label.as_ref() == "Printer" && matches!(c.kind, CandidateKind::Constructor { .. })
            })
            .expect("expected constructor candidate Printer");
        assert_eq!(ctor.insert_text, "Printer(${1:message}, ${2:count})$0");
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
        let ctx = at(src, 0, col);
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
        let ctx = at(src, 0, 0);
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

        let extractor = super::JavaContextExtractor::new(src, 0, None);

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
    fn test_locals_ignore_incomplete_following_declaration_type_token() {
        let src = indoc::indoc! {r#"
    class A {
        void f() {
            var b = make();
            b nums = makeNums();
        }
    }
    "#};
        let line = 3u32;
        let col = src
            .lines()
            .nth(line as usize)
            .and_then(|l| l.find("b nums").map(|c| c as u32 + 1))
            .expect("expected b nums marker");
        let ctx = at(src, line, col);
        assert!(
            ctx.local_variables.iter().any(|v| v.name.as_ref() == "b"),
            "previous local b should be visible"
        );
        assert!(
            !ctx.local_variables
                .iter()
                .any(|v| v.name.as_ref() == "nums"),
            "incomplete next declaration should not leak nums into locals: {:?}",
            ctx.local_variables
                .iter()
                .map(|v| (
                    v.name.to_string(),
                    v.type_internal.to_internal_with_generics()
                ))
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
        let extractor = JavaContextExtractor::new(src, offset, None);

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
        let extractor = JavaContextExtractor::new(src, offset, None);

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
            JavaContextExtractor::new(src, 22, None) // 离开作用域 src 已 move 进去
        };
        // ctx 仍然有效
        assert_eq!(ctx.source_str().len(), 35);
        assert!(!ctx.is_in_comment());
    }

    #[test]
    fn test_extractor_for_indexing() {
        let ctx = JavaContextExtractor::for_indexing("class A {}", None);
        assert_eq!(ctx.offset, 0);
        assert_eq!(ctx.source_str(), "class A {}");
    }

    #[test]
    fn test_extractor_byte_slice() {
        let src = "class A { void f() {} }";
        let ctx = JavaContextExtractor::new(src, 0, None);
        // tree-sitter 给出字节偏移，byte_slice 应正确切片
        assert_eq!(ctx.byte_slice(0, 5), "class");
        assert_eq!(ctx.byte_slice(6, 7), "A");
    }

    #[test]
    fn test_extractor_is_in_comment_true() {
        let src = "class A { // comment\n void f() {} }";
        // offset 在 comment 内部
        let col = src.find("//").unwrap() + 5;
        let ctx = JavaContextExtractor::new(src, col, None);
        assert!(ctx.is_in_comment());
    }

    #[test]
    fn test_extractor_is_in_comment_false() {
        let src = "class A { void f() {} }";
        let ctx = JavaContextExtractor::new(src, 10, None);
        assert!(!ctx.is_in_comment());
    }

    #[test]
    fn test_extractor_with_rope_reuses_rope() {
        // with_rope 构造不重复建 Rope（性能路径）
        let src = String::from("package a; class B {}");
        let rope = ropey::Rope::from_str(&src);
        let line_count = rope.len_lines();
        let ctx = JavaContextExtractor::with_rope(src, 0, rope, None);
        assert_eq!(ctx.rope.len_lines(), line_count);
    }

    #[test]
    fn test_extractor_node_text_multibyte() {
        // 含 CJK 字符的类名，node_text 应正确返回
        let src = "class 测试类 {}";
        let ctx = JavaContextExtractor::new(src, 0, None);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        // 仅确保不 panic，multibyte 处理正确
        let _ = parser.parse(ctx.source_str(), None);
    }

    fn make_nested_chaincheck_index() -> WorkspaceIndex {
        let src = indoc::indoc! {r#"
            package org.cubewhy;

            class ChainCheck {
                static class Box<T> {
                    static class BoxV<V> {
                        V getV() { return null; }
                    }

                    Box(T value) {
                        var a = new BoxV();
                        a.getV();
                    }
                }
            }

            class Top {}
        "#};
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        idx
    }

    #[test]
    fn test_package_like_member_access_excludes_nested_classes() {
        let idx = make_nested_chaincheck_index();
        let view = idx.view(root_scope());
        let (_, labels) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                package org.cubewhy;
                class Probe {
                    void test() {
                        org.cubewhy.|
                    }
                }
            "#},
            &view,
        );
        assert!(
            labels.iter().any(|l| l == "org.cubewhy.ChainCheck"),
            "{labels:?}"
        );
        assert!(labels.iter().any(|l| l == "org.cubewhy.Top"), "{labels:?}");
        assert!(
            labels
                .iter()
                .all(|l| l != "org.cubewhy.Box" && l != "org.cubewhy.BoxV"),
            "{labels:?}"
        );
    }

    #[test]
    fn test_class_qualifier_member_access_exposes_nested_class() {
        let idx = make_nested_chaincheck_index();
        let view = idx.view(root_scope());
        let (_, candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                package org.cubewhy;
                class Probe {
                    void test() {
                        ChainCheck.|
                    }
                }
            "#},
            &view,
        );
        let labels: Vec<String> = candidates.iter().map(|c| c.label.to_string()).collect();
        assert!(labels.iter().any(|l| l == "Box"), "{labels:?}");
        assert!(
            candidates
                .iter()
                .any(|c| c.label.as_ref() == "Box" && c.source == "expression"),
            "expected Box from expression provider, got {:?}",
            candidates
                .iter()
                .map(|c| format!("{}@{}", c.label, c.source))
                .collect::<Vec<_>>()
        );
        assert!(
            candidates.iter().all(|c| c.source != "package"),
            "package provider should be gated off here: {:?}",
            candidates
                .iter()
                .map(|c| format!("{}@{}", c.label, c.source))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_nested_qualifier_member_access_exposes_nested_child_class() {
        let idx = make_nested_chaincheck_index();
        let view = idx.view(root_scope());
        let (_, labels) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                package org.cubewhy;
                class Probe {
                    void test() {
                        ChainCheck.Box.|
                    }
                }
            "#},
            &view,
        );
        assert!(labels.iter().any(|l| l == "BoxV"), "{labels:?}");
    }

    #[test]
    fn test_nested_constructor_var_materializes_local_type() {
        let idx = make_nested_chaincheck_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            package org.cubewhy;

            class ChainCheck {
                static class Box<T> {
                    static class BoxV<V> {
                        V getV() { return null; }
                    }

                    Box(T value) {
                        var a = new BoxV();
                        a
                    }
                }
            }
        "#};
        let line = src
            .lines()
            .position(|l| l.trim() == "a")
            .expect("line containing a") as u32;
        let ctx = at(src, line, 9);
        let mut enriched = ctx.with_extension(Arc::new(SourceTypeCtx::new(
            Some(Arc::from("org/cubewhy")),
            vec![],
            Some(view.build_name_table()),
        )));
        JavaLanguage.enrich_completion_context(&mut enriched, root_scope(), &view);
        let a = enriched
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "a")
            .expect("local a");
        assert_eq!(
            a.type_internal.to_internal_with_generics(),
            "org/cubewhy/ChainCheck$Box$BoxV",
            "location={:?} enclosing={:?} locals={:?}",
            enriched.location,
            enriched.enclosing_internal_name,
            enriched
                .local_variables
                .iter()
                .map(|lv| format!(
                    "{}:{} init={:?}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics(),
                    lv.init_expr
                ))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_nested_constructor_var_chain_completion_resolves_getv() {
        let idx = make_nested_chaincheck_index();
        let view = idx.view(root_scope());
        let (_, labels) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                package org.cubewhy;

                class ChainCheck {
                    static class Box<T> {
                        static class BoxV<V> {
                            V getV() { return null; }
                        }

                        Box(T value) {
                            var a = new BoxV();
                            a.g|
                        }
                    }
                }
            "#},
            &view,
        );
        assert!(labels.iter().any(|l| l == "getV"), "{labels:?}");
    }

    #[test]
    fn test_completion_timing_nested_qualifiers() {
        use std::time::Instant;

        let idx = make_nested_chaincheck_index();
        let view = idx.view(root_scope());

        let t1 = Instant::now();
        let (_, labels1) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                package org.cubewhy;
                class Probe { void t() { ChainCheck.| } }
            "#},
            &view,
        );
        let d1 = t1.elapsed().as_secs_f64() * 1000.0;

        let t2 = Instant::now();
        let (_, labels2) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                package org.cubewhy;
                class Probe { void t() { ChainCheck.Box.| } }
            "#},
            &view,
        );
        let d2 = t2.elapsed().as_secs_f64() * 1000.0;

        eprintln!("nested_completion_timing_ms: ChainCheck.={d1:.3} ChainCheck.Box.={d2:.3}");
        assert!(labels1.iter().any(|l| l == "Box"), "{labels1:?}");
        assert!(labels2.iter().any(|l| l == "BoxV"), "{labels2:?}");
    }

    #[test]
    fn test_context_class_body_slot_after_nested_class_not_unknown() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                public class VarargsExample {
                    public static class Test implements Runnable {
                        @Override
                        public void run() {
                            throw new RuntimeException("Not implemented yet");
                        }
                    }

                    |
                }
            "#},
            &view,
        );
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "class body slot should not be Unknown: {:?}",
            ctx.location
        );
        assert!(
            ctx.is_class_member_position,
            "outer class body slot should be class-member position"
        );
    }

    #[test]
    fn test_context_nested_class_body_slot_not_unknown() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                public class VarargsExample {
                    public static class Test implements Runnable {
                        |
                    }
                }
            "#},
            &view,
        );
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "nested class body slot should not be Unknown: {:?}",
            ctx.location
        );
        assert!(
            ctx.is_class_member_position,
            "nested class body slot should be class-member position"
        );
    }

    #[test]
    fn test_context_top_level_class_body_slot_not_unknown() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                public class VarargsExample {
                    |
                }
            "#},
            &view,
        );
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "top-level class body slot should not be Unknown: {:?}",
            ctx.location
        );
        assert!(
            ctx.is_class_member_position,
            "top-level class body slot should be class-member position"
        );
    }

    #[test]
    fn test_context_method_and_constructor_bodies_not_class_member_position() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());

        let (method_ctx, _) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                public class VarargsExample {
                    void f() {
                        |
                    }
                }
            "#},
            &view,
        );
        assert!(
            !method_ctx.is_class_member_position,
            "method body must not be class-member position"
        );

        let (ctor_ctx, _) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                public class VarargsExample {
                    VarargsExample() {
                        |
                    }
                }
            "#},
            &view,
        );
        assert!(
            !ctor_ctx.is_class_member_position,
            "constructor body must not be class-member position"
        );
    }

    #[test]
    fn test_context_partial_member_prefix_top_level_class_not_unknown() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                public class A {
                    prote|
                }
            "#},
            &view,
        );
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "partial class member prefix should not be Unknown: {:?}",
            ctx.location
        );
        assert!(ctx.is_class_member_position);
    }

    #[test]
    fn test_context_partial_member_prefix_after_nested_class_not_unknown() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                public class A {
                    class B {}
                    prote|
                }
            "#},
            &view,
        );
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "partial class member prefix after nested class should not be Unknown: {:?}",
            ctx.location
        );
        assert!(ctx.is_class_member_position);
    }

    #[test]
    fn test_context_partial_member_prefix_in_nested_class_not_unknown() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
                public class A {
                    class B {
                        prote|
                    }
                }
            "#},
            &view,
        );
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "partial class member prefix in nested class should not be Unknown: {:?}",
            ctx.location
        );
        assert!(ctx.is_class_member_position);
    }
}
