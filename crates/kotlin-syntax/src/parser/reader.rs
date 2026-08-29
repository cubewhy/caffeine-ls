use crate::{lexer::token::Token, syntax_kind::SyntaxKind};

pub struct TokenSource<'a> {
    tokens: Vec<Token<'a>>,
    indices: Vec<usize>,
    cursor: usize,
}

impl<'a> TokenSource<'a> {
    pub fn new(tokens: Vec<Token<'a>>) -> Self {
        let indices = tokens
            .iter()
            .enumerate()
            .filter_map(|(i, t)| (!t.kind.is_trivia()).then_some(i))
            .collect();

        Self {
            tokens,
            indices,
            cursor: 0,
        }
    }

    pub fn current(&self) -> Option<SyntaxKind> {
        self.nth(0).map(|token| token.kind)
    }

    pub fn current_lexeme(&'a self) -> Option<&'a str> {
        self.nth(0).map(|token| token.lexeme)
    }

    pub fn nth_lexeme(&'a self, n: usize) -> Option<&'a str> {
        self.nth(n).map(|token| token.lexeme)
    }

    pub fn nth(&self, n: usize) -> Option<&'_ Token<'a>> {
        let idx = *self.indices.get(self.cursor + n)?;
        Some(&self.tokens[idx])
    }

    pub fn bump(&mut self) {
        if self.cursor < self.indices.len() {
            self.cursor += 1;
        }
    }

    pub fn is_at_end(&self) -> bool {
        self.cursor >= self.indices.len()
    }

    pub fn into_inner(self) -> Vec<Token<'a>> {
        self.tokens
    }

    /// Whether a line break (`NL` token) separates the last consumed
    /// significant token from the current one.
    ///
    /// `NL` is a real token in `indices` (it is not trivia), so a preceding
    /// newline is visible even when the caller already consumed it with
    /// `eat_nl`: in that case the *previous* significant token is the `NL`
    /// itself and falls inside the scanned window.
    pub fn line_break_before(&self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = self.indices[self.cursor - 1];
        let end = self
            .indices
            .get(self.cursor)
            .copied()
            .unwrap_or(self.tokens.len());
        (prev..end).any(|i| self.tokens[i].kind == SyntaxKind::NEWLINE)
    }

    pub fn pos(&self) -> usize {
        self.cursor
    }

    pub fn set_pos(&mut self, new_pos: usize) {
        assert!(
            new_pos <= self.indices.len(),
            "TokenSource::set_pos out of bounds"
        );
        self.cursor = new_pos;
    }
}
