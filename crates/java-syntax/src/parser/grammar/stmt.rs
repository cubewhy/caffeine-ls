use stacksafe::stacksafe;

use crate::grammar::decl::{
    class_decl_rest, enum_decl_rest, interface_decl_rest, record_decl_rest,
    variable_declarator_list, variable_declarator_no_init_expr,
};
use crate::grammar::error_recover::{
    recover_block_statement, recover_catch_parameter, recover_switch_statement, recover_until,
    recover_until_or_eat,
};
use crate::grammar::expr::{
    case_pattern_or_constant, expression, expression_list, is_expression_start,
};
use crate::grammar::modifiers::variable_modifier;
use crate::grammar::types::{dimensions, type_};
use crate::kinds::SyntaxKind::*;
use crate::parser::marker::{CompletedMarker, Marker};
use crate::parser::{ExpectedConstruct, Parser};
use crate::{ContextualKeyword, SyntaxKind, tokenset};

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
    }

    let cp = p.checkpoint();
    // try local variable decl
    if local_variable_declaration_statement(p).is_ok() {
        return;
    }
    p.rewind(cp);

    // not local variable decl, parse statement
    statement(p);
}

#[stacksafe]
fn statement(p: &mut Parser) {
    match p.current() {
        Some(L_BRACE) => block(p),
        Some(SEMICOLON) => empty_statement(p),
        Some(IF_KW) => if_statement(p),
        Some(WHILE_KW) => while_statement(p),
        Some(DO_KW) => do_statement(p),
        Some(FOR_KW) => for_statement(p),
        Some(TRY_KW) => try_statement(p),
        Some(SYNCHRONIZED_KW) => synchronized_statement(p),
        Some(SWITCH_KW) => switch_statement(p),
        Some(RETURN_KW) => return_statement(p),
        Some(THROW_KW) => throw_statement(p),
        Some(BREAK_KW) => break_statement(p),
        Some(CONTINUE_KW) => continue_statement(p),
        Some(ASSERT_KW) => assert_statement(p),
        _ => {
            if is_at_yield_statement_start(p) {
                yield_statement(p);
            } else if is_at_labeled_statement_start(p) {
                labeled_statement(p);
            } else {
                expression_statement(p);
            }
        }
    }
}

fn is_at_labeled_statement_start(p: &Parser) -> bool {
    p.matches(&[IDENTIFIER, COLON])
}

/// LabeledStatement:
///   Identifier : Statement
///
/// LabeledStatementNoShortIf:
///   Identifier : StatementNoShortIf
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.7
fn labeled_statement(p: &mut Parser) {
    let m = p.start();

    p.expect(IDENTIFIER);
    p.expect(COLON);

    statement(p);

    m.complete(p, LABELED_STMT);
}

/// SwitchStatement:
///   switch ( Expression ) SwitchBlock
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11
fn switch_statement(p: &mut Parser) {
    let m = switch_common(p);
    m.complete(p, SWITCH_STMT);
}

pub fn switch_common(p: &mut Parser) -> Marker {
    let m = p.start();

    p.expect(SWITCH_KW);

    // ( Expression )
    if p.expect(L_PAREN) && expression(p).is_err() {
        recover_until(p, &[R_PAREN, L_BRACE]);
    }

    p.expect(R_PAREN);

    // switch block
    if p.at(L_BRACE) {
        switch_block(p);
    } else {
        p.error_expected(&[L_BRACE]);
        recover_block_statement(p);
    }

    m
}

fn switch_block(p: &mut Parser) {
    let m = p.start();

    p.expect(L_BRACE);

    while !p.at(R_BRACE) && !p.at(EOF) {
        if p.at(CASE_KW) || p.at(DEFAULT_KW) {
            if is_switch_rule(p) {
                switch_rule(p);
            } else {
                switch_block_statement_group(p);
            }
        } else {
            p.error_expected(&[CASE_KW, DEFAULT_KW]);
            recover_switch_statement(p);
        }
    }

    p.expect(R_BRACE);
    m.complete(p, SWITCH_BLOCK);
}

/// SwitchBlockStatementGroup:
///   SwitchLabel : {SwitchLabel :} BlockStatements
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-SwitchBlockStatementGroup
fn switch_block_statement_group(p: &mut Parser) {
    let m = p.start();

    switch_label(p);
    p.expect(COLON);

    while p.at_set(tokenset![CASE_KW, DEFAULT_KW]) {
        switch_label(p);
        p.expect(COLON);
    }

    while !p.at_set(tokenset![EOF, R_BRACE, CASE_KW, DEFAULT_KW]) {
        block_statement(p);
    }

    m.complete(p, SWITCH_BLOCK_STATEMENT_GROUP);
}

/// SwitchRule:
///   SwitchLabel -> Expression ;
///   SwitchLabel -> Block
///   SwitchLabel -> ThrowStatement
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-SwitchRule
fn switch_rule(p: &mut Parser) {
    let m = p.start();

    // switch label
    switch_label(p);

    p.expect(ARROW);

    match p.current() {
        Some(L_BRACE) => block(p),
        Some(THROW_KW) => throw_statement(p),
        // NOTE: expression_statement() will handle the trailing semicolon
        _ => expression_statement(p),
    }

    m.complete(p, SWITCH_RULE);
}

/// SwitchLabel:
///   case CaseConstant {, CaseConstant}
///   case null [, default]
///   case CasePattern {, CasePattern} [Guard]
///   default
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-SwitchLabel
fn switch_label(p: &mut Parser) {
    let m = p.start();
    // case or default keyword
    let current_token = p.current().unwrap_or(EOF);
    if p.at_set(tokenset![CASE_KW, DEFAULT_KW]) {
        p.bump();
    } else {
        // unreachable, we just report an error there
        p.error_expected(&[CASE_KW, DEFAULT_KW]);
    }

    // `case`
    if current_token == CASE_KW {
        // parse the patterns
        let mut meet_null = false;
        loop {
            if p.eat(NULL_LITERAL) {
                meet_null = true;
            } else if p.eat(DEFAULT_KW) {
                if !meet_null {
                    // https://github.com/JetBrains/intellij-community/blob/759cab24b385cf1a5a8ec66dcdc19ffb2d9e2dea/java/codeserver/highlighting/src/com/intellij/java/codeserver/highlighting/SwitchChecker.java#L571
                    p.error_message("Default label not allowed here: 'default' can only be used as a single case label or paired only with 'null'");
                }
            } else {
                // parse constant
                case_pattern_or_constant(p);
            }

            if !p.eat(COMMA) {
                break;
            }
        }

        if p.eat_contextual_kw(ContextualKeyword::When) {
            // switch guard
            expression(p).ok();
        }
    }

    // otherwise it is `default` branch
    m.complete(p, SWITCH_LABEL);
}

fn is_switch_rule(p: &Parser) -> bool {
    let mut i = 0;
    loop {
        let Some(kind) = p.nth(i) else {
            // try to peak a token after EOF
            // probably unreachable
            return false;
        };
        match kind {
            ARROW => return true,
            COLON | L_BRACE | R_BRACE | EOF => return false,
            _ => i += 1, // move the cursor
        }
    }
}

/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14
fn for_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(FOR_KW);
    p.expect(L_PAREN);

    let separator = scan_for_separator(p);

    let node_kind = match separator {
        Some(SEMICOLON) => {
            basic_for_stmt(p).ok();
            FOR_STMT
        }
        Some(COLON) => {
            enhanced_for_stmt(p).ok();
            ENHANCED_FOR_STMT
        }
        _ => {
            recover_until(p, &[R_PAREN, L_BRACE]);
            FOR_STMT
        }
    };

    if !p.expect(R_PAREN) {
        recover_block_statement(p);
    }
    statement(p);
    m.complete(p, node_kind);
}

fn scan_for_separator(p: &mut Parser) -> Option<SyntaxKind> {
    fn inner(p: &mut Parser) -> Option<SyntaxKind> {
        let mut paren_depth = 0;
        while !p.at(EOF) && !p.at(R_PAREN) {
            if p.at(L_PAREN) {
                paren_depth += 1;
            } else if p.at(R_PAREN) {
                paren_depth -= 1;
            } else if paren_depth == 0 {
                if p.at(SEMICOLON) {
                    return Some(SEMICOLON);
                }
                if p.at(COLON) {
                    return Some(COLON);
                }
            }
            p.bump();
        }
        None
    }

    let ckpt = p.checkpoint();
    let sep_type = inner(p);
    p.rewind(ckpt);

    sep_type
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

/// VariableAccess:
///   ExpressionName
///   FieldAccess
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-VariableAccess
fn variable_access(p: &mut Parser) -> CompletedMarker {
    let m = p.start();

    // Nud
    if p.at(IDENTIFIER) || p.at(THIS_KW) || p.at(SUPER_KW) {
        p.bump();
    } else {
        p.error_expected(&[IDENTIFIER, THIS_KW, SUPER_KW]);
        return m.complete(p, ERROR);
    }

    let mut left = m.complete(p, VARIABLE_ACCESS);

    // Led: .identifier
    while p.at(DOT) {
        let m = left.precede(p);
        p.expect(DOT);

        if p.at(IDENTIFIER) || p.at(THIS_KW) || p.at(SUPER_KW) {
            p.bump();
            left = m.complete(p, VARIABLE_ACCESS);
        } else {
            p.error_expected(&[IDENTIFIER]);
            left = m.complete(p, ERROR);
            break;
        }
    }

    left
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
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.8
fn expression_statement(p: &mut Parser) {
    let m = p.start();

    let expr_marker = match expression(p) {
        Ok(marker) => marker,
        Err(_) => {
            p.error_expected_construct(ExpectedConstruct::Statement);
            recover_block_statement(p);
            // expression will construct the ERROR node
            m.abandon(p);
            return;
        }
    };

    if !is_legal_statement_expression(expr_marker.kind()) {
        p.error_message("Not a statement: only assignment, increment, decrement, method call, and new object creation are allowed");
    }

    p.expect(SEMICOLON);
    m.complete(p, EXPRESSION_STMT);
}

fn is_legal_statement_expression(kind: SyntaxKind) -> bool {
    match kind {
        ASSIGNMENT_EXPR |      // a = 1
        POSTFIX_EXPR |
        ASSIGN_EXPR |
        PREFIX_EXPR |
        METHOD_CALL |    // method()
        NEW_EXPR => true,      // new Object()
        _ => false,
    }
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

fn is_at_yield_statement_start(p: &Parser) -> bool {
    if !p.at_contextual_kw(ContextualKeyword::Yield) {
        return false;
    }

    match p.nth(1) {
        Some(L_PAREN) => false,
        Some(EQUAL) => false,
        Some(kind) => is_expression_start(kind),
        None => false,
    }
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

pub fn is_local_variable_declaration(p: &mut Parser) -> bool {
    let cp = p.checkpoint();
    let ok = local_variable_declaration(p).is_ok();
    p.rewind(cp);
    ok
}

fn local_variable_declaration_statement(p: &mut Parser) -> Result<(), ()> {
    let m = p.start();

    if local_variable_declaration(p).is_err() {
        recover_block_statement(p);
        m.complete(p, ERROR);
        return Err(());
    }

    p.expect(SEMICOLON);
    m.complete(p, LOCAL_VARIABLE_DECLARATION_STMT);

    Ok(())
}

pub fn local_variable_declaration(p: &mut Parser) -> Result<(), ()> {
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
