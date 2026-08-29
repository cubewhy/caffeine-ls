use crate::{
    Parser, SyntaxKind,
    SyntaxKind::*,
    grammar::{eat_nl, names::simple_identifier},
};

/// `annotation`: (singleAnnotation | multiAnnotation) {NL}
/// [spec: grammar-rule-annotation] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-annotation
///
/// Annotation arguments are expressions; full argument parsing lands with the
/// expression grammar. Until then, a balanced-paren skip keeps parsing alive.
pub(crate) fn annotation(p: &mut Parser) {
    let m = p.start();
    p.expect(AT);
    if p.nth(1) == Some(IDENTIFIER) && p.nth(2) == Some(COLON) {
        // annotationUseSiteTarget: @get: ..., @field: ...
        simple_identifier(p);
        p.bump(); // colon
        eat_nl(p);
    }
    unescaped_annotation(p);
    eat_nl(p);
    m.complete(p, ANNOTATION);
}

/// `unescapedAnnotation`: constructorInvocation | userType
/// [spec: grammar-rule-unescapedAnnotation] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-unescapedAnnotation
pub(crate) fn unescaped_annotation(p: &mut Parser) {
    crate::parser::grammar::types::user_type(p);
    if p.at(L_PAREN) {
        skip_balanced(p, L_PAREN, R_PAREN);
    }
}

/// Consumes a balanced pair of delimiters (bail-out until the expression
/// grammar provides `valueArguments`).
fn skip_balanced(p: &mut Parser, open: SyntaxKind, close: SyntaxKind) {
    let mut depth = 0usize;
    loop {
        match p.current() {
            Some(k) if k == open => depth += 1,
            Some(k) if k == close => {
                depth -= 1;
                if depth == 0 {
                    p.bump();
                    return;
                }
            }
            Some(EOF) | None => return,
            _ => {}
        }
        p.bump();
    }
}
