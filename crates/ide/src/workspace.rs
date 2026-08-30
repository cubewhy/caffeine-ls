//! Workspace-wide analysis: the parallel diagnostic report of every source
//! file.

use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};
use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};
use vfs::FileId;

use crate::RootDatabase;
use ide_diagnostics::Diagnostic;

/// A file's complete diagnostic report within an
/// [`Analysis::workspace_reports`] pull.
#[derive(Debug, Clone)]
pub struct WorkspaceReport {
    /// The file the report belongs to.
    pub file: FileId,
    /// The file's syntax plus merged type and declaration diagnostics
    /// ([`ide_diagnostics::file_report`]).
    pub report: Arc<Vec<Diagnostic>>,
    /// The precomputed, client-opaque LSP `resultId` of the report: a
    /// deterministic hash of the diagnostics plus the client lint keys,
    /// computed on the rayon worker that derived the report. Equal content
    /// yields the same id, so an unchanged file keeps its id across edits to
    /// unrelated files and the LSP layer can echo `Unchanged` without
    /// converting a single item.
    pub result_id: String,
}

/// The deterministic `resultId` of a file's report: a 64-bit hash of the
/// diagnostics plus the client lint keys. `DefaultHasher` (SipHash, zero keys)
/// is deterministic across runs, and folding the lints in keeps the id
/// sensitive to the client's `rawtypes`/`unchecked` config — a
/// `didChangeConfiguration` change still forces full re-sends.
fn report_result_id(report: &[Diagnostic], lints: &[String]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    report.hash(&mut hasher);
    lints.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The complete diagnostic report of every workspace source file, computed in
/// parallel.
///
/// A `RootDatabase` is `Send` but not `Sync` — salsa keeps per-thread query
/// state — so each rayon worker runs on its own [`RootDatabase`] clone. The
/// clones share the memo tables with the source database through salsa's
/// `StorageHandle`, so the per-file [`ide_diagnostics::file_report_query`]
/// results are shared: an edit that moved a single file's inputs recomputes
/// only that file's report, everything else re-reads the cache.
///
/// Cancellation is salsa's: a pending write unwinds the in-flight queries on
/// every worker, rayon propagates the unwind (preserving the `Cancelled`
/// payload), and the caller's [`Analysis::with_db`] boundary turns it into a
/// `Cancellable::Err`. Results are sorted by file id for determinism.
///
/// `lints` are the client-enabled lint keys (`rawtypes`, `unchecked`, ...):
/// each worker folds them into the report's `result_id` so a lint-config
/// change invalidates every cached id without a main-thread conversion pass.
pub fn workspace_reports(db: &RootDatabase, lints: &[String]) -> Vec<WorkspaceReport> {
    let files = db.source_files();
    if files.is_empty() {
        return Vec::new();
    }

    let num_workers = rayon::current_num_threads().max(1);
    let chunk_size = files.len().div_ceil(num_workers);
    let chunks: Vec<Vec<FileId>> = files
        .chunks(chunk_size.max(1))
        .map(|chunk| chunk.to_vec())
        .collect();

    let databases: Vec<RootDatabase> = (0..chunks.len()).map(|_| db.clone()).collect();

    let mut reports: Vec<WorkspaceReport> = chunks
        .into_par_iter()
        .zip(databases.into_par_iter())
        .flat_map_iter(|(chunk, db)| {
            chunk.into_iter().map(move |file| {
                let report = ide_diagnostics::file_report(&db, file);
                let result_id = report_result_id(&report, lints);
                WorkspaceReport {
                    file,
                    report,
                    result_id,
                }
            })
        })
        .collect();

    reports.sort_by_key(|report| report.file);
    reports
}
