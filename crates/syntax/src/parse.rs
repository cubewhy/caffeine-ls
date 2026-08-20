use rowan::{GreenNode, TextRange};

use crate::{KotlinDiagnosticCode, LanguageKind, diagnostics::DiagnosticCode, java, kotlin};

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
        match language {
            LanguageKind::Java => Parse::from_java(java::SourceFile::parse(text)),
            LanguageKind::Kotlin => Parse::from_kotlin(kotlin::SourceFile::parse(text)),
            LanguageKind::Unknown => Parse::empty(),
        }
    }

    pub fn language(&self) -> LanguageKind {
        match self {
            SourceFile::Java(_) => LanguageKind::Java,
            SourceFile::Kotlin(_) => LanguageKind::Kotlin,
        }
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
        match language {
            LanguageKind::Java => SourceFile::Java(java::SourceFile {
                syntax_node: rowan::SyntaxNode::new_root(green),
            }),
            LanguageKind::Kotlin => SourceFile::Kotlin(kotlin::SourceFile {
                syntax_node: rowan::SyntaxNode::new_root(green),
            }),
            LanguageKind::Unknown => {
                panic!("cannot create a syntax node for an unknown language")
            }
        }
    }

    fn from_java(parse: java::Parse<java::SourceFile>) -> Parse {
        let (green, errors) = parse.into();
        let errors = errors.into_iter().map(java_syntax_error).collect();
        Parse {
            green: Some(green),
            errors,
        }
    }

    fn from_kotlin(parse: kotlin::Parse<kotlin::SourceFile>) -> Parse {
        let (green, errors) = parse.into();
        let errors = errors.into_iter().map(kotlin_syntax_error).collect();
        Parse {
            green: Some(green),
            errors,
        }
    }
}

fn java_syntax_error(err: java::SyntaxError) -> SyntaxError {
    let code = DiagnosticCode::from_java_syntax(&err.kind);
    let message = err.kind.desc();

    SyntaxError {
        message,
        range: err.range,
        code,
    }
}

fn kotlin_syntax_error(err: kotlin::SyntaxError) -> SyntaxError {
    let code = DiagnosticCode::from_kotlin_syntax(&err.kind);
    let message = err.kind.desc();

    SyntaxError {
        message,
        range: err.range,
        code,
    }
}
