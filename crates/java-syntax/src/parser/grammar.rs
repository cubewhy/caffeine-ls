use crate::{kinds::SyntaxKind::*, parser::Parser};

pub fn root(p: &mut Parser) {
    // the root node
    let m = p.start();

    while !p.is_at_end() {
        item(p);
    }

    m.complete(p, ROOT);
}

fn item(p: &mut Parser) {
    // TODO: parse tree
}
