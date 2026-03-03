use std::sync::Arc;

use crate::{
    completion::{CompletionCandidate, CompletionContext},
    lsp::semantic_tokens::{get_modifier_mask, get_type_idx},
};
use ropey::Rope;
use smallvec::SmallVec;
use tower_lsp::lsp_types::{
    DocumentSymbol, SemanticToken, SemanticTokenModifier, SemanticTokenType,
};
use tree_sitter::{Node, Parser};

pub(crate) mod rope_utils;
pub(crate) mod ts_utils;

pub mod java;
pub mod kotlin;
pub use java::JavaLanguage;
pub use kotlin::KotlinLanguage;

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

    fn parse_completion_context(
        &self,
        source: &str,
        line: u32,
        character: u32,
        trigger_char: Option<char>,
    ) -> Option<CompletionContext>;

    fn post_process_candidates(
        &self,
        candidates: Vec<CompletionCandidate>,
        _ctx: &CompletionContext,
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
        _node: tree_sitter::Node<'a>,
        _bytes: &'a [u8],
    ) -> Option<ClassifiedToken> {
        None
    }

    fn supports_collecting_symbols(&self) -> bool {
        false
    }

    fn collect_symbols<'a>(
        &self,
        _node: tree_sitter::Node<'a>,
        _bytes: &'a [u8],
    ) -> Option<Vec<DocumentSymbol>> {
        None
    }
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
    bytes: &'a [u8],
    rope: &'a Rope,
    lang: &'a dyn Language,

    data: Vec<SemanticToken>,
    last_line: u32,
    last_col_utf16: u32,
}

impl<'a> TokenCollector<'a> {
    pub fn new(bytes: &'a [u8], rope: &'a Rope, lang: &'a dyn Language) -> Self {
        Self {
            bytes,
            rope,
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
        let start_char = self.rope.byte_to_char(start_byte);
        let end_char = self.rope.byte_to_char(end_byte);

        // char -> line + column(char)
        let line_idx = self.rope.char_to_line(start_char);
        let line_start_char = self.rope.line_to_char(line_idx);
        let col_char = start_char.saturating_sub(line_start_char);

        // column/length in UTF-16 code units (LSP required)
        let col_utf16 =
            utf16_units_in_rope_char_range(self.rope, line_start_char, line_start_char + col_char);
        let len_utf16 = utf16_units_in_rope_char_range(self.rope, start_char, end_char);

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
            if let Some(classified) = self.lang.classify_semantic_token(child, self.bytes) {
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
