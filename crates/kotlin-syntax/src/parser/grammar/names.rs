use crate::{Parser, SyntaxKind::*};

/// `simpleIdentifier`: Identifier | soft keyword
///
/// Soft keywords are lexed as plain IDENTIFIER tokens, so a bare
/// `IDENTIFIER` here also covers them ([spec: grammar-rule-simpleIdentifier]).
pub(crate) fn simple_identifier(p: &mut Parser) {
    if !p.eat(IDENTIFIER) {
        p.error_expected(&[IDENTIFIER]);
    }
}

/// `identifier`: simpleIdentifier {{NL} '.' simpleIdentifier}
/// [spec: grammar-rule-identifier] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-identifier
pub(crate) fn identifier(p: &mut Parser) {
    let m = p.start();
    simple_identifier(p);
    while p.at(DOT) && p.nth(1) == Some(IDENTIFIER) {
        p.bump();
        simple_identifier(p);
    }
    m.complete(p, QUALIFIED_NAME);
}
