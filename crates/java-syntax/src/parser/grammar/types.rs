use crate::{
    grammar::error_recover::recover_type_bound,
    kinds::SyntaxKind::*,
    parser::{
        ExpectedConstruct, Parser,
        grammar::{error_recover::recover_parameter, modifiers::modifiers, names::qualified_name},
    },
    tokenset,
};

pub fn formal_parameters(p: &mut Parser) {
    // [modifiers] <type> <identifier>, [modifiers] <type> <identifier>
    let m = p.start();

    p.expect(L_PAREN);

    // parameters
    if !p.at(R_PAREN) {
        formal_parameter(p);
        while p.eat(COMMA) {
            formal_parameter(p);
        }
    }

    p.expect(R_PAREN);

    m.complete(p, FORMAL_PARAMETERS);
}

fn formal_parameter(p: &mut Parser) {
    let m = p.start();
    modifiers(p);

    if type_(p).is_err() {
        recover_parameter(p);
        m.complete(p, ERROR);
        return;
    }

    if p.at(IDENTIFIER) {
        p.bump();
        m.complete(p, FORMAL_PARAMETER);
    } else {
        p.error_expected(&[IDENTIFIER]);
        recover_parameter(p);
        m.complete(p, ERROR);
    }
}

pub fn at_type_start(p: &Parser) -> bool {
    at_primitive_type(p) || p.at(IDENTIFIER)
}

pub fn at_primitive_type(p: &Parser) -> bool {
    p.at_set(tokenset![
        INT_KW, SHORT_KW, LONG_KW, FLOAT_KW, DOUBLE_KW, BYTE_KW, BOOLEAN_KW, CHAR_KW,
    ])
}

pub fn dimensions(p: &mut Parser) {
    let m = p.start();

    let mut seen = false;
    while p.at(L_BRACKET) && p.nth(1) == Some(R_BRACKET) {
        seen = true;
        p.bump(); // [
        p.bump(); // ]
    }

    if seen {
        m.complete(p, DIMENSIONS);
    } else {
        m.abandon(p);
    }
}

/// Parse a type identifier
///
/// Return `Err(())` if an ERROR node is generated
pub fn type_(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    if at_primitive_type(p) {
        p.bump();
    } else if p.at(IDENTIFIER) {
        qualified_name(p);
    } else {
        p.error_expected_construct(ExpectedConstruct::Type);
        m.complete(p, ERROR);
        return Err(());
    }

    // array type
    dimensions(p);

    m.complete(p, TYPE);

    Ok(())
}

pub fn type_parameters_opt(p: &mut Parser) {
    if p.at(LESS) {
        type_parameters(p);
    }
}

pub fn type_parameters(p: &mut Parser) {
    let m = p.start();

    p.expect(LESS);

    type_parameter(p);
    while p.eat(COMMA) {
        type_parameter(p);
    }

    p.expect(GREATER);

    m.complete(p, TYPE_PARAMETERS);
}

pub fn type_parameter(p: &mut Parser) {
    let m = p.start();

    p.expect(IDENTIFIER);

    if p.at(EXTENDS_KW) {
        type_bound(p);
    }

    m.complete(p, TYPE_PARAMETER);
}

pub fn type_bound(p: &mut Parser) {
    let m = p.start();

    // extends
    p.expect(EXTENDS_KW);

    if reference_type(p).is_err() {
        recover_type_bound(p);
        m.complete(p, ERROR);
        return;
    }

    // &
    while p.eat(BIT_AND) {
        if reference_type(p).is_err() {
            recover_type_bound(p);
            m.complete(p, ERROR);
            return;
        }
    }

    m.complete(p, TYPE_BOUND);
}

/// Build node for reference type
///
/// Returns:
///
/// Return Err(()) if the current token is not treated as an reference type (IDENTIFIER)
pub fn reference_type(p: &mut Parser) -> Result<(), ()> {
    // <typename>[]
    let m = p.start();

    // <typename>
    if p.at(IDENTIFIER) {
        qualified_name(p);
    } else {
        p.error_expected_construct(ExpectedConstruct::Type);
        m.complete(p, ERROR);
        return Err(());
    }

    // []
    dimensions(p);

    m.complete(p, TYPE);
    Ok(())
}
