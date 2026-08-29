use crate::{Parser, SyntaxKind::*, grammar::eat_nl, parser::ExpectedConstruct};

/// `declaration`: classDeclaration | objectDeclaration | functionDeclaration
///                | propertyDeclaration | typeAlias
/// [spec: grammar-rule-declaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-declaration
pub(crate) fn declaration(p: &mut Parser) {
    eat_nl(p);

    if p.at(CLASS_KW)
        || (p.at(INTERFACE_KW))
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
