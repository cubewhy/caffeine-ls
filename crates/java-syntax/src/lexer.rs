use crate::{
    lexer::{
        identifier::{is_java_identifier_part, is_java_identifier_start},
        token::{JavaToken, TokenType},
    },
    reader::SourceReader,
};

pub mod identifier;
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
            TemplateKind::String => TokenType::StringLit,
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
        // consume BOM
        if self.reader.peek() == '\u{FEFF}' {
            self.reader.advance();
        }

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

                    c if is_java_whitespace(c) => self.handle_whitespace(),

                    c => {
                        if is_java_identifier_start(c) {
                            self.handle_identifier();
                        } else {
                            self.push_token(TokenType::Unknown);
                            self.report_error(LexicalErrorType::UnexpectedChar(c));
                        }
                    }
                }
            }
        }
    }

    fn handle_whitespace(&mut self) {
        // consume remaining whitespace
        while is_java_whitespace(self.reader.peek()) {
            self.reader.advance();
        }
        self.push_token(TokenType::Whitespace);
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
        let mut has_invalid_octal_digit = false;
        // https://docs.oracle.com/javase/specs/jls/se25/html/jls-3.html#jls-3.10.2
        let mut is_float = false;

        loop {
            let c = self.reader.peek();
            if c.is_numeric() || c.is_digit(consume_base) {
                if !c.is_digit(num_base) {
                    if num_base == 8 && c.is_ascii_digit() {
                        has_invalid_octal_digit = true;
                    } else {
                        self.report_error(LexicalErrorType::InvalidNumber);
                    }
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
            is_float = true;

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
            is_float = true;

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
        let suffix = self.reader.peek().to_ascii_lowercase();
        if matches!(suffix, 'l' | 'f' | 'd') {
            if suffix == 'f' || suffix == 'd' {
                is_float = true;
            }
            self.reader.advance();
        }

        if num_base == 8 && has_invalid_octal_digit && !is_float {
            self.report_error(LexicalErrorType::InvalidNumber);
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
        if self.reader.advance_if_matches('/') {
            // single-line comment //
            while let c = self.reader.peek()
                && c != '\n'
                && c != '\r'
                && !self.reader.is_at_end()
            {
                self.reader.advance();
            }
            self.push_token(TokenType::LineComment);
        } else if self.reader.advance_if_matches('*') {
            // multiple line comment /* */ or javadoc /** */
            let is_javadoc = self.reader.peek() == '*' && self.reader.peek_next() != '/';

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

            let token_type = if is_javadoc {
                TokenType::Javadoc
            } else {
                TokenType::BlockComment
            };
            self.push_token(token_type);
        } else if self.reader.advance_if_matches('=') {
            // /=
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
            'b' | 't' | 'n' | 'f' | 'r' | 's' | '"' | '\'' | '\\' => {
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
            logical_char_count += c.len_utf16();
        }

        if self.reader.is_at_end() {
            self.report_error(LexicalErrorType::UnterminatedChar);
            return;
        }

        self.reader.advance(); // '

        if !has_error && logical_char_count != 1 {
            self.report_error(LexicalErrorType::InvalidChar);
        }

        self.push_token(TokenType::CharLit);
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

#[derive(Debug, Clone, Copy)]
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

/// Determinate a char is java whitespace
/// https://docs.oracle.com/javase/specs/jls/se25/html/jls-3.html#jls-3.6
fn is_java_whitespace(c: char) -> bool {
    matches!(c, '\u{0020}' | '\u{0009}' | '\u{000C}' | '\n' | '\r')
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use insta::assert_debug_snapshot;

    /// Helper to lex source and return filtered non-trivia tokens for snapshotting
    fn lex_tokens(source: &str) -> Vec<(TokenType, &str)> {
        let mut lexer = JavaLexer::new(source);
        match lexer.scan_tokens() {
            Ok(tokens) => tokens.iter().map(|t| (t.token_type, t.lexeme)).collect(),
            Err((tokens, errors)) => {
                panic!(
                    "Lexing failed unexpectedly for: '{}'\nTokens: {:#?}\nErrors: {:#?}",
                    source, tokens, errors
                );
            }
        }
    }

    /// Helper to lex source and return errors for snapshotting
    fn lex_errors(source: &str) -> Vec<LexicalErrorType> {
        let mut lexer = JavaLexer::new(source);
        match lexer.scan_tokens() {
            Ok(tokens) => panic!(
                "Expected errors but lexing succeeded for: '{}'\nTokens: {:#?}",
                source, tokens
            ),
            Err((_, errors)) => errors.iter().map(|e| e.error_type).collect(),
        }
    }

    #[test]
    fn test_empty_and_whitespace() {
        assert_debug_snapshot!(lex_tokens(""));
        assert_debug_snapshot!(lex_tokens(" \t\n\r  "));
    }

    #[test]
    fn test_comments() {
        // Comments should be consumed and yield no tokens (if non-trivia)
        assert_debug_snapshot!(lex_tokens("// this is a line comment\n"));
        assert_debug_snapshot!(lex_tokens("/* this is a \n block comment */"));
        assert_debug_snapshot!(lex_tokens("int /* comment */ x // line"));
    }

    #[test]
    fn test_javadoc() {
        // Javadoc is technically a block comment starting with /**
        let source = indoc! {"
            /**
             * This is a Javadoc comment
             * @param x the value
             */
            public int x;
        "};
        // We ensure the lexer skips it correctly or identifies it if configured
        assert_debug_snapshot!(lex_tokens(source));
    }

    #[test]
    fn test_keywords_and_identifiers() {
        assert_debug_snapshot!(lex_tokens("public static void main"));
        assert_debug_snapshot!(lex_tokens("class interface enum record"));
        assert_debug_snapshot!(lex_tokens("$myVar _underscore value123"));
    }

    #[test]
    fn test_boolean_and_null_literals() {
        assert_debug_snapshot!(lex_tokens("true false null"));
    }

    #[test]
    fn test_separators() {
        assert_debug_snapshot!(lex_tokens("( ) { } [ ] ; , . ... :: @"));
    }

    #[test]
    fn test_operators() {
        let source = indoc! {"
            + += ++ - -= -- ->
            * *= / /= % %= == = != !
            < <= << <<= > >= >> >>= >>> >>>=
            & &= | |= ^ ^= && ||
        "};
        assert_debug_snapshot!(lex_tokens(source));
    }

    #[test]
    fn test_integer_literals() {
        assert_debug_snapshot!(lex_tokens("0 123 1_000_000 456L"));
        assert_debug_snapshot!(lex_tokens("0x0 0x1A2B 0XCAFE_BABE 0xFFl"));
        assert_debug_snapshot!(lex_tokens("0b0 0B1010_0101 0b11L"));
    }

    #[test]
    fn test_floating_point_literals() {
        assert_debug_snapshot!(lex_tokens("1.23 .5 10. 3.14f 6.022e23 1e-9d"));
        assert_debug_snapshot!(lex_tokens("0x1.0p3 0x.8P-2f"));
    }

    #[test]
    fn test_string_literals() {
        assert_debug_snapshot!(lex_tokens(r#" "hello world" "escape \" test" "" "#));
    }

    #[test]
    fn test_text_blocks() {
        let source = indoc! {r#"
            """
            Hello
              World
            """
        "#};
        assert_debug_snapshot!(lex_tokens(source));
    }

    #[test]
    fn test_char_literals() {
        assert_debug_snapshot!(lex_tokens("'a' '\\n' '\\''"));
    }

    #[test]
    fn test_complex_jls_scenario() {
        assert_debug_snapshot!(lex_tokens("List<String> list = new ArrayList<>();"));
    }

    #[test]
    fn test_error_unterminated_string() {
        assert_debug_snapshot!(lex_errors("\"this string has no end"));
        assert_debug_snapshot!(lex_errors("\"line1\nline2\""));
    }

    #[test]
    fn test_error_unterminated_comment() {
        assert_debug_snapshot!(lex_errors("/* this block comment never ends "));
    }

    #[test]
    fn test_error_invalid_numbers() {
        assert_debug_snapshot!(lex_errors("123_"));
        assert_debug_snapshot!(lex_errors("0b1012"));
        assert_debug_snapshot!(lex_errors("0._1f"));
        assert_debug_snapshot!(lex_errors("019"));
    }

    #[test]
    fn test_error_illegal_text_block_open() {
        assert_debug_snapshot!(lex_errors("\"\"\"illegal"));
    }

    #[test]
    fn test_error_invalid_char() {
        assert_debug_snapshot!(lex_errors("'abc'"));
        assert_debug_snapshot!(lex_errors("''"));
    }

    #[test]
    fn test_unicode_escapes() {
        // Testing keywords, identifiers and multiple 'u's
        assert_debug_snapshot!(lex_tokens("\\u0070ublic class Test {}"));
        assert_debug_snapshot!(lex_tokens("int my\\u005Fvar = 1;"));
        assert_debug_snapshot!(lex_tokens("char \\uuuu0061 = 'a';"));
        assert_debug_snapshot!(lex_tokens("int a = 1 \\u002B\\u002B;"));
    }

    #[test]
    fn test_unicode_escape_in_strings() {
        assert_debug_snapshot!(lex_errors("String s = \"\\u0022\";"));
        assert_debug_snapshot!(lex_tokens("String s = \"\\u005C\\u0022\";"));
    }

    #[test]
    fn test_error_invalid_unicode_escapes() {
        assert_debug_snapshot!(lex_errors("int \\u006 = 1;"));
        assert_debug_snapshot!(lex_errors("int \\u006G = 1;"));
    }

    #[test]
    fn test_comment_termination_variants() {
        // Unicode escapes acting as line terminators for comments
        assert_debug_snapshot!(lex_tokens("// hidden comment \\u000A int x = 1;"));
        assert_debug_snapshot!(lex_tokens("// normal comment \r int z = 3;"));
        assert_debug_snapshot!(lex_tokens("// normal comment \r\n int w = 4;"));
    }

    #[test]
    fn test_invalid_underscore_placements() {
        assert_debug_snapshot!(lex_errors("123_"));
        assert_debug_snapshot!(lex_errors("123_.45"));
        assert_debug_snapshot!(lex_errors("123._45"));
        assert_debug_snapshot!(lex_errors("1e_10"));
        assert_debug_snapshot!(lex_errors("1e+_10"));
    }

    #[test]
    fn test_string_template_features() {
        assert_debug_snapshot!(lex_tokens("STR.\"Hello \\{name}!\""));
        assert_debug_snapshot!(lex_tokens("\"a \\{b} c \\{d} e\""));

        let nested = indoc! {r#"
            "Result: \{ new int[]{1, 2} }"
        "#};
        assert_debug_snapshot!(lex_tokens(nested));

        assert_debug_snapshot!(lex_tokens("\"Outer \\{ \"Inner \\{x}\" }\""));
    }

    #[test]
    fn test_text_block_template() {
        let source = indoc! {r#"
            """
              Line 1 \{a}
              Line 2"""
        "#};
        assert_debug_snapshot!(lex_tokens(source));
    }

    #[test]
    fn test_special_characters_and_bom() {
        // UTF-8 BOM
        assert_debug_snapshot!(lex_tokens("\u{FEFF}int x = 1;"));
        // EOF Sub character
        assert_debug_snapshot!(lex_tokens("int x = 1;\x1A"));
        // Emoji validation (BMP only for chars)
        assert_debug_snapshot!(lex_tokens("'你'"));
        assert_debug_snapshot!(lex_errors("'🐘'"));
    }

    #[test]
    fn test_escape_sequence_s() {
        assert_debug_snapshot!(lex_tokens(r#" "trailing space\s" "#));
        let text_block = indoc! {r#"
            """
                line 1\s
                line 2
            """
        "#};
        assert_debug_snapshot!(lex_tokens(text_block));
    }
}
