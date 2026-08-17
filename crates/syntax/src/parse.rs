use rowan::{GreenNode, TextRange};

use crate::{LanguageKind, java, kotlin};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
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
    let message = match err.kind {
        java::SyntaxErrorKind::Lexer(kind) => match kind {
            java::LexicalErrorKind::UnexpectedChar(c) => {
                format!("Unexpected character '{c}' found in source code.")
            }
            java::LexicalErrorKind::UnterminatedString => {
                "Missing closing quote '\"' for string literal.".to_string()
            }
            java::LexicalErrorKind::UnterminatedComment => {
                "Missing closing '*/' for block comment.".to_string()
            }
            java::LexicalErrorKind::InvalidChar => {
                "Invalid character literal. Did you forget a closing quote '''?".to_string()
            }
            java::LexicalErrorKind::IllegalTextBlockOpen => {
                "Expected a newline immediately after opening a text block (\"\"\").".to_string()
            }
            java::LexicalErrorKind::UnterminatedTextBlock => {
                "Missing closing '\"\"\"' for text block.".to_string()
            }
            java::LexicalErrorKind::InvalidNumber => "Malformed number literal.".to_string(),
            java::LexicalErrorKind::InvalidUnicodeEscape => {
                "Invalid unicode escape sequence (expected format: \\uXXXX).".to_string()
            }
            java::LexicalErrorKind::UnterminatedChar => {
                "Missing closing quote ''' for character literal.".to_string()
            }
            java::LexicalErrorKind::InvalidEscapeSequence => {
                "Invalid escape sequence inside string or char literal.".to_string()
            }
            java::LexicalErrorKind::UnterminatedTemplate => {
                "Missing closing delimiter for string template.".to_string()
            }
        },
        java::SyntaxErrorKind::Parser(kind) => match kind {
            java::ParseErrorKind::ExpectedToken { expected, found } => {
                let found_str = found
                    .map(|f| format!("'{f}'"))
                    .unwrap_or_else(|| "end of file".to_string());

                let expected_options = expected
                    .iter()
                    .map(|e| {
                        let s = e.to_string();
                        if s.chars().any(|c| !c.is_alphanumeric()) || s.len() == 1 {
                            format!("'{s}'")
                        } else {
                            s
                        }
                    })
                    .collect::<Vec<_>>();

                let expected_msg = if expected_options.len() > 1 {
                    expected_options.join(" or ")
                } else {
                    expected_options.first().cloned().unwrap_or_default()
                };

                format!("Expected {expected_msg}, but found {found_str}.")
            }
            java::ParseErrorKind::ExpectedContextualKeyword { keyword, found } => {
                let found_str = found
                    .map(|f| f.to_string())
                    .unwrap_or_else(|| "end of file".to_string());
                format!(
                    "Expected keyword '{}', but found {found_str}.",
                    keyword.as_str()
                )
            }
            java::ParseErrorKind::ExpectedConstruct(expected_construct) => {
                let construct_str = expected_construct.to_string();
                format!("Expected {construct_str} here.")
            }
            java::ParseErrorKind::Message(msg) => msg.to_string(),
        },
    };

    SyntaxError {
        message,
        range: err.range,
    }
}

fn kotlin_kind_str(kind: kotlin::SyntaxKind) -> String {
    format!("{kind:?}")
}

fn kotlin_syntax_error(err: kotlin::SyntaxError) -> SyntaxError {
    let message = match err.kind {
        kotlin::SyntaxErrorKind::Lexer(kind) => match kind {
            kotlin::LexicalErrorKind::UnterminatedBlockComment => {
                "Missing closing '*/' for block comment.".to_string()
            }
            kotlin::LexicalErrorKind::UnterminatedString => {
                "Missing closing quote '\"' for string literal.".to_string()
            }
            kotlin::LexicalErrorKind::EmptyCharLiteral => "Empty character literal.".to_string(),
            kotlin::LexicalErrorKind::UnterminatedCharLiteral => {
                "Missing closing quote ''' for character literal.".to_string()
            }
            kotlin::LexicalErrorKind::TooManyCharsInCharLiteral => {
                "Too many characters in character literal.".to_string()
            }
            kotlin::LexicalErrorKind::UnsupportedEscapeSequence => {
                "Unsupported escape sequence inside string or char literal.".to_string()
            }
            kotlin::LexicalErrorKind::EmptyIdentifier => "Empty identifier.".to_string(),
            kotlin::LexicalErrorKind::UnterminatedIdentifier => {
                "Unterminated identifier.".to_string()
            }
            kotlin::LexicalErrorKind::UnexpectedChar(c) => {
                format!("Unexpected character '{c}' found in source code.")
            }
            kotlin::LexicalErrorKind::LeadingZerosNotAllowed => {
                "Leading zeros are not allowed in integer literals.".to_string()
            }
            kotlin::LexicalErrorKind::WrongLongSuffixCase => {
                "Use uppercase 'L' for the long literal suffix.".to_string()
            }
            kotlin::LexicalErrorKind::IllegalUnderscore => {
                "Illegal underscore in numeric literal.".to_string()
            }
            kotlin::LexicalErrorKind::MissingExponentDigits => {
                "Missing digits after exponent in numeric literal.".to_string()
            }
            kotlin::LexicalErrorKind::MissingNumericDigits => {
                "Missing digits in numeric literal.".to_string()
            }
        },
        kotlin::SyntaxErrorKind::Parser(kind) => match kind {
            kotlin::ParseErrorKind::ExpectedToken { expected, found } => {
                let found_str = found
                    .map(|f| format!("'{}'", kotlin_kind_str(f)))
                    .unwrap_or_else(|| "end of file".to_string());

                let expected_options = expected
                    .iter()
                    .map(|e| {
                        let s = kotlin_kind_str(*e);
                        if s.chars().any(|c| !c.is_alphanumeric()) || s.len() == 1 {
                            format!("'{s}'")
                        } else {
                            s
                        }
                    })
                    .collect::<Vec<_>>();

                let expected_msg = if expected_options.len() > 1 {
                    expected_options.join(" or ")
                } else {
                    expected_options.first().cloned().unwrap_or_default()
                };

                format!("Expected {expected_msg}, but found {found_str}.")
            }
            kotlin::ParseErrorKind::ExpectedContextualKeyword { keyword, found } => {
                let found_str = found
                    .map(kotlin_kind_str)
                    .unwrap_or_else(|| "end of file".to_string());
                format!(
                    "Expected keyword '{}', but found {found_str}.",
                    keyword.as_str()
                )
            }
            kotlin::ParseErrorKind::ExpectedConstruct(expected_construct) => {
                let construct_str = expected_construct.to_string();
                format!("Expected {construct_str} here.")
            }
            kotlin::ParseErrorKind::Message(msg) => msg.to_string(),
        },
    };

    SyntaxError {
        message,
        range: err.range,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_java_reports_errors() {
        let parse = SourceFile::parse(LanguageKind::Java, "public class Main {");
        assert!(!parse.errors.is_empty());
    }

    #[test]
    fn parse_kotlin_reports_errors() {
        let parse = SourceFile::parse(
            LanguageKind::Kotlin,
            "fun main() {\n    val s = \"unterminated\n}",
        );
        assert_eq!(parse.errors.len(), 1);
        assert_eq!(
            parse.errors[0].message,
            "Missing closing quote '\"' for string literal."
        );
    }

    #[test]
    fn parse_unknown_is_empty() {
        let parse = SourceFile::parse(LanguageKind::Unknown, "anything");
        assert!(parse.errors.is_empty());
        assert!(parse.green.is_none());
    }

    #[test]
    fn parse_without_errors() {
        let parse = SourceFile::parse(
            LanguageKind::Java,
            "public class Main {\n    public void m() {}\n}",
        );
        assert!(parse.errors.is_empty());
        let source_file = parse.syntax_node(LanguageKind::Java);
        assert!(matches!(source_file, SourceFile::Java(_)));
        assert_eq!(source_file.language(), LanguageKind::Java);
    }
}
