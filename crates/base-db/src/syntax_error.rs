use rowan::TextRange;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
}

impl SyntaxError {
    pub(crate) fn from_java_lexer(lex_err: &java_syntax::LexicalError) -> Self {
        let message = match &lex_err.kind {
            java_syntax::LexicalErrorKind::UnexpectedChar(c) => {
                format!("Unexpected character '{c}' found in source code.")
            }
            java_syntax::LexicalErrorKind::UnterminatedString => {
                "Missing closing quote '\"' for string literal.".to_string()
            }
            java_syntax::LexicalErrorKind::UnterminatedComment => {
                "Missing closing '*/' for block comment.".to_string()
            }
            java_syntax::LexicalErrorKind::InvalidChar => {
                "Invalid character literal. Did you forget a closing quote '''?".to_string()
            }
            java_syntax::LexicalErrorKind::IllegalTextBlockOpen => {
                "Expected a newline immediately after opening a text block (\"\"\").".to_string()
            }
            java_syntax::LexicalErrorKind::UnterminatedTextBlock => {
                "Missing closing '\"\"\"' for text block.".to_string()
            }
            java_syntax::LexicalErrorKind::InvalidNumber => "Malformed number literal.".to_string(),
            java_syntax::LexicalErrorKind::InvalidUnicodeEscape => {
                "Invalid unicode escape sequence (expected format: \\uXXXX).".to_string()
            }
            java_syntax::LexicalErrorKind::UnterminatedChar => {
                "Missing closing quote ''' for character literal.".to_string()
            }
            java_syntax::LexicalErrorKind::InvalidEscapeSequence => {
                "Invalid escape sequence inside string or char literal.".to_string()
            }
            java_syntax::LexicalErrorKind::UnterminatedTemplate => {
                "Missing closing delimiter for string template.".to_string()
            }
        };

        Self {
            message,
            range: lex_err.range,
        }
    }

    pub(crate) fn from_java_parser(parse_err: &java_syntax::ParseError) -> Self {
        let message = match &parse_err.kind {
            java_syntax::ParseErrorKind::ExpectedToken { expected, found } => {
                let found_str = found
                    .map(|f| f.to_string())
                    .unwrap_or_else(|| "end of file".to_string());

                if expected.len() == 1 {
                    let expected_str = expected.first().unwrap().to_string();
                    format!("Expected '{expected_str}', but found {found_str}.")
                } else {
                    let expected_options = expected
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" or ");
                    format!("Expected {expected_options}, but found {found_str}.")
                }
            }
            java_syntax::ParseErrorKind::ExpectedContextualKeyword { keyword, found } => {
                let found_str = found
                    .map(|f| f.to_string())
                    .unwrap_or_else(|| "end of file".to_string());
                format!(
                    "Expected keyword '{}', but found {found_str}.",
                    keyword.as_str()
                )
            }
            java_syntax::ParseErrorKind::ExpectedConstruct(expected_construct) => {
                let construct_str = expected_construct.to_string();
                format!("Expected {construct_str} here.")
            }
            java_syntax::ParseErrorKind::Message(msg) => msg.to_string(),
        };

        Self {
            message,
            range: parse_err.range,
        }
    }
}
