use stacksafe::stacksafe;

use crate::ContextualKeyword;
use crate::grammar::decl::{
    class_decl_rest, enum_decl_rest, interface_decl_rest, record_decl_rest,
    variable_declarator_list,
};
use crate::grammar::error_recover::{recover_block_statement, recover_until, recover_until_or_eat};
use crate::grammar::expr::expression;
use crate::grammar::modifiers::annotation;
use crate::grammar::types::type_;
use crate::kinds::SyntaxKind::*;
use crate::parser::{ExpectedConstruct, Parser};

pub fn method_body_or_semicolon(p: &mut Parser) {
    if p.at(L_BRACE) {
        // {
        block(p);
    } else {
        // ;
        p.expect(SEMICOLON);
    }
}

/// Parse a block
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html
#[stacksafe]
pub fn block(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);

    while !p.is_at_end() && !p.at(R_BRACE) {
        block_statement(p);
    }

    p.expect(R_BRACE);
    m.complete(p, BLOCK);
}

fn block_statement(p: &mut Parser) {
    // Local Class and Interface Declarations
    // https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3
    if p.at(CLASS_KW) {
        let m = p.start();
        class_decl_rest(p, m);
    } else if p.at(ENUM_KW) {
        let m = p.start();
        enum_decl_rest(p, m);
    } else if p.at(INTERFACE_KW) {
        let m = p.start();
        interface_decl_rest(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Record) {
        let m = p.start();
        record_decl_rest(p, m);
    } else if is_local_variable_declaration(p) {
        local_variable_declaration_statement(p);
    } else {
        statement(p);
    }
}

#[stacksafe]
fn statement(p: &mut Parser) {
    if p.at(L_BRACE) {
        block(p);
    } else if p.at(SEMICOLON) {
        empty_statement(p);
    } else if p.at(IF_KW) {
        if_statement(p);
    } else if p.at(WHILE_KW) {
        while_statement(p);
    // } else if p.at(DO_KW) {
    //     do_statement(p);
    // } else if p.at(FOR_KW) {
    //     for_statement(p);
    // } else if p.at(TRY_KW) {
    //     try_statement(p);
    // } else if p.at(SWITCH_KW) {
    //     switch_statement(p);
    // } else if p.at(SYNCHRONIZED_KW) {
    //     synchronized_statement(p);
    } else if p.at(RETURN_KW) {
        return_statement(p);
    } else if p.at(THROW_KW) {
        throw_statement(p);
    } else if p.at(BREAK_KW) {
        break_statement(p);
    } else if p.at(CONTINUE_KW) {
        continue_statement(p);
    } else if p.at(ASSERT_KW) {
        assert_statement(p);
    } else if p.at_contextual_kw(ContextualKeyword::Yield) {
        yield_statement(p);
    } else {
        expression_statement(p);
    }
}

/// https://docs.oracle.com/javase/specs/jls/se8/html/jls-14.html#jls-14.10
fn assert_statement(p: &mut Parser) {
    // assert <condition expr> [: <msg expr>];
    let m = p.start();

    p.expect(ASSERT_KW);

    if expression(p).is_err() {
        // recover until COLON or SEMICOLON
        recover_until(p, &[COLON, SEMICOLON]);
    }

    // optional msg expr
    if p.eat(COLON) && expression(p).is_err() {
        // recover until SEMICOLON
        recover_until(p, &[SEMICOLON]);
    }

    p.expect(SEMICOLON);

    m.complete(p, ASSERT_STMT);
}

#[stacksafe]
fn while_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(WHILE_KW); // while

    // condition
    if p.at(L_PAREN) {
        parenthesized_expression(p);
    } else {
        p.error_expected(&[L_PAREN]);
        recover_until_or_eat(p, &[R_PAREN, L_BRACE, SEMICOLON], SEMICOLON);
    }

    statement(p);

    m.complete(p, WHILE_STMT);
}

#[stacksafe]
fn if_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(IF_KW); // if

    // condition
    if p.at(L_PAREN) {
        parenthesized_expression(p);
    } else {
        p.error_expected(&[L_PAREN]);
        recover_until_or_eat(p, &[R_PAREN, L_BRACE, SEMICOLON], SEMICOLON);
    }

    statement(p);

    // else
    if p.eat(ELSE_KW) {
        if p.at(IF_KW) {
            if_statement(p);
        } else {
            statement(p);
        }
    }

    m.complete(p, IF_STMT);
}

fn parenthesized_expression(p: &mut Parser) {
    let m = p.start();

    if !p.expect(L_PAREN) {
        m.abandon(p);
        return;
    }

    if expression(p).is_err() {
        recover_block_statement(p);
    }

    p.expect(R_PAREN);

    m.complete(p, PARENTHESIZED_EXPR);
}

fn expression_statement(p: &mut Parser) {
    let m = p.start();

    if expression(p).is_err() {
        p.error_expected_construct(ExpectedConstruct::Statement);
        recover_block_statement(p);
        m.complete(p, ERROR);
        return;
    }

    p.expect(SEMICOLON);
    m.complete(p, EXPRESSION_STMT);
}

fn empty_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(SEMICOLON);
    m.complete(p, EMPTY_STMT);
}

fn yield_statement(p: &mut Parser) {
    let m = p.start();
    p.expect_contextual_kw(ContextualKeyword::Yield);

    if expression(p).is_err() {
        recover_block_statement(p);
        m.complete(p, ERROR);
        return;
    }

    p.expect(SEMICOLON);
    m.complete(p, YIELD_STMT);
}

fn return_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(RETURN_KW);

    if !p.at(SEMICOLON) && expression(p).is_err() {
        recover_block_statement(p);
        m.complete(p, ERROR);
        return;
    }

    p.expect(SEMICOLON);
    m.complete(p, RETURN_STMT);
}

fn throw_statement(p: &mut Parser) {
    // throw <exception>;
    // exception could be an expr
    let m = p.start();
    p.expect(THROW_KW);

    if expression(p).is_err() {
        recover_block_statement(p);
        m.complete(p, ERROR);
        return;
    }

    p.expect(SEMICOLON);
    m.complete(p, THROW_STMT);
}

fn break_statement(p: &mut Parser) {
    // break [label];
    let m = p.start();
    p.expect(BREAK_KW);
    p.eat(IDENTIFIER); // optional label
    p.expect(SEMICOLON);
    m.complete(p, BREAK_STMT);
}

fn continue_statement(p: &mut Parser) {
    // continue [label];
    let m = p.start();
    p.expect(CONTINUE_KW);
    p.eat(IDENTIFIER); // optional label
    p.expect(SEMICOLON);
    m.complete(p, CONTINUE_STMT);
}

fn is_local_variable_declaration(p: &mut Parser) -> bool {
    let cp = p.checkpoint();
    let ok = local_variable_declaration(p).is_ok() && p.at(SEMICOLON);
    p.rewind(cp);
    ok
}

fn local_variable_declaration_statement(p: &mut Parser) {
    let m = p.start();

    if local_variable_declaration(p).is_err() {
        recover_block_statement(p);
        m.complete(p, ERROR);
        return;
    }

    p.expect(SEMICOLON);
    m.complete(p, LOCAL_VARIABLE_DECLARATION_STMT);
}

fn local_variable_declaration(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    // VariableModifier:
    //   Annotation
    //   final
    // consume modifier (only annotation and final allowed in local variable decl)
    variable_modifier(p);

    // LocalVariableType:
    //   UnannType
    //   var
    // consume type
    if p.at_contextual_kw(ContextualKeyword::Var) {
        p.bump();
    } else if type_(p).is_err() {
        p.error_expected_construct(ExpectedConstruct::Type);
        m.complete(p, ERROR);
        return Err(());
    }

    if variable_declarator_list(p).is_err() {
        m.complete(p, ERROR);
        return Err(());
    }

    m.complete(p, LOCAL_VARIABLE_DECLARATION);
    Ok(())
}

fn variable_modifier(p: &mut Parser) {
    let m = p.start();

    while p.at(AT) || p.at(FINAL_KW) {
        if p.at(AT) {
            annotation(p);
        } else {
            p.expect(FINAL_KW);
        }
    }

    m.complete(p, MODIFIER_LIST);
}
