use crate::{
    Parser, SyntaxKind,
    SyntaxKind::*,
    grammar::{
        annotations::annotation,
        eat_nl,
        names::simple_identifier,
        semis,
        statements::statements,
        types::{type_, type_arguments},
    },
    parser::ExpectedConstruct,
};

/// `expression`: disjunction
/// [spec: grammar-rule-expression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-expression
pub(crate) fn expression(p: &mut Parser) {
    disjunction(p);
}

/// Whether the current token can start an expression.
/// [spec: grammar-rule-primaryExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-primaryExpression
pub(crate) fn at_expression(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(
            L_PAREN
                | L_BRACKET
                | L_BRACE
                | COLON_COLON
                | IDENTIFIER
                | THIS_KW
                | SUPER_KW
                | TRUE_KW
                | FALSE_KW
                | NULL_KW
                | INTEGER_LITERAL
                | FLOAT_LITERAL
                | CHAR_LITERAL
                | OPEN_QUOTE
                | OPEN_RAW_QUOTE
                | PLUS
                | MINUS
                | NOT
                | PLUS_PLUS
                | MINUS_MINUS
                | IF_KW
                | WHEN_KW
                | TRY_KW
                | THROW_KW
                | RETURN_KW
                | CONTINUE_KW
                | BREAK_KW
                | OBJECT_KW
                | FUN_KW
        )
    )
}

/// `||`
/// [spec: grammar-rule-disjunction] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-disjunction
fn disjunction(p: &mut Parser) {
    let m = p.start();
    conjunction(p);
    let mut wrapped = false;
    while p.at(OR) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        conjunction(p);
    }
    if wrapped {
        m.complete(p, BINARY_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `&&`
/// [spec: grammar-rule-conjunction] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-conjunction
fn conjunction(p: &mut Parser) {
    let m = p.start();
    equality(p);
    let mut wrapped = false;
    while p.at(AND) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        equality(p);
    }
    if wrapped {
        m.complete(p, BINARY_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `==` `!=` `===` `!==`
/// [spec: grammar-rule-equality] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-equality
fn equality(p: &mut Parser) {
    let m = p.start();
    comparison(p);
    let mut wrapped = false;
    while matches!(p.current(), Some(EQUAL_EQUAL | NOT_EQUAL | SHEQ | SHNE)) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        comparison(p);
    }
    if wrapped {
        m.complete(p, BINARY_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `<` `>` `<=` `>=`
/// [spec: grammar-rule-comparison] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-comparison
fn comparison(p: &mut Parser) {
    let m = p.start();
    infix_operation(p);
    let mut wrapped = false;
    while matches!(
        p.current(),
        Some(LESS | GREATER | LESS_EQUAL | GREATER_EQUAL)
    ) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        infix_operation(p);
    }
    if wrapped {
        m.complete(p, BINARY_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `in` / `!in` (containment) and `is` / `!is` (type test).
/// [spec: grammar-rule-infixOperation] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-infixOperation
fn infix_operation(p: &mut Parser) {
    let m = p.start();
    elvis_expression(p);
    let mut kind = None;
    loop {
        if p.at(IN_KW) || p.at(NOT_IN) {
            kind.get_or_insert(IN_EXPRESSION);
            p.bump();
            eat_nl(p);
            elvis_expression(p);
        } else if p.at(IS_KW) || p.at(NOT_IS) {
            kind.get_or_insert(IS_EXPRESSION);
            p.bump();
            eat_nl(p);
            type_(p);
        } else {
            break;
        }
    }
    if let Some(kind) = kind {
        m.complete(p, kind);
    } else {
        m.abandon(p);
    }
}

/// `a ?: b`
/// [spec: grammar-rule-elvisExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-elvisExpression
fn elvis_expression(p: &mut Parser) {
    let m = p.start();
    infix_function_call(p);
    let mut wrapped = false;
    while p.at(ELVIS) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        infix_function_call(p);
    }
    if wrapped {
        m.complete(p, ELVIS_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `a foo b` — an infix (user-defined) function call.
/// [spec: grammar-rule-infixFunctionCall] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-infixFunctionCall
fn infix_function_call(p: &mut Parser) {
    let m = p.start();
    range_expression(p);
    let mut wrapped = false;
    while p.at(IDENTIFIER) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        range_expression(p);
    }
    if wrapped {
        m.complete(p, INFIX_FUNCTION_CALL);
    } else {
        m.abandon(p);
    }
}

/// `..` `..<`
/// [spec: grammar-rule-rangeExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-rangeExpression
fn range_expression(p: &mut Parser) {
    let m = p.start();
    additive_expression(p);
    let mut wrapped = false;
    while p.at(RANGE) || p.at(RANGE_UNTIL) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        additive_expression(p);
    }
    if wrapped {
        m.complete(p, RANGE_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `+` `-`
/// [spec: grammar-rule-additiveExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-additiveExpression
fn additive_expression(p: &mut Parser) {
    let m = p.start();
    multiplicative_expression(p);
    let mut wrapped = false;
    while p.at(PLUS) || p.at(MINUS) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        multiplicative_expression(p);
    }
    if wrapped {
        m.complete(p, BINARY_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `*` `/` `%`
/// [spec: grammar-rule-multiplicativeExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-multiplicativeExpression
fn multiplicative_expression(p: &mut Parser) {
    let m = p.start();
    as_expression(p);
    let mut wrapped = false;
    while p.at(STAR) || p.at(SLASH) || p.at(MODULO) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        as_expression(p);
    }
    if wrapped {
        m.complete(p, BINARY_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `x as T` / `x as? T`
/// [spec: grammar-rule-asExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-asExpression
fn as_expression(p: &mut Parser) {
    let m = p.start();
    prefix_unary_expression(p);
    let mut wrapped = false;
    while p.at(AS_KW) || p.at(AS_SAFE) {
        wrapped = true;
        p.bump();
        eat_nl(p);
        type_(p);
    }
    if wrapped {
        m.complete(p, AS_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `{unaryPrefix} postfixUnaryExpression`
/// [spec: grammar-rule-prefixUnaryExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-prefixUnaryExpression
fn prefix_unary_expression(p: &mut Parser) {
    let m = p.start();
    let mut wrapped = false;
    while at_prefix_unary_operator(p) {
        wrapped = true;
        p.bump();
        eat_nl(p);
    }
    postfix_unary_expression(p);
    if wrapped {
        m.complete(p, PREFIX_UNARY_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// `++` `--` `-` `+` `!`
/// [spec: grammar-rule-prefixUnaryOperator] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-prefixUnaryOperator
fn at_prefix_unary_operator(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(PLUS_PLUS | MINUS_MINUS | MINUS | PLUS | NOT)
    )
}

/// `primaryExpression {postfixUnarySuffix}`
/// [spec: grammar-rule-postfixUnaryExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-postfixUnaryExpression
fn postfix_unary_expression(p: &mut Parser) {
    let m = p.start();
    primary_expression(p);
    let mut wrapped = false;

    loop {
        if p.at(PLUS_PLUS) || p.at(MINUS_MINUS) || p.at(NOT_NULL_ASSERT) {
            // postfixUnaryOperator
            p.bump();
            wrapped = true;
        } else if p.at(L_PAREN) {
            // callSuffix: valueArguments
            let s = p.start();
            value_arguments(p);
            s.complete(p, CALL_EXPRESSION);
            wrapped = true;
        } else if p.at(L_BRACKET) {
            // indexingSuffix
            let s = p.start();
            indexing_suffix(p);
            s.complete(p, INDEXING_EXPRESSION);
            wrapped = true;
        } else if p.at(L_BRACE) {
            // callSuffix: annotatedLambda (trailing lambda)
            let s = p.start();
            lambda_literal(p);
            s.complete(p, CALL_EXPRESSION);
            wrapped = true;
        } else if p.at(LESS) && at_type_arguments(p) {
            // typeArguments
            let s = p.start();
            type_arguments(p);
            s.complete(p, TYPE_ARGUMENTS);
            wrapped = true;
        } else if matches!(p.current(), Some(DOT | SAFE_ACCESS | COLON_COLON))
            || (p.at(NEWLINE) && matches!(p.nth(1), Some(DOT | SAFE_ACCESS)))
        {
            // navigationSuffix (may continue across a line break)
            eat_nl(p);
            let s = p.start();
            p.bump();
            eat_nl(p);
            navigation_member(p);
            s.complete(p, NAVIGATION_SUFFIX);
            wrapped = true;
        } else {
            break;
        }
    }

    if wrapped {
        m.complete(p, POSTFIX_UNARY_EXPRESSION);
    } else {
        m.abandon(p);
    }
}

/// The member after `.`, `?.` or `::`: simpleIdentifier | 'class' |
/// parenthesizedExpression.
/// [spec: grammar-rule-navigationSuffix] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-navigationSuffix
fn navigation_member(p: &mut Parser) {
    if p.at(CLASS_KW) {
        p.bump();
    } else if p.at(L_PAREN) {
        parenthesized_expression(p);
    } else if p.at(IDENTIFIER) {
        simple_identifier(p);
    } else {
        p.error_message("expected an identifier after the navigation operator");
    }
}

/// `indexingSuffix`: '[' {NL} expression {{NL} ',' {NL} expression} [{NL} ','] {NL} ']'
/// [spec: grammar-rule-indexingSuffix] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-indexingSuffix
fn indexing_suffix(p: &mut Parser) {
    p.expect(L_BRACKET);
    eat_nl(p);
    expression(p);
    while p.eat(COMMA) {
        eat_nl(p);
        if p.at(R_BRACKET) {
            break;
        }
        expression(p);
    }
    eat_nl(p);
    p.expect(R_BRACKET);
}

/// `valueArguments`: '(' {NL} [valueArgument {{NL} ',' {NL} valueArgument}
///                  [{NL} ','] {NL}] ')'
/// [spec: grammar-rule-valueArguments] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-valueArguments
pub(crate) fn value_arguments(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    eat_nl(p);
    if !p.at(R_PAREN) {
        value_argument(p);
        while p.eat(COMMA) {
            eat_nl(p);
            if p.at(R_PAREN) {
                break; // trailing comma
            }
            value_argument(p);
        }
    }
    eat_nl(p);
    p.expect(R_PAREN);
    m.complete(p, VALUE_ARGUMENTS);
}

/// `valueArgument`: [annotation] {NL} [simpleIdentifier {NL} '=' {NL}] ['*']
///                  {NL} expression
/// [spec: grammar-rule-valueArgument] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-valueArgument
fn value_argument(p: &mut Parser) {
    let m = p.start();
    if p.at(AT) {
        annotation(p);
        eat_nl(p);
    }
    if p.at(IDENTIFIER) && p.nth(1) == Some(EQUAL) {
        simple_identifier(p);
        p.expect(EQUAL);
        eat_nl(p);
    }
    if p.at(STAR) {
        p.bump(); // spread
        eat_nl(p);
    }
    expression(p);
    m.complete(p, VALUE_ARGUMENT);
}

/// `parenthesizedExpression`: '(' {NL} expression {NL} ')'
/// [spec: grammar-rule-parenthesizedExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-parenthesizedExpression
fn parenthesized_expression(p: &mut Parser) {
    p.expect(L_PAREN);
    eat_nl(p);
    expression(p);
    eat_nl(p);
    p.expect(R_PAREN);
}

/// `primaryExpression` — the atomic cases.
/// [spec: grammar-rule-primaryExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-primaryExpression
fn primary_expression(p: &mut Parser) {
    let m = p.start();
    match p.current() {
        Some(L_PAREN) => {
            parenthesized_expression(p);
            m.complete(p, PARENTHESIZED_EXPRESSION);
        }
        Some(L_BRACKET) => {
            collection_literal(p);
            m.complete(p, COLLECTION_LITERAL);
        }
        Some(L_BRACE) => {
            lambda_literal(p);
            m.complete(p, LAMBDA_LITERAL);
        }
        Some(COLON_COLON) | Some(IDENTIFIER) if at_callable_reference(p) => {
            callable_reference(p);
            m.complete(p, CALLABLE_REFERENCE);
        }
        Some(IDENTIFIER) => {
            p.bump();
            m.complete(p, PRIMARY_EXPRESSION);
        }
        Some(THIS_KW) => {
            p.bump();
            m.complete(p, THIS_EXPRESSION);
        }
        Some(SUPER_KW) => {
            p.bump();
            m.complete(p, SUPER_EXPRESSION);
        }
        Some(TRUE_KW | FALSE_KW | NULL_KW | INTEGER_LITERAL | FLOAT_LITERAL | CHAR_LITERAL) => {
            p.bump();
            m.complete(p, PRIMARY_EXPRESSION);
        }
        Some(OPEN_QUOTE) | Some(OPEN_RAW_QUOTE) => {
            string_literal(p);
            m.complete(p, STRING_LITERAL);
        }
        _ => {
            p.error_expected_construct(ExpectedConstruct::Expression);
            m.complete(p, PRIMARY_EXPRESSION);
        }
    }
}

/// `collectionLiteral`: '[' {NL} [expression {{NL} ',' {NL} expression}
///                       [{NL} ','] {NL}] ']'
/// [spec: grammar-rule-collectionLiteral] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-collectionLiteral
fn collection_literal(p: &mut Parser) {
    p.expect(L_BRACKET);
    eat_nl(p);
    if !p.at(R_BRACKET) {
        expression(p);
        while p.eat(COMMA) {
            eat_nl(p);
            if p.at(R_BRACKET) {
                break;
            }
            expression(p);
        }
    }
    eat_nl(p);
    p.expect(R_BRACKET);
}

/// `callableReference`: [receiverType] '::' {NL} (simpleIdentifier | 'class')
/// [spec: grammar-rule-callableReference] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-callableReference
fn callable_reference(p: &mut Parser) {
    let m = p.start();
    if p.at(IDENTIFIER) {
        crate::parser::grammar::types::user_type(p);
    }
    p.expect(COLON_COLON);
    eat_nl(p);
    if p.at(CLASS_KW) {
        p.bump();
    } else {
        simple_identifier(p);
    }
    m.complete(p, CALLABLE_REFERENCE);
}

fn at_callable_reference(p: &Parser) -> bool {
    p.at(COLON_COLON) || (p.at(IDENTIFIER) && p.nth(1) == Some(COLON_COLON))
}

/// `lambdaLiteral`: '{' {NL} [[lambdaParameters] {NL} '->' {NL}] statements {NL} '}'
/// [spec: grammar-rule-lambdaLiteral] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-lambdaLiteral
pub(crate) fn lambda_literal(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);
    eat_nl(p);

    // A lambda may declare parameters before `->`; use a checkpoint since
    // `{ x }` is a single-expression body while `{ x -> x + 1 }` has a
    // parameter list.
    if at_lambda_header(p) {
        let cp = p.checkpoint();
        lambda_parameters(p);
        eat_nl(p);
        if p.at(ARROW) {
            p.bump();
            eat_nl(p);
        } else {
            p.rewind(cp);
        }
    }

    statements(p);
    semis(p);
    eat_nl(p);
    p.expect(R_BRACE);
    m.complete(p, LAMBDA_LITERAL);
}

/// A lambda header starts with an identifier (single param) or `(` (destructured).
fn at_lambda_header(p: &Parser) -> bool {
    p.at(IDENTIFIER) || p.at(L_PAREN)
}

/// `lambdaParameters`: lambdaParameter {{NL} ',' {NL} lambdaParameter} [{NL} ',']
/// `lambdaParameter`: variableDeclaration | (multiVariableDeclaration ':' type)
/// [spec: grammar-rule-lambdaParameters] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-lambdaParameters
fn lambda_parameters(p: &mut Parser) {
    let mp = p.start();
    lambda_parameter(p);
    while p.eat(COMMA) {
        eat_nl(p);
        if p.at(ARROW) {
            break;
        }
        lambda_parameter(p);
    }
    eat_nl(p);
    mp.complete(p, LAMBDA_PARAMETERS);
}

fn lambda_parameter(p: &mut Parser) {
    let m = p.start();
    if p.at(L_PAREN) {
        // multiVariableDeclaration (a, b)
        p.bump();
        eat_nl(p);
        crate::parser::grammar::decl::variable_declaration(p);
        while p.eat(COMMA) {
            eat_nl(p);
            crate::parser::grammar::decl::variable_declaration(p);
        }
        eat_nl(p);
        p.expect(R_PAREN);
        if p.at(COLON) {
            p.bump();
            eat_nl(p);
            type_(p);
        }
    } else {
        crate::parser::grammar::decl::variable_declaration(p);
    }
    m.complete(p, LAMBDA_PARAMETER);
}

/// `stringLiteral`: lineStringLiteral | multiLineStringLiteral
/// [spec: grammar-rule-stringLiteral] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-stringLiteral
///
/// The lexer emits string content in full mode (including the tokens of a
/// `${...}` interpolation), so the parser can simply walk tokens from the
/// opening to the closing quote. Template entries are wrapped in M5.
fn string_literal(p: &mut Parser) {
    if p.at(OPEN_RAW_QUOTE) {
        p.bump();
        while !p.at(CLOSE_RAW_QUOTE) && !p.is_at_end() {
            p.bump();
        }
        p.expect(CLOSE_RAW_QUOTE);
    } else {
        p.bump(); // OPEN_QUOTE
        while !p.at(CLOSE_QUOTE) && !p.is_at_end() {
            p.bump();
        }
        p.expect(CLOSE_QUOTE);
    }
}

/// Whether `<` starts a `typeArguments` list rather than a comparison.
/// Heuristic: scan to the matching `>`; if the token after it cannot
/// continue a comparison (identifier/literal), it is a type-argument list.
fn at_type_arguments(p: &Parser) -> bool {
    let mut depth = 0i32;
    let mut i = 0;
    loop {
        match p.nth(i) {
            Some(LESS) => depth += 1,
            Some(GREATER) => {
                depth -= 1;
                if depth == 0 {
                    return !matches!(
                        p.nth(i + 1),
                        Some(
                            IDENTIFIER
                                | INTEGER_LITERAL
                                | FLOAT_LITERAL
                                | CHAR_LITERAL
                                | OPEN_QUOTE
                                | OPEN_RAW_QUOTE
                                | TRUE_KW
                                | FALSE_KW
                                | NULL_KW
                                | THIS_KW
                                | SUPER_KW
                        )
                    );
                }
            }
            Some(EOF) | None => return false,
            _ => {}
        }
        i += 1;
    }
}

/// Ensures `SyntaxKind` is reachable for the `at_expression` signature.
#[allow(unused_imports)]
use SyntaxKind as _SyntaxKind;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::grammar::tests::parse_with;

    #[test]
    fn binary_precedence() {
        let out = parse_with(expression, "a + b * c + d");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn logical_operators() {
        let out = parse_with(expression, "a && b || c");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn equality_and_comparison() {
        let out = parse_with(expression, "a == b && c < d");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn elvis_and_elvis_chain() {
        let out = parse_with(expression, "a ?: b ?: c");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn range_until() {
        let out = parse_with(expression, "1..<10 step 2");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn is_and_in_operators() {
        let out = parse_with(expression, "x is String && y !in list");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn as_cast() {
        let out = parse_with(expression, "x as String");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn prefix_and_postfix() {
        let out = parse_with(expression, "!x && -y > 0");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn postfix_increment_and_not_null() {
        let out = parse_with(expression, "x++ + y!!");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn call_chain() {
        let out = parse_with(expression, "foo().bar(1, x = 2).baz");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn safe_call_and_nullable() {
        let out = parse_with(expression, "a?.b?.c");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn generic_call() {
        let out = parse_with(expression, "listOf<Int>(1, 2, 3)");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn generic_comparison_ambiguity() {
        let out = parse_with(expression, "a < b > c");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn indexing_suffix() {
        let out = parse_with(expression, "a[b][c]");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn callable_references() {
        let out = parse_with(expression, "String::class.java");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn parens_and_literals() {
        let out = parse_with(expression, "(a + b) * 2 == (4)");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn collection_literal() {
        let out = parse_with(expression, "[1, 2, 3]");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn this_and_super() {
        let out = parse_with(expression, "this.foo() + super.bar");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn lambda_basic() {
        let out = parse_with(expression, "{ x -> x + 1 }");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn trailing_lambda() {
        let out = parse_with(expression, "fold(0) { acc, x -> acc + x }");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn infix_function_call() {
        let out = parse_with(expression, "a to b + 1");
        insta::assert_snapshot!(out);
    }
}
