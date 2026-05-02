use crate::{parser::Parser, syntax_kind::SyntaxKind::*};

pub fn qualified_name(p: &mut Parser) {
    let m = p.start();

    if p.at(IDENTIFIER) {
        p.bump();
        while p.eat(DOT) {
            p.expect(IDENTIFIER);
        }
    } else {
        p.error_expected(&[IDENTIFIER]);
    }

    m.complete(p, QUALIFIED_NAME);
}
