use crate::{
    Parser,
    SyntaxKind::*,
    grammar::{eat_nl, names::simple_identifier},
};

/// `userType`: simpleUserType {{NL} '.' {NL} simpleUserType}
/// [spec: grammar-rule-userType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-userType
///
/// Type arguments are added with the full type grammar.
pub(crate) fn user_type(p: &mut Parser) {
    let m = p.start();
    simple_identifier(p);
    while p.at(DOT) && p.nth(1) == Some(IDENTIFIER) {
        p.bump();
        eat_nl(p);
        simple_identifier(p);
    }
    m.complete(p, USER_TYPE);
}
