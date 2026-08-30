//! Workspace-wide analysis: the parallel diagnostic report of every source
//! file.

use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};
use std::sync::Arc;
use vfs::FileId;

use crate::{LanguageKind, RootDatabase};
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
}

/// The complete diagnostic report of every workspace source file, computed in
/// parallel.
///
/// A `RootDatabase` is `Send` but not `Sync` — salsa keeps per-thread query
/// state — so each rayon worker runs on its own [`RootDatabase`] clone. The
/// clones share the memo tables with the source database through salsa's
/// `StorageHandle`, so the per-file [`ide_diagnostics::file_report_query`]
/// results are shared: an edit that moved a single file's inputs recomputes
/// only that file's report, everything else re-reads the cache. Fallback
/// language kind is [`LanguageKind::Unknown`] — workspace source files always
/// resolve their language from their owning source root, so the fallback is
/// never consulted here.
///
/// Cancellation is salsa's: a pending write unwinds the in-flight queries on
/// every worker, rayon propagates the unwind (preserving the `Cancelled`
/// payload), and the caller's [`Analysis::with_db`] boundary turns it into a
/// `Cancellable::Err`. Results are sorted by file id for determinism.
pub fn workspace_reports(db: &RootDatabase) -> Vec<WorkspaceReport> {
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
            chunk.into_iter().map(move |file| WorkspaceReport {
                file,
                report: ide_diagnostics::file_report(&db, file, LanguageKind::Unknown),
            })
        })
        .collect();

    reports.sort_by_key(|report| report.file);
    reports
}
