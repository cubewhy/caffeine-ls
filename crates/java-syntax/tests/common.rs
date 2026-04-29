#![allow(unused)]
use java_syntax::{Event, LexicalError, ParseError, Parser, Token, grammar, lex};

pub fn collect_lex(src: &str) -> (Vec<Token<'_>>, Vec<LexicalError>) {
    match lex(src) {
        Ok(tokens) => (tokens, vec![]),
        Err((tokens, errors)) => (tokens, errors),
    }
}

pub fn dump_tokens(tokens: &[Token<'_>]) -> String {
    let mut out = String::new();
    for tok in tokens {
        out.push_str(&format!("{:?} {:?}\n", tok.kind, tok.lexeme));
    }
    out
}

pub fn dump_events(events: &[Event]) -> String {
    let mut out = String::new();
    for ev in events {
        match ev {
            Event::Tombstone => out.push_str("Tombstone\n"),
            Event::AddToken => out.push_str("AddToken\n"),
            Event::AddVirtualToken { kind, lexeme } => {
                out.push_str(&format!("AddVirtualToken({kind:?}, {lexeme:?})\n"))
            }
            Event::AdvanceSource => out.push_str("AdvanceSource\n"),
            Event::FinishNode => out.push_str("FinishNode\n"),
            Event::Error(err) => out.push_str(&format!("Error({err:?})\n")),
            Event::StartNode {
                kind,
                forward_parent,
            } => {
                out.push_str(&format!(
                    "StartNode({kind:?}, forward_parent={forward_parent:?})\n"
                ));
            }
        }
    }
    out
}

pub fn dump_lex_errors(errors: &[LexicalError]) -> String {
    if errors.is_empty() {
        return "<none>\n".to_string();
    }

    let mut out = String::new();
    for err in errors {
        out.push_str(&format!("{err:?}\n"));
    }
    out
}

pub fn dump_parse_errors(errors: &[ParseError]) -> String {
    if errors.is_empty() {
        return "<none>\n".to_string();
    }

    let mut out = String::new();
    for err in errors {
        out.push_str(&format!("{err:?}\n"));
    }
    out
}

pub fn check_lexer(src: &str) -> String {
    let (tokens, lex_errors) = collect_lex(src);
    format!(
        "\
SOURCE:
{src}
TOKENS:
{}
LEX_ERRORS:
{}",
        dump_tokens(&tokens),
        dump_lex_errors(&lex_errors),
    )
}

pub fn check_events(src: &str) -> String {
    let (tokens, lex_errors) = collect_lex(src);
    let mut p = Parser::new(tokens.clone());
    grammar::partial(&mut p, java_syntax::EntryPoint::Root);

    format!(
        "\
SOURCE:
{src}
TOKENS:
{}
LEX_ERRORS:
{}
EVENTS:
{}
PARSE_ERRORS:
{}",
        dump_tokens(&tokens),
        dump_lex_errors(&lex_errors),
        dump_events(&p.events),
        dump_parse_errors(&p.errors),
    )
}

pub fn check_parser(src: &str) -> String {
    let (tokens, lex_errors) = collect_lex(src);
    let mut p = Parser::new(tokens.clone());
    let parse = p.parse(java_syntax::EntryPoint::Root);

    format!(
        "\
SOURCE:
{src}
TOKENS:
{}
LEX_ERRORS:
{}
SYNTAX_TREE:
{}
",
        dump_tokens(&tokens),
        dump_lex_errors(&lex_errors),
        parse.debug_dump(),
    )
}

pub fn check_lexer_ok(src: &str) -> String {
    let (tokens, lex_errors) = collect_lex(src);
    assert!(
        lex_errors.is_empty(),
        "lexing failed unexpectedly for input:\n{src}\n\nTOKENS:\n{}LEX_ERRORS:\n{}",
        dump_tokens(&tokens),
        dump_lex_errors(&lex_errors),
    );

    format!(
        "\
SOURCE:
{src}
TOKENS:
{}
LEX_ERRORS:
{}",
        dump_tokens(&tokens),
        dump_lex_errors(&lex_errors),
    )
}

pub fn check_lexer_error(src: &str) -> String {
    let (tokens, lex_errors) = collect_lex(src);
    assert!(
        !lex_errors.is_empty(),
        "expected lexing to fail, but it succeeded for input:\n{src}\n\nTOKENS:\n{}",
        dump_tokens(&tokens),
    );

    format!(
        "\
SOURCE:
{src}
TOKENS:
{}
LEX_ERRORS:
{}",
        dump_tokens(&tokens),
        dump_lex_errors(&lex_errors),
    )
}

macro_rules! lexer_snapshot {
    ($name:ident, $src:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = crate::common::check_lexer_ok($src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

macro_rules! lexer_error_snapshot {
    ($name:ident, $src:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = crate::common::check_lexer_error($src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

macro_rules! lexer_snapshot {
    ($name:ident, $src:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = crate::common::check_lexer($src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

macro_rules! event_snapshot {
    ($name:ident, $src:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = crate::common::check_events($src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

macro_rules! parser_snapshot {
    ($name:ident, $src:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = crate::common::check_parser($src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

pub(crate) use event_snapshot;
pub(crate) use lexer_error_snapshot;
pub(crate) use lexer_snapshot;
pub(crate) use parser_snapshot;
