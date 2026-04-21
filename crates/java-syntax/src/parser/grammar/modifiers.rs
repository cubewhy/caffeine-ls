use crate::{
    ContextualKeyword,
    grammar::expr::element_value,
    kinds::SyntaxKind::*,
    parser::{Parser, grammar::names::qualified_name},
    tokenset,
};

pub fn modifiers(p: &mut Parser) {
    // [public] [private] [protected] [final] [static] [abstract] [default] [@<annotation>[(<arguments>)]]
    let m = p.start();

    let mut is_empty = true;

    loop {
        if p.at_set(tokenset![
            PUBLIC_KW,
            PRIVATE_KW,
            PROTECTED_KW,
            FINAL_KW,
            STATIC_KW,
            ABSTRACT_KW,
            DEFAULT_KW,
            NATIVE_KW,
            SYNCHRONIZED_KW,
            TRANSIENT_KW,
            VOLATILE_KW,
            STRICTFP_KW,
        ]) || p.at_contextual_kw_set(tokenset![
            ContextualKeyword::Sealed,
            ContextualKeyword::NonSealed
        ]) {
            p.bump();
        } else if p.at(AT) && p.nth(1) != Some(INTERFACE_KW) {
            annotation(p);
        } else {
            break;
        }
        is_empty = false;
    }

    if is_empty {
        m.abandon(p);
    } else {
        m.complete(p, MODIFIER_LIST);
    }
}

pub fn annotation(p: &mut Parser) {
    // @<identifier>[(<arguments>)]
    let m = p.start();

    p.expect(AT);

    qualified_name(p);

    if p.at(L_PAREN) {
        // annotation_argument_list
        annotation_argument_list(p);

        m.complete(p, ANNOTATION);
    } else {
        m.complete(p, MARKER_ANNOTATION);
    }
}

fn annotation_argument_list(p: &mut Parser) {
    // (k = v, a = b) or (v)
    let m = p.start();

    p.expect(L_PAREN);

    if p.nth(1) == Some(EQUAL) {
        element_value_pair(p);

        while p.eat(COMMA) {
            element_value_pair(p);
        }
    } else {
        // single argument annotation
        element_value(p);
    }

    p.expect(R_PAREN);

    m.complete(p, ANNOTATION_ARGUMENT_LIST);
}

fn element_value_pair(p: &mut Parser) {
    // key = value
    let m = p.start();

    p.expect(IDENTIFIER);
    p.expect(EQUAL);

    element_value(p);

    m.complete(p, ELEMENT_VALUE_PAIR);
}

pub fn variable_modifier(p: &mut Parser) {
    let m = p.start();

    let mut is_empty = true;

    while p.at(AT) || p.at(FINAL_KW) {
        if p.at(AT) {
            annotation(p);
        } else {
            p.expect(FINAL_KW);
        }
        is_empty = false;
    }

    if is_empty {
        m.abandon(p);
    } else {
        m.complete(p, MODIFIER_LIST);
    }
}
