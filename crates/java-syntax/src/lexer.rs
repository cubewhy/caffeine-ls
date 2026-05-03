use rowan::{TextRange, TextSize};

use crate::{
    lexer::{
        identifier::{is_java_identifier_part, is_java_identifier_start},
        token::Token,
    },
    reader::SourceReader,
    syntax_kind::SyntaxKind,
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
    fn begin_token(self) -> SyntaxKind {
        match self {
            TemplateKind::String => SyntaxKind::STRING_TEMPLATE_BEGIN,
            TemplateKind::TextBlock => SyntaxKind::TEXT_BLOCK_TEMPLATE_BEGIN,
        }
    }

    fn mid_token(self) -> SyntaxKind {
        match self {
            TemplateKind::String => SyntaxKind::STRING_TEMPLATE_MID,
            TemplateKind::TextBlock => SyntaxKind::TEXT_BLOCK_TEMPLATE_MID,
        }
    }

    fn end_token(self) -> SyntaxKind {
        match self {
            TemplateKind::String => SyntaxKind::STRING_TEMPLATE_END,
            TemplateKind::TextBlock => SyntaxKind::TEXT_BLOCK_TEMPLATE_END,
        }
    }

    fn literal_token(self) -> SyntaxKind {
        match self {
            TemplateKind::String => SyntaxKind::STRING_LITERAL,
            TemplateKind::TextBlock => SyntaxKind::TEXT_BLOCK,
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

pub struct Lexer<'a> {
    reader: SourceReader<'a>,
    tokens: Vec<Token<'a>>,
    errors: Vec<LexicalError>,

    mode: LexerMode,
    template_stack: Vec<TemplateContext>,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            reader: SourceReader::new(source),
            tokens: Vec::new(),
            errors: Vec::new(),
            mode: LexerMode::Normal,
            template_stack: Vec::new(),
        }
    }

    pub fn scan_tokens(mut self) -> (Vec<Token<'a>>, Vec<LexicalError>) {
        // consume BOM
        if self.reader.peek() == '\u{FEFF}' {
            self.reader.advance();
        }

        while !self.reader.is_at_end() {
            self.scan_next_token();
        }

        if !self.template_stack.is_empty() {
            // unterminated string/textblock template
            self.report_error(LexicalErrorKind::UnterminatedTemplate);
        }

        self.errors.extend(
            self.reader
                .errors()
                .iter()
                .map(|e| LexicalError::new(LexicalErrorKind::InvalidUnicodeEscape, e.range)),
        );

        (self.tokens, self.errors)
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
                    '(' => self.push_token(SyntaxKind::L_PAREN),
                    ')' => self.push_token(SyntaxKind::R_PAREN),
                    '[' => self.push_token(SyntaxKind::L_BRACKET),
                    ']' => self.push_token(SyntaxKind::R_BRACKET),
                    ';' => self.push_token(SyntaxKind::SEMICOLON),
                    ',' => self.push_token(SyntaxKind::COMMA),
                    ':' => self.handle_colon(),
                    '?' => self.push_token(SyntaxKind::QUESTION),
                    '@' => self.push_token(SyntaxKind::AT),
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
                    '~' => self.push_token(SyntaxKind::TILDE),

                    '\x1A' => {
                        // https://docs.oracle.com/javase/specs/jls/se17/html/jls-3.html#jls-3.5
                        // Ascii SUB character
                        if !self.reader.is_at_end() {
                            self.report_error(LexicalErrorKind::UnexpectedChar('\x1A'));
                        }
                    }

                    c if is_java_whitespace(c) => self.handle_whitespace(),

                    c => {
                        if is_java_identifier_start(c) {
                            self.handle_identifier();
                        } else {
                            self.push_token(SyntaxKind::UNKNOWN);
                            self.report_error(LexicalErrorKind::UnexpectedChar(c));
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
        self.push_token(SyntaxKind::WHITESPACE);
    }

    fn handle_left_brace(&mut self) {
        if self.mode == LexerMode::TemplateExpression
            && let Some(ctx) = self.template_stack.last_mut()
        {
            ctx.brace_depth += 1;
        }

        self.reader.advance();
        self.push_token(SyntaxKind::L_BRACE);
    }

    fn handle_right_brace(&mut self) {
        if self.mode != LexerMode::TemplateExpression {
            self.reader.advance();
            self.push_token(SyntaxKind::R_BRACE);
            return;
        }

        let Some(ctx) = self.template_stack.last_mut() else {
            self.mode = LexerMode::Normal;
            self.reader.advance();
            self.push_token(SyntaxKind::R_BRACE);
            return;
        };

        if ctx.brace_depth > 0 {
            ctx.brace_depth -= 1;
            self.reader.advance();
            self.push_token(SyntaxKind::R_BRACE);
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
                        self.report_error(LexicalErrorKind::InvalidNumber);
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
            self.report_error(LexicalErrorKind::InvalidNumber);
            last_was_underscore = false;
        }

        // Parse float fractional part
        if self.reader.peek() == '.' {
            self.reader.advance(); // '.'
            is_float = true;

            // Java doesn't allow `1._2`
            if self.reader.peek() == '_' {
                self.report_error(LexicalErrorKind::InvalidNumber);
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
                self.report_error(LexicalErrorKind::InvalidNumber);
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
                self.report_error(LexicalErrorKind::InvalidNumber);
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
                self.report_error(LexicalErrorKind::InvalidNumber);
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
            self.report_error(LexicalErrorKind::InvalidNumber);
        }

        self.push_token(SyntaxKind::NUMBER_LITERAL);
    }

    fn handle_dot(&mut self) {
        if self.reader.peek_next().is_numeric() {
            // float number
            self.handle_number();

            return;
        }

        let token_type = if self.reader.advance_if_matches_str("...") {
            // ...
            SyntaxKind::ELLIPSIS
        } else {
            // .
            self.reader.advance();
            SyntaxKind::DOT
        };

        self.push_token(token_type);
    }

    fn handle_colon(&mut self) {
        let token_type = if self.reader.advance_if_matches(':') {
            // ::
            SyntaxKind::COLON_COLON
        } else {
            SyntaxKind::COLON
        };

        self.push_token(token_type);
    }

    fn handle_mod(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            SyntaxKind::MODULO_EQUAL
        } else {
            SyntaxKind::MODULO
        };

        self.push_token(token_type);
    }

    fn handle_bang(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            SyntaxKind::NOT_EQUAL
        } else {
            SyntaxKind::NOT
        };

        self.push_token(token_type);
    }

    fn handle_or(&mut self) {
        let token_type = if self.reader.advance_if_matches('|') {
            SyntaxKind::OR
        } else if self.reader.advance_if_matches('=') {
            SyntaxKind::OR_EQUAL
        } else {
            SyntaxKind::BIT_OR
        };

        self.push_token(token_type);
    }

    fn handle_and(&mut self) {
        let token_type = if self.reader.advance_if_matches('&') {
            SyntaxKind::AND
        } else if self.reader.advance_if_matches('=') {
            SyntaxKind::AND_EQUAL
        } else {
            SyntaxKind::BIT_AND
        };

        self.push_token(token_type);
    }

    fn handle_star(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            SyntaxKind::MULTIPLE_EQUAL
        } else {
            SyntaxKind::STAR
        };

        self.push_token(token_type);
    }

    fn handle_plus(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            SyntaxKind::PLUS_EQUAL
        } else if self.reader.advance_if_matches('+') {
            SyntaxKind::PLUS_PLUS
        } else {
            SyntaxKind::PLUS
        };

        self.push_token(token_type);
    }

    fn handle_caret(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            SyntaxKind::XOR_EQUAL
        } else {
            SyntaxKind::CARET
        };

        self.push_token(token_type);
    }

    fn handle_minus(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            // -=
            SyntaxKind::MINUS_EQUAL
        } else if self.reader.advance_if_matches('-') {
            // --
            SyntaxKind::MINUS_MINUS
        } else if self.reader.advance_if_matches('>') {
            // ->
            SyntaxKind::ARROW
        } else {
            SyntaxKind::MINUS
        };

        self.push_token(token_type);
    }

    fn handle_less(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            SyntaxKind::LESS_EQUAL
        } else if self.reader.advance_if_matches('<') {
            if self.reader.advance_if_matches('=') {
                SyntaxKind::LEFT_SHIFT_EQUAL // <<=
            } else {
                SyntaxKind::LEFT_SHIFT
            }
        } else {
            SyntaxKind::LESS
        };

        self.push_token(token_type);
    }

    fn handle_greater(&mut self) {
        let token_type = if self.reader.advance_if_matches('=') {
            // >=
            SyntaxKind::GREATER_EQUAL
        } else if self.reader.advance_if_matches('>') {
            // >>
            if self.reader.advance_if_matches('>') {
                // >>>
                if self.reader.advance_if_matches('=') {
                    // >>>=
                    SyntaxKind::UNSIGNED_RIGHT_SHIFT_EQUAL
                } else {
                    // >>>
                    SyntaxKind::UNSIGNED_RIGHT_SHIFT
                }
            } else if self.reader.advance_if_matches('=') {
                // >>=
                SyntaxKind::RIGHT_SHIFT_EQUAL
            } else {
                // >>
                SyntaxKind::RIGHT_SHIFT
            }
        } else {
            // >
            SyntaxKind::GREATER
        };

        self.push_token(token_type);
    }

    fn handle_eq(&mut self) {
        let token_type = if self.reader.advance_if_matches_str("=") {
            SyntaxKind::EQUAL_EQUAL // ==
        } else {
            SyntaxKind::EQUAL // =
        };

        self.push_token(token_type);
    }

    fn handle_slash(&mut self) {
        // https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.7
        if self.reader.advance_if_matches('/') {
            let token_type = if self.reader.advance_if_matches('/') {
                SyntaxKind::JAVADOC_LINE
            } else {
                SyntaxKind::LINE_COMMENT
            };
            // single-line comment //
            while let c = self.reader.peek()
                && c != '\n'
                && c != '\r'
                && !self.reader.is_at_end()
            {
                self.reader.advance();
            }
            self.push_token(token_type);
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
                self.report_error(LexicalErrorKind::UnterminatedComment);
            }

            let token_type = if is_javadoc {
                SyntaxKind::JAVADOC
            } else {
                SyntaxKind::BLOCK_COMMENT
            };
            self.push_token(token_type);
        } else if self.reader.advance_if_matches('=') {
            // /=
            self.push_token(SyntaxKind::DIVIDE_EQUAL);
        } else {
            // /
            self.push_token(SyntaxKind::SLASH);
        }
    }

    fn handle_identifier(&mut self) {
        while !self.reader.is_at_end() && is_java_identifier_part(self.reader.peek()) {
            self.reader.advance(); // consume next char
        }

        let text = self.reader.current_token_lexeme();
        let token_type = match text {
            "package" => SyntaxKind::PACKAGE_KW,
            "import" => SyntaxKind::IMPORT_KW,
            "class" => SyntaxKind::CLASS_KW,
            "enum" => SyntaxKind::ENUM_KW,
            "interface" => SyntaxKind::INTERFACE_KW,
            "public" => SyntaxKind::PUBLIC_KW,
            "private" => SyntaxKind::PRIVATE_KW,
            "final" => SyntaxKind::FINAL_KW,
            "static" => SyntaxKind::STATIC_KW,
            "protected" => SyntaxKind::PROTECTED_KW,
            "abstract" => SyntaxKind::ABSTRACT_KW,
            "for" => SyntaxKind::FOR_KW,
            "while" => SyntaxKind::WHILE_KW,
            "continue" => SyntaxKind::CONTINUE_KW,
            "break" => SyntaxKind::BREAK_KW,
            "instanceof" => SyntaxKind::INSTANCEOF_KW,
            "return" => SyntaxKind::RETURN_KW,
            "transient" => SyntaxKind::TRANSIENT_KW,
            "extends" => SyntaxKind::EXTENDS_KW,
            "implements" => SyntaxKind::IMPLEMENTS_KW,
            "new" => SyntaxKind::NEW_KW,
            "assert" => SyntaxKind::ASSERT_KW,
            "switch" => SyntaxKind::SWITCH_KW,
            "case" => SyntaxKind::CASE_KW,
            "default" => SyntaxKind::DEFAULT_KW,
            "synchronized" => SyntaxKind::SYNCHRONIZED_KW,
            "do" => SyntaxKind::DO_KW,
            "if" => SyntaxKind::IF_KW,
            "else" => SyntaxKind::ELSE_KW,
            "this" => SyntaxKind::THIS_KW,
            "super" => SyntaxKind::SUPER_KW,
            "volatile" => SyntaxKind::VOLATILE_KW,
            "native" => SyntaxKind::NATIVE_KW,
            "throw" => SyntaxKind::THROW_KW,
            "throws" => SyntaxKind::THROWS_KW,
            "try" => SyntaxKind::TRY_KW,
            "catch" => SyntaxKind::CATCH_KW,
            "finally" => SyntaxKind::FINALLY_KW,
            "strictfp" => SyntaxKind::STRICTFP_KW,

            // primitive types
            "void" => SyntaxKind::VOID_KW,
            "double" => SyntaxKind::DOUBLE_KW,
            "int" => SyntaxKind::INT_KW,
            "short" => SyntaxKind::SHORT_KW,
            "long" => SyntaxKind::LONG_KW,
            "float" => SyntaxKind::FLOAT_KW,
            "char" => SyntaxKind::CHAR_KW,
            "boolean" => SyntaxKind::BOOLEAN_KW,
            "byte" => SyntaxKind::BYTE_KW,

            // Seems like keywords but they are actually literals
            "null" => SyntaxKind::NULL_LITERAL,
            "true" => SyntaxKind::TRUE_LITERAL,
            "false" => SyntaxKind::FALSE_LITERAL,

            // reserved keywords
            "goto" => SyntaxKind::GOTO_KW,
            "const" => SyntaxKind::CONST_KW,

            "_" => SyntaxKind::UNDERSCORE,

            _ => SyntaxKind::IDENTIFIER,
        };

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
                self.report_error(LexicalErrorKind::UnterminatedChar);
                return;
            }

            if c == '\\' {
                if !self.consume_escape_sequence(false) {
                    self.report_error(LexicalErrorKind::InvalidEscapeSequence);
                    has_error = true;
                }
            } else {
                self.reader.advance();
            }
            logical_char_count += c.len_utf16();
        }

        if self.reader.is_at_end() {
            self.report_error(LexicalErrorKind::UnterminatedChar);
            return;
        }

        self.reader.advance(); // '

        if !has_error && logical_char_count != 1 {
            self.report_error(LexicalErrorKind::InvalidChar);
        }

        self.push_token(SyntaxKind::CHAR_LITERAL);
    }

    fn scan_quoted_content(&mut self, kind: TemplateKind, role: TemplateChunkRole) {
        if kind == TemplateKind::TextBlock && role == TemplateChunkRole::FullLiteral {
            while matches!(self.reader.peek(), '\u{0020}' | '\u{0009}' | '\u{000C}') {
                self.reader.advance();
            }

            let next_char = self.reader.peek();
            if next_char != '\n' && next_char != '\r' {
                self.report_error(LexicalErrorKind::IllegalTextBlockOpen);
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
                        self.report_error(LexicalErrorKind::UnterminatedString);
                    }
                    TemplateChunkRole::Continuation => {
                        self.report_error(LexicalErrorKind::UnterminatedTemplate);
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
                    self.report_error(LexicalErrorKind::InvalidEscapeSequence);
                }
                continue;
            }

            self.reader.advance();
        }

        match (kind, role) {
            (TemplateKind::String, TemplateChunkRole::FullLiteral) => {
                self.report_error(LexicalErrorKind::UnterminatedString);
            }
            (TemplateKind::TextBlock, TemplateChunkRole::FullLiteral) => {
                self.report_error(LexicalErrorKind::UnterminatedTextBlock);
            }
            (_, TemplateChunkRole::Continuation) => {
                self.report_error(LexicalErrorKind::UnterminatedTemplate);
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

    fn push_token(&mut self, token_type: SyntaxKind) {
        self.tokens.push(Token::new(
            token_type,
            self.reader.current_token_lexeme_raw(),
            self.reader.start(),
        ));
    }

    fn report_error(&mut self, error_type: LexicalErrorKind) {
        let start = TextSize::from(self.reader.start() as u32);
        let end = TextSize::from(self.reader.current() as u32);
        self.errors
            .push(LexicalError::new(error_type, TextRange::new(start, end)));
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LexicalError {
    pub kind: LexicalErrorKind,
    pub range: TextRange,
}

impl LexicalError {
    pub fn new(error_type: LexicalErrorKind, range: TextRange) -> Self {
        Self {
            kind: error_type,
            range,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LexicalErrorKind {
    UnexpectedChar(char),
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

pub fn lex(src: &str) -> (Vec<Token<'_>>, Vec<LexicalError>) {
    Lexer::new(src).scan_tokens()
}
