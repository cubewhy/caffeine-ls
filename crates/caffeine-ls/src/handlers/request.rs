use crate::{from_proto::vfs_path, global_state::GlobalStateSnapshot};

use ide_db::base_db::SourceDatabase;
use lsp_types::*;

use crate::lsp::diagnostics;

pub fn on_diagnostic(
    state: GlobalStateSnapshot,
    params: DocumentDiagnosticParams,
) -> anyhow::Result<DocumentDiagnosticReport> {
    tracing::info!(uri = ?params.text_document.uri, "request diagnostics");

    if let Ok(vfs_path) = vfs_path(&params.text_document.uri) {
        let (file_id, text) = {
            let vfs = state.vfs.read();
            let Some((file_id, _)) = vfs.file_id(&vfs_path) else {
                anyhow::bail!("failed to get file id from vfs path: {vfs_path:?}");
            };
            let db = state.analysis.raw_database();
            let file_text = db.file_text(file_id);
            let text = file_text.text(db).clone();
            (file_id, text)
        };

        // TODO: call state.analysis.collect_diagnostics
        let diagnostics = diagnostics::collect_diagnostics(state.analysis, file_id, text)?;

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
