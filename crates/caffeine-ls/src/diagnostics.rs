//! Pull-based diagnostics, delegating all incremental computation to Salsa.
//!
//! The pull handlers are *functional* over a [`GlobalStateSnapshot`]: every
//! report is derived on demand through the memoized salsa queries — the
//! per-file [`ide::Analysis::file_report`] (O(1) cache hits for unaffected
//! files) or the parallel workspace-wide
//! [`ide::Analysis::workspace_reports`] (chunked rayon over shared memo
//! tables). There is no per-file generation counter, no `(nonce, revision)`
//! verification marker, and no background compute pass: the client pulls
//! `workspace/diagnostic` on demand and an edit never invalidates the whole
//! workspace.
//!
//! The LSP `resultId` of a file is a deterministic 64-bit hash of its
//! converted diagnostics. Equal content yields the same id, so after an edit
//! that does not change a file's diagnostics the server echoes
//! `WorkspaceUnchangedDocumentDiagnosticReport` — the `full=1 unchanged=N-1`
//! steady state.

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use ide::Cancellable;
use rustc_hash::FxHashMap;
use vfs::FileId;

use crate::{
    global_state::{ClientCancelled, GlobalStateSnapshot},
    lsp::diagnostics as lsp_diagnostics,
};

/// Whether a diagnostic passes the client's lint configuration; shared with
/// the document-diagnostic handler.
pub(crate) fn lint_allows(lints: &[String], diagnostic: &ide::Diagnostic) -> bool {
    use syntax::{DiagnosticCode, JavaDiagnosticCode};
    let gated = match diagnostic.code {
        Some(DiagnosticCode::Java(JavaDiagnosticCode::RawTypeUse)) => "rawtypes",
        Some(DiagnosticCode::Java(JavaDiagnosticCode::UncheckedConversion)) => "unchecked",
        _ => return true,
    };
    lints.iter().any(|lint| lint == gated)
}

/// A checkpoint an expensive diagnostics pull consults on entry: if the client
/// cancelled the in-flight request, abort instead of doing the work (which
/// would otherwise run to completion, burning CPU past the cancel). Mid-pull
/// cancellation is salsa's — a pending write unwinds the running queries.
pub(crate) fn check_cancelled(snapshot: &GlobalStateSnapshot) -> anyhow::Result<()> {
    if snapshot.cancelled.is_cancelled() {
        return Err(ClientCancelled.into());
    }
    Ok(())
}

/// The wire-level diagnostics of a file, lint-filtered and range-converted
/// from an already-computed report.
pub(crate) fn convert_items(
    snapshot: &GlobalStateSnapshot,
    file_id: FileId,
    report: &Arc<Vec<ide::Diagnostic>>,
) -> Cancellable<Vec<lsp_types::Diagnostic>> {
    let line_index = snapshot.file_line_index(file_id)?;
    let lints = snapshot.config.client_lints();
    let Ok(uri) = snapshot.file_id_to_url(file_id) else {
        return Ok(Vec::new());
    };
    Ok(report
        .iter()
        .filter(|diagnostic| lint_allows(lints, diagnostic))
        .map(|diagnostic| {
            lsp_diagnostics::convert_diagnostic(&line_index, &uri, diagnostic.clone())
        })
        .collect())
}

/// The deterministic LSP `resultId` of a file's converted diagnostics: a
/// 64-bit hash of the items, so equal content yields the same id and an
/// unchanged file keeps its id across edits to unrelated files.
pub(crate) fn result_id(items: &[lsp_types::Diagnostic]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    items.hash(&mut hasher);
    hasher.finish()
}

/// Renders a diagnostics fingerprint as the client-opaque LSP `resultId`.
pub(crate) fn render_id(hash: u64) -> String {
    format!("{hash:016x}")
}

/// The `workspace/diagnostic` pull: one full or unchanged report per source
/// file, sealed with a deterministic fingerprint of its diagnostics. A document
/// whose previous result id matches its current fingerprint is echoed as
/// `Unchanged`; everything else is a full report. Items are sorted by URI for
/// determinism.
///
/// The pull is a pure function of the analysis snapshot: the reports come from
/// [`ide::Analysis::workspace_reports`], which chunks the workspace across
/// rayon workers over shared salsa memo tables — O(1) cache hits for
/// unaffected files, recompute only files whose inputs actually moved. Because
/// the `resultId` is a content hash, an edit that leaves a file's diagnostics
/// alone keeps its id, so a single-file edit with no cascading changes
/// produces `full=1 unchanged=N-1`. A pull issued right after `didChange`
/// always reflects the new text instead of echoing a stale pre-edit
/// `resultId` — there is no cache to go stale.
///
/// Cancellation is salsa's: a pending write unwinds the in-flight queries and
/// surfaces as [`Cancelled`], which the request loop retries on a fresh
/// snapshot after the write lands. The client-cancel token is checked once on
/// entry.
pub(crate) fn workspace_diagnostic_reports(
    snapshot: &GlobalStateSnapshot,
    previous_ids: &FxHashMap<vfs::FileId, String>,
) -> anyhow::Result<Vec<lsp_types::WorkspaceDocumentDiagnosticReport>> {
    let _span = tracing::info_span!("workspace_diagnostic").entered();

    check_cancelled(snapshot)?;

    let reports = snapshot.analysis.workspace_reports()?;
    if reports.is_empty() {
        return Ok(Vec::new());
    }

    let mut items = Vec::with_capacity(reports.len());
    for workspace_report in reports {
        let file = workspace_report.file;
        let Ok(uri) = snapshot.file_id_to_url(file) else {
            continue;
        };
        let version = crate::lsp::from_proto::vfs_path(&uri)
            .ok()
            .and_then(|path| snapshot.open_document_version(&path));

        let diagnostics = convert_items(snapshot, file, &workspace_report.report)?;
        let id = render_id(result_id(&diagnostics));

        let report = if previous_ids.get(&file).map(String::as_str) == Some(id.as_str()) {
            lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
                lsp_types::WorkspaceUnchangedDocumentDiagnosticReport {
                    uri,
                    version,
                    unchanged_document_diagnostic_report:
                        lsp_types::UnchangedDocumentDiagnosticReport { result_id: id },
                },
            )
        } else {
            lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(
                lsp_types::WorkspaceFullDocumentDiagnosticReport {
                    uri,
                    version,
                    full_document_diagnostic_report: lsp_types::FullDocumentDiagnosticReport {
                        result_id: Some(id),
                        items: diagnostics,
                    },
                },
            )
        };
        items.push(report);
    }

    items.sort_by(|a, b| uri_of(a).cmp(uri_of(b)));

    let full = items
        .iter()
        .filter(|it| {
            matches!(
                it,
                lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(
                    _
                )
            )
        })
        .count();
    let unchanged = items.len() - full;

    tracing::info!(
        files = items.len(),
        full,
        unchanged,
        "workspace/diagnostic pull"
    );
    Ok(items)
}

fn uri_of(item: &lsp_types::WorkspaceDocumentDiagnosticReport) -> &str {
    match item {
        lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(r) => {
            r.uri.as_str()
        }
        lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
            r,
        ) => r.uri.as_str(),
    }
}
