use crate::{
    ContextualKeyword, Parser, SyntaxKind,
    SyntaxKind::*,
    grammar::{
        annotations::annotation,
        eat_nl,
        expr::{expression, value_arguments},
        modifiers::{at_modifier, modifiers},
        names::simple_identifier,
        semis,
        types::{type_, type_arguments},
    },
    parser::ExpectedConstruct,
    tokenset,
};

/// `variableDeclaration`: {annotation} {NL} simpleIdentifier [{NL} ':' {NL} type]
/// [spec: grammar-rule-variableDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-variableDeclaration
///
/// A bare `_` is accepted in place of the name. The `Identifier` lexical rule
/// is `(Letter | '_') {Letter | '_' | UnicodeDigit}`, and `_` is the
/// documented placeholder for unused components of a destructuring declaration
/// — `val (_, status) = …`, `for ((_, value) in …)`, `{ (_, value) -> … }`.
pub(crate) fn variable_declaration(p: &mut Parser) {
    let m = p.start();
    if p.at(AT) {
        annotation(p);
        eat_nl(p);
    }
    if p.at(UNDERSCORE) {
        p.bump();
    } else {
        simple_identifier(p);
    }
    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        type_(p);
    }
    m.complete(p, VARIABLE_DECLARATION);
}

/// `multiVariableDeclaration`: '(' {NL} variableDeclaration {{NL} ',' {NL}
///                              variableDeclaration} [{NL} ','] {NL} ')'
/// [spec: grammar-rule-multiVariableDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-multiVariableDeclaration
pub(crate) fn multi_variable_declaration(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    eat_nl(p);
    variable_declaration(p);
    while p.eat(COMMA) {
        eat_nl(p);
        if p.at(R_PAREN) {
            break;
        }
        variable_declaration(p);
    }
    eat_nl(p);
    p.expect(R_PAREN);
    m.complete(p, MULTI_VARIABLE_DECLARATION);
}

/// `declaration`: classDeclaration | objectDeclaration | functionDeclaration
///                | propertyDeclaration | typeAlias
/// [spec: grammar-rule-declaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-declaration
pub(crate) fn declaration(p: &mut Parser) {
    eat_nl(p);
    match declaration_keyword(p) {
        Some(CLASS_KW) => class_declaration(p),
        Some(INTERFACE_KW) => class_declaration(p),
        Some(FUN_KW) if at_fun_interface(p) => class_declaration(p),
        Some(FUN_KW) => function_declaration(p),
        Some(OBJECT_KW) => object_declaration(p),
        Some(VAL_KW) | Some(VAR_KW) => property_declaration(p),
        Some(TYPEALIAS_KW) => type_alias(p),
        _ => {
            p.error_expected_construct(ExpectedConstruct::Declaration);
            p.bump();
        }
    }
}

/// Whether the current stream is `['fun'] 'interface'` (`fun interface X`).
fn at_fun_interface(p: &Parser) -> bool {
    let mut i = 0;
    loop {
        if p.nth(i) == Some(AT) {
            i = skip_annotation(p, i);
        } else if p.nth(i) == Some(NEWLINE)
            || matches!(p.nth(i), Some(IN_KW))
            || nth_is_modifier(p, i)
        {
            i += 1;
        } else {
            break;
        }
    }
    if p.nth(i) != Some(FUN_KW) {
        return false;
    }
    i += 1;
    while p.nth(i) == Some(NEWLINE) {
        i += 1;
    }
    p.nth(i) == Some(INTERFACE_KW)
}

/// The keyword after any leading modifiers/annotations, or `None`.
/// [spec: grammar-rule-declaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-declaration
fn declaration_keyword(p: &Parser) -> Option<SyntaxKind> {
    let mut i = 0;
    loop {
        if i > 256 {
            return None;
        }
        if p.nth(i) == Some(AT) {
            i = skip_annotation(p, i);
            continue;
        }
        if p.nth(i) == Some(NEWLINE) {
            i += 1;
            continue;
        }
        if matches!(p.nth(i), Some(IN_KW)) || nth_is_modifier(p, i) {
            i += 1;
            continue;
        }
        break;
    }
    p.nth(i)
}

/// Whether the current token stream starts a declaration (after optional
/// modifiers and annotations), without consuming anything.
pub(crate) fn at_declaration(p: &Parser) -> bool {
    matches!(
        declaration_keyword(p),
        Some(CLASS_KW | INTERFACE_KW | OBJECT_KW | FUN_KW | VAL_KW | VAR_KW | TYPEALIAS_KW)
    )
}

/// Whether a `primaryConstructor` follows the class name: either the
/// `classParameters` directly, or `[[modifiers] 'constructor']` before them.
/// [spec: grammar-rule-primaryConstructor] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-primaryConstructor
fn at_primary_constructor(p: &Parser) -> bool {
    let mut i = 0;
    loop {
        if p.nth(i) == Some(AT) {
            i = skip_annotation(p, i);
        } else if nth_is_modifier(p, i) {
            i += 1;
        } else {
            break;
        }
    }
    matches!(p.nth(i), Some(L_PAREN))
        || (p.nth(i) == Some(IDENTIFIER) && p.nth_lexeme(i) == Some("constructor"))
}

/// Tries to parse a `receiverType {NL} '.'` for an extension function or
/// property, leaving the cursor on the separating `.` on success; on failure
/// everything is rewound and `false` is returned.
///
/// [spec: grammar-rule-receiverType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-receiverType
///
/// The declaration name is the `simpleIdentifier` right after that trailing
/// `.` ([spec: grammar-rule-functionDeclaration] / [spec: grammar-rule-propertyDeclaration]),
/// so the receiver parse must never "eat" the final `.`/name pair: e.g.
/// `String.toURI` splits into receiver `String` + name `toURI`, not into a
/// qualified type `String.toURI`.
fn parse_receiver_type(p: &mut Parser) -> bool {
    if !p.at(IDENTIFIER) {
        return false;
    }
    let cp = p.checkpoint();
    let r = p.start();

    // First segment: simpleUserType (simpleIdentifier [typeArguments] ['?']).
    simple_identifier(p);
    eat_nl(p);
    receiver_segment_tail(p);

    loop {
        if p.at(DOT) && p.nth(1) == Some(IDENTIFIER) {
            if segment_continues_after_type_args(p) {
                // More segments follow, so this `.simpleIdentifier` is part of
                // the (possibly qualified) receiver type, not the name.
                p.bump();
                eat_nl(p);
                simple_identifier(p);
                eat_nl(p);
                receiver_segment_tail(p);
            } else {
                break; // the split dot; the declaration name follows it
            }
        } else {
            break;
        }
    }

    if p.at(DOT) {
        r.complete(p, RECEIVER_TYPE);
        true
    } else {
        r.abandon(p);
        p.rewind(cp);
        false
    }
}

/// At a `.` marker: is the following segment itself followed by another `.`?
/// The declaration name is always the *last* identifier of the sequence, so a
/// segment continues the receiver (e.g. `Map.Entry<K, V>.`) iff another `.`
/// follows it — independently of whether the name is a function (`name(`),
/// a property (`name: T`), a delegated property (`name by …`) or an
/// expression-bodied one (`name = …`).
fn segment_continues_after_type_args(p: &Parser) -> bool {
    // tokens: . IDENTIFIER [ <…> [<…>…] ] ['?'] … and then a DOT
    if p.nth(1) != Some(IDENTIFIER) {
        return false;
    }
    let mut i = 2;
    // each segment may be a `simpleUserType` with balanced `typeArguments`
    if p.nth(i) == Some(LESS) {
        let after = skip_balanced(p, i, LESS, GREATER);
        if after == i {
            return false; // unbalanced `<`
        }
        i = after;
    }
    while p.nth(i) == Some(QUESTION) {
        i += 1;
    }
    p.nth(i) == Some(DOT)
}

/// Consume the optional `typeArguments {NL}` and trailing `?` of a receiver
/// segment, as spelled by `simpleUserType` / `nullableType`.
fn receiver_segment_tail(p: &mut Parser) {
    if p.at(LESS) {
        type_arguments(p);
        eat_nl(p);
    }
    while p.at(QUESTION) {
        p.bump();
        eat_nl(p);
    }
}

fn nth_is_modifier(p: &Parser, i: usize) -> bool {
    p.nth(i) == Some(IDENTIFIER)
        && matches!(
            p.nth_lexeme(i),
            Some(
                "abstract"
                    | "actual"
                    | "annotation"
                    | "const"
                    | "crossinline"
                    | "data"
                    | "enum"
                    | "expect"
                    | "external"
                    | "final"
                    | "infix"
                    | "inline"
                    | "inner"
                    | "internal"
                    | "lateinit"
                    | "noinline"
                    | "open"
                    | "operator"
                    | "out"
                    | "override"
                    | "private"
                    | "protected"
                    | "public"
                    | "reified"
                    | "sealed"
                    | "suspend"
                    | "tailrec"
                    | "vararg"
                    | "value"
            )
        )
}

/// Skips a balanced `@Name`, `@Name(...)`, `@Name[...]` or `@target:Name`
/// annotation starting at index `i` (pointing at `@`); returns the index
/// after it.
fn skip_annotation(p: &Parser, mut i: usize) -> usize {
    i += 1; // @
    if p.nth(i) == Some(IDENTIFIER) {
        i += 1;
        // annotationUseSiteTarget `get:` before the type name
        if p.nth(i) == Some(COLON) {
            i += 1;
            if p.nth(i) == Some(IDENTIFIER) {
                i += 1;
            }
        }
    }
    match p.nth(i) {
        Some(L_PAREN) => skip_balanced(p, i, L_PAREN, R_PAREN),
        Some(L_BRACKET) => skip_balanced(p, i, L_BRACKET, R_BRACKET),
        _ => i,
    }
}

fn skip_balanced(p: &Parser, mut i: usize, open: SyntaxKind, close: SyntaxKind) -> usize {
    let mut depth = 0;
    loop {
        match p.nth(i) {
            Some(k) if k == open => depth += 1,
            Some(k) if k == close => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            Some(EOF) | None => return i,
            _ => {}
        }
        i += 1;
    }
}

/// `classDeclaration`: [modifiers] ('class' | (['fun'] 'interface')) {NL}
///                     simpleIdentifier [{NL} typeParameters] [{NL}
///                     primaryConstructor] [{NL} ':' {NL}
///                     delegationSpecifiers] [{NL} typeConstraints] [classBody |
///                     enumClassBody]
/// [spec: grammar-rule-classDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-classDeclaration
fn class_declaration(p: &mut Parser) {
    let m = p.start();

    let mut is_enum = modifiers_and_flag(p, |p| p.at_contextual_kw(ContextualKeyword::Enum));
    eat_nl(p);

    // 'fun interface' | 'interface' | 'class'
    if p.at(FUN_KW) {
        p.bump();
        eat_nl(p);
    }
    if p.at(INTERFACE_KW) {
        is_enum = false;
        p.bump();
    } else {
        p.expect(CLASS_KW);
    }
    eat_nl(p);

    simple_identifier(p);
    eat_nl(p);

    if p.at(LESS) {
        type_parameters(p);
        eat_nl(p);
    }

    if at_primary_constructor(p) {
        primary_constructor(p);
        eat_nl(p);
    }

    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        delegation_specifiers(p);
        eat_nl(p);
    }

    if p.at_contextual_kw(ContextualKeyword::Where) {
        type_constraints(p);
        eat_nl(p);
    }

    if p.at(L_BRACE) {
        if is_enum {
            enum_class_body(p);
        } else {
            class_body(p);
        }
    }

    m.complete(p, CLASS_DECL);
}

/// `primaryConstructor`: [[modifiers] 'constructor' {NL}] classParameters
/// [spec: grammar-rule-primaryConstructor] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-primaryConstructor
fn primary_constructor(p: &mut Parser) {
    let m = p.start();
    if p.at_contextual_kw(ContextualKeyword::Constructor) {
        p.bump();
        eat_nl(p);
    } else if p.at(AT) || at_modifier(p) {
        let was_empty = !(p.at(AT) || at_modifier(p));
        if !was_empty {
            modifiers(p);
            eat_nl(p);
        }
        p.expect_contextual_kw(ContextualKeyword::Constructor);
        eat_nl(p);
    }
    class_parameters(p);
    m.complete(p, PRIMARY_CONSTRUCTOR);
}

/// `classParameters`: '(' {NL} [classParameter {{NL} ',' {NL} classParameter}
///                    [{NL} ',']] {NL} ')'
/// [spec: grammar-rule-classParameters] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-classParameters
fn class_parameters(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    eat_nl(p);
    if !p.at(R_PAREN) {
        class_parameter(p);
        while p.eat(COMMA) {
            eat_nl(p);
            if p.at(R_PAREN) {
                break;
            }
            class_parameter(p);
        }
    }
    eat_nl(p);
    p.expect(R_PAREN);
    m.complete(p, CLASS_PARAMETERS);
}

/// `classParameter`: [modifiers] ['val' | 'var'] {NL} simpleIdentifier ':' type
///                   ['=' expression]
/// [spec: grammar-rule-classParameter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-classParameter
fn class_parameter(p: &mut Parser) {
    let m = p.start();
    modifiers_and_flag(p, |_| false);
    eat_nl(p);
    if p.at(VAL_KW) || p.at(VAR_KW) {
        p.bump();
        eat_nl(p);
    }
    simple_identifier(p);
    p.expect(COLON);
    eat_nl(p);
    type_(p);
    eat_nl(p);
    if p.eat(EQUAL) {
        eat_nl(p);
        expression(p);
    }
    m.complete(p, CLASS_PARAMETER);
}

/// `delegationSpecifiers`: annotatedDelegationSpecifier {{NL} ',' {NL}
///                         annotatedDelegationSpecifier}
/// [spec: grammar-rule-delegationSpecifiers] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-delegationSpecifiers
fn delegation_specifiers(p: &mut Parser) {
    let m = p.start();
    delegation_specifier(p);
    while p.eat(COMMA) {
        eat_nl(p);
        delegation_specifier(p);
    }
    m.complete(p, DELEGATION_SPECIFIERS);
}

/// `delegationSpecifier`: constructorInvocation | explicitDelegation |
///                        userType | functionType
/// [spec: grammar-rule-delegationSpecifier] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-delegationSpecifier
fn delegation_specifier(p: &mut Parser) {
    let m = p.start();
    if p.at(AT) {
        annotation(p);
        eat_nl(p);
    }
    // A function type (rare) or a user type head.
    let cp = p.checkpoint();
    let head = p.start();
    type_(p);
    eat_nl(p);
    if p.at(L_PAREN) {
        // constructorInvocation
        head.complete(p, CONSTRUCTOR_INVOCATION);
        value_arguments(p);
    } else if p.at_contextual_kw(ContextualKeyword::By) {
        // explicitDelegation: type 'by' expression
        p.bump();
        eat_nl(p);
        expression(p);
        head.complete(p, EXPLICIT_DELEGATION);
    } else {
        head.abandon(p);
        let _ = cp;
    }
    m.complete(p, DELEGATION_SPECIFIER);
}

/// `typeParameters`: '<' {NL} typeParameter {{NL} ',' {NL} typeParameter}
///                   [{NL} ','] {NL} '>'
/// [spec: grammar-rule-typeParameters] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeParameters
pub(crate) fn type_parameters(p: &mut Parser) {
    let m = p.start();
    p.expect(LESS);
    eat_nl(p);
    if !p.at(GREATER) {
        type_parameter(p);
        while p.eat(COMMA) {
            eat_nl(p);
            if p.at(GREATER) {
                break;
            }
            type_parameter(p);
        }
    }
    eat_nl(p);
    p.expect(GREATER);
    m.complete(p, TYPE_PARAMETERS);
}

/// `typeParameter`: [typeParameterModifiers] simpleIdentifier [':' type]
/// [spec: grammar-rule-typeParameter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeParameter
fn type_parameter(p: &mut Parser) {
    let m = p.start();
    // reified / in / out / annotations
    while p.at_contextual_kw(ContextualKeyword::Reified)
        || p.at(IN_KW)
        || p.at_contextual_kw(ContextualKeyword::Out)
        || p.at(AT)
    {
        if p.at(AT) {
            annotation(p);
        } else {
            p.bump();
        }
        eat_nl(p);
    }
    simple_identifier(p);
    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        type_(p);
    }
    m.complete(p, TYPE_PARAMETER);
}

/// `typeConstraints`: 'where' typeConstraint {{NL} ',' {NL} typeConstraint}
/// [spec: grammar-rule-typeConstraints] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeConstraints
pub(crate) fn type_constraints(p: &mut Parser) {
    let m = p.start();
    p.expect_contextual_kw(ContextualKeyword::Where);
    eat_nl(p);
    type_constraint(p);
    while p.eat(COMMA) {
        eat_nl(p);
        type_constraint(p);
    }
    m.complete(p, TYPE_CONSTRAINTS);
}

/// `typeConstraint`: {annotation} simpleIdentifier ':' type
/// [spec: grammar-rule-typeConstraint] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeConstraint
fn type_constraint(p: &mut Parser) {
    let m = p.start();
    if p.at(AT) {
        annotation(p);
        eat_nl(p);
    }
    simple_identifier(p);
    p.expect(COLON);
    eat_nl(p);
    type_(p);
    m.complete(p, TYPE_CONSTRAINT);
}

/// `functionDeclaration`: [modifiers] 'fun' [typeParameters] [receiverType '.']
///                        simpleIdentifier functionValueParameters [':' type]
///                        [typeConstraints] [functionBody]
/// [spec: grammar-rule-functionDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionDeclaration
fn function_declaration(p: &mut Parser) {
    let m = p.start();
    modifiers(p);
    eat_nl(p);
    p.expect(FUN_KW);
    eat_nl(p);

    if p.at(LESS) {
        type_parameters(p);
        eat_nl(p);
    }

    // receiverType '.'
    if parse_receiver_type(p) {
        p.expect(DOT);
        eat_nl(p);
    }

    simple_identifier(p);
    eat_nl(p);
    function_value_parameters(p);
    eat_nl(p);

    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        type_(p);
        eat_nl(p);
    }

    if p.at_contextual_kw(ContextualKeyword::Where) {
        type_constraints(p);
        eat_nl(p);
    }

    function_body(p);

    m.complete(p, FUNCTION_DECL);
}

/// `functionValueParameters`: '(' {NL} [functionValueParameter {{NL} ',' {NL}
///                             functionValueParameter} [{NL} ',']] {NL} ')'
/// [spec: grammar-rule-functionValueParameters] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionValueParameters
pub(crate) fn function_value_parameters(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    eat_nl(p);
    if !p.at(R_PAREN) {
        loop {
            eat_nl(p);
            if p.at(R_PAREN) {
                break;
            }
            function_value_parameter(p);
            if !p.eat(COMMA) {
                break;
            }
        }
    }
    eat_nl(p);
    p.expect(R_PAREN);
    m.complete(p, VALUE_PARAMETERS);
}

/// `functionValueParameter`: [parameterModifiers] parameter ['=' expression]
/// `parameter`: simpleIdentifier ':' type
/// [spec: grammar-rule-functionValueParameter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionValueParameter
fn function_value_parameter(p: &mut Parser) {
    let m = p.start();
    // parameterModifiers: annotations + vararg / noinline / crossinline
    loop {
        if p.at(AT) {
            annotation(p);
            eat_nl(p);
        } else if p.at_contextual_kw_set(tokenset![
            ContextualKeyword::Vararg,
            ContextualKeyword::NoInline,
            ContextualKeyword::CrossInline
        ]) {
            p.bump();
            eat_nl(p);
        } else {
            break;
        }
    }
    simple_identifier(p);
    p.expect(COLON);
    eat_nl(p);
    type_(p);
    eat_nl(p);
    if p.eat(EQUAL) {
        eat_nl(p);
        expression(p);
    }
    m.complete(p, VALUE_PARAMETER);
}

/// `parametersWithOptionalType`: '(' {NL} [functionValueParameterWithOptionalType
///                               {{NL} ',' {NL} functionValueParameterWithOptionalType}
///                               [{NL} ',']] {NL} ')'
/// [spec: grammar-rule-parametersWithOptionalType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-parametersWithOptionalType
pub(crate) fn parameters_with_optional_type(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    eat_nl(p);
    if !p.at(R_PAREN) {
        loop {
            eat_nl(p);
            if p.at(R_PAREN) {
                break;
            }
            function_value_parameter_with_optional_type(p);
            if !p.eat(COMMA) {
                break;
            }
        }
    }
    eat_nl(p);
    p.expect(R_PAREN);
    m.complete(p, VALUE_PARAMETERS);
}

/// `functionValueParameterWithOptionalType`: [parameterModifiers]
///   parameterWithOptionalType ['=' expression]
/// `parameterWithOptionalType`: simpleIdentifier [':' type]
/// [spec: grammar-rule-functionValueParameterWithOptionalType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionValueParameterWithOptionalType
fn function_value_parameter_with_optional_type(p: &mut Parser) {
    let m = p.start();
    // parameterModifiers: annotations + vararg / noinline / crossinline
    loop {
        if p.at(AT) {
            annotation(p);
            eat_nl(p);
        } else if p.at_contextual_kw_set(tokenset![
            ContextualKeyword::Vararg,
            ContextualKeyword::NoInline,
            ContextualKeyword::CrossInline
        ]) {
            p.bump();
            eat_nl(p);
        } else {
            break;
        }
    }
    simple_identifier(p);
    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        type_(p);
    }
    eat_nl(p);
    if p.eat(EQUAL) {
        eat_nl(p);
        expression(p);
    }
    m.complete(p, VALUE_PARAMETER);
}

/// `functionBody`: block | ('=' {NL} expression)
/// [spec: grammar-rule-functionBody] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionBody
pub(crate) fn function_body(p: &mut Parser) {
    if p.at(L_BRACE) {
        crate::parser::grammar::statements::block(p);
    } else if p.at(EQUAL) {
        p.bump();
        eat_nl(p);
        expression(p);
    }
}

/// `propertyDeclaration`: [modifiers] ('val' | 'var') [typeParameters]
///                        [receiverType '.'] (variableDeclaration |
///                        multiVariableDeclaration) [typeConstraints]
///                        [('=' expression) | propertyDelegate] [';']
///                        [accessors]
/// [spec: grammar-rule-propertyDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-propertyDeclaration
fn property_declaration(p: &mut Parser) {
    let m = p.start();
    modifiers(p);
    eat_nl(p);
    if p.at(VAL_KW) {
        p.bump();
    } else {
        p.expect(VAR_KW);
    }
    eat_nl(p);

    if p.at(LESS) {
        type_parameters(p);
        eat_nl(p);
    }

    // receiverType '.'
    if parse_receiver_type(p) {
        p.expect(DOT);
        eat_nl(p);
    }

    if p.at(L_PAREN) {
        multi_variable_declaration(p);
    } else {
        variable_declaration(p);
    }
    eat_nl(p);

    if p.at_contextual_kw(ContextualKeyword::Where) {
        type_constraints(p);
        eat_nl(p);
    }

    if p.eat(EQUAL) {
        eat_nl(p);
        expression(p);
        eat_nl(p);
    } else if p.at_contextual_kw(ContextualKeyword::By) {
        let d = p.start();
        p.bump();
        eat_nl(p);
        expression(p);
        d.complete(p, PROPERTY_DELEGATE);
        eat_nl(p);
    }

    if p.at(SEMICOLON) {
        p.bump();
    }

    // accessors: entered only when `get`/`set` (possibly preceded by
    // modifiers) actually follows, so a later property member is never
    // mis-consumed ([spec: grammar-rule-propertyDeclaration]).
    loop {
        match accessor_kind(p) {
            Some(ContextualKeyword::Get) => {
                getter(p);
                eat_nl(p);
            }
            Some(ContextualKeyword::Set) => {
                setter(p);
                eat_nl(p);
            }
            _ => break,
        }
    }

    m.complete(p, PROPERTY_DECL);
}

/// The accessor keyword starting the remaining accessors of a property,
/// looking past `[modifiers]` ([spec: grammar-rule-getter] /
/// [spec: grammar-rule-setter]), so `private set` on its own line after the
/// initializer is a property accessor rather than a stray declaration.
///
/// The decision is shape-aware so that a fresh statement that merely *begins*
/// with the soft keywords `get`/`set` is not mis-consumed: a getter's
/// parameter list is empty (`get(url).run()` stays a call expression) while a
/// setter's is a single optional parameter.
fn accessor_kind(p: &Parser) -> Option<ContextualKeyword> {
    let mut i = 0;
    while nth_is_modifier(p, i) {
        i += 1;
    }
    let kind = match p.nth_lexeme(i) {
        Some("get") => ContextualKeyword::Get,
        Some("set") => ContextualKeyword::Set,
        _ => return None,
    };

    let j = i + 1;
    if p.nth(j) == Some(L_PAREN) {
        // `get`/`set` `('(' {NL} ')')` — an empty parameter list.
        let mut k = j + 1;
        while p.nth(k) == Some(NEWLINE) {
            k += 1;
        }
        if p.nth(k) == Some(R_PAREN) {
            return Some(kind);
        }
        // `set(value: Int)` holds exactly one parameter and is therefore a
        // setter; a getter with a non-empty parameter list is not an accessor.
        return (kind == ContextualKeyword::Set).then_some(kind);
    }

    match p.nth(j) {
        None | Some(COLON | EQUAL | L_BRACE | NEWLINE | SEMICOLON) => Some(kind),
        _ => None,
    }
}

/// `getter`: [modifiers] 'get' ['(' {NL} ')' [':' type]] functionBody
/// [spec: grammar-rule-getter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-getter
fn getter(p: &mut Parser) {
    let m = p.start();
    modifiers(p);
    p.expect_contextual_kw(ContextualKeyword::Get);
    if p.at(L_PAREN) {
        p.bump();
        eat_nl(p);
        p.expect(R_PAREN);
        eat_nl(p);
    }
    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        type_(p);
        eat_nl(p);
    }
    function_body(p);
    m.complete(p, GETTER);
}

/// `setter`: [modifiers] 'set' ['(' functionValueParameterWithOptionalType
///            [{NL} ','] ')' [':' type]] functionBody
/// [spec: grammar-rule-setter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-setter
fn setter(p: &mut Parser) {
    let m = p.start();
    modifiers(p);
    p.expect_contextual_kw(ContextualKeyword::Set);
    if p.at(L_PAREN) {
        p.bump();
        eat_nl(p);
        if !p.at(R_PAREN) {
            function_value_parameter_with_optional_type(p);
            eat_nl(p);
            if p.eat(COMMA) {
                eat_nl(p);
            }
        }
        eat_nl(p);
        p.expect(R_PAREN);
        eat_nl(p);
    }
    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        type_(p);
        eat_nl(p);
    }
    function_body(p);
    m.complete(p, SETTER);
}

/// `objectDeclaration`: [modifiers] 'object' simpleIdentifier [':' {NL}
///                      delegationSpecifiers] [classBody]
/// [spec: grammar-rule-objectDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-objectDeclaration
fn object_declaration(p: &mut Parser) {
    let m = p.start();
    modifiers(p);
    eat_nl(p);
    p.expect(OBJECT_KW);
    eat_nl(p);
    simple_identifier(p);
    eat_nl(p);

    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        delegation_specifiers(p);
        eat_nl(p);
    }

    if p.at(L_BRACE) {
        class_body(p);
    }

    m.complete(p, OBJECT_DECL);
}

/// `typeAlias`: [modifiers] 'typealias' simpleIdentifier [typeParameters]
///              {NL} '=' {NL} type
/// [spec: grammar-rule-typeAlias] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeAlias
fn type_alias(p: &mut Parser) {
    let m = p.start();
    modifiers(p);
    eat_nl(p);
    p.expect(TYPEALIAS_KW);
    eat_nl(p);
    simple_identifier(p);
    eat_nl(p);
    if p.at(LESS) {
        type_parameters(p);
        eat_nl(p);
    }
    p.expect(EQUAL);
    eat_nl(p);
    type_(p);
    m.complete(p, TYPE_ALIAS);
}

/// `classBody`: '{' {NL} classMemberDeclarations {NL} '}'
/// [spec: grammar-rule-classBody] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-classBody
fn class_body(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);
    eat_nl(p);
    while !p.at(R_BRACE) && !p.is_at_end() {
        if p.at(SEMICOLON) || p.at(NEWLINE) {
            semis(p);
            eat_nl(p);
            continue;
        }
        class_member_declaration(p);
        semis(p);
        eat_nl(p);
    }
    semis(p);
    eat_nl(p);
    p.expect(R_BRACE);
    m.complete(p, CLASS_BODY);
}

/// `classMemberDeclaration`: declaration | companionObject |
///                           anonymousInitializer | secondaryConstructor
/// [spec: grammar-rule-classMemberDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-classMemberDeclaration
fn class_member_declaration(p: &mut Parser) {
    eat_nl(p);

    if p.at_contextual_kw(ContextualKeyword::Companion) {
        companion_object(p);
        return;
    }
    if p.at_contextual_kw(ContextualKeyword::Init) {
        anonymous_initializer(p);
        return;
    }
    if p.at_contextual_kw(ContextualKeyword::Constructor) {
        secondary_constructor(p);
        return;
    }

    declaration(p);
}

/// `companionObject`: [modifiers] 'companion' ['data'] 'object' [name]
///                    [':' delegationSpecifiers] [classBody]
/// [spec: grammar-rule-companionObject] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-companionObject
fn companion_object(p: &mut Parser) {
    let m = p.start();
    modifiers(p);
    eat_nl(p);
    p.expect_contextual_kw(ContextualKeyword::Companion);
    eat_nl(p);
    if p.at_contextual_kw(ContextualKeyword::Data) {
        p.bump();
        eat_nl(p);
    }
    p.expect(OBJECT_KW);
    eat_nl(p);
    if p.at(IDENTIFIER) {
        simple_identifier(p);
        eat_nl(p);
    }
    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        delegation_specifiers(p);
        eat_nl(p);
    }
    if p.at(L_BRACE) {
        class_body(p);
    }
    m.complete(p, COMPANION_OBJECT);
}

/// `anonymousInitializer`: 'init' {NL} block
/// [spec: grammar-rule-anonymousInitializer] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-anonymousInitializer
fn anonymous_initializer(p: &mut Parser) {
    let m = p.start();
    p.expect_contextual_kw(ContextualKeyword::Init);
    eat_nl(p);
    crate::parser::grammar::statements::block(p);
    m.complete(p, ANONYMOUS_INITIALIZER);
}

/// `secondaryConstructor`: [modifiers] 'constructor' functionValueParameters
///                         [':' constructorDelegationCall] [block]
/// [spec: grammar-rule-secondaryConstructor] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-secondaryConstructor
fn secondary_constructor(p: &mut Parser) {
    let m = p.start();
    modifiers(p);
    eat_nl(p);
    p.expect_contextual_kw(ContextualKeyword::Constructor);
    eat_nl(p);
    function_value_parameters(p);
    eat_nl(p);

    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        // constructorDelegationCall: 'this' | 'super' valueArguments
        let dm = p.start();
        if p.at(THIS_KW) || p.at(SUPER_KW) {
            p.bump();
            eat_nl(p);
            value_arguments(p);
        }
        dm.complete(p, CONSTRUCTOR_DELEGATION_CALL);
        eat_nl(p);
    }

    if p.at(L_BRACE) {
        crate::parser::grammar::statements::block(p);
    }

    m.complete(p, SECONDARY_CONSTRUCTOR);
}

/// `enumClassBody`: '{' [enumEntries] [';' classMemberDeclarations] '}'
/// [spec: grammar-rule-enumClassBody] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-enumClassBody
fn enum_class_body(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);
    eat_nl(p);
    if !p.at(R_BRACE) && !p.at(SEMICOLON) {
        enum_entries(p);
    }
    if p.at(SEMICOLON) {
        semis(p);
        eat_nl(p);
        while !p.at(R_BRACE) && !p.is_at_end() {
            class_member_declaration(p);
            semis(p);
            eat_nl(p);
        }
    }
    semis(p);
    eat_nl(p);
    p.expect(R_BRACE);
    m.complete(p, ENUM_CLASS_BODY);
}

/// `enumEntries`: enumEntry {{NL} ',' {NL} enumEntry} {NL} [',']
/// [spec: grammar-rule-enumEntries] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-enumEntries
fn enum_entries(p: &mut Parser) {
    let m = p.start();
    enum_entry(p);
    while p.eat(COMMA) {
        eat_nl(p);
        if p.at(R_BRACE) || p.at(SEMICOLON) {
            break;
        }
        enum_entry(p);
    }
    eat_nl(p);
    m.complete(p, ENUM_ENTRIES);
}

/// `enumEntry`: [modifiers] simpleIdentifier [valueArguments] [classBody]
/// [spec: grammar-rule-enumEntry] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-enumEntry
fn enum_entry(p: &mut Parser) {
    let m = p.start();
    modifiers_and_flag(p, |_| false);
    eat_nl(p);
    simple_identifier(p);
    eat_nl(p);
    if p.at(L_PAREN) {
        value_arguments(p);
        eat_nl(p);
    }
    if p.at(L_BRACE) {
        class_body(p);
    }
    m.complete(p, ENUM_ENTRY);
}

/// Parses a `MODIFIER_LIST`, invoking `flag` for each modifier token so the
/// caller can record e.g. the `enum` class modifier.
fn modifiers_and_flag(p: &mut Parser, mut flag: impl FnMut(&Parser) -> bool) -> bool {
    let m = p.start();
    let mut is_empty = true;
    let mut has_flag = false;
    while at_modifier(p) {
        if p.at(AT) {
            annotation(p);
        } else {
            if flag(p) {
                has_flag = true;
            }
            p.bump();
            eat_nl(p);
        }
        is_empty = false;
    }
    if is_empty {
        m.abandon(p);
    } else {
        m.complete(p, MODIFIER_LIST);
    }
    has_flag
}
