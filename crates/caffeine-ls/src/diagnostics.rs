//! Pull-based diagnostics, delegating all incremental computation to Salsa.
//!
//! The pull handlers are *functional* over a [`GlobalStateSnapshot`]: every
//! report is derived on demand through the memoized salsa queries
//! ([`ide::Analysis::file_report`]), which are O(1) cache hits for unaffected
//! files and recompute only files whose inputs actually moved. There is no
//! per-file generation counter, no `(nonce, revision)` verification marker,
//! and no access clock: an edit never invalidates the whole workspace.
//!
//! The LSP `resultId` of a file is a deterministic 64-bit hash of its
//! converted diagnostics. Equal content yields the same id, so after an edit
//! that does not change a file's diagnostics the server echoes
//! `WorkspaceUnchangedDocumentDiagnosticReport` — the `full=1 unchanged=N-1`
//! steady state. `resultId` doubles as the change fingerprint for the
//! debounced background refresh pass, which keeps the single remaining piece
//! of state: the last-observed id per file, used only to decide whether a
//! cross-file refresh notification is warranted.

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use ide::{Cancellable, LanguageKind};
use lsp_types::Uri;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
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

/// A checkpoint an expensive diagnostics loop consults every few files: if the
/// client cancelled the in-flight request, abort instead of finishing the work
/// (which would otherwise run to completion, burning CPU past the cancel).
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

/// The language kind inferred from a file URI, for files outside any source
/// root (the fallback `syntax_diagnostics` path).
fn fallback_kind(uri: &Uri) -> LanguageKind {
    LanguageKind::from_path(uri.path())
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

/// The only diagnostics state the server keeps: the last fingerprint each file
/// was observed at by the debounced refresh pass. Used solely to detect whether
/// a file's diagnostics moved between passes (which gates the workspace-wide
/// `diagnosticRefresh` notification); it is never consulted by a pull handler.
///
/// Cheap to clone (one `Arc`), so it lives in both [`crate::GlobalState`] and
/// every [`GlobalStateSnapshot`].
#[derive(Clone, Default)]
pub(crate) struct DiagnosticsMap {
    inner: Arc<Mutex<FxHashMap<FileId, u64>>>,
}

impl DiagnosticsMap {
    /// The fingerprint `file` was last observed at by a refresh pass.
    pub(crate) fn computed(&self, file: FileId) -> Option<u64> {
        self.inner.lock().get(&file).copied()
    }

    /// Records the fingerprint `file`'s diagnostics currently hash to. Rows of
    /// closed files linger harmlessly; the map is tiny and bounded by the files
    /// ever pulled.
    pub(crate) fn set_computed(&self, file: FileId, hash: u64) {
        self.inner.lock().insert(file, hash);
    }
}

/// The `workspace/diagnostic` pull: one full or unchanged report per source
/// file, sealed with a deterministic fingerprint of its diagnostics. A document
/// whose previous result id matches its current fingerprint is echoed as
/// `Unchanged`; everything else is a full report. Items are sorted by URI for
/// determinism.
///
/// The pull is a pure function of the analysis snapshot: each report is
/// (re)derived through the memoized salsa queries, which are O(1) cache hits
/// for unaffected files and recompute only files whose inputs actually moved.
/// Because the `resultId` is a content hash, an edit that leaves a file's
/// diagnostics alone keeps its id, so a single-file edit with no cascading
/// changes produces `full=1 unchanged=N-1`. A pull issued right after
/// `didChange` always reflects the new text instead of echoing a stale
/// pre-edit `resultId` — there is no cache to go stale.
pub(crate) fn workspace_diagnostic_reports(
    snapshot: &GlobalStateSnapshot,
    previous_ids: &FxHashMap<lsp_types::Uri, String>,
) -> anyhow::Result<Vec<lsp_types::WorkspaceDocumentDiagnosticReport>> {
    let analysis = &snapshot.analysis;

    let _span = tracing::info_span!("workspace_diagnostic").entered();

    let source_files = analysis.all_files()?;
    let mut items = Vec::new();
    let mut full = 0usize;
    let mut unchanged = 0usize;
    for file in source_files {
        check_cancelled(snapshot)?;
        let Ok(uri) = snapshot.file_id_to_url(file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        let version = crate::lsp::from_proto::vfs_path(&uri)
            .ok()
            .and_then(|path| snapshot.open_document_version(&path));
        let report = analysis.file_report(file, fallback)?;
        let diagnostics = convert_items(snapshot, file, &report)?;
        let id = render_id(result_id(&diagnostics));
        if previous_ids.get(&uri).map(String::as_str) == Some(id.as_str()) {
            unchanged += 1;
            items.push(
                lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
                    lsp_types::WorkspaceUnchangedDocumentDiagnosticReport {
                        uri,
                        version,
                        unchanged_document_diagnostic_report:
                            lsp_types::UnchangedDocumentDiagnosticReport { result_id: id },
                    },
                ),
            );
        } else {
            full += 1;
            items.push(
                lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(
                    lsp_types::WorkspaceFullDocumentDiagnosticReport {
                        uri,
                        version,
                        full_document_diagnostic_report: lsp_types::FullDocumentDiagnosticReport {
                            result_id: Some(id),
                            items: diagnostics,
                        },
                    },
                ),
            );
        }
    }
    items.sort_by(|a, b| uri_of(a).cmp(uri_of(b)));
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

/// The diagnostic refresh pass of a single background run over a set of changed
/// files: recomputes the edited files plus every currently open document (the
/// only files whose diagnostics the client can surface), fingerprints each
/// report, and returns the files whose fingerprint actually moved plus whether
/// any *other* (non-edited) file's moved — the edited files alone do not
/// warrant a workspace-wide refresh.
///
/// All salsa reads run through [`ide::Analysis`]; a concurrent write cancels
/// the pass ([`Cancelled::PendingWrite`]) and the caller re-enqueues the same
/// changed set once the write is applied. The `computed` fingerprints this
/// consults are updated in-place; a pull handler never touches them.
pub(crate) fn run_diagnostics_pass(
    snapshot: &GlobalStateSnapshot,
    changed: &FxHashSet<FileId>,
) -> anyhow::Result<(Vec<FileId>, bool)> {
    let analysis = &snapshot.analysis;
    let diag = &snapshot.diagnostics;

    let _span = tracing::info_span!("diagnostics_pass", changed = changed.len()).entered();

    let mut candidates: FxHashSet<FileId> = changed.clone();
    candidates.extend(snapshot.open_files());

    let mut changed_files: Vec<FileId> = Vec::new();
    for file in candidates {
        check_cancelled(snapshot)?;
        let Ok(uri) = snapshot.file_id_to_url(file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        let report = analysis.file_report(file, fallback)?;
        let diagnostics = convert_items(snapshot, file, &report)?;
        let hash = result_id(&diagnostics);
        if diag.computed(file) != Some(hash) {
            diag.set_computed(file, hash);
            changed_files.push(file);
        }
    }
    changed_files.sort();
    // Whether any moved file is a *dependent* (not one of the edited files):
    // only those moves can affect files the client is not already re-pulling.
    let cross_file = changed_files.iter().any(|file| !changed.contains(file));
    tracing::debug!(changed = ?changed_files, "diagnostics pass finished");
    Ok((changed_files, cross_file))
}
