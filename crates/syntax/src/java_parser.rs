use lasso::ThreadedRodeo;

use crate::{ClassStub, ParseResult, SyntaxError};
use java_syntax::{Lang, SyntaxKind};
use rowan::SyntaxNode;

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

pub fn parse_java_file(text: &str, _interner: &ThreadedRodeo) -> ParseResult {
    let (tokens, lex_errors) = java_syntax::lex(text);

    let mut errors = Vec::with_capacity(lex_errors.len());

    // collect lexer errors
    errors.extend(lex_errors.iter().map(SyntaxError::from_java_lexer));

    // parse the tree
    let output = java_syntax::Parser::new(tokens).parse(java_syntax::EntryPoint::Root);

    // collect syntax errors
    for err in output.errors() {
        errors.push(SyntaxError::from_java_parser(err));
    }

    let tree = output.into_green_node();
    let root = SyntaxNode::<Lang>::new_root(tree.clone());
    let package = root
        .descendants()
        .find(|node| node.kind() == SyntaxKind::PACKAGE_DECL)
        .map(|node| {
            node.descendants_with_tokens()
                .filter_map(|item| item.into_token())
                .filter(|token| token.kind() == SyntaxKind::IDENTIFIER)
                .map(|token| token.text().to_string())
                .collect::<Vec<_>>()
                .join(".")
        })
        .filter(|package| !package.is_empty());

    let declaration_kinds = [
        SyntaxKind::CLASS_DECL,
        SyntaxKind::INTERFACE_DECL,
        SyntaxKind::ENUM_DECL,
        SyntaxKind::RECORD_DECL,
        SyntaxKind::ANNOTATION_TYPE_DECL,
    ];
    let mut stubs = Vec::new();
    for node in root
        .descendants()
        .filter(|node| declaration_kinds.contains(&node.kind()))
    {
        let Some(simple_name) = declaration_name(&node) else {
            continue;
        };
        let mut nesting = node
            .ancestors()
            .skip(1)
            .filter(|ancestor| declaration_kinds.contains(&ancestor.kind()))
            .filter_map(|ancestor| declaration_name(&ancestor))
            .collect::<Vec<_>>();
        nesting.reverse();
        nesting.push(simple_name);
        let local_name = nesting.join("$");
        let fqn = package
            .as_ref()
            .map(|package| format!("{package}.{local_name}"))
            .unwrap_or(local_name);
        stubs.push(ClassStub {
            name: fqn.into(),
            flags: 0,
            super_class: None,
            interfaces: Vec::new(),
            type_params: Vec::new(),
            permitted_subclasses: Vec::new(),
            record_components: Vec::new(),
            methods: Vec::new(),
            fields: Vec::new(),
            annotations: Vec::new(),
        });
    }

    ParseResult {
        tree,
        errors,
        stubs,
    }
}

fn declaration_name(node: &SyntaxNode<Lang>) -> Option<String> {
    node.children_with_tokens()
        .filter_map(|item| item.into_token())
        .find(|token| token.kind() == SyntaxKind::IDENTIFIER)
        .map(|token| token.text().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_stable_fqns_for_top_level_and_nested_types() {
        let interner = ThreadedRodeo::new();
        let parsed = parse_java_file(
            "package com.example; public class Outer { interface Inner {} } enum Other {}",
            &interner,
        );
        let names = parsed
            .stubs
            .into_iter()
            .map(|stub| stub.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "com.example.Outer",
                "com.example.Outer$Inner",
                "com.example.Other"
            ]
        );
    }
}
