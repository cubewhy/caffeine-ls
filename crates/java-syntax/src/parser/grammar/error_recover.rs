use crate::{
    parser::Parser,
    syntax_kind::SyntaxKind::{self, *},
    tokenset,
};

fn recover_until_internal<F>(p: &mut Parser, recovery: &[SyntaxKind], mut is_contextual: F)
where
    F: FnMut(&mut Parser) -> bool,
{
    while !p.is_at_end() {
        if let Some(kind) = p.current()
            && (recovery.contains(&kind) || is_contextual(p))
        {
            break;
        }
        p.bump();
    }
}

pub fn recover_until(p: &mut Parser, recovery: &[SyntaxKind]) {
    recover_until_internal(p, recovery, |_| false);
}

pub fn recover_until_or_eat(p: &mut Parser, recovery: &[SyntaxKind], eat_if_present: SyntaxKind) {
    recover_until(p, recovery);
    p.eat(eat_if_present);
}

pub fn recover_decl(p: &mut Parser) {
    recover_until(
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
    );
    if p.at_set(tokenset![R_BRACE]) {
        p.bump();
    }
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

pub fn recover_catch_parameter(p: &mut Parser) {
    recover_until(p, &[R_PAREN, L_BRACE]);
}

pub fn recover_annotation_type_parameter(p: &mut Parser) {
    recover_until(p, &[R_PAREN, SEMICOLON, R_BRACE]);

    if p.at(R_PAREN) {
        p.bump(); // )
    }

    if p.at(SEMICOLON) {
        p.bump(); // ;
    }
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

pub fn recover_type_argument(p: &mut Parser) {
    recover_until(p, &[COMMA, GREATER]);
}

pub fn recover_block_statement(p: &mut Parser) {
    let recovery_kinds = &[
        SEMICOLON,
        R_BRACE,
        L_BRACE,
        IF_KW,
        WHILE_KW,
        DO_KW,
        FOR_KW,
        TRY_KW,
        SWITCH_KW,
        SYNCHRONIZED_KW,
        RETURN_KW,
        THROW_KW,
        BREAK_KW,
        CONTINUE_KW,
        ASSERT_KW,
        CLASS_KW,
        INTERFACE_KW,
        ENUM_KW,
    ];

    let is_contextual_kw = |p: &mut Parser| {
        let Some(lexeme) = p.current_lexeme() else {
            return false;
        };
        p.at(IDENTIFIER) && matches!(lexeme, "var" | "yield" | "record")
    };

    recover_until_internal(p, recovery_kinds, is_contextual_kw);

    p.eat(SEMICOLON);
}

pub fn recover_switch_statement(p: &mut Parser) {
    recover_until(p, &[CASE_KW, DEFAULT_KW, L_BRACE, R_BRACE]);
}
