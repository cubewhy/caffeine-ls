use stacksafe::stacksafe;

use crate::{
    SyntaxKind,
    grammar::{
        decl::class_body,
        error_recover::{recover_parameter, recover_until},
        modifiers::annotation,
        stmt::switch_common,
        types::{dimensions, reference_type, type_},
    },
    kinds::SyntaxKind::*,
    parser::{
        ExpectedConstruct, Parser,
        marker::{CompletedMarker, Marker},
    },
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

/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-ArrayInitializer
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

fn get_infix_bp(kind: SyntaxKind) -> Option<(u8, u8)> {
    let bp = match kind {
        // assignment
        EQUAL | PLUS_EQUAL | MINUS_EQUAL | MULTIPLE_EQUAL | DIVIDE_EQUAL | MODULO_EQUAL
        | AND_EQUAL | OR_EQUAL | XOR_EQUAL => (2, 1),

        // Conditional Operator
        QUESTION => (3, 4),

        // Conditional-or
        OR => (5, 6),

        // Conditional-and
        AND => (7, 8),

        // Bitwise and Logical Operators
        BIT_OR => (9, 10),
        CARET => (11, 12),
        BIT_AND => (13, 14),

        // Equality Operators
        EQUAL_EQUAL | NOT_EQUAL => (15, 16),

        // compare and type checking
        LESS | LESS_EQUAL | GREATER | GREATER_EQUAL | INSTANCEOF_KW => (17, 18),

        PLUS | MINUS => (19, 20),
        STAR | SLASH | MODULO => (21, 22),

        PLUS_PLUS | MINUS_MINUS => (23, 24),

        // access and invocation
        DOT | L_PAREN | L_BRACKET | COLON_COLON => (25, 26),

        _ => return None,
    };

    Some(bp)
}

fn expr_prefix(p: &mut Parser) -> Result<CompletedMarker, ()> {
    let kind = p.current().ok_or(())?;

    match kind {
        NUMBER_LITERAL | STRING_LITERAL | IDENTIFIER | THIS_KW | SUPER_KW | TRUE_LITERAL
        | FALSE_LITERAL | NULL_LITERAL => {
            let m = p.start();
            p.bump();
            Ok(m.complete(p, LITERAL))
        }
        L_PAREN => cast_or_paren_expr(p),
        SWITCH_KW => Ok(switch_expression(p)),

        // JLS 15.15.1 & 15.15.2: PreIncrement & PreDecrement
        PLUS_PLUS | MINUS_MINUS => {
            let m = p.start();
            p.bump();
            if expr_bp(p, 25).is_err() {
                m.complete(p, ERROR);
                return Err(());
            }
            Ok(m.complete(p, PREFIX_EXPR))
        }

        // JLS 15.15.3 - 15.15.6: +, -, ~, !
        PLUS | MINUS | TILDE | NOT => {
            let m = p.start();
            p.bump();
            if expr_bp(p, 25).is_err() {
                m.complete(p, ERROR);
                return Err(());
            }
            Ok(m.complete(p, UNARY_EXPR))
        }

        NEW_KW => new_expression(p),
        _ => Err(()),
    }
}

/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28
fn switch_expression(p: &mut Parser) -> CompletedMarker {
    let m = switch_common(p);
    m.complete(p, SWITCH_EXPR)
}

/// UnqualifiedClassInstanceCreationExpression:
///   new [TypeArguments] ClassOrInterfaceTypeToInstantiate ( [ArgumentList] ) [ClassBody]
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-UnqualifiedClassInstanceCreationExpression
fn new_expression(p: &mut Parser) -> Result<CompletedMarker, ()> {
    let m = p.start();
    p.expect(NEW_KW);

    // type
    type_(p).ok();

    match p.current() {
        Some(L_PAREN) => {
            // object construction
            argument_list(p);

            if p.at(L_BRACE) {
                class_body(p);
            }
            Ok(m.complete(p, NEW_EXPR))
        }
        Some(L_BRACKET) => {
            // array construction
            array_creation_rest(p, m)
        }
        _ => {
            p.error_expected(&[L_PAREN, L_BRACKET]);
            m.complete(p, ERROR);
            Err(())
        }
    }
}

/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.10.1
fn array_creation_rest(p: &mut Parser, m: Marker) -> Result<CompletedMarker, ()> {
    dimensions(p);

    // array initializer
    if p.at(L_BRACE) {
        array_initializer(p);
    }

    Ok(m.complete(p, NEW_EXPR))
}

/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html
pub fn expression(p: &mut Parser) -> Result<CompletedMarker, ()> {
    expr_bp(p, 0)
}

#[stacksafe]
fn expr_bp(p: &mut Parser, min_bp: u8) -> Result<CompletedMarker, ()> {
    // Nud
    let mut left = match expr_prefix(p) {
        Ok(m) => m,
        Err(_) => return Err(()),
    };

    // Led
    while let Some(kind) = p.current() {
        if let Some((l_bp, r_bp)) = get_infix_bp(kind) {
            if l_bp < min_bp {
                break;
            }

            let m = left.precede(p);

            match kind {
                DOT => {
                    p.expect(DOT); // .
                    if p.at(CLASS_KW) {
                        // https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-ClassLiteral
                        p.bump();
                        left = m.complete(p, CLASS_LITERAL);
                    } else if p.at(IDENTIFIER) {
                        // https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-FieldAccess
                        p.bump();
                        left = m.complete(p, FIELD_ACCESS);
                    } else {
                        p.error_message("Expected identifier or 'class' after '.'");
                        left = m.complete(p, ERROR);
                    }
                }
                L_PAREN => {
                    // method invocation
                    argument_list(p);
                    left = m.complete(p, METHOD_CALL);
                }
                L_BRACKET => {
                    // array access
                    p.expect(L_BRACKET); // [
                    // expr inside []
                    if expression(p).is_err() {
                        m.complete(p, ERROR);
                        return Err(());
                    }
                    p.expect(R_BRACKET); // ]
                    left = m.complete(p, ARRAY_ACCESS);
                }
                PLUS_PLUS | MINUS_MINUS => {
                    p.bump();
                    left = m.complete(p, POSTFIX_EXPR);
                }

                _ => {
                    p.bump();
                    if expr_bp(p, r_bp).is_err() {
                        m.complete(p, ERROR);
                        return Err(());
                    }
                    left = m.complete(p, BINARY_EXPR);
                }
            }
            continue;
        }
        break;
    }

    Ok(left)
}

fn cast_or_paren_expr(p: &mut Parser) -> Result<CompletedMarker, ()> {
    let m = p.start();
    p.expect(L_PAREN);

    if is_type_cast_lookahead(p) {
        // CastExpression:
        //  ( PrimitiveType ) UnaryExpression
        //  ( ReferenceType {AdditionalBound} ) UnaryExpressionNotPlusMinus
        //  ( ReferenceType {AdditionalBound} ) LambdaExpression
        //
        // https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-CastExpression
        type_(p).ok();
        if p.at(BIT_AND) {
            additional_bounds(p);
        }

        p.expect(R_PAREN);

        if expr_bp(p, 25).is_err() {
            m.complete(p, ERROR);
            return Err(());
        }
        Ok(m.complete(p, CAST_EXPR))
    } else {
        if expression(p).is_err() {
            m.complete(p, ERROR);
            return Err(());
        }
        p.expect(R_PAREN);
        Ok(m.complete(p, PAREN_EXPR))
    }
}

fn additional_bounds(p: &mut Parser) {
    while p.eat(BIT_AND) {
        if reference_type(p).is_err() {
            p.error_message("Expected reference type after '&' in cast");
            break;
        }
    }
}

fn is_type_cast_lookahead(p: &mut Parser) -> bool {
    let ckpt = p.checkpoint();

    // PrimitiveType | ReferenceType
    let first_type_ok = type_(p).is_ok();

    // {AdditionalBound}
    if first_type_ok && p.at(BIT_AND) {
        while p.eat(BIT_AND) {
            if reference_type(p).is_err() {
                break;
            }
        }
    }

    let has_r_paren = p.at(R_PAREN);
    let is_cast = if has_r_paren {
        p.bump(); // )
        is_expression_start(p.current().unwrap_or(UNKNOWN))
    } else {
        false
    };

    p.rewind(ckpt);
    is_cast
}

pub fn is_expression_start(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        IDENTIFIER
            | NUMBER_LITERAL
            | STRING_LITERAL
            | TRUE_LITERAL
            | FALSE_LITERAL
            | NULL_LITERAL
            | THIS_KW
            | SUPER_KW
            | NEW_KW
            | SWITCH_KW
            | L_PAREN
            | L_BRACE
            | NOT         // !
            | TILDE       // ~
            | PLUS        // +
            | MINUS       // -
            | PLUS_PLUS   // ++
            | MINUS_MINUS // --
            | AT // Annotation
    )
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
    if type_(p).is_err() {
        p.rewind(ckpt);
        return false;
    };

    let Some(next_token) = p.current() else {
        p.rewind(ckpt);
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
        expression(p).ok();
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
        reference_type(p).ok();
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
