use crate::{
    ContextualKeyword, Parser,
    SyntaxKind::*,
    grammar::{decl::declaration, eat_nl, names::identifier, semis},
};

/// `kotlinFile`: [shebangLine] {NL} {fileAnnotation} packageHeader importList
///               {topLevelObject} EOF
/// [spec: grammar-rule-kotlinFile] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-kotlinFile
pub fn root(p: &mut Parser) {
    let m = p.start();

    if p.at(SHEBANG_LINE) {
        p.bump();
        eat_nl(p);
    }

    eat_nl(p);

    while at_file_annotation(p) {
        file_annotation(p);
    }

    package_header(p);
    import_list(p);

    while !p.is_at_end() {
        if p.at(SEMICOLON) || p.at(NEWLINE) {
            semis(p);
            continue;
        }
        top_level_object(p);
    }

    m.complete(p, ROOT);
}

/// `fileAnnotation` starts with `@file:`.
fn at_file_annotation(p: &Parser) -> bool {
    p.at(AT) && p.nth_at_contextual_kw(1, ContextualKeyword::File) && p.nth(2) == Some(COLON)
}

/// `fileAnnotation`: ('@' 'file' ':' {NL} (('[' unescapedAnnotation
///                    {unescapedAnnotation} ']') | unescapedAnnotation)) {NL}
/// [spec: grammar-rule-fileAnnotation] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-fileAnnotation
fn file_annotation(p: &mut Parser) {
    let m = p.start();
    p.expect(AT);
    p.expect_contextual_kw(ContextualKeyword::File);
    p.expect(COLON);
    eat_nl(p);

    if p.at(L_BRACKET) {
        p.bump();
        eat_nl(p);
        crate::parser::grammar::annotations::unescaped_annotation(p);
        eat_nl(p);
        while p.at(IDENTIFIER) {
            crate::parser::grammar::annotations::unescaped_annotation(p);
            eat_nl(p);
        }
        p.expect(R_BRACKET);
    } else {
        crate::parser::grammar::annotations::unescaped_annotation(p);
    }

    eat_nl(p);
    m.complete(p, FILE_ANNOTATION);
}

/// `packageHeader`: ['package' identifier ['semi']]
/// [spec: grammar-rule-packageHeader] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-packageHeader
fn package_header(p: &mut Parser) {
    if !p.at(PACKAGE_KW) {
        return;
    }

    let m = p.start();
    p.bump();
    identifier(p);
    if p.at(SEMICOLON) {
        p.bump();
    }
    eat_nl(p);
    m.complete(p, PACKAGE_HEADER);
}

/// `importList`: {importHeader}
/// [spec: grammar-rule-importList] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-importList
fn import_list(p: &mut Parser) {
    let m = p.start();
    while p.at_contextual_kw(ContextualKeyword::Import) {
        import_header(p);
    }
    m.complete(p, IMPORT_LIST);
}

/// `importHeader`: 'import' identifier [('.' '*') | importAlias] ['semi']
/// [spec: grammar-rule-importHeader] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-importHeader
fn import_header(p: &mut Parser) {
    let m = p.start();
    p.expect_contextual_kw(ContextualKeyword::Import);
    let path = p.start();
    identifier(p);
    if p.at(DOT) && p.nth(1) == Some(STAR) {
        p.bump();
        p.bump();
    } else if p.at(AS_KW) {
        let alias = p.start();
        p.bump();
        crate::parser::grammar::names::simple_identifier(p);
        alias.complete(p, IMPORT_ALIAS);
    }
    path.complete(p, IMPORT_PATH);
    if p.at(SEMICOLON) {
        p.bump();
    }
    eat_nl(p);
    m.complete(p, IMPORT_HEADER);
}

/// `topLevelObject`: declaration [semis]
/// [spec: grammar-rule-topLevelObject] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-topLevelObject
fn top_level_object(p: &mut Parser) {
    declaration(p);
    semis(p);
}
