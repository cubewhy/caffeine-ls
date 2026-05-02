use crate::{
    kinds::SyntaxKind::*,
    parser::{
        Parser,
        grammar::{decl, names::qualified_name},
    },
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
        Some(_) => decl::decl(p),
        None => {}
    }
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
