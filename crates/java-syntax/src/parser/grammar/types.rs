use crate::{
    SyntaxKind,
    grammar::{
        error_recover::{recover_type_argument, recover_type_bound},
        expr::{expression, is_expression_start},
        names::qualified_name,
    },
    parser::{
        ExpectedConstruct, Parser,
        grammar::{
            error_recover::recover_parameter,
            modifiers::{annotation, modifiers},
        },
    },
    syntax_kind::SyntaxKind::*,
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

pub fn is_formal_parameters(p: &Parser) -> bool {
    let i = 1; // skip L_PAREN
    let kind = p.nth(i);

    // have modifiers
    if matches!(kind, Some(FINAL_KW) | Some(AT)) {
        return true;
    }

    // is primitive type
    if matches!(kind, Some(k) if is_primitive_type(k)) {
        return true;
    }

    if matches!(kind, Some(IDENTIFIER) | Some(UNDERSCORE)) {
        let next = p.nth(i + 1);

        if !matches!(next, Some(COMMA) | Some(R_PAREN)) {
            return true;
        }
    }

    false
}

fn is_concise_param(p: &Parser) -> bool {
    p.at(IDENTIFIER) || p.at(UNDERSCORE)
}

pub fn inferred_parameters(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);

    if !p.at(R_PAREN) {
        loop {
            let m_param = p.start();
            if is_concise_param(p) {
                p.bump();
                m_param.complete(p, INFERRED_PARAMETERS);
            } else {
                p.error_message("Expected identifier or '_'");
                m_param.complete(p, ERROR);
            }

            if !p.eat(COMMA) {
                break;
            }
        }
    }

    p.expect(R_PAREN);
    m.complete(p, INFERRED_PARAMETERS);
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
        // c-style array
        if p.at(L_BRACKET) {
            dimensions(p);
        }

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
    at_primitive_type(p)
        || p.at(IDENTIFIER)
        // [JLS §9.7.4] a type use may begin with its annotations
        // (`@Nullable Object field`).
        || (p.at(AT) && p.nth(1) != Some(INTERFACE_KW))
}

pub fn at_primitive_type(p: &Parser) -> bool {
    p.at_set(tokenset![
        INT_KW, SHORT_KW, LONG_KW, FLOAT_KW, DOUBLE_KW, BYTE_KW, BOOLEAN_KW, CHAR_KW,
    ])
}

pub fn is_primitive_type(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        BOOLEAN_KW | BYTE_KW | SHORT_KW | INT_KW | LONG_KW | CHAR_KW | FLOAT_KW | DOUBLE_KW
    )
}

pub fn dimensions(p: &mut Parser) {
    if !p.at(L_BRACKET) && !at_annotated_dimension(p) {
        return;
    }

    let m = p.start();
    while p.at(L_BRACKET) || at_annotated_dimension(p) {
        // [JLS §9.7.4] an array dimension may carry annotations
        // (`int @Nullable []`).
        type_annotations_opt(p);

        let m_inner = p.start();
        p.expect(L_BRACKET);

        if !p.at(R_BRACKET) && is_expression_start(p.current().unwrap_or(UNKNOWN)) {
            expression(p).ok();
        }

        p.expect(R_BRACKET);
        m_inner.complete(p, DIMENSION);
    }
    m.complete(p, DIMENSIONS);
}

/// Whether the tokens ahead form annotations directly followed by an array
/// dimension (`@Nullable []`) — used to distinguish dimension annotations
/// ([JLS §10.1], [§9.7.4]) from modifiers of the next construct.
fn at_annotated_dimension(p: &Parser) -> bool {
    if !p.at(AT) {
        return false;
    }
    let mut i = 0;
    while p.nth(i) == Some(AT) && p.nth(i + 1) != Some(INTERFACE_KW) {
        i += 1;
        if !matches!(p.nth(i), Some(IDENTIFIER)) {
            return false;
        }
        i += 1;
        while p.nth(i) == Some(DOT) && p.nth(i + 1) == Some(IDENTIFIER) {
            i += 2;
        }
        if p.nth(i) == Some(L_PAREN) {
            let mut depth = 1;
            i += 1;
            while depth > 0 {
                match p.nth(i) {
                    Some(L_PAREN) => depth += 1,
                    Some(R_PAREN) => depth -= 1,
                    Some(EOF) | None => return false,
                    _ => {}
                }
                i += 1;
            }
        }
    }
    p.nth(i) == Some(L_BRACKET)
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

pub fn expect_gt(p: &mut Parser) {
    match p.current() {
        Some(GREATER) => p.bump(),

        Some(RIGHT_SHIFT) => {
            // >>
            p.split_token(RIGHT_SHIFT, 1, GREATER);
        }

        Some(UNSIGNED_RIGHT_SHIFT) => {
            // >>>
            p.split_token(RIGHT_SHIFT, 1, RIGHT_SHIFT);
        }

        _ => p.error_expected(&[GREATER]),
    }
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

    expect_gt(p);

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

    // [JLS §9.7.4] annotations may decorate any type use, including the type
    // itself (`@Nullable String`) and every qualifier segment of a nested
    // type (`Connection.@Nullable Response`).
    type_annotations_opt(p);

    if !p.at(IDENTIFIER) {
        p.error_expected_construct(ExpectedConstruct::Type);
        m.complete(p, ERROR);
        return Err(());
    }

    qualified_name(p);
    type_arguments_opt(p);

    while p.eat(DOT) {
        type_annotations_opt(p);
        p.expect(IDENTIFIER);
        type_arguments_opt(p);
    }

    dimensions(p);

    m.complete(p, TYPE);
    Ok(())
}

/// The leading annotations of a type use ([JLS §9.7.4]); parsed into a
/// `MODIFIER_LIST` so downstream lowering treats them like other modifiers.
pub fn type_annotations_opt(p: &mut Parser) {
    if p.at(AT) && p.nth(1) != Some(INTERFACE_KW) {
        let m = p.start();
        while p.at(AT) && p.nth(1) != Some(INTERFACE_KW) {
            annotation(p);
        }
        m.complete(p, MODIFIER_LIST);
    }
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

    expect_gt(p);

    m.complete(p, TYPE_ARGUMENTS);
}

pub fn type_argument(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    // An array-of-primitive (`List<byte[]>`) is a reference type ([JLS §4.3])
    // and a legal type argument; a bare primitive is not, which the type
    // layer reports.
    let res = if p.at(QUESTION) {
        wildcard_type(p)
    } else if at_primitive_type(p) {
        let m_inner = p.start();
        p.bump();
        dimensions(p);
        m_inner.complete(p, TYPE);
        Ok(())
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
    if (p.at(EXTENDS_KW) || p.at(SUPER_KW)) && wildcard_bounds(p).is_err() {
        m.abandon(p);
        return Err(());
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
