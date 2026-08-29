use crate::{
    Parser,
    SyntaxKind::*,
    grammar::{
        decl::declaration,
        eat_nl,
        expr::{at_expression, expression},
        semis,
    },
};

/// `statements`: [statement {semis statement}] [semis]
/// [spec: grammar-rule-statements] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-statements
pub(crate) fn statements(p: &mut Parser) {
    eat_nl(p);
    while at_statement(p) {
        statement(p);
        if !(p.at(SEMICOLON) || p.at(NEWLINE)) {
            break;
        }
        semis(p);
        eat_nl(p);
    }
}

/// Whether the current token can start a statement.
pub(crate) fn at_statement(p: &Parser) -> bool {
    at_declaration(p) || at_expression(p)
}

fn at_declaration(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(CLASS_KW | INTERFACE_KW | OBJECT_KW | FUN_KW | VAL_KW | VAR_KW | TYPEALIAS_KW)
    )
}

/// `statement`: {label | annotation} (declaration | assignment |
///              loopStatement | expression)
/// [spec: grammar-rule-statement] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-statement
pub(crate) fn statement(p: &mut Parser) {
    if at_declaration(p) {
        declaration(p);
        return;
    }

    let m = p.start();
    expression(p);
    m.complete(p, EXPRESSION_STATEMENT);
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
