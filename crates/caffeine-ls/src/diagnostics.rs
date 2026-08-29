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
use ide_db::base_db::{Nonce, SourceDatabase, salsa};
use lsp_types::{RelatedDocument, Uri};
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;
use vfs::FileId;

use ide::{Analysis, Cancellable, LanguageKind};

use crate::{
    global_state::{ClientCancelled, GlobalStateSnapshot},
    lsp::diagnostics as lsp_diagnostics,
};

/// Cap on the number of related documents embedded in one pull response, so a
/// single request can never balloon into a multi-megabyte payload even after a
/// change to a widely-used declaration.
const MAX_RELATED_DOCS: usize = 64;

/// Cap on the number of per-file rows the store retains (rust-analyzer-style
/// bounded diagnostics map). Rows for watched/subscribed files are never
/// evicted; the least-recently-used unwatched rows are dropped past the cap and
/// recomputed on demand from the memoized salsa query.
const MAX_DIAGNOSTICS_FILES: usize = 4096;

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
    /// `DiagnosticsInner::access_clock` value at the last read/write; used to
    /// evict least-recently-used rows when the store outgrows
    /// [`MAX_DIAGNOSTICS_FILES`].
    last_access: u64,
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
    /// Monotonic access counter for LRU eviction of [`Self::files`].
    access_clock: u64,
    /// The `(nonce, revision)` of the database every row of [`Self::files`] was
    /// last derived against. A pull whose database still matches can serve all
    /// cached generations without touching salsa (no input has changed since).
    last_verified: Option<(Nonce, salsa::Revision)>,
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
        // The working set changed; cached reports must be re-derived against
        // the new set before they can be served as authoritative again.
        inner.last_verified = None;

        inner.watched.iter().copied().collect()
    }

    /// The source files the store covers (the `workspace/diagnostic` set).
    pub(crate) fn source_files(&self) -> Vec<FileId> {
        self.inner.lock().source_files.clone()
    }

    /// Whether the store is fully current with the database: the working set
    /// and every cached generation were derived at `(nonce, revision)`, so a
    /// pull can serve them without any salsa work. The `nonce` guards against
    /// the database being replaced.
    pub(crate) fn is_verified(&self, nonce: Nonce, revision: salsa::Revision) -> bool {
        self.inner.lock().last_verified == Some((nonce, revision))
    }

    /// Records that every source-file row was derived from `(nonce, revision)`.
    /// Only the workspace-wide pull may call this — it is the single code path
    /// that covers *all* source files, so after it returns every cached report
    /// is authoritative until the next input change.
    pub(crate) fn mark_verified(&self, nonce: Nonce, revision: salsa::Revision) {
        self.inner.lock().last_verified = Some((nonce, revision));
    }

    /// Marks `file` as watched (an open document). Subscription edges are
    /// registered lazily by the refresh pass or the next pull.
    pub(crate) fn mark_watched(&self, file: FileId) {
        self.inner.lock().watched.insert(file);
    }

    /// Invalidates the cached diagnostic state of `file` immediately on a text
    /// edit, before the debounced background pass runs. The edit makes any
    /// cached report potentially stale: the file's delivered-generation is
    /// reset so neither pull channel can echo `Unchanged` from pre-edit state,
    /// and the store's verified marker is dropped so the workspace pull
    /// re-derives instead of serving the stale row.
    pub(crate) fn invalidate(&self, file: FileId) {
        let mut inner = self.inner.lock();
        inner.last_verified = None;
        if let Some(entry) = inner.files.get_mut(&file) {
            entry.delivered_generation = 0;
        }
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
        let clock = inner.access_clock;
        let generation = match inner.files.get_mut(&file) {
            Some(entry) => {
                // Salsa returns the identical `Arc` on a memo hit; fall back to
                // a full equality diff only when the memo re-executed.
                let unchanged =
                    Arc::ptr_eq(&entry.diagnostics, &report) || entry.diagnostics == report;
                let generation = if unchanged {
                    entry.generation
                } else {
                    // Persist the bump: `resultId` must advance whenever the
                    // report moved, and stay put otherwise, so a client's
                    // `previousResultId` can only ever match the same content.
                    entry.generation += 1;
                    entry.delivered_generation = 0;
                    entry.diagnostics = Arc::clone(&report);
                    entry.generation
                };
                entry.last_access = clock;
                generation
            }
            None => {
                inner.files.insert(
                    file,
                    FileDiagnostics {
                        generation: 1,
                        delivered_generation: 0,
                        diagnostics: Arc::clone(&report),
                        last_access: clock,
                    },
                );
                1
            }
        };
        inner.access_clock = clock + 1;
        trim_to_capacity(&mut inner);
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

/// Bounds the store's retained per-file rows to [`MAX_DIAGNOSTICS_FILES`]
/// (rust-analyzer-style bounded diagnostics map). Only unwatched, unsubscribed
/// rows are evicted, least-recently-used first; watched/subscribed files are
/// kept so the background pass can keep them fresh. An evicted row restarts its
/// generation at 1 on recompute, which can only force a full (never an
/// incorrect `Unchanged`) re-delivery — the client's higher `resultId` will
/// never match.
fn trim_to_capacity(inner: &mut DiagnosticsInner) {
    if inner.files.len() <= MAX_DIAGNOSTICS_FILES {
        return;
    }
    let mut candidates: Vec<(u64, FileId)> = inner
        .files
        .iter()
        .filter(|(file, _)| !inner.watched.contains(file) && !inner.subscribed.contains_key(file))
        .map(|(file, entry)| (entry.last_access, *file))
        .collect();
    candidates.sort_unstable();
    let overflow = inner.files.len() - MAX_DIAGNOSTICS_FILES;
    for (_, file) in candidates.into_iter().take(overflow) {
        inner.files.remove(&file);
    }
}

/// The `relatedDocuments` of a document-diagnostic pull for `self_file`: every
/// watched file whose diagnostics an edit to `self_file` can move, sealed with
/// its generation. A candidate that already reached the client is reported as
/// `unchanged` (a `resultId` only).
pub(crate) fn related_for(
    snapshot: &GlobalStateSnapshot,
    self_file: FileId,
) -> anyhow::Result<Option<FxHashMap<Uri, RelatedDocument>>> {
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
        check_cancelled(snapshot)?;
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
///
/// The pull evaluates against the *current* analysis snapshot (rust-analyzer
/// model): a cached generation is only served verbatim when the whole store is
/// verified against the exact revision in effect — that is, no input has
/// changed since the last full pull. Otherwise every file is re-derived
/// through the memoized salsa queries, which are O(1) cache hits for unaffected
/// files and recompute only files whose inputs actually moved. The debounced
/// background refresh pass never gates correctness here: a pull issued right
/// after `didChange` always reflects the new text instead of echoing a stale
/// pre-edit `resultId`.
pub(crate) fn workspace_diagnostic_reports(
    snapshot: &GlobalStateSnapshot,
    previous_ids: &FxHashMap<lsp_types::Uri, String>,
) -> anyhow::Result<Vec<lsp_types::WorkspaceDocumentDiagnosticReport>> {
    let analysis = &snapshot.analysis;
    let diag = &snapshot.diagnostics;

    // No input changed since the last fully-verified pull: every cached
    // generation is authoritative, so the whole pull is a digest comparison
    // against `previous_result_ids` with zero salsa work. Once any `didChange`
    // has been applied the revision differs and every report is re-derived.
    let (nonce, revision) = analysis.raw_database().nonce_and_revision();
    let verified = diag.is_verified(nonce, revision);

    let mut items = Vec::new();
    for file in diag.source_files() {
        check_cancelled(snapshot)?;
        let Ok(uri) = snapshot.file_id_to_url(file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        let version = crate::lsp::from_proto::vfs_path(&uri)
            .ok()
            .and_then(|path| snapshot.open_document_version(&path));
        // A cached generation is served only when it is provably current: the
        // store was fully verified against this exact revision, so no input
        // (text, roots, classpath) has moved since the reports were computed.
        // An unverified file's cache may be stale — an edit to it or to a
        // dependency — so it is re-derived through the analysis, a cheap memo
        // hit unless the file's inputs actually moved.
        let cached = {
            let mut inner = diag.inner.lock();
            if verified {
                let clock = inner.access_clock;
                inner.access_clock += 1;
                match inner.files.get_mut(&file) {
                    Some(entry) => {
                        entry.last_access = clock;
                        Some((entry.generation, Arc::clone(&entry.diagnostics)))
                    }
                    None => None,
                }
            } else {
                None
            }
        };
        let (generation, report) = match cached {
            Some(cached) => cached,
            None => diag.file_report(analysis, file, fallback)?,
        };
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
    // The pull loop covered every source file, so the store is now fully
    // current with this revision — later pulls short-circuit until a change.
    diag.mark_verified(nonce, revision);
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
/// report actually moved (their generation changed) plus whether any *other*
/// (dependent) file's report moved — the edited files alone do not warrant a
/// workspace-wide refresh.
///
/// All salsa reads run through [`ide::Analysis`]; a concurrent write cancels
/// the pass ([`Cancelled::PendingWrite`]) and the caller re-enqueues the same
/// changed set once the write is applied.
pub(crate) fn run_diagnostics_pass(
    snapshot: &GlobalStateSnapshot,
    changed: &FxHashSet<FileId>,
) -> anyhow::Result<(Vec<FileId>, bool)> {
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
    check_cancelled(snapshot)?;
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
        check_cancelled(snapshot)?;
        let Ok(uri) = snapshot.file_id_to_url(*file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        reports.push((*file, analysis.file_report(*file, fallback)?));
    }

    let mut changed_files: Vec<FileId> = Vec::new();
    {
        let mut inner = diag.inner.lock();
        let clock = inner.access_clock;
        for (file, report) in reports {
            let moved = match inner.files.get_mut(&file) {
                Some(entry) => {
                    let unchanged =
                        Arc::ptr_eq(&entry.diagnostics, &report) || entry.diagnostics == report;
                    if unchanged {
                        entry.last_access = clock;
                        false
                    } else {
                        entry.generation += 1;
                        entry.delivered_generation = 0;
                        entry.diagnostics = report;
                        entry.last_access = clock;
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
                            last_access: clock,
                        },
                    );
                    true
                }
            };
            if moved {
                changed_files.push(file);
            }
        }
        inner.access_clock = clock + 1;
        trim_to_capacity(&mut inner);
    }
    changed_files.sort();
    // Whether any moved file is a *dependent* (not one of the edited files):
    // only those moves can affect files the client is not already re-pulling.
    let cross_file = changed_files.iter().any(|file| !changed.contains(file));
    tracing::debug!(changed = ?changed_files, "diagnostics pass finished");
    Ok((changed_files, cross_file))
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
