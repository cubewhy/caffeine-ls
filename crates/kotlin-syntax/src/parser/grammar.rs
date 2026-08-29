mod annotations;
mod compilation_unit;
mod decl;
mod modifiers;
mod names;
mod types;

pub use compilation_unit::root;

use crate::{Parser, SyntaxKind::*};

/// `NL` is a real (non-trivia) token produced by the lexer, so the parser
/// sees it directly. Grammar rules sprinkle `{NL}` liberally; this helper
/// consumes any run of them.
pub(crate) fn eat_nl(p: &mut Parser) {
    while p.at(NEWLINE) {
        p.bump();
    }
}

/// `semi`: (';' | NL) {NL}
/// [spec: grammar-rule-semi] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-semi
pub(crate) fn semi(p: &mut Parser) {
    if p.at(SEMICOLON) || p.at(NEWLINE) {
        p.bump();
    }
    eat_nl(p);
}

/// `semis`: ';' | NL {';' | NL}
/// [spec: grammar-rule-semis] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-semis
pub(crate) fn semis(p: &mut Parser) {
    while p.at(SEMICOLON) || p.at(NEWLINE) {
        p.bump();
    }
}

/// Whether the current token can start a `simpleIdentifier`:
/// any IDENTIFIER token (soft keywords are lexed as IDENTIFIER).
pub(crate) fn at_simple_identifier(p: &Parser) -> bool {
    p.at(IDENTIFIER)
}
