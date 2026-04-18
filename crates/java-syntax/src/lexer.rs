use crate::{
    lexer::token::{JavaToken, TokenType},
    reader::SourceReader,
};
use unicode_categories::UnicodeCategories;

pub mod token;

pub struct JavaLexer<'a> {
    reader: SourceReader<'a>,
    tokens: Vec<JavaToken<'a>>,
    errors: Vec<JavaLexicalError>,
}

impl<'a> JavaLexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            reader: SourceReader::new(source),
            tokens: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn scan_tokens(
        &mut self,
    ) -> Result<&[JavaToken<'a>], (&[JavaToken<'a>], &[JavaLexicalError])> {
        while !self.reader.is_at_end() {
            self.scan_next_token();
        }

        self.errors.extend(
            self.reader
                .errors()
                .iter()
                .map(|e| JavaLexicalError::new(LexicalErrorType::InvalidUnicodeEscape, e.position)),
        );

        if !self.errors.is_empty() {
            Err((&self.tokens, &self.errors))
        } else {
            Ok(&self.tokens)
        }
    }

    fn scan_next_token(&mut self) {
        self.reader.new_token();

        match self.reader.peek() {
            '.' => self.handle_dot(),
            c if c.is_numeric() => self.handle_number(),

            _ => {
                match self.reader.advance() {
                    '(' => self.push_token(TokenType::LeftParen),
                    ')' => self.push_token(TokenType::RightParen),
                    '[' => self.push_token(TokenType::LeftBracket),
                    ']' => self.push_token(TokenType::RightBracket),
                    '{' => self.push_token(TokenType::LeftBrace),
                    '}' => self.push_token(TokenType::RightBrace),
                    ';' => self.push_token(TokenType::Semicolon),
                    ',' => self.push_token(TokenType::Comma),
                    ':' => self.handle_colon(),
                    '?' => self.push_token(TokenType::Question),
                    '@' => self.push_token(TokenType::At),
                    '+' => self.handle_plus(),
                    '-' => self.handle_minus(),
                    '*' => self.handle_star(),
                    '^' => self.handle_caret(),
                    '<' => self.handle_less(),
                    '>' => self.handle_greater(),
                    '=' => self.handle_eq(),
                    '\'' => self.handle_char_literal(),
                    '/' => self.handle_slash(),
                    '"' => {
                        if self.reader.advance_if_matches_str("\"\"") {
                            // Java 15+ text block
                            self.handle_text_block();
                        } else {
                            // String literal
                            self.handle_string_literal();
                        }
                    }
                    '|' => self.handle_or(),
                    '&' => self.handle_and(),
                    '%' => self.handle_mod(),
                    '!' => self.handle_bang(),

                    c if is_java_whitespace(c) => {
                        // consume whitespace
                    }

                    c => {
                        if is_java_identifier_start(c) {
                            self.handle_identifier();
                        } else {
                            self.report_error(LexicalErrorType::UnexpectedChar(c));
                        }
                    }
                }
            }
        }
    }

    fn handle_number(&mut self) {
        let mut num_base = 10;
        // We use this flag to accurately catch trailing underscores without eager consumption
        let mut last_was_underscore = false;

        if self.reader.peek() == '0' {
            let next = self.reader.peek_next();
            if next == 'x' || next == 'X' {
                self.reader.advance(); // '0'
                self.reader.advance(); // 'x'
                num_base = 16;
            } else if next == 'b' || next == 'B' {
                self.reader.advance(); // '0'
                self.reader.advance(); // 'b'
                num_base = 2;
            } else if next == '_' || next.is_ascii_digit() {
                self.reader.advance(); // '0'
                // We set base to 8, but will consume using base 10 below to prevent
                // splitting valid floats (like 09.5) or splitting invalid octals (like 09)
                num_base = 8;
            }
        }

        // Consume the integer part
        // Even if it's octal, consume base 10 digits to keep the token intact.
        // The parser/semantic analyzer will catch an invalid octal like '09'.
        let consume_base = if num_base == 8 { 10 } else { num_base };

        loop {
            let c = self.reader.peek();
            if c.is_numeric() || c.is_digit(consume_base) {
                if !c.is_digit(num_base) {
                    self.report_error(LexicalErrorType::InvalidNumber);
                }
                self.reader.advance();
                last_was_underscore = false; // Reset flag when a valid digit is seen
            } else if c == '_' {
                self.reader.advance();
                last_was_underscore = true; // Mark that the last char we saw was '_'
            } else {
                break;
            }
        }

        // Catch trailing underscores on the integer part (e.g., `123_`)
        if last_was_underscore {
            self.report_error(LexicalErrorType::InvalidNumber);
            last_was_underscore = false;
        }

        // Parse float fractional part
        if self.reader.peek() == '.' {
            self.reader.advance(); // '.'

            // Java doesn't allow `1._2`
            if self.reader.peek() == '_' {
                self.report_error(LexicalErrorType::InvalidNumber);
            }

            loop {
                let c = self.reader.peek();
                if c.is_digit(consume_base) {
                    self.reader.advance();
                    last_was_underscore = false;
                } else if c == '_' {
                    self.reader.advance();
                    last_was_underscore = true;
                } else {
                    break;
                }
            }

            if last_was_underscore {
                self.report_error(LexicalErrorType::InvalidNumber);
                last_was_underscore = false;
            }
        }

        // Parse exponent
        let c = self.reader.peek();
        let is_dec_exp = num_base != 16 && c.eq_ignore_ascii_case(&'e');
        let is_hex_exp = num_base == 16 && c.eq_ignore_ascii_case(&'p');

        if is_dec_exp || is_hex_exp {
            self.reader.advance(); // consume 'e' or 'p'

            // Optional sign
            let sign = self.reader.peek();
            if sign == '+' || sign == '-' {
                self.reader.advance();
            }

            // Underscores immediately after exponent indicator are invalid (e.g., `1e_10`)
            if self.reader.peek() == '_' {
                self.report_error(LexicalErrorType::InvalidNumber);
            }

            let mut has_exp_digits = false;
            loop {
                let c = self.reader.peek();
                if c.is_ascii_digit() {
                    self.reader.advance();
                    last_was_underscore = false;
                    has_exp_digits = true;
                } else if c == '_' {
                    self.reader.advance();
                    last_was_underscore = true;
                } else {
                    break;
                }
            }

            // Catch missing digits after 'e' or trailing underscores
            if !has_exp_digits || last_was_underscore {
                self.report_error(LexicalErrorType::InvalidNumber);
            }
        }

        // Parse type suffix
        if matches!(self.reader.peek().to_ascii_lowercase(), 'l' | 'f' | 'd') {
            self.reader.advance();
        }

        self.push_token(TokenType::NumberLiteral);
    }

    fn handle_dot(&mut self) {
        if self.reader.peek_next().is_numeric() {
            // float number
            self.handle_number();

            return;
        }

        let token_type = if self.reader.advance_if_matches_str("...") {
            // ...
            TokenType::Ellipsis
        } else {
            // .
            self.reader.advance();
            TokenType::Dot
        };

        self.push_token(token_type);
    }

    fn handle_colon(&mut self) {
        let token_type = if self.reader.advance_if_matches(':') {
            // ::
            TokenType::ColonColon
        } else {
            TokenType::Colon
        };

        self.push_token(token_type);
    }

    fn handle_mod(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            TokenType::ModuloEqual
        } else {
            TokenType::Modulo
        };

        self.push_token(token_type);
    }

    fn handle_bang(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            TokenType::NotEqual
        } else {
            TokenType::Not
        };

        self.push_token(token_type);
    }

    fn handle_or(&mut self) {
        let token_type = if self.reader.advance_if_matches('|') {
            TokenType::Or
        } else if self.reader.advance_if_matches('=') {
            TokenType::OrEqual
        } else {
            TokenType::BitOr
        };

        self.push_token(token_type);
    }

    fn handle_and(&mut self) {
        let token_type = if self.reader.advance_if_matches('&') {
            TokenType::And
        } else if self.reader.advance_if_matches('=') {
            TokenType::AndEqual
        } else {
            TokenType::BitAnd
        };

        self.push_token(token_type);
    }

    fn handle_star(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            TokenType::MultipleEqual
        } else {
            TokenType::Star
        };

        self.push_token(token_type);
    }

    fn handle_plus(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            TokenType::PlusEqual
        } else if self.reader.advance_if_matches('+') {
            TokenType::PlusPlus
        } else {
            TokenType::Plus
        };

        self.push_token(token_type);
    }

    fn handle_caret(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            TokenType::XorEqual
        } else {
            TokenType::Caret
        };

        self.push_token(token_type);
    }

    fn handle_minus(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            // -=
            TokenType::MinusEqual
        } else if self.reader.advance_if_matches('-') {
            // --
            TokenType::MinusMinus
        } else if self.reader.advance_if_matches('>') {
            // ->
            TokenType::Arrow
        } else {
            TokenType::Minus
        };

        self.push_token(token_type);
    }

    fn handle_less(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            TokenType::LessEq // <=
        } else if self.reader.advance_if_matches_str("<=") {
            TokenType::ShlEqual // <<=
        } else if self.reader.advance_if_matches('<') {
            TokenType::Shl // <<
        } else {
            TokenType::Less // <
        };

        self.push_token(token_type);
    }

    fn handle_greater(&mut self) {
        let token_type = if self.reader.advance_if_matches('>') {
            if self.reader.advance_if_matches('>') {
                if self.reader.advance_if_matches('=') {
                    TokenType::UnsignedShrEqual // >>>=
                } else {
                    TokenType::UnsignedShr // >>>
                }
            } else if self.reader.advance_if_matches('=') {
                TokenType::ShrEqual // >>=
            } else {
                TokenType::Shr // >>
            }
        } else if self.reader.advance_if_matches('=') {
            TokenType::GreaterEq // >=
        } else {
            TokenType::Greater // >
        };

        self.push_token(token_type);
    }

    fn handle_eq(&mut self) {
        let token_type = if self.reader.advance_if_matches_str("=") {
            TokenType::EqualEqual // ==
        } else {
            TokenType::Equal // =
        };

        self.push_token(token_type);
    }

    fn handle_slash(&mut self) {
        if self.reader.peek() == '/' {
            // single-line comment //
            while let c = self.reader.peek()
                && c != '\n'
                && c != '\r'
                && !self.reader.is_at_end()
            {
                self.reader.advance();
            }
        } else if self.reader.peek() == '*' {
            // multiple line comment /* */
            // find */
            let mut has_terminated = false;
            while !self.reader.is_at_end() {
                if self.reader.advance_if_matches_str("*/") {
                    has_terminated = true;
                    break;
                }
                self.reader.advance();
            }

            if !has_terminated {
                self.report_error(LexicalErrorType::UnterminatedComment);
            }
        } else if self.reader.peek() == '=' {
            // /=
            self.reader.advance();
            self.push_token(TokenType::DivideEqual);
        } else {
            // /
            self.push_token(TokenType::Slash);
        }
    }

    fn handle_identifier(&mut self) {
        while !self.reader.is_at_end() && is_java_identifier_part(self.reader.peek()) {
            self.reader.advance(); // consume next char
        }

        let text = self.reader.current_token_lexeme();
        let token_type = TokenType::parse(text);

        self.push_token(token_type);
    }

    fn handle_char_literal(&mut self) {
        let mut last: Option<char> = None;

        while !self.reader.is_at_end() && (self.reader.peek() != '\'' || last == Some('\\')) {
            last = Some(self.reader.advance());
        }

        if self.reader.is_at_end() {
            // Unterminated char
            self.report_error(LexicalErrorType::UnterminatedString);
        }

        // consume tailing quotation mark
        self.reader.advance(); // '

        let lexeme = self.reader.current_token_lexeme();
        let s1 = lexeme.strip_prefix('\'').unwrap_or(lexeme);
        let s2 = s1.strip_suffix('\'').unwrap_or(s1);
        let s3 = s2.strip_prefix('\\').unwrap_or(s2);

        if s3.chars().count() != 1 {
            self.report_error(LexicalErrorType::InvalidChar);
        }

        self.push_token(TokenType::CharLiteral);
    }

    fn handle_text_block(&mut self) {
        if self.reader.peek() != '\n' && self.reader.peek() != '\r' {
            self.report_error(LexicalErrorType::IllegalTextBlockOpen);
        }

        let mut is_escaped = false;
        let mut is_terminated = false;

        while !self.reader.is_at_end() {
            let c = self.reader.peek();

            if !is_escaped && self.reader.advance_if_matches_str("\"\"\"") {
                is_terminated = true;
                break;
            }

            if c == '\\' {
                is_escaped = !is_escaped;
            } else {
                is_escaped = false;
            }
            self.reader.advance();
        }

        if !is_terminated {
            self.report_error(LexicalErrorType::UnterminatedTextBlock);
        }

        self.push_token(TokenType::TextBlock);
    }

    fn handle_string_literal(&mut self) {
        let mut is_escaped = false;

        while !self.reader.is_at_end() {
            let c = self.reader.peek();

            if c == '"' && !is_escaped {
                break;
            }

            if c == '\n' || c == '\r' {
                self.report_error(LexicalErrorType::UnterminatedString);
                self.reader.advance();
                return;
            }

            if c == '\\' {
                is_escaped = !is_escaped;
            } else {
                is_escaped = false;
            }

            self.reader.advance();
        }

        if self.reader.is_at_end() {
            self.report_error(LexicalErrorType::UnterminatedString);
            return;
        }

        // consume last "
        self.reader.advance();

        self.push_token(TokenType::StringLiteral);
    }

    fn push_token(&mut self, token_type: TokenType) {
        self.tokens.push(JavaToken::new(
            token_type,
            self.reader.current_token_lexeme(),
            self.reader.start(),
        ));
    }

    fn report_error(&mut self, error_type: LexicalErrorType) {
        self.errors
            .push(JavaLexicalError::new(error_type, self.reader.start()));
    }
}

#[derive(Debug)]
pub struct JavaLexicalError {
    pub error_type: LexicalErrorType,
    pub at_offset: usize,
}

impl JavaLexicalError {
    pub fn new(error_type: LexicalErrorType, offset: usize) -> Self {
        Self {
            error_type,
            at_offset: offset,
        }
    }
}

#[derive(Debug)]
pub enum LexicalErrorType {
    UnexpectedChar(char),
    MissingSemicolon,
    UnterminatedString,
    UnterminatedComment,
    InvalidChar,
    IllegalTextBlockOpen,
    UnterminatedTextBlock,
    InvalidNumber,
    InvalidUnicodeEscape,
}

fn is_java_identifier_start(c: char) -> bool {
    c.is_alphabetic() || c.is_symbol_currency() || c.is_punctuation_connector()
}

fn is_java_identifier_part(c: char) -> bool {
    is_java_identifier_start(c) || c.is_numeric()
}

// https://docs.oracle.com/javase/specs/jls/se25/html/jls-3.html#jls-3.6
fn is_java_whitespace(c: char) -> bool {
    matches!(c, '\u{0020}' | '\u{0009}' | '\u{000C}' | '\n' | '\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! assert_lex {
        ($source:expr, [ $( ($expected_type:expr, $expected_lexeme:expr) ),* $(,)? ]) => {
            let mut lexer = JavaLexer::new($source);
            match lexer.scan_tokens() {
                Ok(tokens) => {
                    let expected: Vec<(TokenType, &str)> = vec![
                        $( ($expected_type, $expected_lexeme) ),*
                    ];

                    assert_eq!(
                        tokens.len(),
                        expected.len(),
                        "Token count mismatch for source: '{}'\nActual tokens: {:#?}",
                        $source,
                        tokens
                    );

                    for (i, token) in tokens.iter().enumerate() {
                        assert_eq!(
                            token.token_type, expected[i].0,
                            "Type mismatch at index {} for source '{}'", i, $source
                        );
                        assert_eq!(
                            token.lexeme, expected[i].1,
                            "Lexeme mismatch at index {} for source '{}'", i, $source
                        );
                    }
                }
                Err((tokens, errors)) => {
                    panic!("Lexing failed for '{}'\nTokens: {:#?}\nErrors: {:#?}", $source, tokens, errors);
                }
            }
        };
    }

    macro_rules! assert_lex_errors {
        ($source:expr, [ $( $expected_error_pattern:pat ),* $(,)? ]) => {
            let mut lexer = JavaLexer::new($source);
            match lexer.scan_tokens() {
                Ok(tokens) => panic!("Expected errors but lexing succeeded for: '{}'\nTokens: {:#?}", $source, tokens),
                Err((_, errors)) => {
                    let mut err_iter = errors.iter();

                    $(
                        let err = err_iter.next().expect(&format!("Not enough errors returned for source: '{}'", $source));
                        assert!(
                            matches!(&err.error_type, $expected_error_pattern),
                            "Error type mismatch. Expected pattern '{}' but got {:?}\nActual errors: {:#?}",
                            stringify!($expected_error_pattern), err.error_type, errors
                        );
                    )*

                    assert!(
                        err_iter.next().is_none(),
                        "Too many errors returned for source: '{}'\nActual errors: {:#?}",
                        $source, errors
                    );
                }
            }
        };
    }

    #[test]
    fn test_empty_and_whitespace() {
        assert_lex!("", []);
        assert_lex!(" \t\n\r  ", []);
    }

    #[test]
    fn test_comments() {
        // Comments should be consumed and yield no tokens
        assert_lex!("// this is a line comment\n", []);
        assert_lex!("/* this is a \n block comment */", []);

        // Mixed with tokens
        assert_lex!(
            "int /* comment */ x // line",
            [(TokenType::Int, "int"), (TokenType::Identifier, "x"),]
        );
    }

    #[test]
    fn test_keywords_and_identifiers() {
        assert_lex!(
            "public static void main",
            [
                (TokenType::Public, "public"),
                (TokenType::Static, "static"),
                (TokenType::Void, "void"),
                (TokenType::Identifier, "main")
            ]
        );

        assert_lex!(
            "class interface enum record",
            [
                (TokenType::Class, "class"),
                (TokenType::Interface, "interface"),
                (TokenType::Enum, "enum"),
                (TokenType::Identifier, "record") // "record" is a contextual keyword, so Identifier is correct for a basic lexer
            ]
        );

        assert_lex!(
            "$myVar _underscore value123",
            [
                (TokenType::Identifier, "$myVar"),
                (TokenType::Identifier, "_underscore"),
                (TokenType::Identifier, "value123")
            ]
        );
    }

    #[test]
    fn test_boolean_and_null_literals() {
        assert_lex!(
            "true false null",
            [
                (TokenType::True, "true"),
                (TokenType::False, "false"),
                (TokenType::Null, "null")
            ]
        );
    }

    #[test]
    fn test_separators() {
        assert_lex!(
            "( ) { } [ ] ; , . ... :: @",
            [
                (TokenType::LeftParen, "("),
                (TokenType::RightParen, ")"),
                (TokenType::LeftBrace, "{"),
                (TokenType::RightBrace, "}"),
                (TokenType::LeftBracket, "["),
                (TokenType::RightBracket, "]"),
                (TokenType::Semicolon, ";"),
                (TokenType::Comma, ","),
                (TokenType::Dot, "."),
                (TokenType::Ellipsis, "..."),
                (TokenType::ColonColon, "::"),
                (TokenType::At, "@")
            ]
        );
    }

    #[test]
    fn test_operators() {
        assert_lex!(
            "+ += ++ - -= -- ->",
            [
                (TokenType::Plus, "+"),
                (TokenType::PlusEqual, "+="),
                (TokenType::PlusPlus, "++"),
                (TokenType::Minus, "-"),
                (TokenType::MinusEqual, "-="),
                (TokenType::MinusMinus, "--"),
                (TokenType::Arrow, "->")
            ]
        );

        assert_lex!(
            "* *= / /= % %= == = != !",
            [
                (TokenType::Star, "*"),
                (TokenType::MultipleEqual, "*="),
                (TokenType::Slash, "/"),
                (TokenType::DivideEqual, "/="),
                (TokenType::Modulo, "%"),
                (TokenType::ModuloEqual, "%="),
                (TokenType::EqualEqual, "=="),
                (TokenType::Equal, "="),
                (TokenType::NotEqual, "!="),
                (TokenType::Not, "!")
            ]
        );

        assert_lex!(
            "< <= << <<= > >= >> >>= >>> >>>=",
            [
                (TokenType::Less, "<"),
                (TokenType::LessEq, "<="),
                (TokenType::Shl, "<<"),
                (TokenType::ShlEqual, "<<="),
                (TokenType::Greater, ">"),
                (TokenType::GreaterEq, ">="),
                (TokenType::Shr, ">>"),
                (TokenType::ShrEqual, ">>="),
                (TokenType::UnsignedShr, ">>>"),
                (TokenType::UnsignedShrEqual, ">>>=")
            ]
        );

        assert_lex!(
            "& &= | |= ^ ^= && ||",
            [
                (TokenType::BitAnd, "&"),
                (TokenType::AndEqual, "&="),
                (TokenType::BitOr, "|"),
                (TokenType::OrEqual, "|="),
                (TokenType::Caret, "^"),
                (TokenType::XorEqual, "^="),
                (TokenType::And, "&&"),
                (TokenType::Or, "||")
            ]
        );
    }

    #[test]
    fn test_integer_literals() {
        // Decimal
        assert_lex!(
            "0 123 1_000_000 456L",
            [
                (TokenType::NumberLiteral, "0"),
                (TokenType::NumberLiteral, "123"),
                (TokenType::NumberLiteral, "1_000_000"),
                (TokenType::NumberLiteral, "456L")
            ]
        );

        // Hexadecimal
        assert_lex!(
            "0x0 0x1A2B 0XCAFE_BABE 0xFFl",
            [
                (TokenType::NumberLiteral, "0x0"),
                (TokenType::NumberLiteral, "0x1A2B"),
                (TokenType::NumberLiteral, "0XCAFE_BABE"),
                (TokenType::NumberLiteral, "0xFFl")
            ]
        );

        // Binary
        assert_lex!(
            "0b0 0B1010_0101 0b11L",
            [
                (TokenType::NumberLiteral, "0b0"),
                (TokenType::NumberLiteral, "0B1010_0101"),
                (TokenType::NumberLiteral, "0b11L")
            ]
        );
    }

    #[test]
    fn test_floating_point_literals() {
        assert_lex!(
            "1.23 .5 10. 3.14f 6.022e23 1e-9d",
            [
                (TokenType::NumberLiteral, "1.23"),
                (TokenType::NumberLiteral, ".5"),
                (TokenType::NumberLiteral, "10."),
                (TokenType::NumberLiteral, "3.14f"),
                (TokenType::NumberLiteral, "6.022e23"),
                (TokenType::NumberLiteral, "1e-9d")
            ]
        );

        // Hexadecimal float
        assert_lex!(
            "0x1.0p3 0x.8P-2f",
            [
                (TokenType::NumberLiteral, "0x1.0p3"),
                (TokenType::NumberLiteral, "0x.8P-2f")
            ]
        );
    }

    #[test]
    fn test_string_literals() {
        assert_lex!(
            r#" "hello world" "escape \" test" "" "#,
            [
                (TokenType::StringLiteral, r#""hello world""#),
                (TokenType::StringLiteral, r#""escape \" test""#),
                (TokenType::StringLiteral, r#""""#)
            ]
        );
    }

    #[test]
    fn test_text_blocks() {
        // Valid Java 15+ text block starts with """ followed by a newline
        assert_lex!(
            "\"\"\"\nHello\n  World\n\"\"\"",
            [(TokenType::TextBlock, "\"\"\"\nHello\n  World\n\"\"\"")]
        );
    }

    #[test]
    fn test_char_literals() {
        assert_lex!(
            "'a' '\\n' '\\''",
            [
                (TokenType::CharLiteral, "'a'"),
                (TokenType::CharLiteral, "'\\n'"),
                (TokenType::CharLiteral, "'\\''")
            ]
        );
    }

    #[test]
    fn test_complex_jls_scenario() {
        let source = "List<String> list = new ArrayList<>();";
        assert_lex!(
            source,
            [
                (TokenType::Identifier, "List"),
                (TokenType::Less, "<"),
                (TokenType::Identifier, "String"),
                (TokenType::Greater, ">"),
                (TokenType::Identifier, "list"),
                (TokenType::Equal, "="),
                (TokenType::New, "new"),
                (TokenType::Identifier, "ArrayList"),
                (TokenType::Less, "<"),
                (TokenType::Greater, ">"),
                (TokenType::LeftParen, "("),
                (TokenType::RightParen, ")"),
                (TokenType::Semicolon, ";")
            ]
        );
    }

    #[test]
    fn test_error_unterminated_string() {
        assert_lex_errors!(
            "\"this string has no end",
            [LexicalErrorType::UnterminatedString]
        );
        // Strings cannot span across physical newlines
        assert_lex_errors!(
            "\"line1\nline2\"",
            [
                LexicalErrorType::UnterminatedString,
                LexicalErrorType::UnterminatedString
            ]
        );
    }

    #[test]
    fn test_error_unterminated_comment() {
        assert_lex_errors!(
            "/* this block comment never ends ",
            [LexicalErrorType::UnterminatedComment]
        );
    }

    #[test]
    fn test_error_invalid_numbers() {
        // Underscore at the end of a number is invalid in JLS
        assert_lex_errors!("123_", [LexicalErrorType::InvalidNumber]);

        // Binary with non 0/1
        assert_lex_errors!("0b1012", [LexicalErrorType::InvalidNumber]);

        // _ after .
        assert_lex_errors!("0._1f", [LexicalErrorType::InvalidNumber]);

        // Invalid Octal
        assert_lex_errors!("019", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("0_19", [LexicalErrorType::InvalidNumber]);
    }

    #[test]
    fn test_error_illegal_text_block_open() {
        // Text block must have a newline immediately after """
        assert_lex_errors!(
            "\"\"\"illegal",
            [
                LexicalErrorType::IllegalTextBlockOpen,
                LexicalErrorType::UnterminatedTextBlock
            ]
        );
    }

    #[test]
    fn test_error_invalid_char() {
        // Char literals can only contain exactly one character (or one escape sequence)
        assert_lex_errors!("'abc'", [LexicalErrorType::InvalidChar]);
        assert_lex_errors!("''", [LexicalErrorType::InvalidChar]);
    }

    #[test]
    fn test_unicode_escape_in_keywords_and_identifiers() {
        // \u0070 -> 'p'
        assert_lex!(
            "\\u0070ublic class Test {}",
            [
                (TokenType::Public, "public"),
                (TokenType::Class, "class"),
                (TokenType::Identifier, "Test"),
                (TokenType::LeftBrace, "{"),
                (TokenType::RightBrace, "}"),
            ]
        );

        // \u005F -> '_'
        assert_lex!(
            "int my\\u005Fvar = 1;",
            [
                (TokenType::Int, "int"),
                (TokenType::Identifier, "my_var"),
                (TokenType::Equal, "="),
                (TokenType::NumberLiteral, "1"),
                (TokenType::Semicolon, ";"),
            ]
        );
    }

    #[test]
    fn test_unicode_escape_multiple_u() {
        // \uuuu0061 -> 'a'
        assert_lex!(
            "char \\uuuu0061 = 'a';",
            [
                (TokenType::Char, "char"),
                (TokenType::Identifier, "a"),
                (TokenType::Equal, "="),
                (TokenType::CharLiteral, "'a'"),
                (TokenType::Semicolon, ";"),
            ]
        );
    }

    #[test]
    fn test_unicode_escape_operators() {
        assert_lex!(
            "int a = 1 \\u002B\\u002B;",
            [
                (TokenType::Int, "int"),
                (TokenType::Identifier, "a"),
                (TokenType::Equal, "="),
                (TokenType::NumberLiteral, "1"),
                (TokenType::PlusPlus, "++"),
                (TokenType::Semicolon, ";"),
            ]
        );
    }

    #[test]
    fn test_unicode_escape_in_strings() {
        assert_lex_errors!(
            "String s = \"\\u0022\";",
            [LexicalErrorType::UnterminatedString]
        );

        assert_lex!(
            "String s = \"\\u005C\\u0022\";",
            [
                (TokenType::Identifier, "String"),
                (TokenType::Identifier, "s"),
                (TokenType::Equal, "="),
                (TokenType::StringLiteral, "\"\\\"\""),
                (TokenType::Semicolon, ";")
            ]
        );
    }

    #[test]
    fn test_error_invalid_unicode_escapes() {
        assert_lex_errors!(
            "int \\u006 = 1;",
            [
                LexicalErrorType::UnexpectedChar('\\'),
                LexicalErrorType::InvalidUnicodeEscape,
            ]
        );

        // Invalid hex (G is not hex)
        assert_lex_errors!(
            "int \\u006G = 1;",
            [
                LexicalErrorType::UnexpectedChar('\\'),
                LexicalErrorType::InvalidUnicodeEscape,
            ]
        );
    }

    #[test]
    fn test_comment_terminated_by_unicode_escapes() {
        // \u000A -> '\n'
        assert_lex!(
            "// hidden comment \\u000A int x = 1;",
            [
                (TokenType::Int, "int"),
                (TokenType::Identifier, "x"),
                (TokenType::Equal, "="),
                (TokenType::NumberLiteral, "1"),
                (TokenType::Semicolon, ";"),
            ]
        );

        // \u000D -> '\r'
        assert_lex!(
            "// hidden comment \\u000D int y = 2;",
            [
                (TokenType::Int, "int"),
                (TokenType::Identifier, "y"),
                (TokenType::Equal, "="),
                (TokenType::NumberLiteral, "2"),
                (TokenType::Semicolon, ";"),
            ]
        );
    }

    #[test]
    fn test_comment_terminated_by_raw_cr_and_crlf() {
        // CR
        assert_lex!(
            "// normal comment \r int z = 3;",
            [
                (TokenType::Int, "int"),
                (TokenType::Identifier, "z"),
                (TokenType::Equal, "="),
                (TokenType::NumberLiteral, "3"),
                (TokenType::Semicolon, ";"),
            ]
        );

        // CRLF
        assert_lex!(
            "// normal comment \r\n int w = 4;",
            [
                (TokenType::Int, "int"),
                (TokenType::Identifier, "w"),
                (TokenType::Equal, "="),
                (TokenType::NumberLiteral, "4"),
                (TokenType::Semicolon, ";"),
            ]
        );
    }

    #[test]
    fn test_valid_number_extremes() {
        assert_lex!(
            "0x0.0p0f 0b1_0 00_0 1.e2 .1e-2 1f 1d 1l 0x.8p1 0X1.P-1D 0_123",
            [
                (TokenType::NumberLiteral, "0x0.0p0f"),
                (TokenType::NumberLiteral, "0b1_0"),
                (TokenType::NumberLiteral, "00_0"),
                (TokenType::NumberLiteral, "1.e2"),
                (TokenType::NumberLiteral, ".1e-2"),
                (TokenType::NumberLiteral, "1f"),
                (TokenType::NumberLiteral, "1d"),
                (TokenType::NumberLiteral, "1l"),
                (TokenType::NumberLiteral, "0x.8p1"),
                (TokenType::NumberLiteral, "0X1.P-1D"),
                (TokenType::NumberLiteral, "0_123"),
            ]
        );
    }

    #[test]
    fn test_invalid_underscore_placement_integer() {
        // Trailing underscores on integer
        assert_lex_errors!("123_", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("0x123_", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("0b10_", [LexicalErrorType::InvalidNumber]);
    }

    #[test]
    fn test_invalid_underscore_placement_float() {
        // Underscore immediately before decimal
        assert_lex_errors!("123_.45", [LexicalErrorType::InvalidNumber]);

        // Underscore immediately after decimal
        assert_lex_errors!("123._45", [LexicalErrorType::InvalidNumber]);

        // Trailing underscore on fractional part
        assert_lex_errors!("123.45_", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!(".45_", [LexicalErrorType::InvalidNumber]);
    }

    #[test]
    fn test_invalid_underscore_placement_exponent() {
        // Underscore immediately before exponent (caught by fractional/integer trailing check)
        assert_lex_errors!("123_e10", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("123.4_e10", [LexicalErrorType::InvalidNumber]);

        // Underscore immediately after exponent indicator
        assert_lex_errors!("1e_10", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("0x1p_10", [LexicalErrorType::InvalidNumber]);

        // Underscore immediately after exponent sign
        assert_lex_errors!("1e+_10", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("1e-_10", [LexicalErrorType::InvalidNumber]);

        // Trailing underscore on exponent
        assert_lex_errors!("1e10_", [LexicalErrorType::InvalidNumber]);
    }

    #[test]
    fn test_invalid_octals() {
        // Base 8 literal containing 8 or 9
        assert_lex_errors!("08", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("09", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("0128", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("0_9", [LexicalErrorType::InvalidNumber]);
    }

    #[test]
    fn test_invalid_exponents_missing_digits() {
        // Exponent indicators with no digits following
        assert_lex_errors!("1e", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("1e+", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("1e-", [LexicalErrorType::InvalidNumber]);

        // Hex exponent missing digits
        assert_lex_errors!("0x1p", [LexicalErrorType::InvalidNumber]);
        assert_lex_errors!("0x1p+", [LexicalErrorType::InvalidNumber]);
    }

    #[test]
    fn test_multiple_invalid_factors() {
        // Both invalid octal AND invalid underscore placement
        // Should report the first encountered issue or both depending on exact iteration,
        // but based on current implementation, it processes the whole integer loop.
        // It will report InvalidNumber for '9' and then again if there's a trailing underscore.
        assert_lex_errors!(
            "09_",
            [
                LexicalErrorType::InvalidNumber,
                LexicalErrorType::InvalidNumber
            ]
        );
    }
}
