use crate::{global_state::GlobalStateSnapshot, lsp::diagnostics};

use ide::LanguageKind;
use lsp_types::*;

pub fn on_diagnostic(
    state: GlobalStateSnapshot,
    params: DocumentDiagnosticParams,
) -> anyhow::Result<DocumentDiagnosticReport> {
    tracing::info!(uri = ?params.text_document.uri, "request diagnostics");

    if let Ok(Some(file_id)) = state.url_to_file_id(&params.text_document.uri) {
        let line_index = state.file_line_index(file_id)?;
        // Before the workspace is loaded, files are not part of any source
        // root, so fall back to the language kind inferred from the path.
        let fallback_language_kind = LanguageKind::from_path(params.text_document.uri.path());
        let diagnostics = state
            .analysis
            .syntax_diagnostics(file_id, fallback_language_kind)?
            .into_iter()
            .chain(state.analysis.type_diagnostics(file_id)?)
            .map(|diagnostic| diagnostics::convert_diagnostic(&line_index, diagnostic))
            .collect();

        tracing::debug!(?diagnostics, "finished collecting diagnostics");

        Ok(RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: None,
                items: diagnostics,
            },
        }
        .into())
    } else {
        anyhow::bail!("failed to get vfs path from uri")
    }
}
