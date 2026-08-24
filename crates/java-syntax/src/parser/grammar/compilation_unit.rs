use crate::{
    parser::{
        Parser,
        grammar::{decl, modifiers::modifiers, names::qualified_name},
    },
    syntax_kind::SyntaxKind::*,
};

pub fn root(p: &mut Parser) {
    // the root node
    let m = p.start();

    while !p.is_at_end() {
        item(p);
    }

    m.complete(p, ROOT);
}

fn item(p: &mut Parser) {
    match p.current() {
        Some(PACKAGE_KW) => package_decl(p),
        Some(IMPORT_KW) => import_decl(p),
        // [JLS §7.4] a compilation unit may start with package annotations:
        // `@NullMarked package org.example;`
        Some(AT) if at_annotated_package(p) => {
            modifiers(p);
            package_decl(p);
        }
        Some(EOF) => {}
        Some(_) => decl::decl(p),
        None => {}
    }
}

/// Whether the annotations starting at the current `@` are directly followed
/// by a `package` declaration ([JLS §7.4]). Skips balanced annotation
/// arguments so element values containing parens do not confuse the scan.
fn at_annotated_package(p: &Parser) -> bool {
    let mut i = 0;
    while p.nth(i) == Some(AT) {
        i += 1;
        if !matches!(p.nth(i), Some(IDENTIFIER)) {
            return false;
        }
        i += 1;
        while p.nth(i) == Some(DOT) && p.nth(i + 1) == Some(IDENTIFIER) {
            i += 2;
        }
        if p.nth(i) == Some(L_PAREN) {
            let mut depth = 0;
            loop {
                match p.nth(i) {
                    Some(L_PAREN) => depth += 1,
                    Some(R_PAREN) => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    Some(EOF) | None => return false,
                    _ => {}
                }
                i += 1;
            }
        }
    }
    p.nth(i) == Some(PACKAGE_KW)
}

fn package_decl(p: &mut Parser) {
    // package <pkg>;
    let m = p.start();
    p.expect(PACKAGE_KW);
    qualified_name(p);
    p.expect(SEMICOLON);
    m.complete(p, PACKAGE_DECL);
}

fn import_decl(p: &mut Parser) {
    let m = p.start();

    p.expect(IMPORT_KW);
    p.eat(STATIC_KW);
    import_path(p);
    p.expect(SEMICOLON);

    m.complete(p, IMPORT_DECL);
}

fn import_path(p: &mut Parser) {
    let m = p.start();

    p.expect(IDENTIFIER);
    while p.eat(DOT) {
        if p.eat(STAR) {
            break;
        }
        p.expect(IDENTIFIER);
    }

    m.complete(p, IMPORT_PATH);
}
