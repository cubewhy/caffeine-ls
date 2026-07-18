use lasso::ThreadedRodeo;

use crate::{ParseResult, SyntaxError};

pub fn parse_kotlin_file(text: &str, _interner: &ThreadedRodeo) -> ParseResult {
    let (tokens, lexical_errors) = kotlin_syntax::lex(text);
    let output = kotlin_syntax::Parser::new(tokens).parse();
    let mut errors = lexical_errors
        .into_iter()
        .map(|error| SyntaxError::new(format!("{error:?}"), error.range))
        .collect::<Vec<_>>();
    errors.extend(
        output
            .errors()
            .iter()
            .map(|error| SyntaxError::new(format!("{:?}", error.kind), error.range)),
    );
    ParseResult {
        tree: output.into_green_node(),
        errors,
        stubs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kotlin_files_are_safe_to_parse_before_stub_support() {
        let parsed = parse_kotlin_file("package sample\nclass Example", &ThreadedRodeo::new());
        assert!(parsed.stubs.is_empty());
    }
}
