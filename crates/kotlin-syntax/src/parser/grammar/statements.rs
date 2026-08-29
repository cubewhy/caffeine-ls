use crate::{
    Parser,
    SyntaxKind::*,
    grammar::{
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
    crate::parser::grammar::decl::at_declaration(p) || at_expression(p)
}

/// `statement`: {label | annotation} (declaration | assignment |
///              loopStatement | expression)
/// [spec: grammar-rule-statement] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-statement
pub(crate) fn statement(p: &mut Parser) {
    if crate::parser::grammar::decl::at_declaration(p) {
        crate::parser::grammar::decl::declaration(p);
        return;
    }

    let m = p.start();
    expression(p);
    m.complete(p, EXPRESSION_STATEMENT);
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
