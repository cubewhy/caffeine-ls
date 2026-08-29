use crate::{
    Parser,
    SyntaxKind::*,
    grammar::{
        annotations::annotation,
        decl::{at_declaration, declaration, multi_variable_declaration, variable_declaration},
        eat_nl,
        expr::{at_expression, expression},
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

    // A `=`, `+=`, ... after an expression turns it into an assignment.
    let m = p.start();
    expression(p);
    if at_assignment_operator(p) {
        p.bump();
        eat_nl(p);
        expression(p);
        m.complete(p, ASSIGNMENT_STATEMENT);
    } else {
        m.complete(p, EXPRESSION_STATEMENT);
    }
}

fn at_assignment_operator(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(EQUAL | PLUS_EQUAL | MINUS_EQUAL | MUL_EQUAL | DIV_EQUAL | MODULO_EQUAL)
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
