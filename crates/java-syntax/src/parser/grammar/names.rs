use crate::{parser::Parser, syntax_kind::SyntaxKind::*};

pub fn qualified_name(p: &mut Parser) {
    let m = p.start();

    if p.at(IDENTIFIER) {
        p.bump();
        // Stop before a `.` that does not start another name segment —
        // notably `Outer.@Anno Inner`, where the annotated segment belongs
        // to the enclosing *type* production ([JLS §9.7.4]).
        while p.at(DOT) && p.nth(1) == Some(IDENTIFIER) {
            p.bump();
            p.bump();
        }
    } else {
        p.error_expected(&[IDENTIFIER]);
    }

    m.complete(p, QUALIFIED_NAME);
}
