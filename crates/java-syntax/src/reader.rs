use std::borrow::Cow;
use std::char;

use rowan::{TextRange, TextSize};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct UnicodeEscapeError {
    pub range: TextRange,
}

impl UnicodeEscapeError {
    fn new(current: usize, len: usize) -> Self {
        UnicodeEscapeError {
            range: TextRange::at(TextSize::from(current as u32), TextSize::from(len as u32)),
        }
    }
}

/// Result of classifying a single logical character in the raw source.
enum ScanResult {
    /// A well-formed character: a literal UTF-8 char *or* a valid `\uXXXX` escape.
    Char(char, usize),
    /// The byte at this offset is the `\` that *starts* an ill-formed `\uXXXX`
    /// sequence (missing digits, non-hex digits, or isolated surrogate).
    /// The logical character is `\` (1 raw byte); the error is recorded only
    /// when `advance()` actually consumes it.
    InvalidEscape(usize),
}

/// The characters `text` denotes, with [JLS §3.3]'s Unicode escapes
/// translated — the character stream [`SourceReader`] walks, and the
/// translation the lexer tokenizes by.
///
/// The lexer keeps every token's *text* as written in the source, so a
/// consumer that needs a literal's value (or an identifier's name) out of that
/// text re-reads it through here rather than re-implementing the escape.
///
/// [JLS §3.3]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.3
pub fn translate_unicode_escapes(text: &str) -> Cow<'_, str> {
    // A source without a backslash carries no escape.
    if !text.as_bytes().contains(&b'\\') {
        return Cow::Borrowed(text);
    }
    let mut reader = SourceReader::new(text);
    let mut out = String::with_capacity(text.len());
    while !reader.is_at_end() {
        out.push(reader.advance());
    }
    Cow::Owned(out)
}

pub struct SourceReader<'a> {
    source: &'a str,
    start: usize,
    current: usize,
    errors: Vec<UnicodeEscapeError>,
}

impl<'a> SourceReader<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            start: 0,
            current: 0,
            errors: Vec::new(),
        }
    }

    /// All errors recorded so far (lazily populated by `advance()`).
    pub fn errors(&self) -> &[UnicodeEscapeError] {
        &self.errors
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn new_token(&mut self) {
        self.start = self.current;
    }

    pub fn current_token_lexeme_raw(&self) -> &'a str {
        &self.source[self.start..self.current]
    }

    /// The token's lexeme with [JLS §3.3]'s Unicode escapes translated — what
    /// the *token* means, as opposed to [`Self::current_token_lexeme_raw`],
    /// which is what the source says. Borrowed unless the token carries an
    /// escape.
    ///
    /// [JLS §3.3]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.3
    pub fn current_token_lexeme(&self) -> Cow<'a, str> {
        translate_unicode_escapes(&self.source[self.start..self.current])
    }

    pub fn is_at_end(&self) -> bool {
        self.current >= self.source.len()
    }

    /// Logical character at the current position, **without** advancing.
    ///
    /// - Valid `\uXXXX` escape → decoded `char`
    /// - Invalid `\uXXXX` escape → raw `\` (no error recorded yet)
    /// - End of input → `\0`
    pub fn peek(&self) -> char {
        self.peek_n(0)
    }

    /// Logical character **after** the current one, without advancing.
    pub fn peek_next(&self) -> char {
        self.peek_n(1)
    }

    /// Logical character **after** n, without advancing.
    pub fn peek_n(&self, n: usize) -> char {
        let mut offset = self.current;

        for _ in 0..n {
            let len = self.logical_len_at(offset);
            if len == 0 {
                return '\0';
            }
            offset += len;
        }

        self.logical_char_at(offset)
    }

    /// Advance if the current logical character equals `expected`.
    /// Records an error (without panicking) if `expected == '\\'` and the
    /// current position is an invalid unicode escape.
    pub fn advance_if_matches(&mut self, expected: char) -> bool {
        match self.scan_at(self.current) {
            ScanResult::Char(c, len) if c == expected => {
                self.current += len;
                true
            }
            ScanResult::InvalidEscape(len) if expected == '\\' => {
                self.errors.push(UnicodeEscapeError::new(self.current, len));
                self.current += 1;
                true
            }
            _ => false,
        }
    }

    /// Raw-byte prefix match — does **not** interpret unicode escapes.
    pub fn matches(&self, expected: &str) -> bool {
        self.source[self.current..].starts_with(expected)
    }

    /// Raw-byte advance — advances by the byte length of `expected`.
    /// Does **not** interpret unicode escapes.
    pub fn advance_if_matches_str(&mut self, expected: &str) -> bool {
        if self.matches(expected) {
            self.current += expected.len();
            true
        } else {
            false
        }
    }

    /// Consume and return the current logical character.
    ///
    /// - Valid escape or literal → decoded char; `current` jumps past all raw bytes.
    /// - Invalid escape → records an error, returns `\`, advances **1 byte** so
    ///   subsequent calls expose the raw `u`, hex digits, etc. one at a time.
    pub fn advance(&mut self) -> char {
        match self.scan_at(self.current) {
            ScanResult::Char(c, len) => {
                self.current += len;
                c
            }
            ScanResult::InvalidEscape(len) => {
                self.errors.push(UnicodeEscapeError {
                    range: TextRange::at(
                        TextSize::from(self.current as u32),
                        TextSize::from(len as u32),
                    ),
                });
                self.current += 1;
                '\\'
            }
        }
    }

    #[inline]
    fn logical_char_at(&self, offset: usize) -> char {
        match self.scan_at(offset) {
            ScanResult::Char(c, _) => c,
            ScanResult::InvalidEscape(_) => '\\',
        }
    }

    #[inline]
    fn logical_len_at(&self, offset: usize) -> usize {
        match self.scan_at(offset) {
            ScanResult::Char(_, len) => len,
            ScanResult::InvalidEscape(_) => 1,
        }
    }

    /// Classify the character that starts at byte `offset` in the raw source.
    ///
    /// ## JLS §3.3 rules implemented
    ///
    /// 1. A `\u` is a unicode escape only when preceded by an **even** number
    ///    of backslashes (0, 2, 4 …) in the raw source.
    ///    - `\u0041`   → 'A'
    ///    - `\\u0041`  → `\`, `\`, then literal `u0041`
    ///    - `\\\u0041` → `\`, `\`, then 'A'  (third `\` has even preceding count)
    ///
    /// 2. One or more `u` characters are allowed: `\uuu0041` → 'A'.
    ///
    /// 3. Exactly 4 hex digits must follow the `u` sequence.
    ///
    /// 4. Translation is a **single pass** on the raw source — a `\` produced
    ///    by e.g. `\u005C` does not trigger further escape processing, because
    ///    the scanner operates on raw byte positions.
    fn scan_at(&self, offset: usize) -> ScanResult {
        match self.scan_code_unit(offset) {
            Ok((code1, len1)) => {
                if let Some(c) = char::from_u32(code1) {
                    return ScanResult::Char(c, len1);
                }

                // High Surrogate: U+D800..=U+DBFF
                if (0xD800..=0xDBFF).contains(&code1)
                    && let Ok((code2, len2)) = self.scan_code_unit(offset + len1)
                {
                    // Expect a Low Surrogate: U+DC00..=U+DFFF
                    if (0xDC00..=0xDFFF).contains(&code2) {
                        // Merge the character
                        let scalar = (0x10000 + ((code1 - 0xD800) << 10)) | (code2 - 0xDC00);

                        let c = char::from_u32(scalar).unwrap();
                        return ScanResult::Char(c, len1 + len2);
                    }
                }

                ScanResult::InvalidEscape(len1)
            }
            Err(err_len) => ScanResult::InvalidEscape(err_len),
        }
    }

    fn scan_code_unit(&self, offset: usize) -> Result<(u32, usize), usize> {
        if offset >= self.source.len() {
            return Ok(('\0' as u32, 0));
        }

        let bytes = self.source.as_bytes();

        if bytes[offset] != b'\\' {
            let c = self.source[offset..].chars().next().unwrap_or('\0');
            return Ok((c as u32, c.len_utf8()));
        }

        if !self.count_preceding_backslashes(offset).is_multiple_of(2) {
            return Ok(('\\' as u32, 1));
        }

        let next = offset + 1;
        if next >= bytes.len() || bytes[next] != b'u' {
            return Ok(('\\' as u32, 1));
        }

        let mut i = next;
        while i < bytes.len() && bytes[i] == b'u' {
            i += 1;
        }

        let hex_start = i;
        let hex_end = hex_start + 4;

        if hex_end > bytes.len() {
            return Err(bytes.len() - offset);
        }

        let hex_str = &self.source[hex_start..hex_end];
        if !hex_str.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(hex_end - offset);
        }

        let code = u32::from_str_radix(hex_str, 16).unwrap();
        Ok((code, hex_end - offset))
    }

    /// Count the number of backslash bytes immediately *before* `offset`
    /// in the raw source (does not cross a non-backslash byte).
    fn count_preceding_backslashes(&self, offset: usize) -> usize {
        let bytes = self.source.as_bytes();
        let mut count = 0;
        let mut i = offset;
        while i > 0 && bytes[i - 1] == b'\\' {
            count += 1;
            i -= 1;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_ascii() {
        let mut reader = SourceReader::new("abc");
        assert_eq!(reader.peek(), 'a');
        assert_eq!(reader.advance(), 'a');
        assert_eq!(reader.peek(), 'b');
        assert_eq!(reader.peek_next(), 'c');
        assert_eq!(reader.advance(), 'b');
        assert!(reader.advance_if_matches('c'));
        assert!(reader.is_at_end());
        assert_eq!(reader.advance(), '\0');
    }

    #[test]
    fn test_multi_byte_utf8() {
        let mut reader = SourceReader::new("你好");
        assert_eq!(reader.peek(), '你');
        assert_eq!(reader.current(), 0);
        assert_eq!(reader.advance(), '你');
        assert_eq!(reader.current(), 3); // '你' is 3 bytes
        assert_eq!(reader.peek(), '好');
        assert_eq!(reader.advance(), '好');
        assert_eq!(reader.current(), 6);
        assert!(reader.is_at_end());
    }

    #[test]
    fn test_current_token_lexeme() {
        let mut reader = SourceReader::new("let value = 1;");
        reader.new_token();
        reader.advance(); // 'l'
        reader.advance(); // 'e'
        reader.advance(); // 't'
        assert_eq!(reader.current_token_lexeme_raw(), "let");
    }

    #[test]
    fn test_translate_unicode_escapes() {
        // Text without an escape is borrowed as it stands.
        assert!(matches!(
            translate_unicode_escapes("plain"),
            Cow::Borrowed("plain")
        ));
        // `\u`+ four hex digits is one character, whatever the digits spell.
        assert_eq!(translate_unicode_escapes(r"\u0041"), "A");
        assert_eq!(translate_unicode_escapes(r"\uuuu0061"), "a");
        // §3.3's even-backslash rule: the `u` after `\\` is not an escape —
        // the characters stay as written — while the `\u0042` after it does
        // translate.
        assert_eq!(translate_unicode_escapes(r"\\u0041\u0042"), r"\\u0041B");
        assert_eq!(translate_unicode_escapes(r"\\\u0041"), r"\\A");
        // A surrogate pair is one character; a lone half is not an escape, so
        // the characters stay as written.
        assert_eq!(translate_unicode_escapes(r"\uD83D\uDE00"), "\u{1F600}");
        assert_eq!(translate_unicode_escapes(r"\uD83D"), r"\uD83D");
        // Exactly four digits: a fifth is an ordinary character.
        assert_eq!(translate_unicode_escapes(r"\u00411"), "A1");
        assert_eq!(translate_unicode_escapes(r"\uZZZZ"), r"\uZZZZ");
    }

    #[test]
    fn test_peek_unicode_escape() {
        // r"\u0041B" bytes: `\u0041B`; logical: 'A', 'B'
        let reader = SourceReader::new(r"\u0041B");
        assert_eq!(reader.peek(), 'A');
        assert_eq!(reader.peek_next(), 'B');
    }

    #[test]
    fn test_advance_unicode_escape() {
        let mut reader = SourceReader::new(r"\u0041B");
        assert_eq!(reader.advance(), 'A');
        assert_eq!(reader.current(), 6); // consumed 6 raw bytes
        assert_eq!(reader.peek(), 'B');
        assert!(!reader.is_at_end());
        assert_eq!(reader.advance(), 'B');
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 0);
    }

    #[test]
    fn test_advance_if_matches_escaped() {
        let mut reader = SourceReader::new(r"\u0041"); // logical 'A'
        assert!(reader.advance_if_matches('A'));
        assert!(reader.is_at_end());
        assert_eq!(reader.current(), 6);
    }

    #[test]
    fn test_advance_if_matches_literal() {
        let mut reader = SourceReader::new("A");
        assert!(reader.advance_if_matches('A'));
        assert!(reader.is_at_end());
        assert_eq!(reader.current(), 1);
    }

    #[test]
    fn test_adjacent_escapes() {
        // r"\u0048\u0069" → logical "Hi"
        let mut reader = SourceReader::new(r"\u0048\u0069");
        assert_eq!(reader.peek(), 'H');
        assert_eq!(reader.peek_next(), 'i');
        assert_eq!(reader.advance(), 'H');
        assert_eq!(reader.current(), 6);
        assert_eq!(reader.advance(), 'i');
        assert_eq!(reader.current(), 12);
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 0);
    }

    #[test]
    fn test_mixed_escaped_and_literal() {
        // r"f\u006Fo" → logical "foo"
        let mut reader = SourceReader::new(r"f\u006Fo");
        assert_eq!(reader.advance(), 'f');
        assert_eq!(reader.current(), 1);
        assert_eq!(reader.advance(), 'o');
        assert_eq!(reader.current(), 7); // 1 + 6
        assert_eq!(reader.advance(), 'o');
        assert_eq!(reader.current(), 8);
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 0);
    }

    /// JLS §3.3 allows one or more `u` markers.
    #[test]
    fn test_multiple_u_markers() {
        // r"\uuu0041" = 8 raw bytes, logical 'A'
        let mut reader = SourceReader::new(r"\uuu0041");
        assert_eq!(reader.peek(), 'A');
        assert_eq!(reader.advance(), 'A');
        assert_eq!(reader.current(), 8);
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 0);
    }

    #[test]
    fn test_escape_newline() {
        let reader = SourceReader::new(r"\u000a");
        assert_eq!(reader.peek(), '\n');
    }

    /// `\u005C` → `\`.  Single-pass: the produced backslash does NOT retrigger
    /// escape processing.
    #[test]
    fn test_escape_produces_backslash() {
        let mut reader = SourceReader::new(r"\u005C");
        assert_eq!(reader.peek(), '\\');
        assert_eq!(reader.advance(), '\\');
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 0);
    }

    /// Fewer than 4 hex digits before EOF → error recorded on `advance()`;
    /// subsequent advances return the remaining raw bytes one at a time.
    #[test]
    fn test_invalid_escape_incomplete() {
        let mut reader = SourceReader::new(r"\u123"); // only 3 digits
        assert_eq!(reader.peek(), '\\');
        assert_eq!(reader.errors().len(), 0);

        assert_eq!(reader.advance(), '\\'); // records error, offset +1
        assert_eq!(reader.current(), 1);
        assert_eq!(reader.errors().len(), 1);

        assert_eq!(reader.advance(), 'u');
        assert_eq!(reader.advance(), '1');
        assert_eq!(reader.advance(), '2');
        assert_eq!(reader.advance(), '3');
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 1); // still exactly one error
    }

    /// Non-hex digit in the 4-digit group → error on first `advance()`.
    #[test]
    fn test_invalid_escape_non_hex() {
        let mut reader = SourceReader::new(r"\uZZZZ");
        assert_eq!(reader.peek(), '\\');
        assert_eq!(reader.errors().len(), 0);

        assert_eq!(reader.advance(), '\\'); // records error
        assert_eq!(reader.errors().len(), 1);
        assert_eq!(reader.current(), 1);

        // Remaining bytes are plain literals.
        assert_eq!(reader.advance(), 'u');
        assert_eq!(reader.advance(), 'Z');
        assert_eq!(reader.advance(), 'Z');
        assert_eq!(reader.advance(), 'Z');
        assert_eq!(reader.advance(), 'Z');
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 1); // one error total
    }

    /// `\` not followed by `u` is a plain backslash — no error.
    #[test]
    fn test_backslash_not_followed_by_u() {
        let mut reader = SourceReader::new(r"\n0041");
        assert_eq!(reader.peek(), '\\');
        assert_eq!(reader.peek_next(), 'n');
        assert_eq!(reader.advance(), '\\');
        assert_eq!(reader.advance(), 'n');
        assert_eq!(reader.errors().len(), 0);
    }

    // ── JLS §3.3  Even / Odd Backslash Rule ──────────────────────────────────

    /// `\\u0041` in raw source (2 backslashes + u0041).
    ///
    /// Bytes: [0]`\` [1]`\` [2]`u` [3]`0` [4]`0` [5]`4` [6]`1`
    /// - scan_at(0): preceding=0 (even), bytes[1]=`\` ≠ `u` → Char('\\', 1)
    /// - scan_at(1): preceding=1 (odd)                       → Char('\\', 1)
    /// - scan_at(2): `u` → plain 'u'
    #[test]
    fn test_double_backslash_not_an_escape() {
        let mut reader = SourceReader::new(r"\\u0041");
        assert_eq!(reader.advance(), '\\');
        assert_eq!(reader.advance(), '\\'); // odd preceding → not an escape
        assert_eq!(reader.advance(), 'u');
        assert_eq!(reader.advance(), '0');
        assert_eq!(reader.advance(), '0');
        assert_eq!(reader.advance(), '4');
        assert_eq!(reader.advance(), '1');
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 0);
    }

    /// `\\\u0041` in raw source (3 backslashes + u0041).
    ///
    /// Bytes: [0]`\` [1]`\` [2]`\` [3]`u` [4]`0` [5]`0` [6]`4` [7]`1`
    /// - scan_at(0): preceding=0 (even), bytes[1]=`\` ≠ `u` → Char('\\', 1)
    /// - scan_at(1): preceding=1 (odd)                       → Char('\\', 1)
    /// - scan_at(2): preceding=2 (even), bytes[3]=`u`, `0041` valid → Char('A', 6)
    #[test]
    fn test_triple_backslash_third_is_escape() {
        let mut reader = SourceReader::new(r"\\\u0041");
        assert_eq!(reader.advance(), '\\');
        assert_eq!(reader.advance(), '\\'); // odd preceding → literal
        assert_eq!(reader.advance(), 'A'); // even preceding → valid escape
        assert!(reader.is_at_end());
        assert_eq!(reader.errors().len(), 0);
    }

    /// `matches` and `advance_if_matches_str` operate on raw bytes, never
    /// interpreting unicode escapes.
    #[test]
    fn test_matches_str_is_raw() {
        let mut reader = SourceReader::new(r"\u0041");
        assert!(!reader.matches("A"));
        assert!(reader.matches(r"\u0041"));
        assert!(reader.advance_if_matches_str(r"\u0041"));
        assert!(reader.is_at_end());
    }

    #[test]
    fn test_peek_n_ascii() {
        let reader = SourceReader::new("abcd");
        assert_eq!(reader.peek_n(0), 'a');
        assert_eq!(reader.peek_n(1), 'b');
        assert_eq!(reader.peek_n(2), 'c');
        assert_eq!(reader.peek_n(3), 'd');
        assert_eq!(reader.peek_n(4), '\0');
    }

    #[test]
    fn test_peek_n_with_unicode_escape() {
        let reader = SourceReader::new(r"\u0041BC");
        assert_eq!(reader.peek_n(0), 'A');
        assert_eq!(reader.peek_n(1), 'B');
        assert_eq!(reader.peek_n(2), 'C');
        assert_eq!(reader.peek_n(3), '\0');
    }

    #[test]
    fn test_peek_n_with_utf8_and_escape() {
        let reader = SourceReader::new("你\u{597d}");
        assert_eq!(reader.peek_n(0), '你');
        assert_eq!(reader.peek_n(1), '好');
        assert_eq!(reader.peek_n(2), '\0');
    }

    #[test]
    fn test_peek_n_with_invalid_escape() {
        let reader = SourceReader::new(r"\u12Z4A");
        assert_eq!(reader.peek_n(0), '\\');
        assert_eq!(reader.peek_n(1), 'u');
        assert_eq!(reader.peek_n(2), '1');
    }

    #[test]
    fn test_peek_n_with_mixed_logical_chars() {
        let reader = SourceReader::new("A\\u4F60B");
        assert_eq!(reader.peek_n(0), 'A');
        assert_eq!(reader.peek_n(1), '你');
        assert_eq!(reader.peek_n(2), 'B');
        assert_eq!(reader.peek_n(3), '\0');
    }
}
