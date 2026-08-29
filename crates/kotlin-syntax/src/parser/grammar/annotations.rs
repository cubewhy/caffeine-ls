use crate::{
    Parser, SyntaxKind,
    SyntaxKind::*,
    grammar::{eat_nl, names::simple_identifier},
};

/// `annotation`: (singleAnnotation | multiAnnotation) {NL}
/// [spec: grammar-rule-annotation] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-annotation
///
/// The lexer does not distinguish the whitespace-flavoured `@` terminals
/// (`AT_NO_WS`, `AT_PRE_WS`, `AT_POST_WS`), so a plain `AT` token is accepted
/// for all of them.
pub(crate) fn annotation(p: &mut Parser) {
    let m = p.start();
    p.expect(AT);

    // annotationUseSiteTarget: `@get:` / `@field:` / ...
    if p.at(IDENTIFIER) && p.nth(1) == Some(COLON) {
        let target = p.start();
        simple_identifier(p);
        p.expect(COLON);
        eat_nl(p);
        target.complete(p, ANNOTATION_USE_SITE_TARGET);
    }

    if p.at(L_BRACKET) {
        // multiAnnotation: '@' '[' (unescapedAnnotation {unescapedAnnotation}) ']'
        p.bump();
        eat_nl(p);
        unescaped_annotation(p);
        eat_nl(p);
        while at_annotation_body(p) {
            unescaped_annotation(p);
            eat_nl(p);
        }
        p.expect(R_BRACKET);
    } else {
        unescaped_annotation(p);
    }

    eat_nl(p);
    m.complete(p, ANNOTATION);
}

/// The body of an annotation is either a user type name or a
/// `constructorInvocation`; inside `[...]` brackets the leading `@` is
/// shared, so a bare `IDENTIFIER` starts the next one.
fn at_annotation_body(p: &Parser) -> bool {
    p.at(IDENTIFIER)
}

/// `unescapedAnnotation`: constructorInvocation | userType
/// [spec: grammar-rule-unescapedAnnotation] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-unescapedAnnotation
///
/// `constructorInvocation` needs `valueArguments`, which arrives with the
/// expression grammar; a balanced-paren skip keeps the tree well-formed
/// until then.
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
