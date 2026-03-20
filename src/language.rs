use std::sync::Arc;

use crate::{
    completion::{CompletionCandidate, provider::CompletionProvider},
    index::{IndexScope, IndexView, NameTable},
    lsp::semantic_tokens::{get_modifier_mask, get_type_idx},
    semantic::SemanticContext,
};
use ropey::Rope;
use smallvec::SmallVec;
use tower_lsp::lsp_types::{
    DocumentSymbol, InlayHint, Range, SemanticToken, SemanticTokenModifier, SemanticTokenType,
};
use tree_sitter::{Node, Parser, Tree};

use crate::workspace::SourceFile;

pub(crate) mod rope_utils;
pub mod salsa_context;
pub(crate) mod ts_utils;

pub mod java;
pub mod kotlin;
pub use java::JavaLanguage;
pub use kotlin::KotlinLanguage;
pub use salsa_context::SalsaContext;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LanguageId(pub Arc<str>);

impl LanguageId {
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub type TokenMods = SmallVec<SemanticTokenModifier, 2>;

pub struct ClassifiedToken {
    pub ty: SemanticTokenType,
    pub modifiers: TokenMods,
}

pub trait Language: Send + Sync + std::fmt::Debug {
    fn id(&self) -> &'static str;
    fn supports(&self, language_id: &str) -> bool;

    fn make_parser(&self) -> Parser;

    /// parse a syntax tree (optionally incrementally, if old_tree is provided)
    fn parse_tree(&self, source: &str, old_tree: Option<&Tree>) -> Option<Tree> {
        let mut parser = self.make_parser();
        parser.parse(source, old_tree)
    }

    #[deprecated]
    fn parse_completion_context(
        &self,
        _source: &str,
        _line: u32,
        _character: u32,
        _trigger_char: Option<char>,
    ) -> Option<SemanticContext> {
        None
    }

    #[allow(deprecated)]
    /// build completion context using an existing syntax tree
    fn parse_completion_context_with_tree(
        &self,
        file: &SourceFile,
        line: u32,
        character: u32,
        trigger_char: Option<char>,
        _env: &ParseEnv,
    ) -> Option<SemanticContext> {
        // Default fallback: reparse (keeps other languages working)
        self.parse_completion_context(file.text(), line, character, trigger_char)
    }

    fn completion_providers(&self) -> &[&'static dyn CompletionProvider] {
        &[]
    }

    fn enrich_completion_context(
        &self,
        _ctx: &mut SemanticContext,
        _scope: IndexScope,
        _index: &IndexView,
    ) {
    }

    fn post_process_candidates(
        &self,
        candidates: Vec<CompletionCandidate>,
        _ctx: &SemanticContext,
    ) -> Vec<CompletionCandidate> {
        candidates
    }

    fn class_file_extensions(&self) -> &[&str] {
        &["jar", "class"]
    }

    fn supports_semantic_tokens(&self) -> bool {
        false
    }

    fn classify_semantic_token<'a>(
        &self,
        _node: Node<'a>,
        _file: &'a SourceFile,
    ) -> Option<ClassifiedToken> {
        None
    }

    fn supports_collecting_symbols(&self) -> bool {
        false
    }

    fn collect_symbols<'a>(
        &self,
        _node: Node<'a>,
        _file: &'a SourceFile,
    ) -> Option<Vec<DocumentSymbol>> {
        None
    }

    fn supports_inlay_hints(&self) -> bool {
        false
    }

    fn collect_inlay_hints_with_tree(
        &self,
        _file: &SourceFile,
        _range: Range,
        _env: &ParseEnv,
        _index: &IndexView,
    ) -> Option<Vec<InlayHint>> {
        None
    }

    // ========================================================================
    // NEW: Salsa-based methods for incremental computation
    // ========================================================================

    /// Extract completion context using Salsa queries (CACHED)
    ///
    /// This is the new Salsa-based method that provides automatic memoization.
    /// It delegates to language-specific Salsa queries.
    fn extract_completion_context_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        line: u32,
        character: u32,
        trigger_char: Option<char>,
    ) -> Option<Arc<crate::salsa_queries::CompletionContextData>> {
        // Default implementation delegates to the generic query
        Some(crate::salsa_queries::extract_completion_context(
            db,
            file,
            line,
            character,
            trigger_char,
        ))
    }

    /// Resolve symbol at position using Salsa queries (CACHED)
    ///
    /// This is the new Salsa-based method for goto definition.
    fn resolve_symbol_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        line: u32,
        character: u32,
    ) -> Option<Arc<crate::salsa_queries::ResolvedSymbolData>> {
        crate::salsa_queries::resolve_symbol_at_position(db, file, line, character)
    }

    /// Compute inlay hints using Salsa queries (CACHED)
    ///
    /// This is the new Salsa-based method for inlay hints.
    fn compute_inlay_hints_salsa(
        &self,
        db: &dyn crate::salsa_queries::Db,
        file: crate::salsa_db::SourceFile,
        range: Range,
    ) -> Option<Arc<Vec<crate::salsa_queries::InlayHintData>>> {
        Some(crate::salsa_queries::compute_inlay_hints(
            db,
            file,
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
        ))
    }
}

#[derive(Clone, Default)]
pub struct ParseEnv {
    pub name_table: Option<Arc<NameTable>>,
    pub workspace: Option<Arc<crate::workspace::Workspace>>,
}

pub struct LanguageRegistry {
    languages: Vec<Box<dyn Language>>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self {
            languages: vec![Box::new(JavaLanguage), Box::new(KotlinLanguage)],
        }
    }

    pub fn find(&self, language_id: &str) -> Option<&dyn Language> {
        self.languages
            .iter()
            .find(|l| l.supports(language_id))
            .map(|l| l.as_ref())
    }

    pub fn register(&mut self, lang: Box<dyn Language>) {
        self.languages.push(lang);
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct TokenCollector<'a> {
    file: &'a SourceFile,
    lang: &'a dyn Language,

    data: Vec<SemanticToken>,
    last_line: u32,
    last_col_utf16: u32,
}

impl<'a> TokenCollector<'a> {
    pub fn new(file: &'a SourceFile, lang: &'a dyn Language) -> Self {
        Self {
            file,
            lang,
            data: Vec::new(),
            last_line: 0,
            last_col_utf16: 0,
        }
    }

    #[inline]
    fn mods_to_bitset(mods: &[SemanticTokenModifier]) -> u32 {
        let mut bitset = 0u32;
        for m in mods {
            bitset |= get_modifier_mask(m);
        }
        bitset
    }

    fn push_token(
        &mut self,
        node: Node,
        ty: SemanticTokenType,
        modifiers: &[SemanticTokenModifier],
    ) {
        let start_byte = node.start_byte();
        let end_byte = node.end_byte();

        // byte -> char index (Unicode scalar index)
        let start_char = self.file.rope.byte_to_char(start_byte);
        let end_char = self.file.rope.byte_to_char(end_byte);

        // char -> line + column(char)
        let line_idx = self.file.rope.char_to_line(start_char);
        let line_start_char = self.file.rope.line_to_char(line_idx);
        let col_char = start_char.saturating_sub(line_start_char);

        // column/length in UTF-16 code units (LSP required)
        let col_utf16 = utf16_units_in_rope_char_range(
            &self.file.rope,
            line_start_char,
            line_start_char + col_char,
        );
        let len_utf16 = utf16_units_in_rope_char_range(&self.file.rope, start_char, end_char);

        let line = line_idx as u32;
        let col = col_utf16 as u32;
        let length = len_utf16 as u32;

        // LSP SemanticTokens delta encoding
        let delta_line = line.saturating_sub(self.last_line);
        let delta_start = if delta_line == 0 {
            col.saturating_sub(self.last_col_utf16)
        } else {
            col
        };

        self.data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: get_type_idx(&ty),
            token_modifiers_bitset: Self::mods_to_bitset(modifiers),
        });

        self.last_line = line;
        self.last_col_utf16 = col;
    }

    /// DFS 遍历，交给语言侧 classify 来决定是否为 token
    pub fn collect(&mut self, node: Node) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(classified) = self.lang.classify_semantic_token(child, self.file) {
                self.push_token(child, classified.ty, &classified.modifiers);
            }
            if child.child_count() > 0 {
                self.collect(child);
            }
        }
    }

    pub fn finish(self) -> Vec<SemanticToken> {
        self.data
    }

    /// Collect tokens only for nodes that intersect the given byte range
    /// [range_start_byte, range_end_byte). The returned token list uses the
    /// standard LSP delta encoding relative to the first token in the range,
    /// so callers must re-encode it if they want absolute positions — but
    /// since the spec says range results use the same encoding as full
    /// results, we return a self-contained delta sequence starting from 0.
    pub fn collect_range(&mut self, node: Node, range_start_byte: usize, range_end_byte: usize) {
        // Prune subtrees that are entirely outside the range
        if node.end_byte() <= range_start_byte || node.start_byte() >= range_end_byte {
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            // Skip children wholly outside the range
            if child.end_byte() <= range_start_byte || child.start_byte() >= range_end_byte {
                continue;
            }
            if let Some(classified) = self.lang.classify_semantic_token(child, self.file) {
                self.push_token(child, classified.ty, &classified.modifiers);
            }
            if child.child_count() > 0 {
                self.collect_range(child, range_start_byte, range_end_byte);
            }
        }
    }
}

/// 计算 rope 的 [start_char, end_char) 区间内 UTF-16 code units 数量
fn utf16_units_in_rope_char_range(rope: &Rope, start_char: usize, end_char: usize) -> usize {
    if end_char <= start_char {
        return 0;
    }
    rope.slice(start_char..end_char)
        .chars()
        .map(|c| if (c as u32) >= 0x10000 { 2 } else { 1 })
        .sum()
}
