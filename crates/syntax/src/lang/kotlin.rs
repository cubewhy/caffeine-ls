//! Kotlin (KLS) as the syntax layer sees it: a `kotlinFile` and, for a `.kts`
//! script, the `script` production ([spec: grammar-rule-script]).

use rowan::{GreenNode, SyntaxNode};

use crate::{
    KotlinDiagnosticCode, LanguageKind, Parse, SourceFile, SyntaxError,
    diagnostics::DiagnosticCode,
    kotlin::{self, LexicalErrorKind, ParseErrorKind},
};

pub(crate) struct Kotlin;

pub(crate) static KOTLIN: Kotlin = Kotlin;

impl super::LanguageSyntax for Kotlin {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Kotlin, LanguageKind::KotlinScript]
    }

    fn file_extensions(&self) -> &'static [(&'static str, LanguageKind)] {
        &[
            ("kts", LanguageKind::KotlinScript),
            ("kt", LanguageKind::Kotlin),
        ]
    }

    fn language_id(&self) -> &'static str {
        "kotlin"
    }

    fn name(&self) -> &'static str {
        "kotlin"
    }

    fn parse(&self, kind: LanguageKind, text: &str) -> Parse {
        // `.kts` files are parsed with the KLS `script` grammar
        // ([spec: grammar-rule-script]).
        let parse = match kind {
            LanguageKind::KotlinScript => kotlin::SourceFile::parse_script(text),
            LanguageKind::Kotlin => kotlin::SourceFile::parse(text),
            kind => unreachable!("kotlin syntax does not answer for {kind:?}"),
        };
        let (green, errors) = parse.into();
        Parse {
            green: Some(green),
            errors: errors.into_iter().map(syntax_error).collect(),
        }
    }

    fn syntax_node(&self, _kind: LanguageKind, green: GreenNode) -> SourceFile {
        SourceFile::Kotlin(kotlin::SourceFile {
            syntax_node: SyntaxNode::new_root(green),
        })
    }
}

fn syntax_error(err: kotlin::SyntaxError) -> SyntaxError {
    let code = syntax_code(&err.kind);
    let message = err.kind.desc();

    SyntaxError {
        message,
        range: err.range,
        code,
    }
}

/// The code of a syntax error, so a client keys on it independently of the
/// message's wording ([`KotlinDiagnosticCode`]), exactly as the Java half does
/// ([`crate::lang::java`]).
///
/// A lexer error is the diagnostic it names; a parser error is the production
/// it was raised for — the expected token, the expected contextual keyword, the
/// expected construct — and a *recovery* message ([`ParseErrorKind::Message`])
/// is [`KotlinDiagnosticCode::SyntaxError`], the one shape no narrower code
/// describes.
fn syntax_code(kind: &kotlin::SyntaxErrorKind) -> Option<DiagnosticCode> {
    let code = match kind {
        kotlin::SyntaxErrorKind::Lexer(kind) => match kind {
            LexicalErrorKind::UnterminatedBlockComment => {
                KotlinDiagnosticCode::UnterminatedBlockComment
            }
            LexicalErrorKind::UnterminatedString => KotlinDiagnosticCode::UnterminatedString,
            LexicalErrorKind::EmptyCharLiteral => KotlinDiagnosticCode::EmptyCharLiteral,
            LexicalErrorKind::UnterminatedCharLiteral => {
                KotlinDiagnosticCode::UnterminatedCharLiteral
            }
            LexicalErrorKind::TooManyCharsInCharLiteral => {
                KotlinDiagnosticCode::TooManyCharsInCharLiteral
            }
            LexicalErrorKind::UnsupportedEscapeSequence => {
                KotlinDiagnosticCode::UnsupportedEscapeSequence
            }
            LexicalErrorKind::EmptyIdentifier => KotlinDiagnosticCode::EmptyIdentifier,
            LexicalErrorKind::UnterminatedIdentifier => {
                KotlinDiagnosticCode::UnterminatedIdentifier
            }
            LexicalErrorKind::UnexpectedChar(_) => KotlinDiagnosticCode::UnexpectedChar,
            LexicalErrorKind::LeadingZerosNotAllowed => {
                KotlinDiagnosticCode::LeadingZerosNotAllowed
            }
            LexicalErrorKind::WrongLongSuffixCase => KotlinDiagnosticCode::WrongLongSuffixCase,
            LexicalErrorKind::IllegalUnderscore => KotlinDiagnosticCode::IllegalUnderscore,
            LexicalErrorKind::MissingExponentDigits => KotlinDiagnosticCode::MissingExponentDigits,
            LexicalErrorKind::MissingNumericDigits => KotlinDiagnosticCode::MissingNumericDigits,
        },
        kotlin::SyntaxErrorKind::Parser(kind) => match kind {
            ParseErrorKind::ExpectedToken { .. } => KotlinDiagnosticCode::ExpectedToken,
            ParseErrorKind::ExpectedContextualKeyword { .. } => {
                KotlinDiagnosticCode::ExpectedKeyword
            }
            ParseErrorKind::ExpectedConstruct(_) => KotlinDiagnosticCode::ExpectedConstruct,
            ParseErrorKind::Message(_) => KotlinDiagnosticCode::SyntaxError,
        },
    };
    Some(DiagnosticCode::Kotlin(code))
}
