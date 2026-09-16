use rowan::{GreenNode, TextRange};

use crate::{LanguageKind, diagnostics::DiagnosticCode, java, kotlin, lang};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
    /// The diagnostic code of the error, when the underlying kind is
    /// structured enough to carry one (free-text messages have none; Kotlin
    /// kinds currently all degrade to `None`).
    pub code: Option<DiagnosticCode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Parse {
    pub green: Option<GreenNode>,
    pub errors: Vec<SyntaxError>,
}

#[derive(Debug, Clone)]
pub enum SourceFile {
    Java(java::SourceFile),
    Kotlin(kotlin::SourceFile),
}

impl SourceFile {
    pub fn parse(language: LanguageKind, text: &str) -> Parse {
        lang::for_kind(language).map_or_else(Parse::empty, |lang| lang.parse(language, text))
    }

    pub fn language(&self) -> LanguageKind {
        lang::kind_of(self)
    }
}

impl Parse {
    pub fn empty() -> Parse {
        Parse {
            green: None,
            errors: Vec::new(),
        }
    }

    pub fn errors(&self) -> &[SyntaxError] {
        &self.errors
    }

    /// Re-attaches the cached green tree to a language-specific syntax node.
    pub fn syntax_node(&self, language: LanguageKind) -> SourceFile {
        let green = self
            .green
            .clone()
            .expect("empty parse result has no syntax tree");
        lang::for_kind(language)
            .expect("cannot create a syntax node for an unknown language")
            .syntax_node(language, green)
    }
}
