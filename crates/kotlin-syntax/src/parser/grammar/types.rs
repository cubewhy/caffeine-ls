use crate::{
    ContextualKeyword, Parser, SyntaxKind,
    SyntaxKind::*,
    grammar::{annotations::annotation, eat_nl, names::simple_identifier},
    parser::marker::Marker,
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

/// Whether the current token starts a `functionType`
/// ([spec: grammar-rule-functionType]): either a receiver followed by the
/// parameter list, or a parameter list followed by `->`. Pure lookahead, so a
/// parenthesized, nullable or user type is never mistaken for a function type.
fn at_function_type(p: &Parser) -> bool {
    if at_receiver_type(p) {
        return true;
    }
    // `functionTypeParameters {NL} '->' {NL} type` — the receiver-less form,
    // whose parameter list is the parenthesized type the `type` alternative
    // would otherwise claim.
    let start = skip_type_modifiers(p, 0);
    p.nth(start) == Some(L_PAREN) && scan_closing_paren_then(p, start, at_arrow)
}

/// Whether the current tokens are a `receiverType` followed by `{NL} '.'` —
/// the optional prefix of a `functionType` ([spec: grammar-rule-functionType]).
///
/// The three forms `receiverType` allows are all covered
/// ([spec: grammar-rule-receiverType]):
/// `[typeModifiers] (parenthesizedType | nullableType | typeReference)`.
/// `at_function_type` shares this, so the receiver's presence is decided
/// exactly once and the two callers cannot disagree.
pub(crate) fn at_receiver_type(p: &Parser) -> bool {
    let start = skip_type_modifiers(p, 0);
    if p.nth(start) == Some(L_PAREN) {
        // `(A).(B) -> C`: the parenthesized *receiver* (the parameter list of
        // a receiver-less function type is followed by `->` instead). A
        // nullable parenthesized receiver — `(A)?.(B) -> C` — writes its
        // quests between the parens and the dot.
        return scan_closing_paren_then(p, start, |p, after| {
            let mut i = skip_nl(p, after);
            while p.nth(i) == Some(QUESTION) {
                i = skip_nl(p, i + 1);
            }
            at_receiver_separator(p, i)
        });
    }
    scan_type_reference(p, start).is_some_and(|after| at_receiver_separator(p, after))
}

/// The index of the first token after any `typeModifiers`
/// ([spec: grammar-rule-typeModifiers]: `annotation | ('suspend' {NL})`).
fn skip_type_modifiers(p: &Parser, start: usize) -> usize {
    let mut i = start;
    loop {
        if p.nth(i) == Some(AT) {
            i = super::decl::skip_annotation(p, i);
        } else if p.nth_at_contextual_kw(i, ContextualKeyword::Suspend) {
            i = skip_nl(p, i + 1);
        } else {
            return i;
        }
    }
}

/// `p.nth(i)` skipping NEWLINE tokens.
fn nth_nl(p: &Parser, i: usize) -> Option<SyntaxKind> {
    p.nth(skip_nl(p, i))
}

/// The index of the first token at or after `i` that is not a newline.
fn skip_nl(p: &Parser, mut i: usize) -> usize {
    while p.nth(i) == Some(NEWLINE) {
        i += 1;
    }
    i
}

/// `{NL} '->'` — the arrow that follows a `functionType`'s parameter list.
fn at_arrow(p: &Parser, at: usize) -> bool {
    p.nth(skip_nl(p, at)) == Some(ARROW)
}

/// Scan a `typeReference` textually and return the index just past it, or
/// `None` when the tokens do not spell one.
///
/// `typeReference: userType | 'dynamic'` and
/// `userType: simpleUserType {{NL} '.' {NL} simpleUserType}` with
/// `simpleUserType: simpleIdentifier [{NL} typeArguments]`
/// ([spec: grammar-rule-typeReference]); `typeArguments`' angle brackets are
/// balanced, so a nested `List<Map<K, V>>` scans as one type. A trailing `?`
/// set is consumed too, since `nullableType` is one of the forms a receiver
/// may take ([spec: grammar-rule-nullableType]).
fn scan_type_reference(p: &Parser, start: usize) -> Option<usize> {
    let mut i = skip_nl(p, start);
    if p.nth_at_contextual_kw(i, ContextualKeyword::Dynamic) {
        i = skip_nl(p, i + 1);
    } else {
        loop {
            if p.nth(i) != Some(IDENTIFIER) {
                return None;
            }
            i = skip_nl(p, i + 1);
            if p.nth(i) == Some(LESS) {
                let after = super::decl::skip_balanced(p, i, LESS, GREATER);
                if after == i {
                    return None; // unbalanced `<`
                }
                i = skip_nl(p, after);
            }
            // A `.` continues the qualification only when a `simpleUserType`
            // follows it — otherwise it is the receiver's own separator.
            if p.nth(i) == Some(DOT) && nth_nl(p, i + 1) == Some(IDENTIFIER) {
                i = skip_nl(p, i + 1);
                continue;
            }
            break;
        }
    }
    while p.nth(skip_nl(p, i)) == Some(QUESTION) {
        i = skip_nl(p, i) + 1;
    }
    Some(i)
}

/// Scan forward from `start` until a balanced `)` is found, then test
/// `test(p, after)` with `after` the index just past that `)`.
fn scan_closing_paren_then(
    p: &Parser,
    start: usize,
    test: impl Fn(&Parser, usize) -> bool,
) -> bool {
    let mut depth = 0;
    let mut i = start;
    loop {
        match p.nth(i) {
            Some(L_PAREN) => depth += 1,
            Some(R_PAREN) => {
                depth -= 1;
                if depth == 0 {
                    return test(p, i + 1);
                }
            }
            Some(EOF) | None => return false,
            _ => {}
        }
        i += 1;
    }
}

/// Which token spelled the `{NL} '.'` that ends a `functionType`'s receiver
/// ([spec: grammar-rule-functionType]: `[receiverType {NL} '.' {NL}]`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiverSeparator {
    /// A `.` token of its own, after the receiver's type.
    Dot,
    /// A single `SAFE_ACCESS` token. The lexer writes an adjacent `?` and `.`
    /// as one token — KLS's `safeNav: QUEST_NO_WS '.'` ([spec:
    /// grammar-rule-safeNav]) lexed as a unit — so on a nullable receiver the
    /// token carries both the trailing quest *and* the separator, and no `.`
    /// is left to consume.
    SafeAccess,
}

/// `receiverType`: [typeModifiers] (parenthesizedType | nullableType | typeReference)
/// [spec: grammar-rule-receiverType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-receiverType
///
/// Consumes the separating `{NL} '.'` as well and reports which token spelled
/// it ([`ReceiverSeparator`]), so the caller knows whether a `.` still
/// follows.
///
/// The node holds the receiver's type *alone*: one inner type node, which is
/// what `lower_function_type` reads as the function's first parameter and as
/// the `this` of the function's body. A declaration's receiver
/// (`fun T.name()`) is a different `RECEIVER_TYPE` shape — see
/// `decl::parse_receiver_type` — because its node also has to spell out where
/// the declaration's name begins.
pub(crate) fn receiver_type(p: &mut Parser) -> ReceiverSeparator {
    let m = p.start();
    type_modifiers(p);
    eat_nl(p);

    let separator = if p.at(L_PAREN) {
        // `parenthesizedType`, and the `nullableType` that may wrap it:
        // `(A).(B) -> C` and `(A)?.(B) -> C`.
        let wrapper = p.start();
        let inner = p.start();
        parenthesized_type(p);
        inner.complete(p, PARENTHESIZED_TYPE);
        eat_nl(p);
        finish_receiver_type(p, wrapper)
    } else {
        // `typeReference`, and the `nullableType` that may wrap it.
        let wrapper = p.start();
        type_reference(p);
        eat_nl(p);
        finish_receiver_type(p, wrapper)
    };

    m.complete(p, RECEIVER_TYPE);
    separator
}

/// Ends a receiver's type: the `{NL} (quest {quest})` of a `nullableType`
/// ([spec: grammar-rule-nullableType]) and the separating `{NL} '.'`
/// ([spec: grammar-rule-functionType]).
///
/// `wrapper` is the marker opened before the inner type node, so the
/// `NULLABLE_TYPE` it completes — or abandons — spans that node.
fn finish_receiver_type(p: &mut Parser, wrapper: Marker) -> ReceiverSeparator {
    if p.at(QUESTION) {
        nullable_quests(p);
        eat_nl(p);
        wrapper.complete(p, NULLABLE_TYPE);
    } else if p.at(SAFE_ACCESS) {
        // The merged quest and dot ([`ReceiverSeparator::SafeAccess`]).
        p.bump();
        wrapper.complete(p, NULLABLE_TYPE);
        return ReceiverSeparator::SafeAccess;
    } else {
        wrapper.abandon(p);
    }
    p.expect(DOT);
    ReceiverSeparator::Dot
}

/// Whether the tokens at `at` are the separating `{NL} '.'` of a
/// `functionType` receiver followed by the parameter list's `(`
/// ([spec: grammar-rule-functionType]: `[receiverType {NL} '.' {NL}]`).
fn at_receiver_separator(p: &Parser, at: usize) -> bool {
    let i = skip_nl(p, at);
    match p.nth(i) {
        Some(DOT | SAFE_ACCESS) => p.nth(skip_nl(p, i + 1)) == Some(L_PAREN),
        _ => false,
    }
}

/// `functionType`: [receiverType {NL} '.' {NL}] functionTypeParameters
///                 {NL} '->' {NL} type
/// [spec: grammar-rule-functionType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionType
fn function_type(p: &mut Parser) {
    let m = p.start();

    if at_receiver_type(p) {
        receiver_type(p);
        eat_nl(p);
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
            // `[{NL} ',']` — the trailing comma the rule allows, as every other
            // list in this grammar parses it ([spec:
            // grammar-rule-typeArguments]).
            if p.at(R_PAREN) {
                break;
            }
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
