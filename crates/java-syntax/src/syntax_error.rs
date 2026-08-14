use rowan::TextRange;

use crate::{LexicalError, LexicalErrorKind, ParseError, ParseErrorKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxErrorKind {
    Lexer(LexicalErrorKind),
    Parser(ParseErrorKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub kind: SyntaxErrorKind,
    pub range: TextRange,
}

impl From<LexicalError> for SyntaxError {
    fn from(value: LexicalError) -> Self {
        Self {
            kind: SyntaxErrorKind::Lexer(value.kind),
            range: value.range,
        }
    }
}

impl From<ParseError> for SyntaxError {
    fn from(value: ParseError) -> Self {
        Self {
            kind: SyntaxErrorKind::Parser(value.kind),
            range: value.range,
        }
    }
}
