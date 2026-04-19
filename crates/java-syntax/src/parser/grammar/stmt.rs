use crate::ContextualKeyword;
use crate::grammar::decl::{
    class_decl_rest, enum_decl_rest, interface_decl_rest, record_decl_rest,
};
use crate::kinds::SyntaxKind::*;
use crate::parser::Parser;

pub fn method_body_or_semicolon(p: &mut Parser) {
    if p.at(L_BRACE) {
        // {
        block(p);
    } else {
        // ;
        p.expect(SEMICOLON);
    }
}

/// Parse a block
///
/// https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html
pub fn block(p: &mut Parser) {
    let m = p.start();
    p.expect(L_BRACE);

    while !p.is_at_end() && !p.at(R_BRACE) {
        block_statement(p);
    }

    p.expect(R_BRACE);
    m.complete(p, BLOCK);
}

fn block_statement(p: &mut Parser) {
    let m = p.start();

    // Local Class and Interface Declarations
    // https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3
    if p.at(CLASS_KW) {
        class_decl_rest(p, m);
    } else if p.at(ENUM_KW) {
        enum_decl_rest(p, m);
    } else if p.at(INTERFACE_KW) {
        interface_decl_rest(p, m);
    } else if p.at_contextual_kw(ContextualKeyword::Record) {
        record_decl_rest(p, m);
    } else {
        // TODO: statements
    }
}
