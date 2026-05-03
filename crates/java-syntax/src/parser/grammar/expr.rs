use stacksafe::stacksafe;

use crate::{
    SyntaxKind,
    grammar::{
        decl::class_body,
        error_recover::{recover_parameter, recover_until},
        modifiers::annotation,
        names::qualified_name,
        stmt::{block, switch_common},
        types::{
            at_primitive_type, dimensions, formal_parameters, inferred_parameters,
            is_formal_parameters, reference_type, type_, type_arguments, type_arguments_opt,
        },
    },
    parser::{
        ExpectedConstruct, Parser,
        marker::{CompletedMarker, Marker},
    },
    syntax_kind::SyntaxKind::*,
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

/// ArrayInitializer:
///   { [VariableInitializerList] [,] }
///
/// VariableInitializerList:
///   VariableInitializer {, VariableInitializer}
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-ArrayInitializer
#[stacksafe]
pub fn array_initializer(p: &mut Parser) -> CompletedMarker {
    let m = p.start();

    p.expect(L_BRACE); // {

    if !p.at(R_BRACE) {
        expression(p).ok();

        while p.eat(COMMA) {
            if p.at(R_BRACE) {
                break; // trailing comma
            }
            expression(p).ok();
        }
    }

    p.expect(R_BRACE);

    m.complete(p, ARRAY_INITIALIZER)
}

fn get_infix_bp(kind: SyntaxKind) -> Option<(u8, u8)> {
    let bp = match kind {
        // assignment
        EQUAL
        | PLUS_EQUAL
        | MINUS_EQUAL
        | MULTIPLE_EQUAL
        | DIVIDE_EQUAL
        | MODULO_EQUAL
        | AND_EQUAL
        | OR_EQUAL
        | XOR_EQUAL
        | LEFT_SHIFT_EQUAL
        | RIGHT_SHIFT_EQUAL
        | UNSIGNED_RIGHT_SHIFT_EQUAL => (2, 1),

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
        LEFT_SHIFT | RIGHT_SHIFT | UNSIGNED_RIGHT_SHIFT => (19, 20),

        PLUS | MINUS => (21, 22),
        STAR | SLASH | MODULO => (23, 24),

        PLUS_PLUS | MINUS_MINUS => (25, 26),

        // access and invocation
        DOT | L_PAREN | L_BRACKET | COLON_COLON => (27, 28),

        _ => return None,
    };

    Some(bp)
}

fn expr_prefix(p: &mut Parser) -> Result<CompletedMarker, ()> {
    let kind = p.current().ok_or(())?;

    match kind {
        NUMBER_LITERAL | STRING_LITERAL | THIS_KW | SUPER_KW | TRUE_LITERAL | FALSE_LITERAL
        | CHAR_LITERAL | NULL_LITERAL => {
            let m = p.start();
            p.bump();
            Ok(m.complete(p, LITERAL))
        }

        STRING_TEMPLATE_BEGIN | TEXT_BLOCK_TEMPLATE_BEGIN => {
            let m = p.start();

            p.error_message("String template is missing a processor (e.g., 'STR.')");

            template_argument(p);

            Ok(m.complete(p, TEMPLATE_EXPR))
        }

        IDENTIFIER | UNDERSCORE => {
            if is_lambda_lookahead(p) {
                lambda_expression(p)
            } else {
                let m = p.start();
                p.bump();
                Ok(m.complete(p, LITERAL))
            }
        }

        BOOLEAN_KW | BYTE_KW | SHORT_KW | INT_KW | LONG_KW | CHAR_KW | FLOAT_KW | DOUBLE_KW
        | VOID_KW => {
            let m = p.start();
            p.bump();
            Ok(m.complete(p, PRIMITIVE_TYPE_EXPR))
        }

        L_PAREN => {
            if is_lambda_lookahead(p) {
                lambda_expression(p)
            } else {
                cast_or_paren_expr(p)
            }
        }
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

        L_BRACE => Ok(array_initializer(p)),

        NEW_KW => new_expression(p),
        _ => Err(()),
    }
}

/// TemplateArgument:
///   StringTemplate
///   TextBlockTemplate
///
/// StringTemplate:
///   STRING_TEMPLATE_BEGIN Expression { STRING_TEMPLATE_MID Expression } STRING_TEMPLATE_END
///
/// NOTE: String templates exists in Java 22 (as a preview feature) but were removed in Java 23
///
/// https://docs.oracle.com/javase/specs/jls/se22/preview/specs/string-templates-jls.html
pub fn template_argument(p: &mut Parser) {
    let m = p.start();

    // STRING_TEMPLATE_BEGIN | TEXT_BLOCK_TEMPLATE_BEGIN
    p.bump();

    loop {
        // \{ expr }
        if expression(p).is_err() {
            p.error_message("Expected expression inside string template");
            recover_until(p, &[STRING_TEMPLATE_MID, STRING_TEMPLATE_END]);
        }

        match p.current() {
            Some(STRING_TEMPLATE_MID) | Some(TEXT_BLOCK_TEMPLATE_MID) => {
                p.bump();
            }
            Some(STRING_TEMPLATE_END) | Some(TEXT_BLOCK_TEMPLATE_END) => {
                p.bump();
                break;
            }
            _ => {
                p.error_message("Expected string template MID or END");
                break;
            }
        }
    }

    m.complete(p, TEMPLATE_ARGUMENT);
}

/// MethodReference:
///   ExpressionName :: [TypeArguments] Identifier
///   Primary :: [TypeArguments] Identifier
///   ReferenceType :: [TypeArguments] Identifier
///   super :: [TypeArguments] Identifier
///   TypeName . super :: [TypeArguments] Identifier
///   ClassType :: [TypeArguments] new
///   ArrayType :: new
///
/// Note: this function only parses tokens after `::`
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13
fn method_reference_rest(p: &mut Parser, m: Marker) -> CompletedMarker {
    p.expect(COLON_COLON);

    if p.at(LESS) {
        type_arguments(p);
    }

    if p.at(IDENTIFIER) || p.at(NEW_KW) {
        p.bump();
    } else {
        p.error_message("Expected identifier or 'new' after '::'");
    }

    m.complete(p, METHOD_REFERENCE)
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
    if at_primitive_type(p) {
        p.bump();
    } else {
        qualified_name(p);
    }

    // optional type argument
    type_arguments_opt(p);

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
                EQUAL
                | PLUS_EQUAL
                | MINUS_EQUAL
                | MULTIPLE_EQUAL
                | DIVIDE_EQUAL
                | MODULO_EQUAL
                | AND_EQUAL
                | OR_EQUAL
                | XOR_EQUAL
                | LEFT_SHIFT_EQUAL
                | RIGHT_SHIFT_EQUAL
                | UNSIGNED_RIGHT_SHIFT_EQUAL => {
                    p.bump(); // operator

                    if expr_bp(p, r_bp).is_err() {
                        m.complete(p, ERROR);
                        return Err(());
                    }

                    left = m.complete(p, ASSIGN_EXPR);
                }
                COLON_COLON => {
                    // https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13
                    left = method_reference_rest(p, m);
                }
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
                    } else if p.at(SUPER_KW) {
                        // Qualified Super: TypeName.super
                        // https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.2
                        p.bump();
                        left = m.complete(p, SUPER_EXPR);
                    } else if p.at(STRING_TEMPLATE_BEGIN) || p.at(TEXT_BLOCK_TEMPLATE_BEGIN) {
                        template_argument(p);
                        left = m.complete(p, TEMPLATE_EXPR);
                    } else if p.at(STRING_LITERAL) {
                        p.bump();
                        left = m.complete(p, TEMPLATE_EXPR);
                    } else {
                        p.error_message(
                            "Expected identifier or 'class' after '.', or template after '.'",
                        );
                        left = m.complete(p, ERROR);
                    }
                }
                L_PAREN => {
                    // method invocation
                    argument_list(p);
                    left = m.complete(p, METHOD_CALL);
                }
                L_BRACKET => {
                    p.expect(L_BRACKET); // [
                    if p.at(R_BRACKET) {
                        p.expect(R_BRACKET);
                        while p.eat(L_BRACKET) {
                            p.expect(R_BRACKET);
                        }
                        left = m.complete(p, TYPE);
                    } else {
                        // array access
                        // expr inside []
                        if expression(p).is_err() {
                            m.complete(p, ERROR);
                            return Err(());
                        }
                        p.expect(R_BRACKET); // ]
                        left = m.complete(p, ARRAY_ACCESS);
                    }
                }
                PLUS_PLUS | MINUS_MINUS => {
                    p.bump();
                    left = m.complete(p, POSTFIX_EXPR);
                }
                QUESTION => {
                    // https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25
                    p.bump(); // ?

                    if expression(p).is_err() {
                        m.complete(p, ERROR);
                        return Err(());
                    }

                    // :
                    if !p.expect(COLON) {
                        m.complete(p, ERROR);
                        return Err(());
                    }

                    if expr_bp(p, r_bp).is_err() {
                        m.complete(p, ERROR);
                        return Err(());
                    }

                    left = m.complete(p, COND_EXPR);
                }

                INSTANCEOF_KW => {
                    // https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.20.2
                    p.bump(); // instanceof

                    if is_pattern(p) {
                        pattern(p);
                    } else {
                        if type_(p).is_err() {
                            p.error_message("Expected type or pattern after 'instanceof'");
                        }
                    }

                    left = m.complete(p, INSTANCEOF_EXPR);
                }

                LESS if is_method_ref_lookahead(p) => {
                    // type argument in method ref
                    type_arguments(p);
                    left = m.complete(p, TYPE);
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

/// LambdaExpression:
///   LambdaParameters -> LambdaBody
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27
fn lambda_expression(p: &mut Parser) -> Result<CompletedMarker, ()> {
    let m = p.start();

    // LambdaParameters:
    //   ( [LambdaParameterList] )
    //   ConciseLambdaParameter
    //
    // LambdaParameterList:
    //   NormalLambdaParameter {, NormalLambdaParameter}
    //   ConciseLambdaParameter {, ConciseLambdaParameter}
    //
    // NormalLambdaParameter:
    //   {VariableModifier} LambdaParameterType VariableDeclaratorId
    //   VariableArityParameter
    //
    // LambdaParameterType:
    //   UnannType
    //   var
    //
    // ConciseLambdaParameter:
    //   Identifier
    //   _
    if (p.at(IDENTIFIER) || p.at(UNDERSCORE)) && p.nth(1) == Some(ARROW) {
        // ConciseLambdaParameter
        p.bump();
    } else if p.at(L_PAREN) {
        if is_formal_parameters(p) {
            // NormalLambdaParameter
            formal_parameters(p);
        } else {
            // ConciseLambdaParameter
            inferred_parameters(p);
        }
    }

    p.expect(ARROW);

    // LambdaBody: Expression | Block
    if p.at(L_BRACE) {
        block(p);
    } else {
        expression(p).ok();
    }

    Ok(m.complete(p, LAMBDA_EXPR))
}

fn is_lambda_lookahead(p: &Parser) -> bool {
    // x -> ...
    if (p.at(IDENTIFIER) || p.at(UNDERSCORE)) && p.nth(1) == Some(ARROW) {
        return true;
    }

    // (...) -> ...
    if p.at(L_PAREN) {
        let mut i = 1;
        let mut depth = 1;
        while depth > 0 {
            match p.nth(i) {
                Some(L_PAREN) => depth += 1,
                Some(R_PAREN) => depth -= 1,
                None => return false,
                _ => {}
            }
            i += 1;
        }
        return p.nth(i) == Some(ARROW);
    }

    false
}

fn is_method_ref_lookahead(p: &Parser) -> bool {
    let mut i = 0;

    if p.at(LESS) {
        let mut depth = 0;
        loop {
            match p.nth(i) {
                Some(LESS) => depth += 1,
                Some(GREATER) => depth -= 1,
                Some(RIGHT_SHIFT) => depth -= 2,
                Some(UNSIGNED_RIGHT_SHIFT) => depth -= 3,
                Some(IDENTIFIER) | Some(DOT) | Some(COMMA) | Some(QUESTION) | Some(EXTENDS_KW)
                | Some(SUPER_KW) | Some(AT) | Some(L_BRACKET) | Some(R_BRACKET) => {}
                _ => return false,
            }
            i += 1;
            if depth == 0 {
                break;
            }
            if depth < 0 {
                return false;
            }
        }
    }

    while p.nth(i) == Some(L_BRACKET) {
        if p.nth(i + 1) == Some(R_BRACKET) {
            i += 2;
        } else {
            break;
        }
    }

    matches!(p.nth(i), Some(COLON_COLON))
        || (p.nth(i) == Some(DOT) && p.nth(i + 1) == Some(CLASS_KW))
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

    let is_primitive = at_primitive_type(p);

    // PrimitiveType | ReferenceType
    let first_type_ok = type_(p).is_ok();

    // {AdditionalBound}
    let mut has_additional_bounds = false;
    if first_type_ok && p.at(BIT_AND) {
        has_additional_bounds = true;
        while p.eat(BIT_AND) {
            if reference_type(p).is_err() {
                break;
            }
        }
    }

    let has_r_paren = p.at(R_PAREN);
    let is_cast = if has_r_paren {
        p.bump(); // )
        let next_token = p.current().unwrap_or(UNKNOWN);

        if is_primitive && !has_additional_bounds {
            is_expression_start(next_token)
        } else {
            is_expression_start(next_token)
                && !matches!(next_token, PLUS | MINUS | PLUS_PLUS | MINUS_MINUS)
        }
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
            | CHAR_LITERAL
            | TRUE_LITERAL
            | FALSE_LITERAL
            | NULL_LITERAL
            | THIS_KW
            | SUPER_KW
            | NEW_KW
            | SWITCH_KW
            | L_PAREN // (
            | L_BRACE // {
            | NOT         // !
            | TILDE       // ~
            | PLUS        // +
            | MINUS       // -
            | PLUS_PLUS   // ++
            | MINUS_MINUS // --
            | AT // Annotation
            | STRING_TEMPLATE_BEGIN
            | TEXT_BLOCK_TEMPLATE_BEGIN
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
