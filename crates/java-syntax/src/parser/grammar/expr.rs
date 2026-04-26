use crate::{
    grammar::{
        error_recover::{recover_parameter, recover_until},
        modifiers::annotation,
        types::{reference_type, type_},
    },
    kinds::SyntaxKind::*,
    parser::{ExpectedConstruct, Parser},
};

pub fn argument_list(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);

    if !p.at(R_PAREN) {
        loop {
            if expression(p).is_err() {
                recover_parameter(p);
            }

            if !p.eat(COMMA) {
                break;
            }
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

pub fn variable_access(p: &mut Parser) {
    // TODO: Stub variable access
    let m = p.start();

    if p.at(IDENTIFIER) || p.at(THIS_KW) || p.at(SUPER_KW) {
        p.bump();
    } else {
        p.error_expected(&[IDENTIFIER, THIS_KW, SUPER_KW]);
        m.complete(p, ERROR);
        return;
    }

    while p.eat(DOT) {
        if p.at(IDENTIFIER) || p.at(THIS_KW) || p.at(SUPER_KW) {
            p.bump();
        } else {
            p.error_expected(&[IDENTIFIER]);
            break;
        }
    }

    m.complete(p, VARIABLE_ACCESS);
}

pub fn expression_list(p: &mut Parser) {
    loop {
        if expression(p).is_err() {
            recover_until(p, &[COMMA, R_PAREN]);
        }

        if !p.eat(COMMA) {
            break;
        }
    }
}

pub fn is_pattern(p: &mut Parser) -> bool {
    let ckpt = p.checkpoint();
    type_(p);

    let Some(next_token) = p.current() else {
        return false;
    };

    let is_pattern = matches!(next_token, IDENTIFIER | UNDERSCORE | L_PAREN);

    p.rewind(ckpt);

    is_pattern
}

pub fn case_pattern_or_constant(p: &mut Parser) {
    if is_pattern(p) {
        pattern(p);
    } else {
        expression(p);
    }
}

/// Pattern:
///   TypePattern
///   RecordPattern
///
/// TypePattern:
///   LocalVariableDeclaration
///
/// RecordPattern:
///   ReferenceType ( [ComponentPatternList] )
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-Pattern
fn pattern(p: &mut Parser) {
    let m = p.start();

    if is_record_pattern_lookahead(p) {
        reference_type(p);
        p.expect(L_PAREN);
        if !p.at(R_PAREN) {
            component_pattern_list(p);
        }
        p.expect(R_PAREN);
        m.complete(p, RECORD_PATTERN);
    } else {
        if type_pattern(p).is_ok() {
            m.complete(p, TYPE_PATTERN);
        } else {
            m.abandon(p);
            p.error_expected_construct(ExpectedConstruct::Pattern);
        }
    }
}

fn is_record_pattern_lookahead(p: &Parser) -> bool {
    let mut i = 0;
    while matches!(p.nth(i), Some(IDENTIFIER) | Some(DOT)) {
        i += 1;
    }
    p.nth(i) == Some(L_PAREN)
}

fn type_pattern(p: &mut Parser) -> Result<(), ()> {
    if type_(p).is_err() {
        return Err(());
    }

    if p.at(IDENTIFIER) || p.at(UNDERSCORE) {
        p.bump();
        Ok(())
    } else {
        Err(())
    }
}

/// ComponentPatternList:
///   ComponentPattern {, ComponentPattern }
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-ComponentPatternList
fn component_pattern_list(p: &mut Parser) {
    component_pattern(p);
    while p.eat(COMMA) {
        component_pattern(p);
    }
}

/// ComponentPattern:
///   Pattern
///   MatchAllPattern
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-ComponentPattern
fn component_pattern(p: &mut Parser) {
    if p.at(UNDERSCORE) {
        let m = p.start();
        p.expect(UNDERSCORE);
        m.complete(p, MATCH_ALL_PATTERN);
    } else {
        pattern(p);
    }
}
