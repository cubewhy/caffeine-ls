pub mod incremental;
pub(crate) mod lexer;
pub(crate) mod parser;
pub(crate) mod reader;
pub(crate) mod syntax_kind;

pub use lexer::{Lexer, LexicalError, LexicalErrorKind, lex, token::Token};
pub use parser::{
    EntryPoint, Event, Lang, Parse, ParseError, ParseErrorKind, Parser, grammar, parse,
};
pub use syntax_kind::{ContextualKeyword, SyntaxKind};
