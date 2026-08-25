pub(crate) mod lexer;
pub(crate) mod parser;
pub(crate) mod syntax_error;
pub(crate) mod syntax_kind;

pub use crate::syntax_error::{SyntaxError, SyntaxErrorKind};
pub use lexer::{Lexer, LexicalError, LexicalErrorKind, lex, token::Token};
pub use parser::{Event, Lang, Parse, ParseError, ParseErrorKind, Parser};
pub use syntax_kind::{ContextualKeyword, SyntaxKind};

use rowan::SyntaxNode;

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub syntax_node: SyntaxNode<Lang>,
}

impl SourceFile {
    pub fn parse(text: &str) -> Parse<SourceFile> {
        // lex stage
        let (tokens, lex_errors) = lex(text);

        // parse stage
        let parser = Parser::new(tokens);
        let (green_node, parse_errors) = parser.parse().into();

        // collect errors
        let errors: Vec<SyntaxError> = lex_errors
            .into_iter()
            .map(|e| e.into())
            .chain(parse_errors.into_iter().map(|e| e))
            .collect();

        Parse::new(green_node, errors)
    }
}
