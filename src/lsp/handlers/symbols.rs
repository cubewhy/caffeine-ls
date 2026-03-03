use std::sync::Arc;
use tower_lsp::lsp_types::*;

use crate::language::LanguageRegistry;
use crate::workspace::Workspace;

pub async fn handle_document_symbol(
    registry: Arc<LanguageRegistry>,
    workspace: Arc<Workspace>,
    params: DocumentSymbolParams,
) -> Option<DocumentSymbolResponse> {
    let uri = params.text_document.uri;
    let doc = workspace.documents.get(&uri)?;

    let lang = registry.find(&doc.language_id)?;

    if lang.supports_collecting_symbols() {
        let mut parser = lang.make_parser();
        let tree = parser.parse(doc.content.as_ref(), None)?;
        let root = tree.root_node();
        let symbols = lang.collect_symbols(root, doc.content.as_bytes())?;
        Some(DocumentSymbolResponse::Nested(symbols))
    } else {
        None
    }
}
