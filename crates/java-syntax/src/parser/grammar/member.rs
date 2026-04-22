use crate::grammar::error_recover::recover_annotation_type_parameter;
use crate::grammar::expr::expression;
use crate::grammar::types::{type_or_void, type_parameters_opt};
use crate::kinds::{ContextualKeyword, SyntaxKind::*};
use crate::parser::grammar::clauses::{throws_clause, throws_clause_opt};
use crate::parser::grammar::decl::{
    annotation_type_decl_rest, class_decl_rest, enum_decl_rest, interface_decl_rest,
    record_decl_rest, variable_declarator_list,
};
use crate::parser::grammar::error_recover::recover_member;
use crate::parser::grammar::modifiers::modifiers;
use crate::parser::grammar::stmt::{block, method_body_or_semicolon};
use crate::parser::grammar::types::{at_type_start, formal_parameters, type_};
use crate::parser::marker::Marker;
use crate::parser::{ExpectedConstruct, Parser};
use crate::tokenset;

pub fn at_member_start(p: &Parser) -> bool {
    p.at_set(tokenset![
        CLASS_KW,
        INTERFACE_KW,
        DEFAULT_KW,
        ENUM_KW,
        VOID_KW,
        INT_KW,
        SHORT_KW,
        LONG_KW,
        FLOAT_KW,
        DOUBLE_KW,
        BYTE_KW,
        BOOLEAN_KW,
        CHAR_KW,
        IDENTIFIER,
        L_BRACE,
        SEMICOLON,
        STATIC_KW,
        PUBLIC_KW,
        PRIVATE_KW,
        PROTECTED_KW,
        FINAL_KW,
        ABSTRACT_KW,
        AT,
        LESS, // type parameters (generics)
    ]) || p.at_contextual_kw(ContextualKeyword::Record)
}

pub fn class_member_decl(p: &mut Parser) {
    // static initializer
    if p.at(STATIC_KW) && p.nth(1) == Some(L_BRACE) {
        let m = p.start();
        p.bump(); // static
        block(p);
        m.complete(p, STATIC_INITIALIZER);
        return;
    }

    // instance initializer
    if p.at(L_BRACE) {
        let m = p.start();
        block(p);
        m.complete(p, INSTANCE_INITIALIZER);
        return;
    }

    let m = p.start();

    modifiers(p);

    if p.at(CLASS_KW) {
        class_decl_rest(p, m);
    } else if p.at(INTERFACE_KW) {
        interface_decl_rest(p, m);
    } else if p.at(ENUM_KW) {
        enum_decl_rest(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Record) {
        record_decl_rest(p, m);
    } else if p.at(AT) && p.nth(1) == Some(INTERFACE_KW) {
        annotation_type_decl_rest(p, m);
    } else if p.at(SEMICOLON) {
        p.bump();
        m.complete(p, EMPTY_DECL);
    } else {
        member_decl_rest(p, m);
    }
}

pub fn record_member_decl(p: &mut Parser) {
    // static initializer
    if p.at(STATIC_KW) && p.nth(1) == Some(L_BRACE) {
        let m = p.start();
        p.bump(); // static
        block(p);
        m.complete(p, STATIC_INITIALIZER);
        return;
    }

    // instance initializer
    if p.at(L_BRACE) {
        let m = p.start();
        block(p);
        m.complete(p, INSTANCE_INITIALIZER);
        return;
    }

    let m = p.start();
    modifiers(p);

    if is_compact_constructor_like(p) {
        compact_constructor_rest(p, m);
    } else if is_constructor_like(p) {
        constructor_rest(p, m);
    } else if p.at(CLASS_KW) {
        class_decl_rest(p, m);
    } else if p.at(INTERFACE_KW) {
        interface_decl_rest(p, m);
    } else if p.at(ENUM_KW) {
        enum_decl_rest(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Record) {
        record_decl_rest(p, m);
    } else if p.at(AT) && p.nth(1) == Some(INTERFACE_KW) {
        annotation_type_decl_rest(p, m);
    } else if p.at(SEMICOLON) {
        p.bump();
        m.complete(p, EMPTY_DECL);
    } else {
        member_decl_rest(p, m);
    }
}

fn compact_constructor_rest(p: &mut Parser, m: Marker) {
    // <identifier><block>
    p.expect(IDENTIFIER);

    block(p);
    m.complete(p, COMPACT_CONSTRUCTOR_DECL);
}

fn constructor_rest(p: &mut Parser, m: Marker) {
    // <identifier><formal_parameters><block>
    p.expect(IDENTIFIER);

    formal_parameters(p);

    // throws
    if p.at(THROWS_KW) {
        throws_clause(p);
    }

    block(p);
    m.complete(p, CONSTRUCTOR_DECL);
}

fn is_compact_constructor_like(p: &Parser) -> bool {
    // <identifier>{
    p.at(IDENTIFIER) && p.nth(1) == Some(L_BRACE)
}

fn is_constructor_like(p: &Parser) -> bool {
    // <identifier>(
    p.at(IDENTIFIER) && p.nth(1) == Some(L_PAREN)
}

pub fn member_decl_rest(p: &mut Parser, m: Marker) {
    // method type parameters (generics), e.g. <T> void f() {}
    type_parameters_opt(p);

    if p.at(VOID_KW) {
        // definitely a method.
        p.bump(); // void
        p.expect(IDENTIFIER); // method name
        formal_parameters(p);
        method_body_or_semicolon(p);

        m.complete(p, METHOD_DECL);
        return;
    }

    // constructor: Name(...)
    if is_constructor_like(p) {
        constructor_rest(p, m);
        return;
    }

    // typed member: <type> <name> ...
    // if has L_PAREN -> method
    // else -> field
    if at_type_start(p) {
        if type_(p).is_err() {
            recover_member(p);
            m.complete(p, ERROR);
            return;
        }

        if p.at(IDENTIFIER) && p.nth(1) == Some(L_PAREN) {
            p.bump(); // method name
            formal_parameters(p);
            throws_clause_opt(p);
            method_body_or_semicolon(p);
            m.complete(p, METHOD_DECL);
        } else {
            variable_declarator_list(p).ok();
            p.expect(SEMICOLON);
            m.complete(p, FIELD_DECL);
        }
        return;
    }

    // bad member declaration
    // start error recover
    p.error_expected_construct(ExpectedConstruct::MemberDeclaration);
    recover_member(p);
    m.complete(p, ERROR);
}

pub fn interface_member_decl(p: &mut Parser) {
    let m = p.start();
    modifiers(p);

    if p.at(CLASS_KW) {
        class_decl_rest(p, m);
    } else if p.at(INTERFACE_KW) {
        interface_decl_rest(p, m);
    } else if p.at(ENUM_KW) {
        enum_decl_rest(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Record) {
        record_decl_rest(p, m);
    } else if p.at(AT) && p.nth(1) == Some(INTERFACE_KW) {
        annotation_type_decl_rest(p, m);
    } else if p.at(SEMICOLON) {
        p.bump();
        m.complete(p, EMPTY_DECL);
    } else {
        member_decl_rest(p, m);
    }
}

fn annotation_type_member_decl_rest(p: &mut Parser, m: Marker) {
    // fields and methods without parameter list are supported

    // type
    if type_or_void(p).is_err() {
        recover_member(p);
        m.complete(p, ERROR);
        return;
    }

    // member name
    if p.at(IDENTIFIER) && p.nth(1) == Some(L_PAREN) {
        p.expect(IDENTIFIER);
        p.expect(L_PAREN);

        if !p.eat(R_PAREN) {
            // the parameter list is not empty, start error recover
            recover_annotation_type_parameter(p);
            m.complete(p, ERROR);
        } else {
            // <type> <identifier>() [default] [default value];

            // optional default
            if p.eat(DEFAULT_KW) {
                expression(p).ok(); // default value
            }

            p.expect(SEMICOLON);

            m.complete(p, ANNOTATION_TYPE_ELEMENT_DECL);
        }
    } else {
        // field
        variable_declarator_list(p).ok();
        p.expect(SEMICOLON);
        m.complete(p, FIELD_DECL);
    }
}

pub fn annotation_type_member_decl(p: &mut Parser) {
    let m = p.start();
    modifiers(p);

    if p.at(CLASS_KW) {
        class_decl_rest(p, m);
    } else if p.at(INTERFACE_KW) {
        interface_decl_rest(p, m);
    } else if p.at(ENUM_KW) {
        enum_decl_rest(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Record) {
        record_decl_rest(p, m);
    } else if p.at(AT) && p.nth(1) == Some(INTERFACE_KW) {
        annotation_type_decl_rest(p, m);
    } else if p.at(SEMICOLON) {
        p.bump();
        m.complete(p, EMPTY_DECL);
    } else {
        annotation_type_member_decl_rest(p, m);
    }
}
