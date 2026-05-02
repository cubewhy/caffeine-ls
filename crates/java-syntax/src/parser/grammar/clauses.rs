use crate::{grammar::types::type_, parser::Parser, syntax_kind::SyntaxKind::*};

pub fn throws_clause_opt(p: &mut Parser) {
    if p.at(THROWS_KW) {
        throws_clause(p);
    }
}

pub fn throws_clause(p: &mut Parser) {
    let m = p.start();
    p.expect(THROWS_KW);

    type_(p).ok();
    while p.eat(COMMA) {
        type_(p).ok();
    }

    m.complete(p, THROWS_CLAUSE);
}

pub fn extends_clause(p: &mut Parser) {
    let m = p.start();
    p.expect(EXTENDS_KW);

    type_(p).ok();

    m.complete(p, EXTENDS_CLAUSE);
}

pub fn implements_clause(p: &mut Parser) {
    let m = p.start();
    p.expect(IMPLEMENTS_KW);

    type_(p).ok();
    while p.eat(COMMA) {
        type_(p).ok();
    }

    m.complete(p, IMPLEMENTS_CLAUSE);
}

pub fn interface_extends_clause(p: &mut Parser) {
    let m = p.start();
    p.expect(EXTENDS_KW);

    type_(p).ok();
    while p.eat(COMMA) {
        type_(p).ok();
    }

    m.complete(p, INTERFACE_EXTENDS_CLAUSE);
}
