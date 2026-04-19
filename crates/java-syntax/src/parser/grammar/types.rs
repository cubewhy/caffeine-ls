use crate::{
    grammar::error_recover::{recover_type_argument, recover_type_bound},
    kinds::SyntaxKind::*,
    parser::{
        ExpectedConstruct, Parser,
        grammar::{error_recover::recover_parameter, modifiers::modifiers},
    },
    tokenset,
};

pub fn formal_parameters(p: &mut Parser) {
    // [modifiers] <type>[...] <identifier>, [modifiers] <type>[...] <identifier>
    let m = p.start();

    p.expect(L_PAREN);

    // parameters
    if !p.at(R_PAREN) {
        parameter(p);
        while p.eat(COMMA) {
            parameter(p);
        }
    }

    p.expect(R_PAREN);

    m.complete(p, FORMAL_PARAMETERS);
}

fn parameter(p: &mut Parser) {
    let m = p.start();

    modifiers(p);

    // type
    if type_(p).is_err() {
        recover_parameter(p);
        m.complete(p, ERROR);
        return;
    }

    // ...
    let mut is_spread = false;
    if p.eat(ELLIPSIS) {
        is_spread = true;
    }

    // parameter name
    if p.eat(IDENTIFIER) {
        let kind = if is_spread {
            SPREAD_PARAMETER
        } else {
            FORMAL_PARAMETER
        };
        m.complete(p, kind);
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

pub fn type_or_void(p: &mut Parser) -> Result<(), ()> {
    if p.eat(VOID_KW) {
        return Ok(());
    }
    type_(p)
}

/// Parse a type identifier
///
/// Return `Err(())` if an ERROR node is generated
pub fn type_(p: &mut Parser) -> Result<(), ()> {
    if at_primitive_type(p) {
        let m = p.start();
        p.bump();
        dimensions(p);
        m.complete(p, TYPE);
        return Ok(());
    }

    reference_type(p)
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
    let m = p.start();

    if !p.at(IDENTIFIER) {
        p.error_expected_construct(ExpectedConstruct::Type);
        m.complete(p, ERROR);
        return Err(());
    }

    p.expect(IDENTIFIER);
    type_arguments_opt(p);

    while p.eat(DOT) {
        p.expect(IDENTIFIER);
        type_arguments_opt(p);
    }

    dimensions(p);

    m.complete(p, TYPE);
    Ok(())
}

pub fn type_arguments_opt(p: &mut Parser) {
    if p.at(LESS) {
        type_arguments(p);
    }
}

pub fn type_arguments(p: &mut Parser) {
    let m = p.start();

    p.expect(LESS);

    if !p.at(GREATER) {
        if type_argument(p).is_err() {
            recover_type_argument(p);
        }

        while p.eat(COMMA) {
            if p.at(GREATER) {
                break;
            }

            if type_argument(p).is_err() {
                recover_type_argument(p);
            }
        }
    }

    p.expect(GREATER);

    m.complete(p, TYPE_ARGUMENTS);
}

pub fn type_argument(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    let res = if p.at(QUESTION) {
        wildcard_type(p)
    } else {
        reference_type(p)
    };

    // types in generics should be reference type
    if res.is_err() {
        m.complete(p, ERROR);
        return Err(());
    }

    m.complete(p, TYPE_ARGUMENT);
    Ok(())
}

pub fn wildcard_type(p: &mut Parser) -> Result<(), ()> {
    // <? extends/super bound>
    let m = p.start();

    p.expect(QUESTION); // ?

    // extends or super
    if p.at(EXTENDS_KW) || p.at(SUPER_KW) {
        wildcard_bounds(p)?;
    }

    m.complete(p, WILDCARD_TYPE);
    Ok(())
}

fn wildcard_bounds(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    // consume extends or super keyword
    if p.at(EXTENDS_KW) || p.at(SUPER_KW) {
        p.bump();
    } else {
        p.error_expected(&[EXTENDS_KW, SUPER_KW]);
        m.complete(p, ERROR);
        return Err(());
    }

    // parse bound
    if reference_type(p).is_err() {
        m.complete(p, ERROR);
        return Err(());
    }

    m.complete(p, WILDCARD_BOUNDS);
    Ok(())
}
