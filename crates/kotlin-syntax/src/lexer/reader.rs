pub struct SourceReader<'a> {
    source: &'a str,
    current: usize,
    start: usize,
}

impl<'a> SourceReader<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            current: 0,
            start: 0,
        }
    }

    pub fn peek(&self) -> char {
        self.peek_n(0)
    }

    pub fn peek_next(&self) -> char {
        self.peek_n(1)
    }

    pub fn peek_n(&self, n: usize) -> char {
        let mut offset = self.current;

        for _ in 0..n {
            match self.source[offset..].chars().next() {
                Some(c) => offset += c.len_utf8(),
                None => return '\0',
            }
        }

        self.source[offset..].chars().next().unwrap_or('\0')
    }

    /// Move the cursor and return the advanced character
    pub fn advance(&mut self) -> char {
        let c = self.peek();

        self.current += c.len_utf8();

        c
    }

    pub fn is_at_end(&self) -> bool {
        self.current >= self.source.len()
    }

    /// Start a new token
    pub fn new_token(&mut self) {
        self.start = self.current;
    }

    /// Get the start position (byte offset) of the token
    pub fn start(&self) -> usize {
        self.start
    }

    /// Get the current cursor position (byte offset)
    pub fn current(&self) -> usize {
        self.current
    }

    /// Get the current token lexeme
    pub fn current_lexeme(&self) -> &'a str {
        &self.source[self.start..self.current]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_byte_utf8() {
        let mut reader = SourceReader::new("简");
        assert_eq!(reader.peek(), '简');
        assert_eq!(reader.current(), 0);
        assert_eq!(reader.advance(), '简');
        assert_eq!(reader.current(), 3); // '简' is 3 bytes
        assert!(reader.is_at_end());
        assert_eq!(reader.peek(), '\0');
        assert_eq!(reader.advance(), '\0');
    }

    #[test]
    fn test_advance_skips_full_char_bytes() {
        let mut reader = SourceReader::new("你好");
        assert_eq!(reader.advance(), '你');
        assert_eq!(reader.current(), 3);
        assert_eq!(reader.peek(), '好');
        assert_eq!(reader.advance(), '好');
        assert_eq!(reader.current(), 6);
        assert!(reader.is_at_end());
    }

    #[test]
    fn test_peek_n_skips_logical_chars() {
        let reader = SourceReader::new("简好a");
        assert_eq!(reader.peek_n(0), '简');
        assert_eq!(reader.peek_n(1), '好');
        assert_eq!(reader.peek_n(2), 'a');
        assert_eq!(reader.peek_n(3), '\0');
    }

    #[test]
    fn test_peek_n_mixed_ascii_and_utf8() {
        let reader = SourceReader::new("a简b");
        assert_eq!(reader.peek_n(0), 'a');
        assert_eq!(reader.peek_n(1), '简');
        assert_eq!(reader.peek_n(2), 'b');
        assert_eq!(reader.peek_n(3), '\0');
    }

    #[test]
    fn test_peek_next_and_lexeme_preserve_chars() {
        let mut reader = SourceReader::new("欢乐");
        reader.new_token();
        assert_eq!(reader.peek_next(), '乐');
        reader.advance();
        assert_eq!(reader.current_lexeme(), "欢");
        assert_eq!(reader.current(), 3);
        reader.advance();
        assert_eq!(reader.current_lexeme(), "欢乐");
    }
}
