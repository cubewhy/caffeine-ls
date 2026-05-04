use crate::{from_proto::to_vfs_path, global_state::GlobalStateSnapshot};

use lsp_types::*;

use crate::lsp::diagnostics;

pub fn on_diagnostic(
    state: GlobalStateSnapshot,
    params: DocumentDiagnosticParams,
) -> anyhow::Result<DocumentDiagnosticReportResult> {
    tracing::info!(uri = ?params.text_document.uri, "request diagnostics");

    let Some(vfs_path) = to_vfs_path(&params.text_document.uri) else {
        anyhow::bail!("Internal error");
    };

    let file_id = {
        let vfs = state.vfs.read();
        let Some((id, _)) = vfs.file_id(&vfs_path) else {
            anyhow::bail!("Internal error");
        };
        id
    };

    let diagnostics = diagnostics::collect_diagnostics(state.analysis.raw_database(), file_id)?;

    Ok(DocumentDiagnosticReportResult::Report(
        DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: None,
                items: diagnostics,
            },
        }),
    ))
}
