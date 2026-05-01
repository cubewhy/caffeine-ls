use crate::{
    grammar::{
        expr::expression, member::annotation_type_member_decl, names::qualified_name,
        types::type_parameters_opt,
    },
    kinds::{ContextualKeyword, SyntaxKind::*},
    parser::{
        ExpectedConstruct, Parser,
        grammar::{
            clauses::{extends_clause, implements_clause, interface_extends_clause},
            error_recover::{recover_decl, recover_member},
            expr::argument_list,
            member::{
                at_member_start, class_member_decl, interface_member_decl, member_decl_rest,
                record_member_decl,
            },
            modifiers::{annotation, modifiers},
            types::{dimensions, formal_parameters},
        },
        marker::Marker,
    },
    tokenset,
};

pub fn decl(p: &mut Parser) {
    let m = p.start();
    modifiers(p);

    if p.at(CLASS_KW) {
        class_decl_rest(p, m);
    } else if p.at(INTERFACE_KW) {
        interface_decl_rest(p, m);
    } else if p.at(ENUM_KW) {
        enum_decl_rest(p, m);
    } else if is_record_decl(p) {
        record_decl_rest(p, m);
    } else if is_module_decl_start(p) {
        module_decl_rest(p, m);
    } else if p.at(AT) && p.nth(1) == Some(INTERFACE_KW) {
        annotation_type_decl_rest(p, m);
    } else if at_member_start(p) {
        member_decl_rest(p, m);
    } else {
        // unexpected token, start error recover
        p.error_expected_construct(ExpectedConstruct::Declaration);
        recover_decl(p);
        m.complete(p, ERROR);
    }
}

pub fn is_record_decl(p: &Parser) -> bool {
    if !p.at_contextual_kw(ContextualKeyword::Record) {
        return false;
    }

    if p.nth(1) != Some(IDENTIFIER) {
        return false;
    }

    let mut cur = 2;
    loop {
        match p.nth(cur) {
            Some(L_PAREN) | Some(LESS) => return true,
            Some(L_BRACE) | Some(SEMICOLON) | Some(EQUAL) | Some(DOT) | Some(EOF) | None => break,
            _ => {}
        }
        cur += 1;
    }

    false
}

fn is_module_decl_start(p: &Parser) -> bool {
    p.at_contextual_kw(ContextualKeyword::Module)
        || (p.at_contextual_kw(ContextualKeyword::Open)
            && p.nth_at_contextual_kw(1, ContextualKeyword::Module))
}

/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7
fn module_decl_rest(p: &mut Parser, m: Marker) {
    // optional `open` keyword
    p.eat_contextual_kw(ContextualKeyword::Open);

    p.expect_contextual_kw(ContextualKeyword::Module);

    // module name
    qualified_name(p);

    module_body(p);

    m.complete(p, MODULE_DECL);
}

pub fn module_body(p: &mut Parser) {
    let m = p.start();

    p.expect(L_BRACE);

    while !p.at_set(tokenset![EOF, R_BRACE]) {
        module_directive(p);
    }

    p.expect(R_BRACE);

    m.complete(p, MODULE_BODY);
}

fn module_directive(p: &mut Parser) {
    let m = p.start();

    if p.at_contextual_kw(ContextualKeyword::Requires) {
        requires_directive(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Exports) {
        exports_directive(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Opens) {
        opens_directive(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Uses) {
        uses_directive(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Provides) {
        provides_directive(p, m);
    } else {
        p.error_expected_construct(ExpectedConstruct::ModuleDirective);
        recover_member(p);
        m.complete(p, ERROR);
    }
}

/// requires {RequiresModifier} ModuleName ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-ModuleDirective
fn requires_directive(p: &mut Parser, m: Marker) {
    p.expect_contextual_kw(ContextualKeyword::Requires);

    requires_modifier(p);

    qualified_name(p);
    p.expect(SEMICOLON);
    m.complete(p, REQUIRES_DIRECTIVE);
}

/// RequiresModifier:
///   (one of)
///   transitive static
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-RequiresModifier
fn requires_modifier(p: &mut Parser) {
    let m = p.start();
    let mut is_empty = true;
    while p.at_contextual_kw(ContextualKeyword::Transitive) || p.at(STATIC_KW) {
        p.bump();
        is_empty = false;
    }

    if is_empty {
        m.abandon(p);
    } else {
        m.complete(p, MODIFIER_LIST);
    }
}

/// exports PackageName [to ModuleName {, ModuleName}] ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-RequiresModifier
fn exports_directive(p: &mut Parser, m: Marker) {
    p.expect_contextual_kw(ContextualKeyword::Exports);
    qualified_name(p);

    // Optional [to ModuleName {, ModuleName}]
    if p.at_contextual_kw(ContextualKeyword::To) {
        p.bump();
        loop {
            qualified_name(p);
            if !p.eat(COMMA) {
                break;
            }
        }
    }

    p.expect(SEMICOLON);
    m.complete(p, EXPORTS_DIRECTIVE);
}

/// opens PackageName [to ModuleName {, ModuleName}] ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-RequiresModifier
fn opens_directive(p: &mut Parser, m: Marker) {
    p.expect_contextual_kw(ContextualKeyword::Opens);
    qualified_name(p);

    if p.at_contextual_kw(ContextualKeyword::To) {
        p.bump();
        loop {
            qualified_name(p);
            if !p.eat(COMMA) {
                break;
            }
        }
    }

    p.expect(SEMICOLON);
    m.complete(p, OPENS_DIRECTIVE);
}

/// uses TypeName ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-RequiresModifier
fn uses_directive(p: &mut Parser, m: Marker) {
    p.expect_contextual_kw(ContextualKeyword::Uses);
    qualified_name(p);
    p.expect(SEMICOLON);
    m.complete(p, USES_DIRECTIVE);
}

/// provides TypeName with TypeName {, TypeName} ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-RequiresModifier
fn provides_directive(p: &mut Parser, m: Marker) {
    p.expect_contextual_kw(ContextualKeyword::Provides);

    // interface
    qualified_name(p);

    p.expect_contextual_kw(ContextualKeyword::With);
    loop {
        // impl
        qualified_name(p);
        if !p.eat(COMMA) {
            break;
        }
    }

    p.expect(SEMICOLON);
    m.complete(p, PROVIDES_DIRECTIVE);
}

pub fn annotation_type_decl_rest(p: &mut Parser, m: Marker) {
    p.expect(AT); // @
    p.expect(INTERFACE_KW); // interface

    // annotation name
    p.expect(IDENTIFIER);

    // { ... }
    annotation_type_body(p);

    m.complete(p, ANNOTATION_TYPE_DECL);
}

pub fn annotation_type_body(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);
    while !p.is_at_end() && !p.at(R_BRACE) {
        annotation_type_member_decl(p);
    }
    p.expect(R_BRACE);
    m.complete(p, ANNOTATION_TYPE_BODY);
}

pub fn record_decl_rest(p: &mut Parser, m: Marker) {
    p.expect_contextual_kw(ContextualKeyword::Record); // record

    // record name
    p.expect(IDENTIFIER);

    // generics
    type_parameters_opt(p);

    // record header
    formal_parameters(p);

    // implements
    if p.at(IMPLEMENTS_KW) {
        implements_clause(p);
    }

    // { ... }
    record_body(p);

    m.complete(p, RECORD_DECL);
}

pub fn record_body(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);
    while !p.is_at_end() && !p.at(R_BRACE) {
        record_member_decl(p);
    }
    p.expect(R_BRACE);
    m.complete(p, RECORD_BODY);
}

pub fn enum_decl_rest(p: &mut Parser, m: Marker) {
    p.expect(ENUM_KW); // bump ENUM_KW

    // enum name
    p.expect(IDENTIFIER);

    // implements
    if p.at(IMPLEMENTS_KW) {
        implements_clause(p);
    }

    enum_body(p);

    m.complete(p, ENUM_DECL);
}

pub fn class_decl_rest(p: &mut Parser, m: Marker) {
    p.expect(CLASS_KW); // bump CLASS_KW

    // class name
    p.expect(IDENTIFIER);

    // generics
    type_parameters_opt(p);

    // extends
    if p.at(EXTENDS_KW) {
        extends_clause(p);
    }

    // implements
    if p.at(IMPLEMENTS_KW) {
        implements_clause(p);
    }

    // class body
    class_body(p);

    m.complete(p, CLASS_DECL);
}

pub fn class_body(p: &mut Parser) {
    let m = p.start();

    p.expect(L_BRACE);

    while !p.is_at_end() && !p.at(R_BRACE) {
        if at_member_start(p) {
            class_member_decl(p);
        } else {
            let err = p.start();
            p.error_expected_construct(ExpectedConstruct::MemberDeclaration);
            recover_member(p);
            err.complete(p, ERROR);
        }
    }

    p.expect(R_BRACE);

    m.complete(p, CLASS_BODY);
}

/// Enum body:
/// { <enum_constant_list>? [,]? [; <class_member_decl>*] }
///
/// Enum constant:
/// [annotations] <identifier> [argument_list] [class_body]
///
/// After the optional ';', enum body members are class-like members.
pub fn enum_body(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);

    // enum constants
    if at_enum_constant_start(p) {
        loop {
            enum_constant(p);

            if !p.eat(COMMA) {
                break;
            }

            if !at_enum_constant_start(p) {
                break;
            }
        }
    }

    // optional trailing comma
    p.eat(COMMA);

    // optional ';' then members
    if p.eat(SEMICOLON) {
        while !p.is_at_end() && !p.at(R_BRACE) {
            class_member_decl(p);
        }
    }

    p.expect(R_BRACE);
    m.complete(p, ENUM_BODY);
}

fn at_enum_constant_start(p: &Parser) -> bool {
    p.at(IDENTIFIER) || p.at(AT)
}

fn enum_constant(p: &mut Parser) {
    let m = p.start();

    while p.at(AT) && p.nth(1) != Some(INTERFACE_KW) {
        annotation(p);
    }

    p.expect(IDENTIFIER);

    if p.at(L_PAREN) {
        argument_list(p);
    }

    if p.at(L_BRACE) {
        class_body(p);
    }

    m.complete(p, ENUM_CONSTANT);
}

pub fn interface_decl_rest(p: &mut Parser, m: Marker) {
    p.expect(INTERFACE_KW); // bump INTERFACE_KW

    // interface name
    p.expect(IDENTIFIER);

    // generics
    type_parameters_opt(p);

    // extends
    if p.at(EXTENDS_KW) {
        interface_extends_clause(p);
    }

    interface_body(p);

    m.complete(p, INTERFACE_DECL);
}

pub fn interface_body(p: &mut Parser) {
    let m = p.start();

    p.expect(L_BRACE);

    while !p.is_at_end() && !p.at(R_BRACE) {
        if at_member_start(p) {
            interface_member_decl(p);
        } else {
            // error recover
            let err = p.start();
            p.error_expected_construct(ExpectedConstruct::MemberDeclaration);
            recover_member(p);
            err.complete(p, ERROR);
        }
    }

    p.expect(R_BRACE);

    m.complete(p, INTERFACE_BODY);
}

pub fn variable_declarator_list(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    if variable_declarator(p).is_err() {
        m.complete(p, VARIABLE_DECLARATOR_LIST);
        return Err(());
    }

    while p.eat(COMMA) {
        if variable_declarator(p).is_err() {
            break;
        }
    }

    m.complete(p, VARIABLE_DECLARATOR_LIST);
    Ok(())
}

fn variable_id(p: &mut Parser) -> Result<(), ()> {
    if p.at(IDENTIFIER) || p.at(UNDERSCORE) {
        p.bump();
        Ok(())
    } else {
        p.error_expected(&[IDENTIFIER, UNDERSCORE]);
        Err(())
    }
}

fn variable_declarator(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    // variable id
    if variable_id(p).is_err() {
        m.complete(p, VARIABLE_DECLARATOR);
        return Err(());
    }

    // dimensions on variable id
    // a[]
    dimensions(p);

    // init expr
    if p.eat(EQUAL) && expression(p).is_err() {
        m.complete(p, VARIABLE_DECLARATOR);
        return Err(());
    }

    m.complete(p, VARIABLE_DECLARATOR);
    Ok(())
}

pub fn variable_declarator_no_init_expr(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    // variable id
    if variable_id(p).is_err() {
        m.complete(p, VARIABLE_DECLARATOR);
        return Err(());
    }

    // dimensions on variable id
    // a[]
    dimensions(p);

    m.complete(p, VARIABLE_DECLARATOR);
    Ok(())
}
