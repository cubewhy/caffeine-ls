use crate::{
    ContextualKeyword, Parser, SyntaxKind,
    SyntaxKind::*,
    grammar::{annotations::annotation, eat_nl, names::simple_identifier},
};

/// `type`: [typeModifiers] (functionType | parenthesizedType | nullableType
///         | typeReference | definitelyNonNullableType)
/// [spec: grammar-rule-type] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-type
pub(crate) fn type_(p: &mut Parser) {
    let m = p.start();
    type_modifiers(p);
    eat_nl(p);

    if at_function_type(p) {
        function_type(p);
    } else if p.at(L_PAREN) && p.nth(1) != Some(R_PAREN) {
        let inner = p.start();
        parenthesized_type(p);
        eat_nl(p);
        if p.at(QUESTION) {
            nullable_quests(p);
            inner.complete(p, PARENTHESIZED_TYPE);
            m.complete(p, NULLABLE_TYPE);
        } else if p.at(BIT_AND) {
            definitely_non_nullable_rest(p);
            inner.complete(p, PARENTHESIZED_TYPE);
            m.complete(p, DEFINITELY_NON_NULLABLE_TYPE);
        } else {
            inner.complete(p, PARENTHESIZED_TYPE);
            m.complete(p, TYPE);
        }
        return;
    } else if p.at(IDENTIFIER) {
        let head = p.start();
        type_reference(p);
        eat_nl(p);
        head.abandon(p);
        if p.at(QUESTION) {
            nullable_quests(p);
            m.complete(p, NULLABLE_TYPE);
        } else if p.at(BIT_AND) {
            definitely_non_nullable_rest(p);
            m.complete(p, DEFINITELY_NON_NULLABLE_TYPE);
        } else {
            m.complete(p, TYPE);
        }
        return;
    } else {
        p.error_message("expected a type");
        m.complete(p, TYPE);
        return;
    }

    m.complete(p, TYPE);
}

fn at_type_modifier(p: &Parser) -> bool {
    p.at_contextual_kw(ContextualKeyword::Suspend) || p.at(AT)
}

/// `typeModifiers`: typeModifier {typeModifier}
/// `typeModifier`: annotation | ('suspend' {NL})
/// [spec: grammar-rule-typeModifiers] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeModifiers
fn type_modifiers(p: &mut Parser) {
    while at_type_modifier(p) {
        if p.at(AT) {
            annotation(p);
        } else {
            p.bump(); // suspend
        }
        eat_nl(p);
    }
}

/// `typeReference`: userType | 'dynamic'
/// [spec: grammar-rule-typeReference] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeReference
fn type_reference(p: &mut Parser) {
    if p.at_contextual_kw(ContextualKeyword::Dynamic) {
        p.bump();
        return;
    }
    user_type(p);
}

/// `userType`: simpleUserType {{NL} '.' {NL} simpleUserType}
/// `simpleUserType`: simpleIdentifier [{NL} typeArguments]
/// [spec: grammar-rule-userType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-userType
pub(crate) fn user_type(p: &mut Parser) {
    let m = p.start();
    simple_identifier(p);
    eat_nl(p);
    if p.at(LESS) {
        type_arguments(p);
        eat_nl(p);
    }
    while p.at(DOT) && p.nth(1) == Some(IDENTIFIER) {
        p.bump();
        eat_nl(p);
        simple_identifier(p);
        eat_nl(p);
        if p.at(LESS) {
            type_arguments(p);
            eat_nl(p);
        }
    }
    m.complete(p, USER_TYPE);
}

/// `typeArguments`: '<' {NL} typeProjection {{NL} ',' {NL} typeProjection}
///                  [{NL} ','] {NL} '>'
/// [spec: grammar-rule-typeArguments] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeArguments
pub(crate) fn type_arguments(p: &mut Parser) {
    let m = p.start();
    p.expect(LESS);
    eat_nl(p);
    if !p.at(GREATER) {
        type_projection(p);
        while p.eat(COMMA) {
            eat_nl(p);
            if p.at(GREATER) {
                break; // trailing comma
            }
            type_projection(p);
        }
    }
    eat_nl(p);
    p.expect(GREATER);
    m.complete(p, TYPE_ARGUMENTS);
}

/// `typeProjection`: ([typeProjectionModifiers] type) | '*'
/// [spec: grammar-rule-typeProjection] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeProjection
fn type_projection(p: &mut Parser) {
    let m = p.start();
    if p.at(STAR) {
        p.bump();
    } else {
        type_projection_modifiers(p);
        eat_nl(p);
        type_(p);
    }
    m.complete(p, TYPE_PROJECTION);
}

/// `typeProjectionModifiers`: typeProjectionModifier {typeProjectionModifier}
/// `typeProjectionModifier`: (varianceModifier {NL}) | annotation
/// [spec: grammar-rule-typeProjectionModifiers] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeProjectionModifiers
fn type_projection_modifiers(p: &mut Parser) {
    loop {
        if p.at(IN_KW) || p.at_contextual_kw(ContextualKeyword::Out) {
            p.bump();
            eat_nl(p);
        } else if p.at(AT) {
            annotation(p);
        } else {
            break;
        }
    }
}

/// Whether the current token starts a `functionType` (with or without a
/// receiver). Uses pure lookahead so a parenthesized / nullable / user type
/// is never mistaken for a function type.
fn at_function_type(p: &Parser) -> bool {
    // `( … )` followed by `->`
    if p.at(L_PAREN) {
        return scan_closing_paren_then(p, 0, |after, after2| {
            after == Some(ARROW) || (after == Some(NEWLINE) && after2 == Some(ARROW))
        });
    }

    // receiver form `T. ( … ) -> R` or `T.( … ) -> R`
    if p.at(IDENTIFIER) {
        // walk `simpleIdentifier` (possibly qualified), then require `. (`.
        let mut i = 0;
        loop {
            match nth_nl(p, i) {
                Some(IDENTIFIER) => {
                    if matches!(nth_nl(p, i + 1), Some(DOT))
                        && matches!(nth_nl(p, i + 2), Some(L_PAREN))
                    {
                        return scan_closing_paren_then(p, i + 2, |after, after2| {
                            after == Some(ARROW)
                                || (after == Some(NEWLINE) && after2 == Some(ARROW))
                        });
                    }
                    i += 1;
                }
                Some(DOT) => i += 1,
                _ => return false,
            }
        }
    }

    false
}

/// `p.nth(i)` skipping NEWLINE tokens.
fn nth_nl(p: &Parser, i: usize) -> Option<SyntaxKind> {
    let mut j = i;
    while p.nth(j) == Some(NEWLINE) {
        j += 1;
    }
    p.nth(j)
}

/// Scan forward from `start` skipping newlines until a balanced `)` is found,
/// then test `fn(after, after2)` against the following tokens.
fn scan_closing_paren_then(
    p: &Parser,
    start: usize,
    test: impl Fn(Option<SyntaxKind>, Option<SyntaxKind>) -> bool,
) -> bool {
    let mut depth = 0;
    let mut i = start;
    loop {
        match p.nth(i) {
            Some(L_PAREN) => depth += 1,
            Some(R_PAREN) => {
                depth -= 1;
                if depth == 0 {
                    return test(p.nth(i + 1), p.nth(i + 2));
                }
            }
            Some(EOF) | None => return false,
            _ => {}
        }
        i += 1;
    }
}

/// `functionType`: [receiverType {NL} '.' {NL}] functionTypeParameters
///                 {NL} '->' {NL} type
/// [spec: grammar-rule-functionType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionType
fn function_type(p: &mut Parser) {
    let m = p.start();

    // Optional receiver: an identifier list followed by `.` and `(`.
    if p.at(IDENTIFIER)
        && matches!(nth_nl(p, 1), Some(DOT))
        && matches!(nth_nl(p, 2), Some(L_PAREN))
    {
        let r = p.start();
        type_reference(p);
        eat_nl(p);
        p.expect(DOT);
        eat_nl(p);
        r.complete(p, RECEIVER_TYPE);
    }

    function_type_parameters(p);
    eat_nl(p);
    p.expect(ARROW);
    eat_nl(p);
    type_(p);

    m.complete(p, FUNCTION_TYPE);
}

/// `functionTypeParameters`: '(' [parameter | type] {',' (parameter | type)}
///                            [','] ')'
/// [spec: grammar-rule-functionTypeParameters] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionTypeParameters
fn function_type_parameters(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    eat_nl(p);

    if !p.at(R_PAREN) {
        loop {
            if p.at(IDENTIFIER) && p.nth(1) == Some(COLON) {
                // parameter: simpleIdentifier ':' type
                let pm = p.start();
                simple_identifier(p);
                p.expect(COLON);
                eat_nl(p);
                type_(p);
                pm.complete(p, VALUE_PARAMETER);
            } else {
                type_(p);
            }
            eat_nl(p);
            if !p.eat(COMMA) {
                break;
            }
            eat_nl(p);
        }
    }

    eat_nl(p);
    p.expect(R_PAREN);
    m.complete(p, VALUE_PARAMETERS);
}

/// `parenthesizedType`: '(' {NL} type {NL} ')'
/// [spec: grammar-rule-parenthesizedType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-parenthesizedType
fn parenthesized_type(p: &mut Parser) {
    p.expect(L_PAREN);
    eat_nl(p);
    type_(p);
    eat_nl(p);
    p.expect(R_PAREN);
}

/// `quest`: QUEST_NO_WS | QUEST_WS — repeated `?` are permitted by the
/// nullableType rule (syntactically), so consume any number.
fn nullable_quests(p: &mut Parser) {
    while p.at(QUESTION) {
        p.bump();
    }
}

/// `definitelyNonNullableType` continuation after (userType|parenthesizedType):
/// {NL} '&' {NL} [typeModifiers] (userType | parenthesizedUserType)
/// [spec: grammar-rule-definitelyNonNullableType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-definitelyNonNullableType
fn definitely_non_nullable_rest(p: &mut Parser) {
    p.expect(BIT_AND);
    eat_nl(p);
    type_modifiers(p);
    eat_nl(p);
    if p.at(L_PAREN) {
        parenthesized_type(p);
    } else {
        user_type(p);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::parser::grammar::tests::parse_with;

    #[test]
    fn simple_and_qualified_user_type() {
        let out = parse_with(type_, "String");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn nullable_type() {
        let out = parse_with(type_, "String?");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn generic_type_arguments() {
        let out = parse_with(type_, "Map<String, out List<Int>>");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn star_projection() {
        let out = parse_with(type_, "List<*>");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn function_type() {
        let out = parse_with(type_, "(Int, String) -> Boolean");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn function_type_with_receiver() {
        let out = parse_with(type_, "String.(Int) -> Boolean");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn function_type_with_named_parameter() {
        let out = parse_with(type_, "(x: Int, y: Int) -> Unit");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn parenthesized_type() {
        let out = parse_with(type_, "(Int)");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn definitely_non_nullable_type() {
        let out = parse_with(type_, "T & Any");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn suspend_function_type() {
        let out = parse_with(type_, indoc! {"suspend () -> Unit"});
        insta::assert_snapshot!(out);
    }
}
