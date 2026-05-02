use rowan::TextSize;

use crate::kinds::SyntaxKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'source> {
    pub kind: SyntaxKind,
    pub lexeme: &'source str,
    pub offset: TextSize, // the start position of the token
}

impl<'s> Token<'s> {
    pub fn new(kind: SyntaxKind, lexeme: &'s str, offset: usize) -> Self {
        Self {
            kind,
            lexeme,
            offset: TextSize::new(offset as u32),
        }
    }
}
