use stacksafe::stacksafe;

use crate::ContextualKeyword;
use crate::grammar::decl::{
    class_decl_rest, enum_decl_rest, interface_decl_rest, record_decl_rest,
    variable_declarator_list, variable_declarator_no_init_expr,
};
use crate::grammar::error_recover::{
    recover_block_statement, recover_catch_parameter, recover_until, recover_until_or_eat,
};
use crate::grammar::expr::{expression, expression_list, variable_access};
use crate::grammar::modifiers::variable_modifier;
use crate::grammar::types::{dimensions, type_};
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
    } else if p.at(DO_KW) {
        do_statement(p);
    } else if p.at(FOR_KW) {
        for_statement(p);
    } else if p.at(TRY_KW) {
        try_statement(p);
    // } else if p.at(SWITCH_KW) {
    //     switch_statement(p);
    } else if p.at(SYNCHRONIZED_KW) {
        synchronized_statement(p);
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

/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14
fn for_statement(p: &mut Parser) {
    let m = p.start();

    p.expect(FOR_KW);
    p.expect(L_PAREN);

    let node_kind = if is_basic_for_stmt(p) {
        basic_for_stmt(p).ok();
        FOR_STMT
    } else if is_enhanced_for_stmt(p) {
        enhanced_for_stmt(p).ok();
        ENHANCED_FOR_STMT
    } else {
        recover_until(p, &[R_PAREN, L_BRACE]);
        FOR_STMT
    };

    p.expect(R_PAREN);

    statement(p);

    m.complete(p, node_kind);
}

fn is_basic_for_stmt(p: &mut Parser) -> bool {
    let ckpt = p.checkpoint();

    if basic_for_stmt(p).is_ok() {
        return true;
    }

    p.rewind(ckpt);
    false
}

fn is_enhanced_for_stmt(p: &mut Parser) -> bool {
    let cp = p.checkpoint();
    let ok = enhanced_for_stmt(p).is_ok();
    p.rewind(cp);
    ok
}

/// EnhancedForStatement:
///   for ( LocalVariableDeclaration : Expression ) Statement
///
/// EnhancedForStatementNoShortIf:
///   for ( LocalVariableDeclaration : Expression ) StatementNoShortIf
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2
fn enhanced_for_stmt(p: &mut Parser) -> Result<(), ()> {
    variable_modifier(p);

    // type
    if type_(p).is_err() {
        p.error_expected_construct(ExpectedConstruct::Type);
        return Err(());
    }

    // variable name
    if variable_declarator_no_init_expr(p).is_err() {
        return Err(());
    };

    // :
    if !p.expect(COLON) {
        return Err(());
    }

    if expression(p).is_err() {
        return Err(());
    };

    Ok(())
}

/// BasicForStatement:
///   for ( [ForInit] ; [Expression] ; [ForUpdate] ) Statement
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.1
fn basic_for_stmt(p: &mut Parser) -> Result<(), ()> {
    // ForInit
    if !p.at(SEMICOLON) {
        if is_local_variable_declaration(p) {
            local_variable_declaration(p).ok();
        } else {
            expression_list(p);
        }
    }

    if !p.expect(SEMICOLON) {
        return Err(());
    }

    // Condition
    if !p.at(SEMICOLON) && expression(p).is_err() {
        p.error_expected_construct(ExpectedConstruct::Expression);
        recover_until(p, &[SEMICOLON, R_PAREN, L_BRACE]);
    }

    if !p.expect(SEMICOLON) {
        return Err(());
    }

    // ForUpdate
    if !p.at(R_PAREN) {
        expression_list(p);
    }

    Ok(())
}

/// TryStatement:
///   try Block Catches
///   try Block [Catches] Finally
///   TryWithResourcesStatement
///
/// Catches:
///   CatchClause {CatchClause}
///
/// CatchClause:
///   catch ( CatchFormalParameter ) Block
///
/// CatchFormalParameter:
///   {VariableModifier} CatchType VariableDeclaratorId
///
/// CatchType:
///   UnannClassType {| ClassType}
///
/// Finally:
///   finally Block
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.20
fn try_statement(p: &mut Parser) {
    let m = p.start();

    p.expect(TRY_KW);

    // try-with-resources
    let mut is_twr = false;
    if p.at(L_PAREN) {
        resource_specification(p);
        is_twr = true;
    }

    // block after `try`
    block(p);

    // catch clauses
    let mut has_catch = false;
    while p.at(CATCH_KW) {
        catch_clause(p);
        has_catch = true;
    }

    // final clause
    let mut has_finally = false;
    if p.at(FINALLY_KW) {
        finally_clause(p);
        has_finally = true;
    }

    // syntax check
    if !has_catch && !has_finally && !is_twr {
        p.error_expected(&[CATCH_KW, FINALLY_KW]);
    }

    if is_twr {
        m.complete(p, TRY_WITH_RESOURCES_STMT);
    } else {
        m.complete(p, TRY_STMT);
    }
}

fn catch_clause(p: &mut Parser) {
    let m = p.start();

    p.expect(CATCH_KW);

    if p.expect(L_PAREN) {
        catch_formal_parameter(p);
        p.expect(R_PAREN);
    } else {
        recover_catch_parameter(p);
    }

    block(p);

    m.complete(p, CATCH_CLAUSE);
}

/// CatchFormalParameter:
///   {VariableModifier} CatchType VariableDeclaratorId
///
/// VariableDeclaratorId:
///   Identifier [Dims]
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-CatchFormalParameter
fn catch_formal_parameter(p: &mut Parser) {
    let m = p.start();
    variable_modifier(p);
    type_(p).ok();

    p.expect(IDENTIFIER);

    if p.at(L_BRACKET) {
        dimensions(p);
    }

    m.complete(p, CATCH_FORMAL_PARAMETER);
}

fn finally_clause(p: &mut Parser) {
    let m = p.start();

    p.expect(FINALLY_KW);

    block(p);

    m.complete(p, FINALLY_CLAUSE);
}

/// DoStatement:
///   do Statement while ( Expression ) ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.13
fn do_statement(p: &mut Parser) {
    let m = p.start();

    p.expect(DO_KW);

    if p.at(L_BRACE) {
        // do { /*...*/ } while (condition);
        block(p);
    } else if p.at(WHILE_KW) || p.at(EOF) {
        p.error_expected_construct(ExpectedConstruct::Statement);
    } else {
        // do i++; while (condition);
        statement(p);
    }

    p.expect(WHILE_KW);

    // condition
    if p.at(L_PAREN) {
        parenthesized_expression(p);
    } else {
        p.error_expected(&[L_PAREN]);
    }

    p.expect(SEMICOLON);

    m.complete(p, DO_STMT);
}

/// SynchronizedStatement:
///   synchronized ( Expression ) Block
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.19
fn synchronized_statement(p: &mut Parser) {
    let m = p.start();

    p.expect(SYNCHRONIZED_KW);

    if p.at(L_PAREN) {
        parenthesized_expression(p);
    } else {
        p.error_expected(&[L_PAREN]);
        recover_until_or_eat(p, &[R_PAREN, L_BRACE, SEMICOLON], SEMICOLON);
    }

    if p.at(L_BRACE) {
        block(p);
    } else {
        p.error_expected(&[L_BRACE]);
        recover_block_statement(p);
    }

    m.complete(p, SYNCHRONIZED_STMT);
}

/// AssertStatement:
///   assert Expression ;
///   assert Expression : Expression ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.10
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

/// WhileStatement:
///   while ( Expression ) Statement
///
/// WhileStatementNoShortIf:
///   while ( Expression ) StatementNoShortIf
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.12
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

/// IfThenStatement:
///   if ( Expression ) Statement
///
/// IfThenElseStatement:
///   if ( Expression ) StatementNoShortIf else Statement
///
/// IfThenElseStatementNoShortIf:
///   if ( Expression ) StatementNoShortIf else StatementNoShortIf
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.9
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

/// ResourceSpecification:
///  ( ResourceList [;] )
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-ResourceSpecification
fn resource_specification(p: &mut Parser) {
    let m = p.start();

    p.expect(L_PAREN);

    // resource list
    // https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-ResourceList
    loop {
        if resource(p).is_err() {
            recover_until(p, &[SEMICOLON, R_PAREN, L_BRACE]);
        }

        if !p.at(SEMICOLON) {
            break;
        }

        p.expect(SEMICOLON);

        if p.at(R_PAREN) {
            break;
        }
    }

    p.expect(R_PAREN);

    m.complete(p, RESOURCE_SPECIFICATION);
}

/// Resource:
///  LocalVariableDeclaration
///  VariableAccess
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-Resource
fn resource(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    if is_local_variable_declaration(p) {
        if local_variable_declaration(p).is_err() {
            return Err(());
        }
    } else {
        variable_access(p);
    }

    m.complete(p, RESOURCE);

    Ok(())
}

/// ExpressionStatement:
///   StatementExpression ;
///
/// StatementExpression:
///   Assignment
///   PreIncrementExpression
///   PreDecrementExpression
///   PostIncrementExpression
///   PostDecrementExpression
///   MethodInvocation
///   ClassInstanceCreationExpression
///
/// docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.8
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

/// EmptyStatement:
///   ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.8
fn empty_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(SEMICOLON);
    m.complete(p, EMPTY_STMT);
}

/// YieldStatement:
///   yield Expression ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.21
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

/// ReturnStatement:
///   return [Expression] ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.17
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

/// ThrowStatement:
///   throw Expression ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.18
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

/// BreakStatement:
///   break [Identifier] ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.15
fn break_statement(p: &mut Parser) {
    // break [label];
    let m = p.start();
    p.expect(BREAK_KW);
    p.eat(IDENTIFIER); // optional label
    p.expect(SEMICOLON);
    m.complete(p, BREAK_STMT);
}

/// ContinueStatement:
///   continue [Identifier] ;
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.16
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
    let ok = local_variable_declaration(p).is_ok();
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
