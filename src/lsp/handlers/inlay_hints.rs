use std::sync::Arc;

use tower_lsp::lsp_types::{InlayHint, InlayHintParams};

use crate::language::LanguageRegistry;
use crate::lsp::request_context::PreparedRequest;
use crate::workspace::Workspace;

pub async fn handle_inlay_hints(
    workspace: Arc<Workspace>,
    registry: Arc<LanguageRegistry>,
    params: InlayHintParams,
) -> Option<Vec<InlayHint>> {
    let uri = &params.text_document.uri;
    let request = PreparedRequest::prepare(Arc::clone(&workspace), registry.as_ref(), uri)?;
    let lang = request.lang();
    if !lang.supports_inlay_hints() {
        return None;
    }

    let env = request.parse_env();
    lang.collect_inlay_hints_with_tree(request.file(), params.range, &env, request.view())
}
