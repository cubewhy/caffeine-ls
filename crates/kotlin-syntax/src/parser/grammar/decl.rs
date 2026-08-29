use crate::{
    Parser,
    SyntaxKind::*,
    grammar::{eat_nl, names::simple_identifier},
    parser::ExpectedConstruct,
};

/// `variableDeclaration`: {annotation} {NL} simpleIdentifier [{NL} ':' {NL} type]
/// [spec: grammar-rule-variableDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-variableDeclaration
pub(crate) fn variable_declaration(p: &mut Parser) {
    let m = p.start();
    simple_identifier(p);
    if p.at(COLON) {
        p.bump();
        eat_nl(p);
        crate::parser::grammar::types::type_(p);
    }
    m.complete(p, VARIABLE_DECLARATION);
}

/// `declaration`: classDeclaration | objectDeclaration | functionDeclaration
///                | propertyDeclaration | typeAlias
/// [spec: grammar-rule-declaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-declaration
pub(crate) fn declaration(p: &mut Parser) {
    eat_nl(p);

    if p.at(CLASS_KW)
        || p.at(INTERFACE_KW)
        || p.at(OBJECT_KW)
        || p.at(FUN_KW)
        || p.at(VAL_KW)
        || p.at(VAR_KW)
        || p.at(TYPEALIAS_KW)
    {
        // Filled in by later milestones.
        p.error_expected_construct(ExpectedConstruct::Declaration);
        p.bump();
        return;
    }

    p.error_expected_construct(ExpectedConstruct::Declaration);
    p.bump();
}
