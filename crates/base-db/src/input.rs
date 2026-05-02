use std::cell::RefCell;

use rowan::{GreenNode, NodeCache, TextRange};

use crate::{FileText, LanguageId, SourceDatabase};

thread_local! {
    static SYNTAX_CACHE: RefCell<NodeCache> = RefCell::new(NodeCache::default());
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
}

#[salsa::tracked]
pub struct ParseResult<'db> {
    pub green_node: GreenNode,
    pub errors: Vec<SyntaxError>,
}

#[salsa::tracked]
pub fn parse_node(db: &dyn SourceDatabase, file_text: FileText) -> Option<ParseResult<'_>> {
    let language_id = file_text.language(db);
    match language_id {
        LanguageId::Java => Some(parse_java_node(db, file_text)),
        LanguageId::Kotlin => None,
        LanguageId::Unknown => None,
    }
}

#[salsa::tracked]
pub fn parse_java_node(db: &dyn SourceDatabase, file_text: FileText) -> ParseResult<'_> {
    let content = file_text.text(db);

    let mut errors = Vec::new();
    let (tokens, lex_errors) = java_syntax::lex(content);

    let output = SYNTAX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let parser = java_syntax::Parser::new(tokens);
        let output = parser.parse_with_cache(Some(&mut cache));

        output
    });

    // TODO: collect errors

    ParseResult::new(db, output.into_green_node(), errors)
}
