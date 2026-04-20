use crate::{
    grammar::{error_recover::recover_parameter, modifiers::annotation},
    kinds::SyntaxKind::*,
    parser::{ExpectedConstruct, Parser},
};

pub fn argument_list(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);

    if !p.at(R_PAREN) {
        expression(p);
        while p.eat(COMMA) {
            expression(p);
        }
    }

    p.expect(R_PAREN);
    m.complete(p, ARGUMENT_LIST);
}

pub fn array_initializer(p: &mut Parser) {
    let m = p.start();

    p.expect(L_BRACE); // {

    if !p.at(R_BRACE) {
        element_value(p);

        while p.eat(COMMA) {
            if p.at(R_BRACE) {
                break; // trailing comma
            }
            element_value(p);
        }
    }

    p.expect(R_BRACE);

    m.complete(p, ARRAY_INITIALIZER);
}

pub fn expression(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();
    let start_pos = p.pos();

    // TODO: parse java expressions
    while !p.is_at_end() && !p.at(COMMA) && !p.at(SEMICOLON) && !p.at(R_PAREN) && !p.at(R_BRACE) {
        p.bump();
    }

    if p.pos() == start_pos {
        p.error_expected_construct(ExpectedConstruct::Expression);
        m.complete(p, ERROR);
        Err(())
    } else {
        m.complete(p, EXPRESSION);
        Ok(())
    }
}

pub fn element_value(p: &mut Parser) {
    if p.at(AT) {
        annotation(p);
    } else if p.at(L_BRACE) {
        array_initializer(p);
    } else {
        if expression(p).is_err() {
            recover_parameter(p);
        }
    }
}
