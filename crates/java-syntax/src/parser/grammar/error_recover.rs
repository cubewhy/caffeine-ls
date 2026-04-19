use crate::{kinds::SyntaxKind, kinds::SyntaxKind::*, parser::Parser};

pub fn recover_until(p: &mut Parser, recovery: &[SyntaxKind]) {
    while !p.is_at_end() {
        match p.current() {
            Some(kind) if recovery.contains(&kind) => break,
            Some(_) => p.bump(),
            None => break,
        }
    }
}

pub fn recover_until_or_eat(p: &mut Parser, recovery: &[SyntaxKind], eat_if_present: SyntaxKind) {
    recover_until(p, recovery);
    p.eat(eat_if_present);
}

pub fn recover_decl(p: &mut Parser) {
    recover_until_or_eat(
        p,
        &[
            SEMICOLON,
            R_BRACE,
            PACKAGE_KW,
            IMPORT_KW,
            CLASS_KW,
            INTERFACE_KW,
            ENUM_KW,
        ],
        SEMICOLON,
    );
}

pub fn recover_member(p: &mut Parser) {
    recover_until_or_eat(
        p,
        &[
            SEMICOLON,
            R_BRACE,
            CLASS_KW,
            INTERFACE_KW,
            ENUM_KW,
            PUBLIC_KW,
            PRIVATE_KW,
            PROTECTED_KW,
            STATIC_KW,
            FINAL_KW,
            ABSTRACT_KW,
            DEFAULT_KW,
            AT,
        ],
        SEMICOLON,
    );
}

pub fn recover_parameter(p: &mut Parser) {
    recover_until(p, &[COMMA, R_PAREN]);
}

pub fn recover_type_bound(p: &mut Parser) {
    recover_until(
        p,
        &[
            COMMA,
            GREATER,
            BIT_AND,
            L_BRACE,
            L_PAREN,
            IMPLEMENTS_KW,
            EXTENDS_KW,
        ],
    );
}
