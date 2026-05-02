use std::cell::RefCell;

use rowan::{GreenNode, NodeCache};

use crate::{FileText, LanguageId, SourceDatabase};

thread_local! {
    static SYNTAX_CACHE: RefCell<NodeCache> = RefCell::new(NodeCache::default());
}

#[salsa::tracked]
pub fn parse_node(db: &dyn SourceDatabase, file_text: FileText) -> Option<GreenNode> {
    let language_id = file_text.language(db);
    let content = file_text.text(db);

    match language_id {
        LanguageId::Java => Some(parse_java_node(content)),
        LanguageId::Kotlin => None,
        LanguageId::Unknown => None,
    }
}

pub fn parse_java_node(content: &str) -> GreenNode {
    // let mut errors = Vec::new();
    let (tokens, _errors) = java_syntax::lex(content);

    // TODO: collect lex errors

    SYNTAX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let parser = java_syntax::Parser::new(tokens);
        let output = parser.parse_with_cache(Some(&mut cache));
        // TODO: collect parse errors

        output.into_green_node()
    })
}
