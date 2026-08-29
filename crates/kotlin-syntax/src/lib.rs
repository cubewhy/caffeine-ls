pub(crate) mod lexer;
pub(crate) mod parser;
pub(crate) mod syntax_error;
pub(crate) mod syntax_kind;

pub use crate::syntax_error::{SyntaxError, SyntaxErrorKind};
pub use lexer::{Lexer, LexicalError, LexicalErrorKind, lex, token::Token};
pub use parser::{Event, Lang, Parse, ParseError, ParseErrorKind, Parser, grammar};
pub use syntax_kind::{ContextualKeyword, SyntaxKind};

use rowan::SyntaxNode;

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub syntax_node: SyntaxNode<Lang>,
}

impl SourceFile {
    /// Parses a `kotlinFile` ([spec: grammar-rule-kotlinFile]).
    pub fn parse(text: &str) -> Parse<SourceFile> {
        let (tokens, lex_errors) = lex(text);
        Self::assemble(Parser::new(tokens).parse(), lex_errors)
    }

    /// Parses a `.kts` script file as the KLS `script` grammar
    /// ([spec: grammar-rule-script]).
    pub fn parse_script(text: &str) -> Parse<SourceFile> {
        let (tokens, lex_errors) = lex(text);
        Self::assemble(Parser::new(tokens).parse_script(), lex_errors)
    }

    fn assemble(parse: Parse, lex_errors: Vec<LexicalError>) -> Parse<SourceFile> {
        let (green_node, mut errors) = parse.into();

        errors.splice(0..0, lex_errors.into_iter().map(Into::into));

        Parse::new(green_node, errors)
    }
}
