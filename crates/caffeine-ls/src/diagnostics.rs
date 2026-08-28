//! Central per-file diagnostics store with subscription-based cross-file
//! invalidation, in the style of rust-analyzer's `DiagnosticsMapConfig`.
//!
//! Diagnostics are never eager for the whole workspace. Instead the server
//! maintains a small set of *watched* files — the documents the client has open
//! or has explicitly pulled — and, for each watched file `B`, a *subscription*
//! recording the source files `B`'s resolution depends on. The forward edges
//! come from the salsa-tracked [`ide::Analysis::file_resolved_deps`] (exact
//! type-level dependencies) and [`ide::Analysis::file_dependency_refs`]
//! (reference names, the sound fallback for statically-imported members). The
//! reverse direction — which watched files does an edit to `A` invalidate — is
//! only ever built over the watched set, keyed by `A`'s file id and by the
//! names `A` exports.
//!
//! The store is the single source of truth for "what are the diagnostics of
//! file `X` right now": each entry holds the current report
//! (`Arc<Vec<Diagnostic>>`) and a monotonically increasing per-file
//! `generation`. A recompute that yields an `Eq`-equal report keeps its
//! generation; one that differs bumps it. `generation` doubles as the LSP
//! `result_id`, so change detection is plain equality, not a content hash.
//!
//! Delivery is pull-based: semantic diagnostics travel over
//! `textDocument/diagnostic` and `workspace/diagnostic` (sealed with the
//! generation), while the background pass only *refreshes* (via
//! `workspace/diagnosticRefresh`) once a watched file's diagnostics moved.

use std::{hash::Hash, sync::Arc};

use hir::hir_expand::name::Name;
use lsp_types::{RelatedDocument, Uri};
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;
use vfs::FileId;

use ide::{Analysis, Cancellable, LanguageKind};

use crate::{global_state::GlobalStateSnapshot, lsp::diagnostics as lsp_diagnostics};

/// Cap on the number of related documents embedded in one pull response, so a
/// single request can never balloon into a multi-megabyte payload even after a
/// change to a widely-used declaration.
const MAX_RELATED_DOCS: usize = 64;

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

/// The wire-level diagnostics of a file, lint-filtered and range-converted
/// from an already-computed report.
pub(crate) fn convert_items(
    snapshot: &GlobalStateSnapshot,
    file_id: FileId,
    report: &Arc<Vec<ide::Diagnostic>>,
) -> Cancellable<Vec<lsp_types::Diagnostic>> {
    let line_index = snapshot.file_line_index(file_id)?;
    let lints = snapshot.config.client_lints();
    Ok(report
        .iter()
        .filter(|diagnostic| lint_allows(lints, diagnostic))
        .map(|diagnostic| lsp_diagnostics::convert_diagnostic(&line_index, diagnostic.clone()))
        .collect())
}

/// The language kind inferred from a file URI, for files outside any source
/// root (the fallback `syntax_diagnostics` path).
fn fallback_kind(uri: &Uri) -> LanguageKind {
    LanguageKind::from_path(uri.path())
}

/// The names a file exports, probed against the name subscription map to find
/// the watched files that may be affected by editing it. Both the canonical
/// qualified name and its trailing simple name are returned, matching how
/// referenced names are recorded in `file_dependency_refs`.
fn exported_names(analysis: &Analysis, file_id: FileId) -> Cancellable<Vec<SmolStr>> {
    let symbols = analysis.document_symbols(file_id)?;
    let mut out = Vec::with_capacity(symbols.len() * 2);
    for symbol in &symbols {
        let name = symbol.name.as_str();
        if name.is_empty() {
            continue;
        }
        out.push(SmolStr::new(name));
        if let Some(dot) = name.rfind('.') {
            let simple = &name[dot + 1..];
            if !simple.is_empty() {
                out.push(SmolStr::new(simple));
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The current diagnostics of one file, together with the identity of that
/// content (`generation`, doubles as the LSP `result_id`) and the generation
/// last handed to the client in a full report.
struct FileDiagnostics {
    generation: u64,
    delivered_generation: u64,
    diagnostics: Arc<Vec<ide::Diagnostic>>,
}

/// The dependency edges a watched file subscribed for: the source files its
/// types resolve against, and the reference names it consults.
#[derive(Clone)]
struct Subscription {
    deps: Arc<FxHashSet<FileId>>,
    names: Arc<FxHashSet<Name>>,
}

#[derive(Default)]
struct DiagnosticsInner {
    /// Current per-file reports (watched and pulled files alike).
    files: FxHashMap<FileId, FileDiagnostics>,
    /// Files whose diagnostics must be tracked across edits (open or pulled).
    watched: FxHashSet<FileId>,
    /// Watched files whose dependency edges are registered.
    subscribed: FxHashMap<FileId, Subscription>,
    /// `dep` → watched files whose `file_resolved_deps` include `dep`.
    deps_subs: FxHashMap<FileId, FxHashSet<FileId>>,
    /// referenced name → watched files whose `file_dependency_refs` include it.
    name_subs: FxHashMap<SmolStr, FxHashSet<FileId>>,
    /// The working set of the workspace (for `workspace/diagnostic`).
    source_files: Vec<FileId>,
}

/// Shared diagnostics store. Cheap to clone (one `Arc`), so it lives in both
/// [`crate::GlobalState`] and every [`GlobalStateSnapshot`].
#[derive(Clone, Default)]
pub(crate) struct DiagnosticsMap {
    inner: Arc<Mutex<DiagnosticsInner>>,
}

impl DiagnosticsMap {
    // ----- main-thread plumbing ---------------------------------------------

    /// Replaces the working set of the store (called on source-root
    /// repartition, e.g. when files are created or deleted). Drops every
    /// per-file row of files that left the set and prunes them from the
    /// subscription maps.
    ///
    /// Returns the watched files whose diagnostics may have moved because of
    /// the repartition, so the caller can hand them back into the refresh pass.
    /// A file add can resolve (or conflict with) references in watched files;
    /// a delete can regress them — so every watched file is re-verified.
    pub(crate) fn set_source_files(&self, files: Vec<FileId>) -> Vec<FileId> {
        let mut inner = self.inner.lock();
        let old: FxHashSet<FileId> = inner.source_files.iter().copied().collect();
        let new: FxHashSet<FileId> = files.iter().copied().collect();
        let removed: Vec<FileId> = old.difference(&new).copied().collect();
        let removed_set: FxHashSet<FileId> = removed.iter().copied().collect();

        for r in &removed {
            inner.files.remove(r);
            // Drop the deleted file's own subscription edges.
            if let Some(sub) = inner.subscribed.remove(r) {
                for dep in sub.deps.iter() {
                    unsubscribe_edge(&mut inner.deps_subs, dep, *r);
                }
                for name in sub.names.iter() {
                    if let Some(key) = trim(name.as_str()) {
                        unsubscribe_edge(&mut inner.name_subs, &SmolStr::new(key), *r);
                    }
                }
            }
            // The deleted file is never a dependency key anymore.
            inner.deps_subs.remove(r);
        }
        inner.watched.retain(|f| !removed_set.contains(f));
        inner.deps_subs.retain(|_, subs| {
            subs.retain(|f| !removed_set.contains(f));
            !subs.is_empty()
        });
        inner.name_subs.retain(|_, subs| {
            subs.retain(|f| !removed_set.contains(f));
            !subs.is_empty()
        });

        inner.source_files = files;

        inner.watched.iter().copied().collect()
    }

    /// The source files the store covers (the `workspace/diagnostic` set).
    pub(crate) fn source_files(&self) -> Vec<FileId> {
        self.inner.lock().source_files.clone()
    }

    /// Marks `file` as watched (an open document). Subscription edges are
    /// registered lazily by the refresh pass or the next pull.
    pub(crate) fn mark_watched(&self, file: FileId) {
        self.inner.lock().watched.insert(file);
    }

    /// Stops tracking `file` (a closed document).
    pub(crate) fn unwatch(&self, file: FileId) {
        let mut inner = self.inner.lock();
        inner.watched.remove(&file);
        if let Some(sub) = inner.subscribed.remove(&file) {
            for dep in sub.deps.iter() {
                unsubscribe_edge(&mut inner.deps_subs, dep, file);
            }
            for name in sub.names.iter() {
                if let Some(key) = trim(name.as_str()) {
                    unsubscribe_edge(&mut inner.name_subs, &SmolStr::new(key), file);
                }
            }
        }
    }

    // ----- subscription maintenance -----------------------------------------

    /// Registers (or refreshes) `file`'s subscription from its current forward
    /// dependency edges. Marks the file watched.
    pub(crate) fn ensure_subscribed(&self, analysis: &Analysis, file: FileId) -> Cancellable<()> {
        let deps = analysis.file_resolved_deps(file)?;
        let refs = analysis.file_dependency_refs(file)?;
        self.inner.lock().refresh_subscription(file, &deps, &refs);
        Ok(())
    }

    /// Whether `file` has registered subscription edges.
    pub(crate) fn is_subscribed(&self, file: FileId) -> bool {
        self.inner.lock().subscribed.contains_key(&file)
    }

    // ----- report access ----------------------------------------------------

    /// The file's current `(generation, report)`. The report is (re)derived from
    /// the analysis every call — cheap, since salsa memoizes it — and the
    /// generation is bumped only when the report actually changed, so it stays
    /// stable across unchanged recomputes.
    pub(crate) fn file_report(
        &self,
        analysis: &Analysis,
        file: FileId,
        fallback: LanguageKind,
    ) -> Cancellable<(u64, Arc<Vec<ide::Diagnostic>>)> {
        let report = analysis.file_report(file, fallback)?;
        let mut inner = self.inner.lock();
        let generation = match inner.files.get_mut(&file) {
            Some(entry) => {
                if entry.diagnostics == report {
                    entry.generation
                } else {
                    entry.generation += 1;
                    entry.delivered_generation = 0;
                    entry.diagnostics = Arc::clone(&report);
                    entry.generation
                }
            }
            None => {
                inner.files.insert(
                    file,
                    FileDiagnostics {
                        generation: 1,
                        delivered_generation: 0,
                        diagnostics: Arc::clone(&report),
                    },
                );
                1
            }
        };
        Ok((generation, report))
    }

    /// The generation last delivered to the client for `file`, if the file is
    /// tracked.
    pub(crate) fn delivered_generation(&self, file: FileId) -> u64 {
        self.inner
            .lock()
            .files
            .get(&file)
            .map(|f| f.delivered_generation)
            .unwrap_or(0)
    }

    /// Records that `generation` was just delivered to the client in full.
    pub(crate) fn mark_delivered(&self, file: FileId, generation: u64) {
        if let Some(entry) = self.inner.lock().files.get_mut(&file) {
            entry.delivered_generation = generation;
        }
    }
}

impl DiagnosticsInner {
    /// Rebuilds `file`'s subscription edges from fresh forward dependency sets,
    /// diffing against the previous registration so stale reverse rows are
    /// drained. Marks the file as watched and subscribed.
    fn refresh_subscription(
        &mut self,
        file: FileId,
        deps: &Arc<FxHashSet<FileId>>,
        refs: &Arc<FxHashSet<Name>>,
    ) {
        self.watched.insert(file);
        if let Some(old) = self.subscribed.get(&file).cloned() {
            for dep in old.deps.iter() {
                unsubscribe_edge(&mut self.deps_subs, dep, file);
            }
            for name in old.names.iter() {
                if let Some(key) = trim(name.as_str()) {
                    unsubscribe_edge(&mut self.name_subs, &SmolStr::new(key), file);
                }
            }
        }
        for dep in deps.iter() {
            if *dep != file {
                self.deps_subs.entry(*dep).or_default().insert(file);
            }
        }
        for name in refs.iter() {
            if let Some(key) = trim(name.as_str()) {
                self.name_subs
                    .entry(SmolStr::new(key))
                    .or_default()
                    .insert(file);
            }
        }
        self.subscribed.insert(
            file,
            Subscription {
                deps: deps.clone(),
                names: refs.clone(),
            },
        );
    }
}

/// Removes `file` from the subscriber set of `key`, dropping the row when it
/// becomes empty.
fn unsubscribe_edge<V>(map: &mut FxHashMap<V, FxHashSet<FileId>>, key: &V, file: FileId)
where
    V: Eq + Hash,
{
    let emptied = map.get_mut(key).map(|set| {
        set.remove(&file);
        set.is_empty()
    });
    if emptied == Some(true) {
        map.remove(key);
    }
}

/// The `relatedDocuments` of a document-diagnostic pull for `self_file`: every
/// watched file whose diagnostics an edit to `self_file` can move, sealed with
/// its generation. A candidate that already reached the client is reported as
/// `unchanged` (a `resultId` only).
pub(crate) fn related_for(
    snapshot: &GlobalStateSnapshot,
    self_file: FileId,
) -> Cancellable<Option<FxHashMap<Uri, RelatedDocument>>> {
    let analysis = &snapshot.analysis;
    let diag = &snapshot.diagnostics;

    let names = exported_names(analysis, self_file)?;
    let candidates: FxHashSet<FileId> = {
        let inner = diag.inner.lock();
        let mut out = FxHashSet::default();
        if let Some(subs) = inner.deps_subs.get(&self_file) {
            out.extend(subs.iter().copied());
        }
        for name in &names {
            if let Some(subs) = inner.name_subs.get(name) {
                out.extend(subs.iter().copied());
            }
        }
        out.remove(&self_file);
        out
    };
    if candidates.is_empty() {
        return Ok(None);
    }

    let mut out: FxHashMap<Uri, RelatedDocument> = FxHashMap::default();
    for file in candidates {
        let Ok(uri) = snapshot.file_id_to_url(file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        let (generation, report) = diag.file_report(analysis, file, fallback)?;
        if diag.delivered_generation(file) == generation {
            out.insert(
                uri,
                RelatedDocument::UnchangedDocumentDiagnosticReport(
                    lsp_types::UnchangedDocumentDiagnosticReport {
                        result_id: render_id(generation),
                    },
                ),
            );
        } else {
            if out.len() >= MAX_RELATED_DOCS {
                continue;
            }
            let items = convert_items(snapshot, file, &report)?;
            diag.mark_delivered(file, generation);
            out.insert(
                uri,
                RelatedDocument::FullDocumentDiagnosticReport(
                    lsp_types::FullDocumentDiagnosticReport {
                        result_id: Some(render_id(generation)),
                        items,
                    },
                ),
            );
        }
    }
    Ok(Some(out))
}

/// The `workspace/diagnostic` pull: one full or unchanged report per source
/// file, sealed with the file's generation. A document whose previous result
/// id matches its current generation is echoed as `Unchanged`; everything else
/// is a full report. Items are sorted by URI for determinism.
pub(crate) fn workspace_diagnostic_reports(
    snapshot: &GlobalStateSnapshot,
    previous_ids: &FxHashMap<lsp_types::Uri, String>,
) -> Cancellable<Vec<lsp_types::WorkspaceDocumentDiagnosticReport>> {
    let analysis = &snapshot.analysis;
    let diag = &snapshot.diagnostics;

    let mut items = Vec::new();
    for file in diag.source_files() {
        let Ok(uri) = snapshot.file_id_to_url(file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        let (generation, report) = diag.file_report(analysis, file, fallback)?;
        let version = crate::lsp::from_proto::vfs_path(&uri)
            .ok()
            .and_then(|path| snapshot.open_document_version(&path));
        let id = render_id(generation);
        if previous_ids.get(&uri).map(String::as_str) == Some(id.as_str()) {
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
            let diagnostics = convert_items(snapshot, file, &report)?;
            diag.mark_delivered(file, generation);
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
/// files: (re)subscribes watched files lazily and edited files eagerly, derives
/// the candidate set (the edited files themselves plus their watched
/// dependents), recomputes each candidate's report, and returns the files whose
/// report actually moved (their generation changed).
///
/// All salsa reads run through [`ide::Analysis`]; a concurrent write cancels
/// the pass ([`Cancelled::PendingWrite`]) and the caller re-enqueues the same
/// changed set once the write is applied.
pub(crate) fn run_diagnostics_pass(
    snapshot: &GlobalStateSnapshot,
    changed: &FxHashSet<FileId>,
) -> Cancellable<Vec<FileId>> {
    let analysis = &snapshot.analysis;
    let diag = &snapshot.diagnostics;

    // 1. Lazily subscribe watched-but-untracked files; re-subscribe edited
    //    files that are already tracked (their own dependency edges moved).
    let lazy: Vec<FileId> = {
        let inner = diag.inner.lock();
        inner
            .watched
            .iter()
            .copied()
            .filter(|f| !inner.subscribed.contains_key(f))
            .collect()
    };
    for file in lazy {
        diag.ensure_subscribed(analysis, file)?;
    }
    for file in changed {
        if diag.is_subscribed(*file) {
            diag.ensure_subscribed(analysis, *file)?;
        }
    }

    // 2. The exported names of the edited files, for the name-level probe.
    let mut probes: Vec<(FileId, Vec<SmolStr>)> = Vec::with_capacity(changed.len());
    let mut changed_sorted: Vec<FileId> = changed.iter().copied().collect();
    changed_sorted.sort();
    for file in changed_sorted {
        probes.push((file, exported_names(analysis, file)?));
    }

    // 3. Candidate set: the edited files plus their watched dependents.
    let candidates: FxHashSet<FileId> = {
        let inner = diag.inner.lock();
        let mut out: FxHashSet<FileId> = changed.clone();
        for (file, names) in &probes {
            if let Some(subs) = inner.deps_subs.get(file) {
                out.extend(subs.iter().copied());
            }
            for name in names {
                if let Some(subs) = inner.name_subs.get(name) {
                    out.extend(subs.iter().copied());
                }
            }
        }
        out
    };
    tracing::debug!(changed = ?changed, candidates = ?candidates, "diagnostics pass candidates");

    // 4. Recompute each candidate and commit, bumping generations on change.
    let mut reports: Vec<(FileId, Arc<Vec<ide::Diagnostic>>)> = Vec::new();
    for file in &candidates {
        let Ok(uri) = snapshot.file_id_to_url(*file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        reports.push((*file, analysis.file_report(*file, fallback)?));
    }

    let mut changed_files: Vec<FileId> = Vec::new();
    {
        let mut inner = diag.inner.lock();
        for (file, report) in reports {
            let moved = match inner.files.get_mut(&file) {
                Some(entry) => {
                    if entry.diagnostics == report {
                        false
                    } else {
                        entry.generation += 1;
                        entry.delivered_generation = 0;
                        entry.diagnostics = report;
                        true
                    }
                }
                None => {
                    inner.files.insert(
                        file,
                        FileDiagnostics {
                            generation: 1,
                            delivered_generation: 0,
                            diagnostics: report,
                        },
                    );
                    true
                }
            };
            if moved {
                changed_files.push(file);
            }
        }
    }
    changed_files.sort();
    tracing::debug!(changed = ?changed_files, "diagnostics pass finished");
    Ok(changed_files)
}

/// Renders a generation as the client-opaque LSP `resultId`.
fn render_id(generation: u64) -> String {
    generation.to_string()
}

fn trim(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
