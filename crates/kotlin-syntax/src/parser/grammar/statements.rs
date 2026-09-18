use crate::{
    Parser,
    SyntaxKind::*,
    grammar::{
        annotations::annotation,
        decl::{at_declaration, declaration, multi_variable_declaration, variable_declaration},
        eat_nl,
        expr::{
            at_expression, expression, postfix_unary_expression, postfix_unary_suffixes,
            prefix_unary_expression,
        },
        names::simple_identifier,
        semis,
    },
};

/// `statements`: [statement {semis statement}] [semis]
/// [spec: grammar-rule-statements] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-statements
pub(crate) fn statements(p: &mut Parser) {
    eat_nl(p);
    while at_statement(p) {
        statement(p);
        // Statement separators. Declaration parsers may already have
        // consumed the trailing NLs, so continue whenever another statement
        // visibly follows.
        while p.at(SEMICOLON) || p.at(NEWLINE) {
            p.bump();
        }
        eat_nl(p);
    }
}

/// Whether the current token can start a statement.
///
/// `statement`: {label | annotation} (declaration | assignment |
///              loopStatement | expression)
/// [spec: grammar-rule-statement] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-statement
///
/// An annotation (`@Foo`) may prefix a statement, so `AT` starts a statement
/// just like `declaration`/`loop`/`expression`.
pub(crate) fn at_statement(p: &Parser) -> bool {
    p.at(AT) || at_declaration(p) || at_loop(p) || at_expression(p)
}

fn at_loop(p: &Parser) -> bool {
    matches!(p.current(), Some(FOR_KW | WHILE_KW | DO_KW))
}

/// `statement`: {label | annotation} (declaration | assignment |
///              loopStatement | expression)
/// [spec: grammar-rule-statement] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-statement
pub(crate) fn statement(p: &mut Parser) {
    // Labels (`loop@`) and annotations may prefix the statement.
    loop {
        if p.at(IDENTIFIER) && p.nth(1) == Some(AT) {
            let l = p.start();
            simple_identifier(p);
            p.expect(AT);
            eat_nl(p);
            l.complete(p, LABEL);
        } else if p.at(AT) {
            annotation(p);
            eat_nl(p);
        } else {
            break;
        }
    }

    if at_declaration(p) {
        declaration(p);
        return;
    }
    if p.at(FOR_KW) {
        for_statement(p);
        return;
    }
    if p.at(WHILE_KW) {
        while_statement(p);
        return;
    }
    if p.at(DO_KW) {
        do_while_statement(p);
        return;
    }

    assignment_or_expression_statement(p);
}

/// The `assignment | expression` tail of `statement`
/// ([spec: grammar-rule-statement]).
///
/// `assignment`: ((directlyAssignableExpression '=') |
///                (assignableExpression assignmentAndOperator)) {NL} expression
/// [spec: grammar-rule-assignment] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-assignment
///
/// The destination is parsed by the assignable-expression productions rather
/// than by `expression`, so a left-hand side that is not a place never becomes
/// an `ASSIGNMENT_STATEMENT`: the whole of `a + b = c` is one
/// `BINARY_EXPRESSION`, which the assignable productions cannot read up to the
/// operator, so the statement is an `EXPRESSION_STATEMENT` followed by a
/// recovery for the operator.
///
/// That is a *deliberate deviation* from `kotlinc` 2.4.20, which accepts any
/// expression as the destination and rejects `a + b = c` later, with
/// `error: variable expected.` at the left-hand side. Reading the destination
/// here keeps the model honest — `hir-def`'s lowering reads an assignment's
/// first and last expression, so a misread assignment would be a wrong body
/// rather than a diagnostic — and costs only the erroneous forms, which the
/// compiler rejects too.
fn assignment_or_expression_statement(p: &mut Parser) {
    // `directlyAssignableExpression '='`
    // ([spec: grammar-rule-directlyAssignableExpression]).
    let cp = p.checkpoint();
    let m = p.start();
    p.enter_expression();
    let destination = directly_assignable_expression(p);
    p.leave_expression();
    if destination && p.at(EQUAL) {
        p.bump();
        eat_nl(p);
        expression(p);
        m.complete(p, ASSIGNMENT_STATEMENT);
        return;
    }
    m.abandon(p);
    p.rewind(cp);

    // `assignableExpression assignmentAndOperator`
    // ([spec: grammar-rule-assignableExpression]): the compound operators also
    // admit a prefix-unary expression, so `!flag += 1` is an assignment.
    let m = p.start();
    p.enter_expression();
    assignable_expression(p);
    p.leave_expression();
    if at_compound_assignment_operator(p) {
        p.bump();
        eat_nl(p);
        expression(p);
        m.complete(p, ASSIGNMENT_STATEMENT);
        return;
    }
    m.abandon(p);
    p.rewind(cp);

    // Not an assignment: an ordinary expression statement, with a recovery for
    // an operator whose destination neither production could read.
    let m = p.start();
    expression(p);
    if at_assignment_operator(p) {
        m.complete(p, EXPRESSION_STATEMENT);
        p.error_message("expected an assignable expression before the assignment operator");
        p.bump();
        eat_nl(p);
        return;
    }
    m.complete(p, EXPRESSION_STATEMENT);
}

/// `directlyAssignableExpression`: (postfixUnaryExpression assignableSuffix) |
///                                 simpleIdentifier |
///                                 parenthesizedDirectlyAssignableExpression
/// [spec: grammar-rule-directlyAssignableExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-directlyAssignableExpression
///
/// Returns whether the tokens consumed are one of those productions; the
/// caller rewinds when they are not. The first two alternatives overlap on a
/// bare identifier, and the postfix production settles which one applies from
/// the suffix the chain ends with — `a` is a `simpleIdentifier`, `a.b`,
/// `a[0]` and `a<b>` end in an `assignableSuffix`, and `f()`, `a!!` and
/// `a.b()` end in a suffix that is not one.
fn directly_assignable_expression(p: &mut Parser) -> bool {
    let is_simple_identifier = p.at(IDENTIFIER) && p.nth(1) != Some(COLON_COLON);

    if p.at(L_PAREN) {
        // `parenthesizedDirectlyAssignableExpression` — and, since the same
        // node is a `primaryExpression`, whatever suffix chain follows it:
        // `(a).b = v` is the first alternative with `(a)` as its primary, and
        // the chain must end in an `assignableSuffix` just as any other does.
        let cp = p.checkpoint();
        let m = p.start();
        p.bump(); // '('
        eat_nl(p);
        p.enter_expression();
        let inner = directly_assignable_expression(p);
        p.leave_expression();
        eat_nl(p);
        if !(inner && p.at(R_PAREN)) {
            m.abandon(p);
            p.rewind(cp);
            return false;
        }
        p.bump(); // ')'
        let completed = m.complete(p, PARENTHESIZED_EXPRESSION);

        return match postfix_unary_suffixes(p) {
            Some(last) => {
                completed.precede(p).complete(p, POSTFIX_UNARY_EXPRESSION);
                last.is_assignable()
            }
            None => true,
        };
    }

    match postfix_unary_expression(p) {
        Some(last) => last.is_assignable(),
        // With no suffix at all only `simpleIdentifier` is left — a bare
        // identifier is both a `primaryExpression` and the whole alternative.
        None => is_simple_identifier,
    }
}

/// `assignableExpression`: prefixUnaryExpression | parenthesizedAssignableExpression
/// [spec: grammar-rule-assignableExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-assignableExpression
///
/// `prefixUnaryExpression` is `{unaryPrefix} postfixUnaryExpression`
/// ([spec: grammar-rule-prefixUnaryExpression]), which every
/// `directlyAssignableExpression` also is, so this production always reads
/// something; what decides an assignment is whether the caller then finds an
/// `assignmentAndOperator` where the production stopped.
fn assignable_expression(p: &mut Parser) {
    if p.at(L_PAREN) && parenthesized_assignable_expression(p) {
        return;
    }
    prefix_unary_expression(p);
}

/// `parenthesizedAssignableExpression`:
/// `'(' {NL} assignableExpression {NL} ')'`
/// [spec: grammar-rule-parenthesizedAssignableExpression] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-parenthesizedAssignableExpression
///
/// Completes a `PARENTHESIZED_EXPRESSION` and returns whether the parentheses
/// held an `assignableExpression`; the caller rewinds and falls back to the
/// `prefixUnaryExpression` alternative when they did not, which is what makes
/// `(a + b) += v` the parenthesized *primary* of a prefix-unary expression
/// rather than a `parenthesizedAssignableExpression`.
fn parenthesized_assignable_expression(p: &mut Parser) -> bool {
    let cp = p.checkpoint();
    let m = p.start();
    p.bump(); // '('
    eat_nl(p);
    p.enter_expression();
    assignable_expression(p);
    p.leave_expression();
    eat_nl(p);
    if p.at(R_PAREN) {
        p.bump();
        m.complete(p, PARENTHESIZED_EXPRESSION);
        return true;
    }
    m.abandon(p);
    p.rewind(cp);
    false
}

fn at_assignment_operator(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(EQUAL | PLUS_EQUAL | MINUS_EQUAL | MUL_EQUAL | DIV_EQUAL | MODULO_EQUAL)
    )
}

/// `assignmentAndOperator`: '+=' | '-=' | '*=' | '/=' | '%='
/// [spec: grammar-rule-assignmentAndOperator] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-assignmentAndOperator
fn at_compound_assignment_operator(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(PLUS_EQUAL | MINUS_EQUAL | MUL_EQUAL | DIV_EQUAL | MODULO_EQUAL)
    )
}

/// `forStatement`: 'for' {NL} '(' {annotation} (variableDeclaration |
///                 multiVariableDeclaration) 'in' expression ')' {NL}
///                 [controlStructureBody]
/// [spec: grammar-rule-forStatement] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-forStatement
fn for_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(FOR_KW);
    eat_nl(p);
    p.expect(L_PAREN);
    eat_nl(p);
    if p.at(AT) {
        annotation(p);
        eat_nl(p);
    }
    if p.at(L_PAREN) {
        multi_variable_declaration(p);
    } else {
        variable_declaration(p);
    }
    eat_nl(p);
    p.expect(IN_KW);
    eat_nl(p);
    expression(p);
    eat_nl(p);
    p.expect(R_PAREN);
    eat_nl(p);

    if p.at(L_BRACE) {
        block(p);
    } else if at_statement(p) {
        statement(p);
    }

    m.complete(p, FOR_STATEMENT);
}

/// `whileStatement`: 'while' {NL} '(' {NL} expression {NL} ')' {NL}
///                   (controlStructureBody | ';')
/// [spec: grammar-rule-whileStatement] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-whileStatement
fn while_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(WHILE_KW);
    eat_nl(p);
    p.expect(L_PAREN);
    eat_nl(p);
    expression(p);
    eat_nl(p);
    p.expect(R_PAREN);
    eat_nl(p);

    if p.at(L_BRACE) {
        block(p);
    } else if p.at(SEMICOLON) {
        p.bump();
    } else if at_statement(p) {
        statement(p);
    }

    m.complete(p, WHILE_STATEMENT);
}

/// `doWhileStatement`: 'do' {NL} [controlStructureBody] {NL} 'while' {NL}
///                     '(' {NL} expression {NL} ')'
/// [spec: grammar-rule-doWhileStatement] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-doWhileStatement
fn do_while_statement(p: &mut Parser) {
    let m = p.start();
    p.expect(DO_KW);
    eat_nl(p);
    if p.at(L_BRACE) {
        block(p);
        eat_nl(p);
    } else if at_statement(p) {
        statement(p);
        eat_nl(p);
    }
    p.expect(WHILE_KW);
    eat_nl(p);
    p.expect(L_PAREN);
    eat_nl(p);
    expression(p);
    eat_nl(p);
    p.expect(R_PAREN);
    m.complete(p, DO_WHILE_STATEMENT);
}

/// `controlStructureBody`: block | statement
/// [spec: grammar-rule-controlStructureBody] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-controlStructureBody
pub(crate) fn control_structure_body(p: &mut Parser) {
    if p.at(L_BRACE) {
        block(p);
    } else {
        statement(p);
    }
}

/// `block`: '{' {NL} statements {NL} '}'
/// [spec: grammar-rule-block] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-block
pub(crate) fn block(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);
    statements(p);
    semis(p);
    eat_nl(p);
    p.expect(R_BRACE);
    m.complete(p, BLOCK);
}
