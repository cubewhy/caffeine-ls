mod annotations;
mod compilation_unit;
mod decl;
mod expr;
mod modifiers;
mod names;
mod statements;
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

#[cfg(test)]
pub(crate) mod tests {
    use crate::{Event, Parser, lex};

    /// Runs a grammar function over a source and dumps the parser events so
    /// milestones that are not yet reachable from `root` (types, expressions)
    /// can be snapshot-tested in isolation.
    pub(crate) fn parse_with(f: impl FnOnce(&mut Parser), src: &str) -> String {
        let (tokens, _lex_errors) = lex(src);
        let mut p = Parser::new(tokens);
        f(&mut p);

        let mut out = String::new();
        for ev in &p.events {
            match ev {
                Event::Tombstone => out.push_str("Tombstone\n"),
                Event::AddToken => out.push_str("AddToken\n"),
                Event::AddVirtualToken { kind, lexeme } => {
                    out.push_str(&format!("AddVirtualToken({kind:?}, {lexeme:?})\n"))
                }
                Event::AdvanceSource => out.push_str("AdvanceSource\n"),
                Event::FinishNode => out.push_str("FinishNode\n"),
                Event::Error(err) => out.push_str(&format!("Error({err:?})\n")),
                Event::StartNode { kind, .. } => out.push_str(&format!("StartNode({kind:?})\n")),
            }
        }
        for err in &p.errors {
            out.push_str(&format!("ERROR {err:?}\n"));
        }
        out
    }
}
