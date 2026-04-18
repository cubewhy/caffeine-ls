use crate::{kinds::SyntaxKind, lexer::token::Token};

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
        self.nth(0)
    }

    pub fn nth(&self, n: usize) -> Option<SyntaxKind> {
        let idx = *self.indices.get(self.cursor + n)?;
        Some(self.tokens[idx].kind)
    }

    pub fn bump(&mut self) {
        if self.cursor < self.indices.len() {
            self.cursor += 1;
        }
    }

    pub fn is_at_end(&self) -> bool {
        self.cursor >= self.indices.len()
    }

    pub fn current_raw_index(&self) -> Option<usize> {
        self.indices.get(self.cursor).copied()
    }

    pub fn into_inner(self) -> Vec<Token<'a>> {
        self.tokens
    }
}
