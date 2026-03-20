use std::sync::Arc;

use super::Language;
use crate::completion::provider::CompletionProvider;
use crate::index::{IndexScope, IndexView, NameTable};
use crate::language::java::completion::providers::{
    annotation::AnnotationProvider, constructor::ConstructorProvider,
    expression::ExpressionProvider, import::ImportProvider, import_static::ImportStaticProvider,
    intrinsic_member::IntrinsicMemberProvider, keyword::KeywordProvider,
    local_var::LocalVarProvider, member::MemberProvider, name_suggestion::NameSuggestionProvider,
    override_member::OverrideProvider, package::PackageProvider, snippet::SnippetProvider,
    statement_label::StatementLabelProvider, static_import_member::StaticImportMemberProvider,
    static_member::StaticMemberProvider, this_member::ThisMemberProvider,
};
use crate::language::java::inlay_hints::{JavaInlayHintKind, collect_java_inlay_hints};
use crate::language::java::symbols::collect_java_symbols;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::language::rope_utils::rope_line_col_to_offset;
use crate::language::{ClassifiedToken, ParseEnv};
use crate::semantic::{CursorLocation, SemanticContext};
use crate::workspace::SourceFile;
use ropey::Rope;
use smallvec::smallvec;
use tower_lsp::lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, Position, Range, SemanticTokenModifier,
    SemanticTokenType,
};
use tree_sitter::{Node, Parser};
use tree_sitter_utils::traversal::find_node_by_offset;

pub mod class_parser;
pub mod completion;
pub mod completion_context;
pub mod editor_semantics;
pub mod expression_typing;
pub mod flow;
pub mod inlay_hints;
pub mod intrinsics;
pub mod locals;
pub mod location;
pub mod lombok;
pub mod members;
pub mod render;
pub mod scope;
pub mod symbols;
pub mod synthetic;
pub mod type_ctx;
pub mod utils;

static JAVA_COMPLETION_PROVIDERS: [&dyn CompletionProvider; 17] = [
    &LocalVarProvider,
    &StatementLabelProvider,
    &ThisMemberProvider,
    &IntrinsicMemberProvider,
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
        file: &SourceFile,
        line: u32,
        character: u32,
        trigger_char: Option<char>,
        env: &ParseEnv,
    ) -> Option<SemanticContext> {
        let offset = rope_line_col_to_offset(&file.rope, line, character)?;
        tracing::debug!(line, character, trigger = ?trigger_char, "java: parsing context (cached tree)");
        let mut extractor = JavaContextExtractor::with_rope(
            file.text().to_string(),
            offset,
            (*file.rope).clone(),
            env.name_table.clone(),
        );

        // Set workspace and file URI if available for incremental parsing
        if let Some(workspace) = env.workspace.clone() {
            extractor = extractor.with_workspace(workspace);
        }
        extractor = extractor.with_file_uri(Arc::from(file.uri.as_str()));

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
        let root = file.root_node()?;
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
        file: &'a SourceFile,
    ) -> Option<ClassifiedToken> {
        let bytes = file.bytes();
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
        file: &'a SourceFile,
    ) -> Option<Vec<tower_lsp::lsp_types::DocumentSymbol>> {
        Some(collect_java_symbols(node, file.bytes()))
    }

    fn supports_inlay_hints(&self) -> bool {
        true
    }

    fn collect_inlay_hints_with_tree(
        &self,
        file: &SourceFile,
        range: Range,
        env: &ParseEnv,
        index: &IndexView,
    ) -> Option<Vec<InlayHint>> {
        let root = file.root_node()?;
        let byte_range = lsp_range_to_byte_range(&file.rope, range, file.text().len())?;
        let hints = collect_java_inlay_hints(
            file.text(),
            &file.rope,
            root,
            env.name_table.clone(),
            index,
            byte_range,
        );

        Some(
            hints
                .into_iter()
                .map(|hint| InlayHint {
                    position: byte_offset_to_position(&file.rope, hint.offset),
                    label: InlayHintLabel::String(hint.label),
                    kind: Some(match hint.kind {
                        JavaInlayHintKind::Type => InlayHintKind::TYPE,
                        JavaInlayHintKind::Parameter => InlayHintKind::PARAMETER,
                    }),
                    text_edits: None,
                    tooltip: None,
                    padding_left: Some(matches!(hint.kind, JavaInlayHintKind::Type)),
                    padding_right: Some(matches!(hint.kind, JavaInlayHintKind::Parameter)),
                    data: None,
                })
                .collect(),
        )
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
        Some(crate::salsa_queries::java::extract_java_completion_context(
            db,
            file,
            line,
            character,
            trigger_char,
        ))
    }

    fn resolve_symbol_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        line: u32,
        character: u32,
    ) -> Option<Arc<crate::salsa_queries::ResolvedSymbolData>> {
        crate::salsa_queries::java::resolve_java_symbol(db, file, line, character)
    }

    fn compute_inlay_hints_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        range: Range,
    ) -> Option<Arc<Vec<crate::salsa_queries::InlayHintData>>> {
        Some(crate::salsa_queries::java::compute_java_inlay_hints(
            db,
            file,
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
        ))
    }
}

fn is_annotation_name(node: Node) -> bool {
    node.parent().is_some_and(|p| {
        (p.kind() == "annotation" || p.kind() == "marker_annotation")
            && p.child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id())
    })
}

fn lsp_range_to_byte_range(
    rope: &Rope,
    range: Range,
    len: usize,
) -> Option<std::ops::Range<usize>> {
    let start = rope_line_col_to_offset(rope, range.start.line, range.start.character)?;
    let end = rope_line_col_to_offset(rope, range.end.line, range.end.character)?;
    Some(start.min(len)..end.min(len))
}

fn byte_offset_to_position(rope: &Rope, offset: usize) -> Position {
    let char_idx = rope.byte_to_char(offset.min(rope.len_bytes()));
    let line_idx = rope.char_to_line(char_idx);
    let line_char_start = rope.line_to_char(line_idx);
    let character = rope
        .slice(line_char_start..char_idx)
        .chars()
        .map(char::len_utf16)
        .sum::<usize>() as u32;
    Position {
        line: line_idx as u32,
        character,
    }
}

pub struct JavaContextExtractor {
    source: String,
    pub rope: Rope,
    pub offset: usize,
    name_table: Option<Arc<NameTable>>,
    workspace: Option<Arc<crate::workspace::Workspace>>,
    file_uri: Option<Arc<str>>,
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
            workspace: None,
            file_uri: None,
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
            workspace: None,
            file_uri: None,
        }
    }

    /// Set the workspace reference for incremental parsing
    pub fn with_workspace(mut self, workspace: Arc<crate::workspace::Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Set the file URI for incremental parsing
    pub fn with_file_uri(mut self, uri: Arc<str>) -> Self {
        self.file_uri = Some(uri);
        self
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

        let semantic_extractor = &self;
        let semantic_root = root;
        let semantic_cursor_node = cursor_node;

        let functional_target_hint =
            location::infer_functional_target_hint(semantic_extractor, semantic_cursor_node);

        let enclosing_class =
            scope::extract_enclosing_class(semantic_extractor, semantic_cursor_node).or_else(
                || scope::extract_enclosing_class_by_offset(semantic_extractor, semantic_root),
            );
        let enclosing_package = scope::extract_package(semantic_extractor, semantic_root);
        let enclosing_internal_name = scope::extract_enclosing_internal_name(
            semantic_extractor,
            semantic_cursor_node,
            enclosing_package.as_ref(),
        )
        .or_else(|| utils::build_internal_name(&enclosing_package, &enclosing_class));
        let existing_imports = scope::extract_imports(semantic_extractor, semantic_root);
        let type_ctx = Arc::new(SourceTypeCtx::new(
            enclosing_package.clone(),
            existing_imports.clone(),
            self.name_table.clone(),
        ));

        // Feature flag to enable incremental extraction
        let use_incremental = std::env::var("JAVA_ANALYZER_INCREMENTAL")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);

        let local_variables =
            if use_incremental && self.workspace.is_some() && self.file_uri.is_some() {
                // NEW: Incremental extraction with PSI cache
                let workspace = self.workspace.as_ref().unwrap();
                let file_uri = self.file_uri.as_ref().unwrap();

                if let Some(file) = workspace.get_or_create_salsa_file_by_uri_str(file_uri.as_ref())
                {
                    let db = workspace.salsa_db.lock();
                    crate::salsa_queries::extract_method_locals_incremental(
                        &*db,
                        file,
                        self.offset,
                        workspace,
                    )
                } else {
                    // Fallback to old method if file not found
                    locals::extract_locals_with_type_ctx(
                        semantic_extractor,
                        semantic_root,
                        semantic_cursor_node,
                        Some(&type_ctx),
                    )
                }
            } else {
                // OLD: Direct extraction (no cache)
                locals::extract_locals_with_type_ctx(
                    semantic_extractor,
                    semantic_root,
                    semantic_cursor_node,
                    Some(&type_ctx),
                )
            };
        let active_lambda_param_names =
            locals::extract_active_lambda_param_names(semantic_extractor, semantic_cursor_node);
        let active_lambda_param_names = if active_lambda_param_names.is_empty() {
            extract_lambda_params_from_error_arrow(
                semantic_extractor,
                semantic_cursor_node,
                semantic_root,
            )
        } else {
            active_lambda_param_names
        };
        let statement_labels =
            scope::extract_enclosing_statement_labels(semantic_extractor, semantic_cursor_node);
        let flow_type_overrides = flow::extract_instanceof_true_branch_overrides(
            semantic_extractor,
            semantic_cursor_node,
            &type_ctx,
            &local_variables,
        );
        let existing_static_imports =
            scope::extract_static_imports(semantic_extractor, semantic_root);
        let is_class_member_position =
            scope::is_cursor_in_class_member_position(semantic_cursor_node);

        let current_class_members =
            if use_incremental && self.workspace.is_some() && self.file_uri.is_some() {
                // NEW: Incremental extraction with PSI cache
                let workspace = self.workspace.as_ref().unwrap();
                let file_uri = self.file_uri.as_ref().unwrap();

                if let Some(file) = workspace.get_or_create_salsa_file_by_uri_str(file_uri.as_ref())
                {
                    let db = workspace.salsa_db.lock();
                    let members_map = crate::salsa_queries::extract_class_members_incremental(
                        &*db,
                        file,
                        self.offset,
                        workspace,
                    );
                    // Convert HashMap to Vec
                    members_map.into_values().collect()
                } else {
                    // Fallback to old method
                    semantic_cursor_node
                        .and_then(scope::nearest_type_declaration)
                        .map(|decl| {
                            synthetic::extract_type_members_with_synthetics(
                                semantic_extractor,
                                decl,
                                &type_ctx,
                                enclosing_internal_name.as_deref(),
                            )
                        })
                        .or_else(|| {
                            let error_node = utils::find_top_error_node(semantic_root)?;
                            let mut members = Vec::new();
                            members::collect_members_from_node(
                                semantic_extractor,
                                error_node,
                                &type_ctx,
                                &mut members,
                            );
                            Some(members)
                        })
                        .unwrap_or_default()
                }
            } else {
                // OLD: Direct extraction (no cache)
                semantic_cursor_node
                    .and_then(scope::nearest_type_declaration)
                    .map(|decl| {
                        synthetic::extract_type_members_with_synthetics(
                            semantic_extractor,
                            decl,
                            &type_ctx,
                            enclosing_internal_name.as_deref(),
                        )
                    })
                    .or_else(|| {
                        // Fallback: find top-level ERROR node anywhere under program
                        // Use two-phase approach: valid members + error recovery
                        let error_node = utils::find_top_error_node(semantic_root)?;
                        let mut members = Vec::new();
                        members::collect_members_from_node(
                            semantic_extractor,
                            error_node,
                            &type_ctx,
                            &mut members,
                        );
                        Some(members)
                    })
                    .unwrap_or_default()
            };
        let enclosing_class_member = semantic_cursor_node
            .and_then(|n| utils::find_ancestor(n, "method_declaration"))
            .or_else(|| find_node_by_offset(semantic_root, "method_declaration", self.offset))
            .or_else(|| utils::find_enclosing_method_in_error(semantic_root, self.offset))
            .and_then(|m| members::parse_method_node(semantic_extractor, &type_ctx, m));
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
        .with_statement_labels(statement_labels)
        .with_active_lambda_param_names(active_lambda_param_names)
        .with_functional_target_hint(functional_target_hint)
        .with_flow_type_overrides(flow_type_overrides)
        .with_class_member_position(is_class_member_position)
        .with_static_imports(existing_static_imports)
        .with_class_members(current_class_members)
        .with_enclosing_member(enclosing_class_member)
        .with_char_after_cursor(char_after_cursor)
        .with_extension(type_ctx)
    }

    pub(crate) fn find_cursor_node<'tree>(&self, root: Node<'tree>) -> Option<Node<'tree>> {
        if let Some(n) =
            root.named_descendant_for_byte_range(self.offset.saturating_sub(1), self.offset)
            && !utils::is_comment_kind(n.kind())
            && n.end_byte() >= self.offset
        {
            // If we got a very broad node (inter-node gap), try forward lookup
            // for a more precise child node at the cursor position.
            if matches!(n.kind(), "program" | "lambda_expression") {
                if self.offset < self.source.len()
                    && self.source.as_bytes()[self.offset] != b'\n'
                    && let Some(fwd) =
                        root.named_descendant_for_byte_range(self.offset, self.offset + 1)
                    && !utils::is_comment_kind(fwd.kind())
                {
                    return Some(fwd);
                }
                if n.kind() == "program" {
                    return None;
                }
            }
            return Some(n);
        }
        if self.offset < self.source.len()
            && self.source.as_bytes()[self.offset] != b'\n'
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
}

fn extract_lambda_params_from_error_arrow(
    ctx: &JavaContextExtractor,
    _cursor_node: Option<Node>,
    root: Node,
) -> Vec<Arc<str>> {
    // Find the anonymous node with `->` in the root directory, where the cursor is after `->`.
    fn find_arrow_before_cursor<'a>(node: Node<'a>, offset: usize, result: &mut Option<Node<'a>>) {
        if node.kind() == "->" && node.end_byte() <= offset {
            *result = Some(node);
        }
        let mut wc = node.walk();
        for child in node.children(&mut wc) {
            find_arrow_before_cursor(child, offset, result);
        }
    }

    let mut arrow = None;
    find_arrow_before_cursor(root, ctx.offset, &mut arrow);

    let Some(arrow) = arrow else {
        return vec![];
    };

    // Find the identifier or inferred_parameters preceding `->`
    // params is the sibling preceding `->`
    let Some(parent) = arrow.parent() else {
        return vec![];
    };
    let mut wc = parent.walk();
    let children: Vec<Node> = parent.children(&mut wc).collect();
    let Some(arrow_idx) = children.iter().position(|n| n.id() == arrow.id()) else {
        return vec![];
    };
    if arrow_idx == 0 {
        return vec![];
    }
    let params_node = children[arrow_idx - 1];

    match params_node.kind() {
        "identifier" => vec![Arc::from(ctx.node_text(params_node))],
        "inferred_parameters" => {
            let mut wc2 = params_node.walk();
            params_node
                .named_children(&mut wc2)
                .filter(|n| n.kind() == "identifier")
                .map(|n| Arc::from(ctx.node_text(n)))
                .collect()
        }
        "formal_parameters" => {
            let mut wc2 = params_node.walk();
            params_node
                .named_children(&mut wc2)
                .filter_map(|n| {
                    if n.kind() == "formal_parameter" || n.kind() == "spread_parameter" {
                        n.child_by_field_name("name")
                            .map(|name| Arc::from(ctx.node_text(name)))
                    } else {
                        None
                    }
                })
                .collect()
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion::{CandidateKind, CompletionCandidate, engine::CompletionEngine},
        index::{
            ClassMetadata, ClassOrigin, IndexScope, MethodParams, MethodSummary, ModuleId,
            WorkspaceIndex,
        },
        language::{java::class_parser::parse_java_source, rope_utils::line_col_to_offset},
        semantic::{
            LocalVar,
            context::{
                CurrentClassMember, CursorLocation, StatementLabelCompletionKind,
                StatementLabelTargetKind,
            },
            types::{CallArgs, EvalContext, type_name::TypeName},
        },
    };
    use rust_asm::constants::{ACC_ABSTRACT, ACC_PUBLIC};
    use std::sync::Arc;

    fn at(src: &str, line: u32, col: u32) -> SemanticContext {
        at_with_trigger(src, line, col, None)
    }

    fn at_with_trigger(src: &str, line: u32, col: u32, trigger: Option<char>) -> SemanticContext {
        let _rope = ropey::Rope::from_str(src);

        let mut parser = super::make_java_parser();
        let tree = parser.parse(src, None).expect("failed to parse java");

        super::JavaLanguage
            .parse_completion_context_with_tree(
                &crate::workspace::SourceFile::new(
                    tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                    "",
                    0,
                    src,
                    Some(tree),
                ),
                line,
                col,
                trigger,
                &ParseEnv {
                    name_table: None,
                    workspace: None,
                },
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

    fn make_array_completion_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("getClass"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/lang/Class;")),
                    },
                    MethodSummary {
                        name: Arc::from("hashCode"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
                    },
                    MethodSummary {
                        name: Arc::from("equals"),
                        params: MethodParams::from([("Ljava/lang/Object;", "obj")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Z")),
                    },
                    MethodSummary {
                        name: Arc::from("toString"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/lang/String;")),
                    },
                    MethodSummary {
                        name: Arc::from("wait"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("V")),
                    },
                    MethodSummary {
                        name: Arc::from("notify"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("V")),
                    },
                    MethodSummary {
                        name: Arc::from("notifyAll"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("V")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            make_class("java/lang", "Class"),
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Integer"),
                internal_name: Arc::from("java/lang/Integer"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("compareTo"),
                        params: MethodParams::from([("Ljava/lang/Integer;", "other")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
                    },
                    MethodSummary {
                        name: Arc::from("intValue"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
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
                        name: Arc::from("charAt"),
                        params: MethodParams::from([("I", "index")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("C")),
                    },
                    MethodSummary {
                        name: Arc::from("isBlank"),
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
                &crate::workspace::SourceFile::new(
                    tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                    "",
                    0,
                    src,
                    Some(tree),
                ),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                    workspace: None,
                },
            )
            .expect("parse_completion_context_with_tree returned None");
        let engine = CompletionEngine::new();
        let candidates = engine.complete(root_scope(), ctx.clone(), &JavaLanguage, view);
        (ctx, candidates)
    }

    fn local_candidate_descriptor<'a>(
        candidates: &'a [CompletionCandidate],
        label: &str,
    ) -> Option<&'a str> {
        candidates.iter().find_map(|candidate| {
            if candidate.label.as_ref() != label {
                return None;
            }
            match &candidate.kind {
                crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                    Some(type_descriptor.as_ref())
                }
                _ => None,
            }
        })
    }

    fn statement_labels(ctx: &SemanticContext) -> Vec<(String, StatementLabelTargetKind)> {
        ctx.statement_labels
            .iter()
            .map(|label| (label.name.to_string(), label.target_kind))
            .collect()
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

    #[test]
    fn test_statement_labels_recognize_labeled_block() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outer: {
                        break /*caret*/
                    }
                }
            }
            "#},
            &view,
        );

        assert_eq!(
            statement_labels(&ctx),
            vec![("outer".to_string(), StatementLabelTargetKind::Block)]
        );
    }

    #[test]
    fn test_statement_labels_recognize_labeled_loop() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outer:
                    while (true) {
                        break /*caret*/
                    }
                }
            }
            "#},
            &view,
        );

        assert_eq!(
            statement_labels(&ctx),
            vec![("outer".to_string(), StatementLabelTargetKind::While)]
        );
    }

    #[test]
    fn test_statement_labels_recognize_nested_labels_in_enclosing_order() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outer:
                    while (true) {
                        inner:
                        for (;;) {
                            break /*caret*/
                        }
                    }
                }
            }
            "#},
            &view,
        );

        assert_eq!(
            statement_labels(&ctx),
            vec![
                ("inner".to_string(), StatementLabelTargetKind::For),
                ("outer".to_string(), StatementLabelTargetKind::While),
            ]
        );
    }

    #[test]
    fn test_statement_labels_survive_incomplete_jump_statement() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, _) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outer: {
                        break out/*caret*/
                    }
                }
            }
            "#},
            &view,
        );

        assert_eq!(
            statement_labels(&ctx),
            vec![("outer".to_string(), StatementLabelTargetKind::Block)]
        );
        assert!(
            matches!(
                ctx.location,
                CursorLocation::StatementLabel {
                    kind: StatementLabelCompletionKind::Break,
                    ref prefix
                } if prefix == "out"
            ),
            "expected incomplete break to route through statement-label location, got {:?}",
            ctx.location
        );
    }

    #[test]
    fn test_break_label_completion_offers_enclosing_labels() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outer: {
                        break /*caret*/
                    }
                }
            }
            "#},
            &view,
        );

        let labels: Vec<&str> = candidates.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"outer"), "{labels:?}");
    }

    #[test]
    fn test_continue_label_completion_only_offers_loop_labels() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outer: {
                        loop: while (true) {
                            continue /*caret*/
                        }
                    }
                }
            }
            "#},
            &view,
        );

        let labels: Vec<&str> = candidates.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"loop"), "{labels:?}");
        assert!(!labels.contains(&"outer"), "{labels:?}");
    }

    #[test]
    fn test_break_label_completion_prefix_filter() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outerLabel: {
                        break out/*caret*/
                    }
                }
            }
            "#},
            &view,
        );

        let labels: Vec<&str> = candidates.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"outerLabel"), "{labels:?}");
    }

    #[test]
    fn test_continue_label_completion_prefix_filter() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outerLoop: while (true) {
                        continue out/*caret*/
                    }
                }
            }
            "#},
            &view,
        );

        let labels: Vec<&str> = candidates.iter().map(|c| c.label.as_ref()).collect();
        assert!(labels.contains(&"outerLoop"), "{labels:?}");
    }

    #[test]
    fn test_statement_label_completion_keeps_nested_enclosing_order() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    outer: while (true) {
                        inner: for (;;) {
                            break /*caret*/
                        }
                    }
                }
            }
            "#},
            &view,
        );

        let labels: Vec<&str> = candidates
            .iter()
            .filter(|c| matches!(c.kind, CandidateKind::StatementLabel))
            .map(|c| c.label.as_ref())
            .collect();
        assert_eq!(labels, vec!["inner", "outer"], "{labels:?}");
    }

    #[test]
    fn test_statement_label_completion_does_not_regress_normal_completion() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    int localValue = 1;
                    loc/*caret*/
                }
            }
            "#},
            &view,
        );

        assert!(
            candidates.iter().any(|c| c.label.as_ref() == "localValue"),
            "{:?}",
            candidates
                .iter()
                .map(|c| c.label.as_ref())
                .collect::<Vec<_>>()
        );
        assert!(
            candidates
                .iter()
                .all(|c| !matches!(c.kind, CandidateKind::StatementLabel)),
            "statement-label candidates must not leak into ordinary completion"
        );
    }

    #[test]
    fn test_completion_after_finished_break_statement_restores_normal_routing() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (ctx, candidates) = ctx_and_candidates_from_marked_source(
            indoc::indoc! {r#"
            class T {
                void m() {
                    int localValue = 1;
                    outer: {
                        break outer;
                        loc/*caret*/
                    }
                }
            }
            "#},
            &view,
        );

        assert!(
            !matches!(ctx.location, CursorLocation::StatementLabel { .. }),
            "caret after a completed break statement must not remain in StatementLabel: {:?}",
            ctx.location
        );
        assert!(
            candidates.iter().any(|c| c.label.as_ref() == "localValue"),
            "{:?}",
            candidates
                .iter()
                .map(|c| c.label.as_ref())
                .collect::<Vec<_>>()
        );
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
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("toString"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/lang/String;")),
                    },
                    MethodSummary {
                        name: Arc::from("hashCode"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
                    },
                    MethodSummary {
                        name: Arc::from("equals"),
                        params: MethodParams::from([("Ljava/lang/Object;", "obj")]),
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
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("length"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("I")),
                    },
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
                        name: Arc::from("subSequence"),
                        params: MethodParams::from([("I", "beginIndex"), ("I", "endIndex")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: Some(Arc::from("Ljava/lang/CharSequence;")),
                    },
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
                        name: Arc::from("toUpperCase"),
                        params: MethodParams::empty(),
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
            make_class("java/lang", "Integer"),
            make_class("java/lang", "Void"),
            make_class("java/lang", "CharSequence"),
            make_class("java/lang", "System"),
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
                    access_flags: ACC_PUBLIC | ACC_ABSTRACT,
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
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("BiFunction"),
                internal_name: Arc::from("java/util/function/BiFunction"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("apply"),
                    params: MethodParams::from([
                        ("Ljava/lang/Object;", "t"),
                        ("Ljava/lang/Object;", "u"),
                    ]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;TU;)TR;")),
                    return_type: Some(Arc::from("Ljava/lang/Object;")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from(
                    "<T:Ljava/lang/Object;U:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("Consumer"),
                internal_name: Arc::from("java/util/function/Consumer"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("accept"),
                    params: MethodParams::from([("Ljava/lang/Object;", "t")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;)V")),
                    return_type: Some(Arc::from("V")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("ToIntFunction"),
                internal_name: Arc::from("java/util/function/ToIntFunction"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("applyAsInt"),
                    params: MethodParams::from([("Ljava/lang/Object;", "value")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;)I")),
                    return_type: Some(Arc::from("I")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("Predicate"),
                internal_name: Arc::from("java/util/function/Predicate"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("test"),
                    params: MethodParams::from([("Ljava/lang/Object;", "t")]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;)Z")),
                    return_type: Some(Arc::from("Z")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("BiConsumer"),
                internal_name: Arc::from("java/util/function/BiConsumer"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("accept"),
                    params: MethodParams::from([
                        ("Ljava/lang/Object;", "t"),
                        ("Ljava/lang/Object;", "u"),
                    ]),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("(TT;TU;)V")),
                    return_type: Some(Arc::from("V")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from(
                    "<T:Ljava/lang/Object;U:Ljava/lang/Object;>Ljava/lang/Object;",
                )),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/function")),
                name: Arc::from("Runnable"),
                internal_name: Arc::from("java/util/function/Runnable"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![MethodSummary {
                    name: Arc::from("run"),
                    params: MethodParams::empty(),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC | ACC_ABSTRACT,
                    is_synthetic: false,
                    generic_signature: Some(Arc::from("()V")),
                    return_type: Some(Arc::from("V")),
                }],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util/stream")),
                name: Arc::from("Stream"),
                internal_name: Arc::from("java/util/stream/Stream"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("map"),
                        params: MethodParams::from([("Ljava/util/function/Function;", "mapper")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from(
                            "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)Ljava/util/stream/Stream<TR;>;",
                        )),
                        return_type: Some(Arc::from("Ljava/util/stream/Stream;")),
                    },
                    MethodSummary {
                        name: Arc::from("filter"),
                        params: MethodParams::from([("Ljava/util/function/Predicate;", "predicate")]),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from(
                            "(Ljava/util/function/Predicate<-TT;>;)Ljava/util/stream/Stream<TT;>;",
                        )),
                        return_type: Some(Arc::from("Ljava/util/stream/Stream;")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/util")),
                name: Arc::from("ArrayList"),
                internal_name: Arc::from("java/util/ArrayList"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![
                    MethodSummary {
                        name: Arc::from("<init>"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: None,
                        return_type: None,
                    },
                    MethodSummary {
                        name: Arc::from("stream"),
                        params: MethodParams::empty(),
                        annotations: vec![],
                        access_flags: ACC_PUBLIC,
                        is_synthetic: false,
                        generic_signature: Some(Arc::from("()Ljava/util/stream/Stream<TE;>;")),
                        return_type: Some(Arc::from("Ljava/util/stream/Stream;")),
                    },
                ],
                fields: vec![],
                access_flags: ACC_PUBLIC,
                inner_class_of: None,
                generic_signature: Some(Arc::from("<E:Ljava/lang/Object;>Ljava/lang/Object;")),
                origin: ClassOrigin::Unknown,
            },
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
            ctx.local_variables
                .iter()
                .any(|lv| lv.name.as_ref() == "left"),
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
    fn test_lambda_single_param_typed_member_completion_from_function_sam() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Integer> f = s -> s.subs|;
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        if ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .is_some_and(|s| s.type_internal.erased_internal() != "java/lang/String")
        {
            panic!(
                "expected_type={:?} expected_sam={:?} locals={:?} labels={:?}",
                ctx.typed_expr_ctx
                    .as_ref()
                    .and_then(|t| t.expected_type.as_ref())
                    .map(|e| e.ty.to_internal_with_generics()),
                ctx.expected_sam.as_ref().map(|sam| sam
                    .param_types
                    .iter()
                    .map(|t| t.to_internal_with_generics())
                    .collect::<Vec<_>>()),
                ctx.local_variables
                    .iter()
                    .map(|lv| format!(
                        "{}:{}",
                        lv.name,
                        lv.type_internal.to_internal_with_generics()
                    ))
                    .collect::<Vec<_>>(),
                labels
            );
        }
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected lambda param s");
        assert_eq!(s.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_multi_param_typed_member_completion_from_bifunction_sam() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.BiFunction;
            class T {
                void m() {
                    BiFunction<String, String, Integer> f = (left, right) -> left.subs|;
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let left = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "left")
            .expect("expected lambda param left");
        let right = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "right")
            .expect("expected lambda param right");
        assert_eq!(left.type_internal.erased_internal(), "java/lang/String");
        assert_eq!(right.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_single_param_typed_member_completion_from_consumer_sam() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Consumer;
            class T {
                void m() {
                    Consumer<StringBuilder> c = sb -> sb.appe|;
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let sb = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "sb")
            .expect("expected lambda param sb");
        assert_eq!(
            sb.type_internal.erased_internal(),
            "java/lang/StringBuilder"
        );
        assert!(labels.iter().any(|l| l == "append"), "{labels:?}");
    }

    #[test]
    fn test_lambda_block_body_typed_member_completion_from_consumer_sam() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Consumer;
            class T {
                void m() {
                    Consumer<String> c = value -> {
                        value.subs|
                    };
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let value = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "value")
            .expect("expected lambda param value");
        assert_eq!(value.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_block_body_multi_param_typed_member_completion_from_biconsumer_sam() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src_sb = indoc::indoc! {r#"
            import java.util.function.BiConsumer;
            class T {
                void m() {
                    BiConsumer<StringBuilder, String> c = (sb, s) -> {
                        sb.appe|;
                    };
                }
            }
        "#};
        let src_s = indoc::indoc! {r#"
            import java.util.function.BiConsumer;
            class T {
                void m() {
                    BiConsumer<StringBuilder, String> c = (sb, s) -> {
                        s.subs|;
                    };
                }
            }
        "#};

        let (mut ctx_sb, labels_sb) = ctx_and_labels_from_marked_source(src_sb, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx_sb);
        let sb = ctx_sb
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "sb")
            .expect("expected lambda param sb");
        assert_eq!(
            sb.type_internal.erased_internal(),
            "java/lang/StringBuilder"
        );
        assert!(labels_sb.iter().any(|l| l == "append"), "{labels_sb:?}");

        let (mut ctx_s, labels_s) = ctx_and_labels_from_marked_source(src_s, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx_s);
        let s = ctx_s
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected lambda param s");
        assert_eq!(s.type_internal.erased_internal(), "java/lang/String");
        assert!(labels_s.iter().any(|l| l == "substring"), "{labels_s:?}");
    }

    #[test]
    fn test_lambda_single_param_typed_member_completion_from_function_sam_incomplete_initializer() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Integer> f = s -> s.subs/*caret*/
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected lambda param s");
        assert_eq!(ctx.active_lambda_param_names, vec![Arc::from("s")]);
        assert_eq!(s.type_internal.erased_internal(), "java/lang/String");
        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|t| t.expected_type.as_ref())
                .map(|e| e.ty.erased_internal()),
            Some("java/util/function/Function")
        );
        assert_eq!(
            ctx.expected_sam.as_ref().map(|sam| sam
                .param_types
                .iter()
                .map(|t| t.erased_internal())
                .collect::<Vec<_>>()),
            Some(vec!["java/lang/String"])
        );
        assert_eq!(
            ctx.location
                .member_access_receiver_semantic_type()
                .map(|t| t.erased_internal()),
            Some("java/lang/String")
        );
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_block_body_return_expression_keeps_typed_param_visibility() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Integer> f = value -> {
                        return value.subs|;
                    };
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let value = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "value")
            .expect("expected lambda param value");
        assert_eq!(value.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_nested_block_keeps_typed_param_visibility() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Consumer;
            class T {
                void m() {
                    Consumer<StringBuilder> c = sb -> {
                        {
                            sb.appe|;
                        }
                    };
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let sb = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "sb")
            .expect("expected lambda param sb");
        assert_eq!(
            sb.type_internal.erased_internal(),
            "java/lang/StringBuilder"
        );
        assert!(labels.iter().any(|l| l == "append"), "{labels:?}");
    }

    #[test]
    fn test_lambda_inner_block_local_does_not_leak_after_block() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Void> f = s -> {
                        {
                            String s1 = s.trim();
                        }
                        s1/*caret*/
                        return null;
                    };
                }
            }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            ctx.local_variables.iter().any(|lv| lv.name.as_ref() == "s"),
            "lambda param should stay visible: {:?}",
            ctx.local_variables
                .iter()
                .map(|lv| format!(
                    "{}:{}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics()
                ))
                .collect::<Vec<_>>()
        );
        assert!(
            !ctx.local_variables
                .iter()
                .any(|lv| lv.name.as_ref() == "s1"),
            "inner-block local must not leak after block: {:?}",
            ctx.local_variables
                .iter()
                .map(|lv| format!(
                    "{}:{}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics()
                ))
                .collect::<Vec<_>>()
        );
        assert!(!labels.iter().any(|l| l == "s1"), "{labels:?}");
    }

    #[test]
    fn test_lambda_inner_block_local_member_completion_stays_visible_inside_block() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Void> f = s -> {
                        {
                            String s1 = s.trim();
                            s1.subs|
                        }
                        return null;
                    };
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s1 = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s1")
            .expect("expected inner-block local s1");
        assert_eq!(s1.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_param_remains_visible_after_inner_block() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Void> f = s -> {
                        {
                            String s1 = s.trim();
                        }
                        s.subs|
                        return null;
                    };
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        assert!(
            ctx.local_variables.iter().any(|lv| lv.name.as_ref() == "s"),
            "expected lambda param s to remain visible"
        );
        assert!(
            !ctx.local_variables
                .iter()
                .any(|lv| lv.name.as_ref() == "s1"),
            "expired inner-block local must not remain visible"
        );
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_block_body_typed_member_completion_from_consumer_sam_incomplete_statement() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Consumer;
            class T {
                void m() {
                    Consumer<String> c = value -> {
                        value.subs/*caret*/
                    };
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let value = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "value")
            .expect("expected lambda param value");
        assert_eq!(ctx.active_lambda_param_names, vec![Arc::from("value")]);
        assert_eq!(value.type_internal.erased_internal(), "java/lang/String");
        assert_eq!(
            ctx.location
                .member_access_receiver_semantic_type()
                .map(|t| t.erased_internal()),
            Some("java/lang/String")
        );
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_param_shadows_outer_local_for_member_completion() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    StringBuilder value = new StringBuilder();
                    Function<String, Integer> f = value -> value.subs|;
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let value = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "value")
            .expect("expected visible value binding");
        assert_eq!(value.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
        assert!(
            !labels.iter().any(|l| l == "append"),
            "outer StringBuilder binding must be shadowed: {labels:?}"
        );
    }

    #[test]
    fn test_outer_local_remains_visible_when_not_shadowed_in_lambda() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.BiFunction;
            class T {
                void m() {
                    String prefix = "x";
                    BiFunction<String, String, String> f = (left, right) -> prefix.subs|;
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        assert!(
            ctx.local_variables
                .iter()
                .any(|lv| lv.name.as_ref() == "prefix"),
            "outer local should remain visible: {:?}",
            ctx.local_variables
                .iter()
                .map(|lv| format!(
                    "{}:{}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics()
                ))
                .collect::<Vec<_>>()
        );
        assert!(
            ctx.local_variables
                .iter()
                .any(|lv| lv.name.as_ref() == "left"),
            "left lambda param should remain visible"
        );
        assert!(
            ctx.local_variables
                .iter()
                .any(|lv| lv.name.as_ref() == "right"),
            "right lambda param should remain visible"
        );
        let prefix = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "prefix")
            .expect("expected prefix local");
        assert_eq!(prefix.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_method_inner_block_local_does_not_leak_after_block() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    {
                        String s1 = "";
                    }
                    s1/*caret*/
                }
            }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            !ctx.local_variables
                .iter()
                .any(|lv| lv.name.as_ref() == "s1"),
            "method inner-block local must not leak: {:?}",
            ctx.local_variables
                .iter()
                .map(|lv| format!(
                    "{}:{}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics()
                ))
                .collect::<Vec<_>>()
        );
        assert!(!labels.iter().any(|l| l == "s1"), "{labels:?}");
    }

    #[test]
    fn test_nested_block_shadowing_still_prefers_innermost_local() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    String s = "";
                    {
                        StringBuilder s = new StringBuilder();
                        s.appe|
                    }
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected visible local s");
        assert_eq!(s.type_internal.erased_internal(), "java/lang/StringBuilder");
        assert!(labels.iter().any(|l| l == "append"), "{labels:?}");
    }

    #[test]
    fn test_lambda_block_body_merges_params_and_inner_locals() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Integer> f = value -> {
                        String local = value.trim();
                        return local.subs|;
                    };
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let local = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "local")
            .expect("expected inner local");
        let value = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "value")
            .expect("expected lambda param");
        assert_eq!(local.type_internal.erased_internal(), "java/lang/String");
        assert_eq!(value.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_nested_lambda_scope_prefers_innermost_binding_without_crashing() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Function<String, Integer>> f =
                        value -> value -> value.subs|;
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let visible_values: Vec<_> = ctx
            .local_variables
            .iter()
            .filter(|lv| lv.name.as_ref() == "value")
            .collect();
        assert_eq!(
            visible_values.len(),
            1,
            "only the innermost visible binding should remain after scope normalization"
        );
        assert_eq!(ctx.active_lambda_param_names, vec![Arc::from("value")]);
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_invalid_lambda_block_redeclaration_recovers_nearest_binding() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            class T {
                void m() {
                    Function<String, Integer> f = value -> {
                        StringBuilder value = new StringBuilder();
                        return value.appe/*caret*/
                    };
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let value = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "value")
            .expect("expected recovered value binding");
        assert_eq!(
            value.type_internal.erased_internal(),
            "java/lang/StringBuilder"
        );
        assert_eq!(
            ctx.location
                .member_access_receiver_semantic_type()
                .map(|t| t.erased_internal()),
            Some("java/lang/StringBuilder")
        );
        assert!(labels.iter().any(|l| l == "append"), "{labels:?}");
    }

    #[test]
    fn test_invalid_plain_block_redeclaration_recovers_nearest_binding() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    String value = "";
                    {
                        StringBuilder value = new StringBuilder();
                        value.appe|;
                    }
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let value = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "value")
            .expect("expected recovered inner block value binding");
        assert_eq!(
            value.type_internal.erased_internal(),
            "java/lang/StringBuilder"
        );
        assert_eq!(
            ctx.location
                .member_access_receiver_semantic_type()
                .map(|t| t.erased_internal()),
            Some("java/lang/StringBuilder")
        );
        assert!(labels.iter().any(|l| l == "append"), "{labels:?}");
    }

    #[test]
    fn test_generic_method_lambda_target_typing_from_string_argument() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                static <T, R> R apply(Function<T, R> f, T x) {
                    return f.apply(x);
                }

                static void main(String[] args) {
                    Integer n = apply(s -> s.subs|, "hello");
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected inferred lambda param s");
        assert_eq!(s.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_generic_method_lambda_target_typing_from_stringbuilder_argument() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                static <T, R> R apply(Function<T, R> f, T x) {
                    return f.apply(x);
                }

                static void main(String[] args) {
                    Integer n = apply(sb -> sb.appe|, new StringBuilder());
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let sb = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "sb")
            .expect("expected inferred lambda param sb");
        assert_eq!(
            sb.type_internal.erased_internal(),
            "java/lang/StringBuilder"
        );
        assert!(labels.iter().any(|l| l == "append"), "{labels:?}");
    }

    #[test]
    fn test_generic_method_bifunction_lambda_target_typing_from_concrete_args() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.BiFunction;

            class T {
                static <A, B, R> R apply2(BiFunction<A, B, R> f, A a, B b) {
                    return f.apply(a, b);
                }

                static void main(String[] args) {
                    Integer n = apply2((s, i) -> s.subs|, "hello", 2);
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected inferred lambda param s");
        let i = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "i")
            .expect("expected inferred lambda param i");
        assert_eq!(s.type_internal.erased_internal(), "java/lang/String");
        assert!(
            matches!(
                i.type_internal.erased_internal(),
                "int" | "java/lang/Integer"
            ),
            "unexpected inferred numeric type: {}",
            i.type_internal.erased_internal()
        );
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_generic_method_block_bodied_lambda_target_typing_from_string_argument() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                static <T, R> R apply(Function<T, R> f, T x) {
                    return f.apply(x);
                }

                static void main(String[] args) {
                    Integer n = apply(s -> {
                        return s.subs|;
                    }, "hello");
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected inferred lambda param s");
        assert_eq!(s.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_generic_method_lambda_target_typing_falls_back_when_inference_is_ambiguous() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                static <T, R> Function<T, R> id(Function<T, R> f) {
                    return f;
                }

                static void main(String[] args) {
                    Function<?, ?> n = id(s -> s.subs|);
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected lambda param s to remain name-visible");
        assert_eq!(s.type_internal.erased_internal(), "unknown");
        assert!(
            !labels.iter().any(|l| l == "substring"),
            "ambiguous generic inference should stay conservative: {labels:?}"
        );
    }

    #[test]
    fn test_var_initializer_infers_generic_lambda_invocation_return_type() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                static <T, R> R apply(Function<T, R> f, T x) {
                    return f.apply(x);
                }

                static void main(String[] args) {
                    var n = apply(s -> s.length(), "hello");
                    n|
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let n = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "n")
            .expect("expected local n");
        assert_eq!(
            n.type_internal.erased_internal(),
            "java/lang/Integer",
            "locals={:?}",
            ctx.local_variables
                .iter()
                .map(|lv| format!(
                    "{}:{}",
                    lv.name,
                    lv.type_internal.to_internal_with_generics()
                ))
                .collect::<Vec<_>>()
        );
        assert!(labels.iter().any(|l| l == "n"), "{labels:?}");
    }

    #[test]
    fn test_stream_map_lambda_target_typing_from_receiver_generic() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.ArrayList;

            class T {
                void m() {
                    ArrayList<String> list = new ArrayList<String>();
                    list.stream().map(x -> x.toStr/*caret*/);
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let x = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "x")
            .expect("expected inferred lambda param x");
        assert_eq!(x.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "toString"), "{labels:?}");
    }

    #[test]
    fn test_stream_map_parenthesized_lambda_target_typing_from_receiver_generic() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.ArrayList;

            class T {
                void m() {
                    ArrayList<String> list = new ArrayList<String>();
                    list.stream().map((x) -> x.toStr/*caret*/);
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let x = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "x")
            .expect("expected inferred lambda param x");
        assert_eq!(x.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "toString"), "{labels:?}");
    }

    #[test]
    fn test_stream_filter_lambda_target_typing_from_receiver_generic() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.ArrayList;

            class T {
                void m() {
                    ArrayList<String> list = new ArrayList<String>();
                    list.stream().filter(x -> x.toStr/*caret*/ != null);
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let x = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "x")
            .expect("expected inferred lambda param x");
        assert_eq!(x.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "toString"), "{labels:?}");
    }

    #[test]
    fn test_stream_map_lambda_forms_bind_same_param_type() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src_implicit = indoc::indoc! {r#"
            import java.util.ArrayList;

            class T {
                void m() {
                    ArrayList<String> list = new ArrayList<String>();
                    list.stream().map(x -> x.toStr/*caret*/);
                }
            }
        "#};
        let src_parenthesized = indoc::indoc! {r#"
            import java.util.ArrayList;

            class T {
                void m() {
                    ArrayList<String> list = new ArrayList<String>();
                    list.stream().map((x) -> x.toStr/*caret*/);
                }
            }
        "#};

        let (mut implicit_ctx, _) = ctx_and_labels_from_marked_source(src_implicit, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view)
            .enrich(&mut implicit_ctx);
        let implicit_ty = implicit_ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "x")
            .expect("expected implicit lambda param x")
            .type_internal
            .erased_internal()
            .to_string();

        let (mut parenthesized_ctx, _) =
            ctx_and_labels_from_marked_source(src_parenthesized, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view)
            .enrich(&mut parenthesized_ctx);
        let parenthesized_ty = parenthesized_ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "x")
            .expect("expected parenthesized lambda param x")
            .type_internal
            .erased_internal()
            .to_string();

        assert_eq!(implicit_ty, "java/lang/String");
        assert_eq!(parenthesized_ty, implicit_ty);
    }

    #[test]
    fn test_lambda_single_param_explicit_typed_member_completion_from_function_sam() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                void m() {
                    Function<String, Integer> f = (String x) -> x.subs|
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let x = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "x")
            .expect("expected explicit typed lambda param x");
        assert_eq!(ctx.active_lambda_param_names, vec![Arc::from("x")]);
        assert_eq!(x.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_single_param_var_member_completion_from_function_sam() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                void m() {
                    Function<String, Integer> f = (var x) -> x.subs|
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let x = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "x")
            .expect("expected var lambda param x");
        assert_eq!(ctx.active_lambda_param_names, vec![Arc::from("x")]);
        assert_eq!(x.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_empty_expression_body_keeps_param_in_scope() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                void m() {
                    Function<String, Integer> f = x -> /*caret*/
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let x = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "x")
            .expect("expected lambda param x");
        assert_eq!(ctx.active_lambda_param_names, vec![Arc::from("x")]);
        assert_eq!(x.type_internal.erased_internal(), "java/lang/String");
        assert_eq!(
            ctx.typed_expr_ctx
                .as_ref()
                .and_then(|typed| typed.expected_type.as_ref())
                .map(|expected| expected.ty.erased_internal()),
            Some("java/util/function/Function")
        );
        assert!(labels.iter().any(|l| l == "x"), "{labels:?}");
    }

    #[test]
    fn test_lambda_empty_block_body_keeps_param_in_scope_for_consumer_and_function() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());

        let consumer_src = indoc::indoc! {r#"
            import java.util.function.Consumer;

            class T {
                void m() {
                    Consumer<String> c = s -> {
                        /*caret*/
                    };
                }
            }
        "#};
        let function_src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                void m() {
                    Function<String, Integer> f = s -> {
                        /*caret*/
                    };
                }
            }
        "#};

        let (mut consumer_ctx, consumer_labels) =
            ctx_and_labels_from_marked_source(consumer_src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view)
            .enrich(&mut consumer_ctx);
        let consumer_s = consumer_ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected consumer lambda param s");
        assert_eq!(
            consumer_s.type_internal.erased_internal(),
            "java/lang/String"
        );
        assert!(
            consumer_labels.iter().any(|l| l == "s"),
            "{consumer_labels:?}"
        );

        let (mut function_ctx, function_labels) =
            ctx_and_labels_from_marked_source(function_src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view)
            .enrich(&mut function_ctx);
        let function_s = function_ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected function lambda param s");
        assert_eq!(
            function_s.type_internal.erased_internal(),
            "java/lang/String"
        );
        assert!(
            function_labels.iter().any(|l| l == "s"),
            "{function_labels:?}"
        );
    }

    #[test]
    fn test_incomplete_typed_lambda_header_completion_stays_recoverable() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                void m() {
                    Function<String, Integer> f =
                        (String x/*caret*/
                }
            }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "{:?}",
            ctx.location
        );
        assert!(!labels.is_empty(), "completion should stay recoverable");
    }

    #[test]
    fn test_incomplete_single_param_lambda_before_arrow_stays_recoverable() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.ArrayList;

            class T {
                void m() {
                    ArrayList<String> list = new ArrayList<String>();
                    list.stream().map(x/*caret*/);
                }
            }
        "#};

        let (ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(
            !matches!(ctx.location, CursorLocation::Unknown),
            "{:?}",
            ctx.location
        );
        assert_eq!(ctx.query, "x");
        assert!(
            matches!(
                ctx.location,
                CursorLocation::MethodArgument { .. } | CursorLocation::Expression { .. }
            ),
            "{:?}",
            ctx.location
        );
        assert!(labels.is_empty(), "{labels:?}");
    }

    #[test]
    fn test_nested_lambda_member_completion_keeps_innermost_param_visible() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                void m() {
                    Function<String, Function<String, Integer>> f =
                        prefix -> value -> value.subs|;
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);

        let value = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "value")
            .expect("expected inner lambda param value");
        assert_eq!(ctx.active_lambda_param_names, vec![Arc::from("value")]);
        assert_eq!(value.type_internal.erased_internal(), "java/lang/String");
        assert!(labels.iter().any(|l| l == "substring"), "{labels:?}");
    }

    #[test]
    fn test_lambda_this_completion_uses_enclosing_instance_members() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Consumer;

            class T {
                private final String prefix = "P:";

                void run() {
                    Consumer<String> c = x -> {
                        this./*caret*/
                    };
                }

                String getPrefix() {
                    return prefix;
                }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|l| l == "getPrefix"), "{labels:?}");
        assert!(labels.iter().any(|l| l == "run"), "{labels:?}");
    }

    #[test]
    fn test_overloaded_lambda_target_typing_stays_conservative_when_ambiguous() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;
            import java.util.function.ToIntFunction;

            class T {
                static void test(Function<String, Integer> f) {}
                static void test(ToIntFunction<String> f) {}

                void m() {
                    test(s -> s.subs|);
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected lambda param s");
        assert_eq!(s.type_internal.erased_internal(), "unknown");
        assert!(
            !labels.iter().any(|l| l == "substring"),
            "ambiguous overload target typing should stay conservative for now: {labels:?}"
        );
    }

    #[test]
    fn test_var_initializer_infers_generic_lambda_object_return_type() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                static <T, R> R apply(Function<T, R> f, T x) {
                    return f.apply(x);
                }

                static void main(String[] args) {
                    var sb = apply(s -> new StringBuilder(s), "hello");
                    sb|
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let sb = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "sb")
            .expect("expected local sb");
        assert_eq!(
            sb.type_internal.erased_internal(),
            "java/lang/StringBuilder"
        );
        assert!(labels.iter().any(|l| l == "sb"), "{labels:?}");
    }

    #[test]
    fn test_var_initializer_infers_generic_bifunction_lambda_return_type() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.BiFunction;

            class T {
                static <A, B, R> R apply2(BiFunction<A, B, R> f, A a, B b) {
                    return f.apply(a, b);
                }

                static void main(String[] args) {
                    var n = apply2((s, i) -> s.substring(i).length(), "hello", 2);
                    n|
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let n = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "n")
            .expect("expected local n");
        assert_eq!(n.type_internal.erased_internal(), "java/lang/Integer");
        assert!(labels.iter().any(|l| l == "n"), "{labels:?}");
    }

    #[test]
    fn test_var_initializer_keeps_generic_lambda_return_type_conservative_when_unsupported() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            import java.util.function.Function;

            class T {
                static <T, R> R apply(Function<T, R> f, T x) {
                    return f.apply(x);
                }

                static void main(String[] args) {
                    var n = apply(s -> {
                        return s.length();
                    }, "hello");
                    n|
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let n = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "n")
            .expect("expected local n");
        assert!(
            matches!(n.type_internal.erased_internal(), "var" | "R"),
            "unsupported block-bodied return inference should stay conservative: {}",
            n.type_internal.erased_internal()
        );
        assert!(labels.iter().any(|l| l == "n"), "{labels:?}");
    }

    #[test]
    fn test_lambda_falls_back_to_name_only_when_expected_type_is_not_functional() {
        let idx = make_lambda_scope_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
                void m() {
                    Object o = s -> s.subs|;
                }
            }
        "#};

        let (mut ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let s = ctx
            .local_variables
            .iter()
            .find(|lv| lv.name.as_ref() == "s")
            .expect("expected lambda param s");
        assert_eq!(s.type_internal.erased_internal(), "unknown");
        assert!(
            !labels.iter().any(|l| l == "substring"),
            "non-functional target must not fabricate String members: {labels:?}"
        );
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
        let _name_table = view.build_name_table();
        let _rope = ropey::Rope::from_str(src);
        let mut parser = super::make_java_parser();
        let tree = parser.parse(src, None).expect("failed to parse java");
        let ctx = super::JavaLanguage
            .parse_completion_context_with_tree(
                &crate::workspace::SourceFile::new(
                    tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                    "",
                    0,
                    src,
                    Some(tree),
                ),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                    workspace: None,
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
                &crate::workspace::SourceFile::new(
                    tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                    "",
                    0,
                    src_no_cursor,
                    Some(tree),
                ),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                    workspace: None,
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
                &crate::workspace::SourceFile::new(
                    tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                    "",
                    0,
                    src_no_cursor,
                    Some(tree),
                ),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                    workspace: None,
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
        idx.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "Integer"),
        ]);
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
                &crate::workspace::SourceFile::new(
                    tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                    "",
                    0,
                    src_no_cursor,
                    Some(tree),
                ),
                line,
                col,
                None,
                &ParseEnv {
                    name_table: Some(view.build_name_table()),
                    workspace: None,
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
            CallArgs::new(1, &[], &["ArrayList::new".to_string()]),
            EvalContext::new(&ctx.local_variables, ctx.enclosing_internal_name.as_ref())
                .with_qualifier(Some(&|q| type_ctx.resolve_type_name_strict(q))),
        );
        let direct_get = direct_map.as_ref().and_then(|m| {
            resolver.resolve_method_return_with_callsite_and_qualifier_resolver(
                &m.to_internal_with_generics(),
                "get",
                CallArgs::new(0, &[], &[]),
                EvalContext::new(&ctx.local_variables, ctx.enclosing_internal_name.as_ref())
                    .with_qualifier(Some(&|q| type_ctx.resolve_type_name_strict(q))),
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
    fn test_array_member_completion_does_not_leak_element_type_members() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f() {
                String[] s = null;
                s.|
              }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|label| label == "length"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "getClass"), "{labels:?}");
        assert!(
            !labels.iter().any(|label| label == "substring"),
            "{labels:?}"
        );
        assert!(!labels.iter().any(|label| label == "charAt"), "{labels:?}");
        assert!(!labels.iter().any(|label| label == "isBlank"), "{labels:?}");
        assert!(!labels.iter().any(|label| label == "stream"), "{labels:?}");
    }

    #[test]
    fn test_primitive_array_member_completion_behaves_like_array_receiver() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f() {
                int[] xs = null;
                xs.|
              }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|label| label == "length"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "getClass"), "{labels:?}");
        assert!(
            !labels.iter().any(|label| label == "compareTo"),
            "{labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label == "intValue"),
            "{labels:?}"
        );
        assert!(!labels.iter().any(|label| label == "stream"), "{labels:?}");
    }

    #[test]
    fn test_multidimensional_array_member_completion_still_uses_array_semantics() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f() {
                String[][] ss = null;
                ss.|
              }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|label| label == "length"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "getClass"), "{labels:?}");
        assert!(
            !labels.iter().any(|label| label == "substring"),
            "{labels:?}"
        );
        assert!(!labels.iter().any(|label| label == "stream"), "{labels:?}");
    }

    #[test]
    fn test_string_receiver_does_not_expose_array_length_field() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f(String s) {
                s.|
              }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|label| label == "getClass"), "{labels:?}");
        assert!(!labels.iter().any(|label| label == "length"), "{labels:?}");
    }

    #[test]
    fn test_array_member_completion_does_not_offer_class_literal_keyword() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f(String[] s) {
                s.|
              }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|label| label == "length"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "getClass"), "{labels:?}");
        assert!(!labels.iter().any(|label| label == "class"), "{labels:?}");
    }

    #[test]
    fn test_type_operand_completion_offers_class_but_not_get_class() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f() {
                String.|
              }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|label| label == "class"), "{labels:?}");
        assert!(
            !labels.iter().any(|label| label == "getClass"),
            "{labels:?}"
        );
    }

    #[test]
    fn test_array_length_completion_candidate_is_field_not_method() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f(String[] s) {
                s.|
              }
            }
        "#};

        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        assert!(candidates.iter().any(|candidate| {
            candidate.label.as_ref() == "length"
                && matches!(
                    candidate.kind,
                    crate::completion::CandidateKind::Field { .. }
                )
        }));
        assert!(!candidates.iter().any(|candidate| {
            candidate.label.as_ref() == "length"
                && matches!(
                    candidate.kind,
                    crate::completion::CandidateKind::Method { .. }
                )
        }));
    }

    #[test]
    fn test_array_length_var_inference_materializes_int_for_direct_and_multidimensional_arrays() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());

        let src_direct = indoc::indoc! {r#"
            class T {
              void f(String[] s) {
                var n = s.length;
                n|
              }
            }
        "#};
        let (_ctx_direct, candidates_direct) =
            ctx_and_candidates_from_marked_source(src_direct, &view);
        assert_eq!(
            local_candidate_descriptor(&candidates_direct, "n"),
            Some("int")
        );

        let src_multi = indoc::indoc! {r#"
            class T {
              void f(String[][] s) {
                var n = s.length;
                n|
              }
            }
        "#};
        let (_ctx_multi, candidates_multi) =
            ctx_and_candidates_from_marked_source(src_multi, &view);
        assert_eq!(
            local_candidate_descriptor(&candidates_multi, "n"),
            Some("int")
        );
    }

    #[test]
    fn test_array_get_class_var_inference_materializes_reference_array_class_type() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f(Integer[] s) {
                var a = s.getClass();
                a|
              }
            }
        "#};

        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let a = candidates
            .iter()
            .find(|candidate| candidate.label.as_ref() == "a")
            .expect("expected local variable completion for a");

        match &a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(
                    type_descriptor.as_ref(),
                    TypeName::with_args(
                        "java/lang/Class",
                        vec![TypeName::new("java/lang/Integer").with_array_dims(1)],
                    )
                    .to_internal_with_generics()
                );
            }
            other => panic!("expected local variable candidate, got {other:?}"),
        }
    }

    #[test]
    fn test_array_get_class_var_inference_materializes_array_returning_call_chains() {
        let src = indoc::indoc! {r#"
            class T {
              Integer[] g() { return null; }
              void f() {
                var c = g().getClass();
                c|
              }
            }
        "#};
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(src, ClassOrigin::Unknown, None));
        idx.add_classes(vec![
            make_class("java/lang", "Class"),
            make_class("java/lang", "Integer"),
        ]);
        let view = idx.view(root_scope());
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        assert_eq!(
            local_candidate_descriptor(&candidates, "c"),
            Some(
                TypeName::with_args(
                    "java/lang/Class",
                    vec![TypeName::new("java/lang/Integer").with_array_dims(1)],
                )
                .to_internal_with_generics()
                .as_str()
            )
        );
    }

    #[test]
    fn test_array_length_var_inference_materializes_int_for_array_returning_call() {
        let src = indoc::indoc! {r#"
            class T {
              String[] g() { return null; }
              void f() {
                var n = g().length;
                n|
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
        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        assert_eq!(local_candidate_descriptor(&candidates, "n"), Some("int"));
    }

    #[test]
    fn test_class_literal_var_inference_materializes_nested_operands() {
        let src_nested = indoc::indoc! {r#"
            class Outer {
              static class Inner {}
              void f() {
                var c = Inner.class;
                c|
              }
            }
        "#};
        let idx_nested = WorkspaceIndex::new();
        idx_nested.add_classes(parse_java_source(src_nested, ClassOrigin::Unknown, None));
        idx_nested.add_classes(vec![
            make_class("java/lang", "Object"),
            make_class("java/lang", "Class"),
        ]);
        let view_nested = idx_nested.view(root_scope());
        let (_ctx_nested, candidates_nested) =
            ctx_and_candidates_from_marked_source(src_nested, &view_nested);
        assert_eq!(
            local_candidate_descriptor(&candidates_nested, "c"),
            Some(
                TypeName::with_args("java/lang/Class", vec![TypeName::new("Outer$Inner")],)
                    .to_internal_with_generics()
                    .as_str()
            )
        );
    }

    #[test]
    fn test_completion_recovery_array_prefix_get_class() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f() {
                String[] s = null;
                s.getC|
              }
            }
        "#};

        let (_ctx, labels) = ctx_and_labels_from_marked_source(src, &view);
        assert!(labels.iter().any(|label| label == "getClass"), "{labels:?}");
    }

    #[test]
    fn test_array_get_class_var_inference_materializes_multidimensional_array_class_type() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f(String[][] s) {
                var a = s.getClass();
                a|
              }
            }
        "#};

        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let a = candidates
            .iter()
            .find(|candidate| candidate.label.as_ref() == "a")
            .expect("expected local variable completion for a");

        match &a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_eq!(
                    type_descriptor.as_ref(),
                    TypeName::with_args(
                        "java/lang/Class",
                        vec![TypeName::new("java/lang/String").with_array_dims(2)],
                    )
                    .to_internal_with_generics()
                );
            }
            other => panic!("expected local variable candidate, got {other:?}"),
        }
    }

    #[test]
    fn test_array_get_class_var_inference_materializes_primitive_array_class_type() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f(int[] s) {
                var a = s.getClass();
                a|
              }
            }
        "#};

        let (_ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        let a = candidates
            .iter()
            .find(|candidate| candidate.label.as_ref() == "a")
            .expect("expected local variable completion for a");

        match &a.kind {
            crate::completion::CandidateKind::LocalVariable { type_descriptor } => {
                assert_ne!(type_descriptor.as_ref(), "unknown");
                assert_eq!(
                    type_descriptor.as_ref(),
                    TypeName::with_args(
                        "java/lang/Class",
                        vec![TypeName::new("int").with_array_dims(1)],
                    )
                    .to_internal_with_generics()
                );
            }
            other => panic!("expected local variable candidate, got {other:?}"),
        }
    }

    #[test]
    fn test_snapshot_array_get_class_var_inference_provenance() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f(Integer[] s) {
                var a = s.getClass();
                a|
              }
            }
        "#};

        let (mut ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
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
            .filter_map(|candidate| match &candidate.kind {
                crate::completion::CandidateKind::LocalVariable { type_descriptor } => Some(
                    format!("{}|descriptor={}", candidate.label, type_descriptor),
                ),
                _ => None,
            })
            .collect();
        local_candidates.sort();

        insta::assert_snapshot!(
            "array_get_class_var_inference_provenance",
            format!(
                "location={:?}\nlocals=\n{}\nlocal_candidates=\n{}",
                ctx.location,
                locals.join("\n"),
                local_candidates.join("\n"),
            )
        );
    }

    #[test]
    fn test_snapshot_array_receiver_completion_provenance() {
        let idx = make_array_completion_index();
        let view = idx.view(root_scope());
        let src = indoc::indoc! {r#"
            class T {
              void f() {
                String[] s = null;
                s.|
              }
            }
        "#};

        let (mut ctx, candidates) = ctx_and_candidates_from_marked_source(src, &view);
        crate::language::java::completion_context::ContextEnricher::new(&view).enrich(&mut ctx);
        let mut rows: Vec<String> = candidates
            .into_iter()
            .filter(|candidate| {
                matches!(
                    candidate.label.as_ref(),
                    "length" | "getClass" | "substring"
                )
            })
            .map(|candidate| {
                format!(
                    "{}|{:?}|{}",
                    candidate.label,
                    candidate.kind,
                    candidate.detail.unwrap_or_default()
                )
            })
            .collect();
        rows.sort();

        insta::assert_snapshot!(
            "array_receiver_completion_provenance",
            format!(
                "location={:?}\nreceiver_semantic_type={:?}\njava_intrinsic_access={:?}\n{}\n",
                ctx.location,
                ctx.location.member_access_receiver_semantic_type(),
                ctx.java_intrinsic_access,
                rows.join("\n")
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
            let _rope = ropey::Rope::from_str(src_type);
            let mut parser = super::make_java_parser();
            let tree = parser.parse(src_type, None).expect("parse");
            super::JavaLanguage
                .parse_completion_context_with_tree(
                    &crate::workspace::SourceFile::new(
                        tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                        "",
                        0,
                        src_type,
                        Some(tree),
                    ),
                    type_line,
                    type_col,
                    None,
                    &ParseEnv {
                        name_table: Some(view.build_name_table()),
                        workspace: None,
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
            let _rope = ropey::Rope::from_str(src_ctor);
            let mut parser = super::make_java_parser();
            let tree = parser.parse(src_ctor, None).expect("parse");
            super::JavaLanguage
                .parse_completion_context_with_tree(
                    &crate::workspace::SourceFile::new(
                        tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                        "",
                        0,
                        src_ctor,
                        Some(tree),
                    ),
                    ctor_line,
                    ctor_col,
                    None,
                    &ParseEnv {
                        name_table: Some(view.build_name_table()),
                        workspace: None,
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
            let _rope = ropey::Rope::from_str(src_decl);
            let mut parser = super::make_java_parser();
            let tree = parser.parse(src_decl, None).expect("parse");
            super::JavaLanguage
                .parse_completion_context_with_tree(
                    &crate::workspace::SourceFile::new(
                        tower_lsp::lsp_types::Url::parse("file:///test").unwrap(),
                        "",
                        0,
                        src_decl,
                        Some(tree),
                    ),
                    decl_line,
                    decl_col,
                    None,
                    &ParseEnv {
                        name_table: Some(view.build_name_table()),
                        workspace: None,
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
            ctx.is_followed_by_opener(),
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
            ctx.is_followed_by_opener(),
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
            ctx.is_followed_by_opener(),
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
    fn test_injection_import_without_semicolon() {
        let src = indoc::indoc! {r#"
        import java.l
        class A {}
        "#};
        let line = 0u32;
        let col = 13u32;
        let ctx = at(src, line, col);

        assert!(
            matches!(
                &ctx.location,
                CursorLocation::Import { prefix } if prefix == "java.l"
            ),
            "import prefix should be 'java.l', got {:?}",
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

        // The fix should cause handle_constructor to return ConstructorCall
        // with empty class_prefix since injection is no longer used.
        match &ctx.location {
            CursorLocation::ConstructorCall { class_prefix, .. } => {
                assert!(
                    class_prefix.is_empty(),
                    "Expected empty class_prefix, but got '{}'",
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
    fn test_record_this_completion_hides_init() {
        let idx = WorkspaceIndex::new();
        let view = idx.view(root_scope());
        let (_ctx, labels) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                record R(int a, int b) {
                    void m() {
                        this./*caret*/
                    }
                }
            "#},
            &view,
        );
        assert!(!labels.iter().any(|label| label == "<init>"), "{labels:?}");
    }

    #[test]
    fn test_incomplete_enum_constant_trailing_dot_completion_recovers_members() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(
            indoc::indoc! {r#"
                enum RandomEnum {
                    B;

                    public void test() {}
                }
            "#},
            ClassOrigin::Unknown,
            None,
        ));
        let view = idx.view(root_scope());
        let (ctx, labels) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                class T {
                    void m() {
                        RandomEnum.B./*caret*/
                    }
                }
            "#},
            &view,
        );
        assert!(
            matches!(
                ctx.location,
                CursorLocation::MemberAccess { ref receiver_expr, ref member_prefix, .. }
                if receiver_expr == "RandomEnum.B" && member_prefix.is_empty()
            ),
            "location={:?}",
            ctx.location
        );
        assert!(labels.iter().any(|label| label == "test"), "{labels:?}");
    }

    #[test]
    fn test_incomplete_record_local_trailing_dot_completion_recovers_members() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(
            "record RandomRecord(int value) { void test() {} }",
            ClassOrigin::Unknown,
            None,
        ));
        let view = idx.view(root_scope());
        let (ctx, labels) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                class T {
                    void m() {
                        RandomRecord rc;
                        rc./*caret*/
                    }
                }
            "#},
            &view,
        );
        assert!(
            matches!(
                ctx.location,
                CursorLocation::MemberAccess { ref receiver_expr, ref member_prefix, .. }
                if receiver_expr == "rc" && member_prefix.is_empty()
            ),
            "location={:?}",
            ctx.location
        );
        assert!(labels.iter().any(|label| label == "test"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "value"), "{labels:?}");
    }

    #[test]
    fn test_broken_enum_member_access_does_not_corrupt_following_record_local() {
        let idx = WorkspaceIndex::new();
        idx.add_classes(parse_java_source(
            indoc::indoc! {r#"
                enum RandomEnum {
                    B;

                    public void test() {}
                }
            "#},
            ClassOrigin::Unknown,
            None,
        ));
        idx.add_classes(parse_java_source(
            "record RandomRecord(int value) { void test() {} }",
            ClassOrigin::Unknown,
            None,
        ));
        let view = idx.view(root_scope());
        let (ctx, labels) = ctx_and_labels_from_marked_source(
            indoc::indoc! {r#"
                class T {
                    void m() {
                        RandomEnum.B.;

                        RandomRecord rc;
                        rc./*caret*/
                    }
                }
            "#},
            &view,
        );
        let rc = ctx
            .local_variables
            .iter()
            .find(|local| local.name.as_ref() == "rc")
            .expect("rc local");
        assert_eq!(rc.type_internal.to_internal_with_generics(), "RandomRecord");
        assert!(labels.iter().any(|label| label == "test"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "value"), "{labels:?}");
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
