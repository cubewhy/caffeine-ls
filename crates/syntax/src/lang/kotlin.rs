//! Kotlin (KLS) as the syntax layer sees it: a `kotlinFile` and, for a `.kts`
//! script, the `script` production ([spec: grammar-rule-script]).

use rowan::{GreenNode, SyntaxNode};

use crate::{LanguageKind, Parse, SourceFile, SyntaxError, diagnostics::DiagnosticCode, kotlin};

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

fn syntax_code(_kind: &kotlin::SyntaxErrorKind) -> Option<DiagnosticCode> {
    // TODO: add kotlin syntax code
    None
}
