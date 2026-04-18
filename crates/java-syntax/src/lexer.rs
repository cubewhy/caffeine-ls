use crate::{
    lexer::token::{JavaToken, TokenType},
    reader::SourceReader,
};
use unicode_categories::UnicodeCategories;

pub mod token;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexerMode {
    Normal,
    TemplateExpression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateKind {
    String,
    TextBlock,
}

impl TemplateKind {
    fn begin_token(self) -> TokenType {
        match self {
            TemplateKind::String => TokenType::StringTemplateBegin,
            TemplateKind::TextBlock => TokenType::TextBlockTemplateBegin,
        }
    }

    fn mid_token(self) -> TokenType {
        match self {
            TemplateKind::String => TokenType::StringTemplateMid,
            TemplateKind::TextBlock => TokenType::TextBlockTemplateMid,
        }
    }

    fn end_token(self) -> TokenType {
        match self {
            TemplateKind::String => TokenType::StringTemplateEnd,
            TemplateKind::TextBlock => TokenType::TextBlockTemplateEnd,
        }
    }

    fn literal_token(self) -> TokenType {
        match self {
            TemplateKind::String => TokenType::StringLiteral,
            TemplateKind::TextBlock => TokenType::TextBlock,
        }
    }

    fn allows_newline(self) -> bool {
        matches!(self, TemplateKind::TextBlock)
    }
}

#[derive(Debug, Clone, Copy)]
struct TemplateContext {
    kind: TemplateKind,
    brace_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateChunkRole {
    FullLiteral,
    Continuation,
}

pub struct JavaLexer<'a> {
    reader: SourceReader<'a>,
    tokens: Vec<JavaToken<'a>>,
    errors: Vec<JavaLexicalError>,

    mode: LexerMode,
    template_stack: Vec<TemplateContext>,
}

impl<'a> JavaLexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            reader: SourceReader::new(source),
            tokens: Vec::new(),
            errors: Vec::new(),
            mode: LexerMode::Normal,
            template_stack: Vec::new(),
        }
    }

    pub fn scan_tokens(
        &mut self,
    ) -> Result<&[JavaToken<'a>], (&[JavaToken<'a>], &[JavaLexicalError])> {
        while !self.reader.is_at_end() {
            self.scan_next_token();
        }

        if !self.template_stack.is_empty() {
            // unterminated string/textblock template
            self.report_error(LexicalErrorType::UnterminatedTemplate);
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
        self.scan_token_dispatch();
    }

    fn scan_token_dispatch(&mut self) {
        match self.reader.peek() {
            '.' => self.handle_dot(),
            '{' => self.handle_left_brace(),
            '}' => self.handle_right_brace(),
            c if c.is_numeric() => self.handle_number(),

            _ => {
                match self.reader.advance() {
                    '(' => self.push_token(TokenType::LeftParen),
                    ')' => self.push_token(TokenType::RightParen),
                    '[' => self.push_token(TokenType::LeftBracket),
                    ']' => self.push_token(TokenType::RightBracket),
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
                            self.scan_quoted_content(
                                TemplateKind::TextBlock,
                                TemplateChunkRole::FullLiteral,
                            );
                        } else {
                            self.scan_quoted_content(
                                TemplateKind::String,
                                TemplateChunkRole::FullLiteral,
                            );
                        }
                    }
                    '|' => self.handle_or(),
                    '&' => self.handle_and(),
                    '%' => self.handle_mod(),
                    '!' => self.handle_bang(),

                    '\x1A' => {
                        // https://docs.oracle.com/javase/specs/jls/se17/html/jls-3.html#jls-3.5
                        // Ascii SUB character
                        if !self.reader.is_at_end() {
                            self.report_error(LexicalErrorType::UnexpectedChar('\x1A'));
                        }
                    }

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

    fn handle_left_brace(&mut self) {
        if self.mode == LexerMode::TemplateExpression
            && let Some(ctx) = self.template_stack.last_mut()
        {
            ctx.brace_depth += 1;
        }

        self.reader.advance();
        self.push_token(TokenType::LeftBrace);
    }

    fn handle_right_brace(&mut self) {
        if self.mode != LexerMode::TemplateExpression {
            self.reader.advance();
            self.push_token(TokenType::RightBrace);
            return;
        }

        let Some(ctx) = self.template_stack.last_mut() else {
            self.mode = LexerMode::Normal;
            self.reader.advance();
            self.push_token(TokenType::RightBrace);
            return;
        };

        if ctx.brace_depth > 0 {
            ctx.brace_depth -= 1;
            self.reader.advance();
            self.push_token(TokenType::RightBrace);
            return;
        }

        self.reader.advance(); // consume template-closing '}'
        let kind = ctx.kind;
        self.template_stack.pop();
        self.resume_template_after_expression(kind);
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

    fn consume_escape_sequence(&mut self, is_text_block: bool) -> bool {
        self.reader.advance(); // '\'

        if self.reader.is_at_end() {
            return false;
        }

        match self.reader.peek() {
            'b' | 't' | 'n' | 'f' | 'r' | '"' | '\'' | '\\' => {
                self.reader.advance();
                true
            }

            's' => {
                self.reader.advance();
                true
            }

            '\n' | '\r' if is_text_block => {
                self.reader.advance();
                true
            }

            '0'..='7' => {
                let first_digit = self.reader.advance();

                if self.reader.peek() >= '0' && self.reader.peek() <= '7' {
                    self.reader.advance(); // the second num

                    if first_digit <= '3' && self.reader.peek() >= '0' && self.reader.peek() <= '7'
                    {
                        self.reader.advance(); // the third num
                    }
                }
                true
            }
            _ => {
                // invalid escape
                self.reader.advance();
                false
            }
        }
    }

    fn handle_char_literal(&mut self) {
        let mut logical_char_count = 0;
        let mut has_error = false;

        while !self.reader.is_at_end() {
            let c = self.reader.peek();

            if c == '\'' {
                break;
            }
            if c == '\n' || c == '\r' {
                self.report_error(LexicalErrorType::UnterminatedChar);
                return;
            }

            if c == '\\' {
                if !self.consume_escape_sequence(false) {
                    self.report_error(LexicalErrorType::InvalidEscapeSequence);
                    has_error = true;
                }
            } else {
                self.reader.advance();
            }
            logical_char_count += c.len_utf8();
        }

        if self.reader.is_at_end() {
            self.report_error(LexicalErrorType::UnterminatedChar);
            return;
        }

        self.reader.advance(); // '

        if !has_error && logical_char_count != 1 {
            self.report_error(LexicalErrorType::InvalidChar);
        }

        self.push_token(TokenType::CharLiteral);
    }

    fn scan_quoted_content(&mut self, kind: TemplateKind, role: TemplateChunkRole) {
        if kind == TemplateKind::TextBlock && role == TemplateChunkRole::FullLiteral {
            while matches!(self.reader.peek(), '\u{0020}' | '\u{0009}' | '\u{000C}') {
                self.reader.advance();
            }

            let next_char = self.reader.peek();
            if next_char != '\n' && next_char != '\r' {
                self.report_error(LexicalErrorType::IllegalTextBlockOpen);
            }
        }

        while !self.reader.is_at_end() {
            if self.is_template_close(kind) {
                self.consume_template_close(kind);
                self.emit_quoted_terminal_token(kind, role);
                return;
            }

            let c = self.reader.peek();

            if !kind.allows_newline() && (c == '\n' || c == '\r') {
                match role {
                    TemplateChunkRole::FullLiteral => {
                        self.report_error(LexicalErrorType::UnterminatedString);
                    }
                    TemplateChunkRole::Continuation => {
                        self.report_error(LexicalErrorType::UnterminatedTemplate);
                        self.mode = LexerMode::Normal;
                    }
                }
                return;
            }

            if c == '\\' {
                if self.reader.peek_next() == '{' {
                    self.reader.advance(); // '\'
                    self.reader.advance(); // '{'
                    self.emit_template_open_token(kind, role);
                    self.template_stack.push(TemplateContext {
                        kind,
                        brace_depth: 0,
                    });
                    self.mode = LexerMode::TemplateExpression;
                    return;
                }

                if !self.consume_escape_sequence(kind == TemplateKind::TextBlock) {
                    self.report_error(LexicalErrorType::InvalidEscapeSequence);
                }
                continue;
            }

            self.reader.advance();
        }

        match (kind, role) {
            (TemplateKind::String, TemplateChunkRole::FullLiteral) => {
                self.report_error(LexicalErrorType::UnterminatedString);
            }
            (TemplateKind::TextBlock, TemplateChunkRole::FullLiteral) => {
                self.report_error(LexicalErrorType::UnterminatedTextBlock);
            }
            (_, TemplateChunkRole::Continuation) => {
                self.report_error(LexicalErrorType::UnterminatedTemplate);
                self.mode = LexerMode::Normal;
            }
        }
    }

    fn is_template_close(&self, kind: TemplateKind) -> bool {
        match kind {
            TemplateKind::String => self.reader.peek() == '"',
            TemplateKind::TextBlock => {
                self.reader.peek() == '"'
                    && self.reader.peek_next() == '"'
                    && self.reader.peek_n(2) == '"'
            }
        }
    }

    fn consume_template_close(&mut self, kind: TemplateKind) {
        match kind {
            TemplateKind::String => {
                self.reader.advance();
            }
            TemplateKind::TextBlock => {
                self.reader.advance();
                self.reader.advance();
                self.reader.advance();
            }
        }
    }

    fn emit_quoted_terminal_token(&mut self, kind: TemplateKind, role: TemplateChunkRole) {
        let token_type = match role {
            TemplateChunkRole::FullLiteral => kind.literal_token(),
            TemplateChunkRole::Continuation => {
                if self.template_stack.is_empty() {
                    self.mode = LexerMode::Normal;
                }
                kind.end_token()
            }
        };

        self.push_token(token_type);
    }

    fn emit_template_open_token(&mut self, kind: TemplateKind, role: TemplateChunkRole) {
        let token_type = match role {
            TemplateChunkRole::FullLiteral => kind.begin_token(),
            TemplateChunkRole::Continuation => kind.mid_token(),
        };

        self.push_token(token_type);
    }

    fn resume_template_after_expression(&mut self, kind: TemplateKind) {
        self.scan_quoted_content(kind, TemplateChunkRole::Continuation);
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
    UnterminatedChar,
    InvalidEscapeSequence,
    UnterminatedTemplate,
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
                        let err = err_iter.next().expect(&format!("Not enough errors returned for source: '{}'\nActual errors: {:#?}", $source, errors));
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

    #[test]
    fn test_char_literal_strict_validation() {
        assert_lex!(
            "'a' '\\n' '\\t' '\\\\' '\\''",
            [
                (TokenType::CharLiteral, "'a'"),
                (TokenType::CharLiteral, "'\\n'"),
                (TokenType::CharLiteral, "'\\t'"),
                (TokenType::CharLiteral, "'\\\\'"),
                (TokenType::CharLiteral, "'\\''"),
            ]
        );

        // empty char literal
        assert_lex_errors!("''", [LexicalErrorType::InvalidChar]);

        // multiple chars in a char literal
        assert_lex_errors!("'ab'", [LexicalErrorType::InvalidChar]);
        assert_lex_errors!("'\\n\\t'", [LexicalErrorType::InvalidChar]);

        // invalid escape
        assert_lex_errors!("'\\z'", [LexicalErrorType::InvalidEscapeSequence]);
    }

    #[test]
    fn test_string_literal_escapes() {
        assert_lex!(
            r#" "\b\t\n\f\r\"\'\\" "#,
            [(TokenType::StringLiteral, r#""\b\t\n\f\r\"\'\\""#)]
        );

        assert_lex!(
            r#" "hello\sworld" "#,
            [(TokenType::StringLiteral, r#""hello\sworld""#)]
        );

        // invalid escape seq
        assert_lex_errors!(
            r#" "hello \x world" "#,
            [LexicalErrorType::InvalidEscapeSequence]
        );

        // \\n is not supported in single-line strings
        // You may confused why the lexer doesn't throw UnterminatedString in this case
        // IntelliJ will not throw this error; IntelliJ recognizes it as a string.
        assert_lex_errors!(
            "\"hello \\\n world\"",
            [LexicalErrorType::InvalidEscapeSequence]
        );
    }

    #[test]
    fn test_octal_escapes() {
        assert_lex!(
            r#" "\0" "\77" "\177" "\377" "#,
            [
                (TokenType::StringLiteral, r#""\0""#),
                (TokenType::StringLiteral, r#""\77""#),
                (TokenType::StringLiteral, r#""\177""#),
                (TokenType::StringLiteral, r#""\377""#),
            ]
        );

        assert_lex!(r#" "\400" "#, [(TokenType::StringLiteral, r#""\400""#)]);

        assert_lex!(
            "'\\377' '\\0'",
            [
                (TokenType::CharLiteral, "'\\377'"),
                (TokenType::CharLiteral, "'\\0'"),
            ]
        );
    }

    #[test]
    fn test_text_block_specific_escapes() {
        let valid_text_block = "\"\"\"\n line 1 \\\n line 2 \\s\n\"\"\"";
        assert_lex!(valid_text_block, [(TokenType::TextBlock, valid_text_block)]);

        // invalid escape (\p)
        let invalid_text_block = "\"\"\"\n line 1 \\p \n\"\"\"";
        assert_lex_errors!(
            invalid_text_block,
            [LexicalErrorType::InvalidEscapeSequence]
        );
    }

    #[test]
    fn test_multiple_errors_in_string() {
        assert_lex_errors!(
            r#" " \q and \z " "#,
            [
                LexicalErrorType::InvalidEscapeSequence,
                LexicalErrorType::InvalidEscapeSequence
            ]
        );
    }

    #[test]
    fn test_string_template_simple() {
        assert_lex!(
            "STR.\"Hello \\{name}!\"",
            [
                (TokenType::Identifier, "STR"), // Template Processor
                (TokenType::Dot, "."),
                (TokenType::StringTemplateBegin, "\"Hello \\{"),
                (TokenType::Identifier, "name"),
                (TokenType::StringTemplateEnd, "}!\"")
            ]
        );
    }

    #[test]
    fn test_string_template_multiple_fragments() {
        assert_lex!(
            "\"a \\{b} c \\{d} e\"",
            [
                (TokenType::StringTemplateBegin, "\"a \\{"),
                (TokenType::Identifier, "b"),
                (TokenType::StringTemplateMid, "} c \\{"),
                (TokenType::Identifier, "d"),
                (TokenType::StringTemplateEnd, "} e\"")
            ]
        );
    }

    #[test]
    fn test_string_template_with_nested_braces() {
        assert_lex!(
            "\"Result: \\{ new int[]{1, 2} }\"",
            [
                (TokenType::StringTemplateBegin, "\"Result: \\{"),
                (TokenType::New, "new"),
                (TokenType::Int, "int"),
                (TokenType::LeftBracket, "["),
                (TokenType::RightBracket, "]"),
                (TokenType::LeftBrace, "{"),
                (TokenType::NumberLiteral, "1"),
                (TokenType::Comma, ","),
                (TokenType::NumberLiteral, "2"),
                (TokenType::RightBrace, "}"),          // array
                (TokenType::StringTemplateEnd, "}\"")  // template
            ]
        );
    }

    #[test]
    fn test_nested_string_templates() {
        assert_lex!(
            "\"Outer \\{ \"Inner \\{x}\" }\"",
            [
                (TokenType::StringTemplateBegin, "\"Outer \\{"),
                (TokenType::StringTemplateBegin, "\"Inner \\{"),
                (TokenType::Identifier, "x"),
                (TokenType::StringTemplateEnd, "}\""),
                (TokenType::StringTemplateEnd, "}\"")
            ]
        );
    }

    #[test]
    fn test_text_block_template() {
        assert_lex!(
            "\"\"\"\n  Line 1 \\{a}\n  Line 2\"\"\"",
            [
                (TokenType::TextBlockTemplateBegin, "\"\"\"\n  Line 1 \\{"),
                (TokenType::Identifier, "a"),
                (TokenType::TextBlockTemplateEnd, "}\n  Line 2\"\"\"")
            ]
        );
    }

    #[test]
    fn test_error_unterminated_string_template() {
        assert_lex_errors!("\"Hello \\{name", [LexicalErrorType::UnterminatedTemplate]);
    }


    #[test]
    fn test_text_block_opening_whitespace() {
        let valid_text_block = "\"\"\" \t \n  body\n\"\"\"";
        assert_lex!(
            valid_text_block,
            [(TokenType::TextBlock, "\"\"\" \t \n  body\n\"\"\"")]
        );
    }

    #[test]
    fn test_char_literal_bmp_only() {
        assert_lex_errors!("'🐘'", [LexicalErrorType::InvalidChar]);
    }

    #[test]
    fn test_eof_sub_character() {
        assert_lex!(
            "int x = 1;\x1A",
            [
                (TokenType::Int, "int"),
                (TokenType::Identifier, "x"),
                (TokenType::Equal, "="),
                (TokenType::NumberLiteral, "1"),
                (TokenType::Semicolon, ";"),
            ]
        );

        // \x1A should only appear on the end of file
        assert_lex_errors!(
            "int \x1A x = 1;",
            [LexicalErrorType::UnexpectedChar('\u{1a}')]
        );
    }
}
