//! Real-time cross-file diagnostics: the reverse-dependency pipeline that
//! turns an edit to file `A` into diagnostic updates for exactly the files
//! whose diagnostics *actually* changed as a result.
//!
//! The engine (`hir-ty::dep_index`) exposes, per file `B`, the set of source
//! files `B`'s type outputs resolve against ([`ide::Analysis::file_resolved_deps`])
//! and the set of names `B` resolves against ([`ide::Analysis::file_dependency_refs`]).
//! These are *forward* directions. This module maintains the *reverse* index:
//!
//! * `type_reverse`: `A → { B | A ∈ deps(B) }` — the exact type-level dependents.
//! * `name_reverse`: `name → { B | name ∈ refs(B) }` — the sound name-level
//!   fallback (static imports, bare member access), probed with the names the
//!   edited file exports.
//!
//! The reverse index is built lazily (one scan of the source set, on the worker
//! pool), then maintained incrementally: only the files whose own text changed
//! get their entries recomputed and diffed against the stored ones. Candidates
//! are verified against the per-file diagnostic seal
//! ([`diagnostic_seal`], built on [`ide::Analysis::file_diagnostics_digest`]),
//! so the emitted payloads are bounded to files whose diagnostics genuinely
//! changed.
//!
//! Delivery has two channels:
//! 1. **Pull**: `textDocument/diagnostic` embeds the changed candidates in
//!    `related_documents`, digest-sealed via `result_id`.
//! 2. **Push**: the background pass ([`run_cross_file_pass`]) returns full
//!    reports for the changed candidates, which the main loop forwards as
//!    `textDocument/publishDiagnostics` for *unopened* files.

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use hir::hir_expand::name::Name;
use lsp_types::{RelatedDocument, Uri};
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use smol_str::SmolStr;
use vfs::FileId;

use ide::{Analysis, Cancellable, LanguageKind};

use crate::{global_state::GlobalStateSnapshot, lsp::diagnostics as lsp_diagnostics};

/// Cap on the number of related documents embedded in one pull response, so a
/// single request can never balloon into a multi-megabyte payload even after a
/// change to a widely-used declaration.
const MAX_RELATED_DOCS: usize = 64;

/// A freshly computed `(file, deps, refs)` triple fed into the reverse index.
type IndexEntry = (FileId, Arc<FxHashSet<FileId>>, Arc<FxHashSet<Name>>);

/// The per-file diagnostic seal: a deterministic content hash of the file's
/// diagnostics, stable across identical contents. Rendered as the wire
/// `resultId`, so the client can cheaply detect "nothing changed".
///
/// Combines the memoized type + declaration digest (the engine's
/// `file_diagnostics_digest`) with a hash of the file-local syntax
/// diagnostics, so the seal covers *all* reported items.
pub(crate) fn diagnostic_seal(
    analysis: &Analysis,
    file_id: FileId,
    fallback_language_kind: LanguageKind,
) -> Cancellable<String> {
    let syntax = analysis.syntax_diagnostics(file_id, fallback_language_kind)?;
    let mut hasher = FxHasher::default();
    for diagnostic in &syntax {
        diagnostic.hash(&mut hasher);
    }
    let type_digest = analysis.file_diagnostics_digest(file_id)?;
    Ok(format!("{:016x}-{type_digest}", hasher.finish()))
}

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

/// The wire-level diagnostics of a file, lint-filtered and range-converted.
pub(crate) fn diagnostics_items(
    snapshot: &GlobalStateSnapshot,
    file_id: FileId,
    fallback_language_kind: LanguageKind,
) -> Cancellable<Vec<lsp_types::Diagnostic>> {
    let line_index = snapshot.file_line_index(file_id)?;
    let lints = snapshot.config.client_lints();
    let diagnostics = snapshot
        .analysis
        .syntax_diagnostics(file_id, fallback_language_kind)?
        .into_iter()
        .chain(snapshot.analysis.file_diagnostics(file_id)?)
        .filter(|diagnostic| lint_allows(lints, diagnostic))
        .map(|diagnostic| lsp_diagnostics::convert_diagnostic(&line_index, diagnostic))
        .collect();
    Ok(diagnostics)
}

/// The language kind inferred from a file URI, for files outside any source
/// root (the fallback `syntax_diagnostics` path).
fn fallback_kind(uri: &Uri) -> LanguageKind {
    LanguageKind::from_path(uri.path())
}

/// The names a file exports, probed against the reverse index to find the
/// files that may be affected by editing it. Both the canonical qualified name
/// and its trailing simple name are returned, matching how referenced names are
/// recorded in `file_dependency_refs` (qualified type references and bare
/// member names alike).
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

/// A full diagnostic report for one affected file, ready to be pushed.
#[derive(Debug)]
pub(crate) struct PublishPayload {
    pub(crate) file: FileId,
    pub(crate) uri: Uri,
    pub(crate) seal: String,
    pub(crate) diagnostics: Vec<lsp_types::Diagnostic>,
}

/// The files whose diagnostics may have moved after a source-file repartition
/// (see [`CrossFileDiagnostics::set_source_files`]).
pub(crate) struct SourceFileChanges {
    /// Files that entered the working set.
    pub(crate) added: Vec<FileId>,
    /// Files that remain but depended on a file that left the set.
    pub(crate) removed_dependents: Vec<FileId>,
}

#[derive(Default)]
struct CrossFileDiagData {
    /// Every source file the index covers (the working set of the reverse
    /// maps), refreshed by the main loop whenever source roots are repartitioned.
    source_files: Vec<FileId>,
    /// Whether the initial full build has completed at least once.
    built: bool,
    /// `dep(A)` → files whose `file_resolved_deps` include `A`.
    type_reverse: FxHashMap<FileId, FxHashSet<FileId>>,
    /// referenced name → files whose `file_dependency_refs` include it.
    name_reverse: FxHashMap<SmolStr, FxHashSet<FileId>>,
    /// Last-known dep set per indexed file, to diff on edit.
    deps_of: FxHashMap<FileId, Arc<FxHashSet<FileId>>>,
    /// Last-known ref set per indexed file, to diff on edit.
    refs_of: FxHashMap<FileId, Arc<FxHashSet<Name>>>,
    /// The last seal delivered to the client (via push or a related-documents
    /// `Full` report) per file.
    seals: FxHashMap<FileId, String>,
}

/// Shared reverse-dependency index + delivery seals. Cheap to clone (one
/// `Arc`), so it lives in both [`crate::GlobalState`] and every
/// [`GlobalStateSnapshot`].
#[derive(Clone, Default)]
pub(crate) struct CrossFileDiagnostics {
    inner: Arc<Mutex<CrossFileDiagData>>,
}

impl CrossFileDiagnostics {
    // ----- main-thread plumbing ---------------------------------------------

    /// Replaces the working set of the reverse index (called on source-root
    /// repartition, e.g. when files are created or deleted). Invalidates any
    /// previous build and prunes the per-file state of files that left the set,
    /// so a deleted file never resurfaces as a cross-file candidate.
    ///
    /// Returns the files whose diagnostics may have moved because of the
    /// repartition, so the caller can refresh them:
    /// - `added`: files that entered the set. Their symbols may now resolve
    ///   (or conflict) in files that referenced them, so those dependents are
    ///   probed on the next cross-file pass.
    /// - `removed_dependents`: files that remain but depended on a removed
    ///   file, re-verified directly (their diagnostics likely regressed).
    pub(crate) fn set_source_files(&self, files: Vec<FileId>) -> SourceFileChanges {
        let mut data = self.inner.lock();
        let old: FxHashSet<FileId> = data.source_files.iter().copied().collect();
        let new: FxHashSet<FileId> = files.iter().copied().collect();
        let removed: Vec<FileId> = old.difference(&new).copied().collect();
        let added: Vec<FileId> = new.difference(&old).copied().collect();
        // Only report `added` as a change to probe when the index was already
        // built: the initial load populates every file as "added" without an
        // edit ever happening, and probing that would swallow the real seed edit.
        let was_built = data.built;

        // The dependents of a removed file may now have moved diagnostics.
        // Capture them before pruning the reverse rows.
        let mut removed_dependents: FxHashSet<FileId> = FxHashSet::default();
        for dep in &removed {
            if let Some(users) = data.type_reverse.get(dep) {
                removed_dependents.extend(users.iter().copied());
            }
        }

        for file in &removed {
            data.deps_of.remove(file);
            data.refs_of.remove(file);
            data.seals.remove(file);
        }
        // A removed file is never a candidate anymore: drop its reverse rows.
        for dep in &removed {
            data.type_reverse.remove(dep);
        }
        // Files that left the set must also be removed from every dependent's
        // row (as a value) and from the name->users maps, so a deleted file
        // never resurfaces as a cross-file candidate.
        let removed_set: FxHashSet<FileId> = removed.iter().copied().collect();
        data.type_reverse.retain(|_, users| {
            users.retain(|user| !removed_set.contains(user));
            !users.is_empty()
        });
        data.name_reverse.retain(|_, users| {
            users.retain(|user| !removed_set.contains(user));
            !users.is_empty()
        });

        removed_dependents.retain(|file| new.contains(file));
        data.source_files = files;
        data.built = false;
        SourceFileChanges {
            added: if was_built { added } else { Vec::new() },
            removed_dependents: removed_dependents.into_iter().collect(),
        }
    }

    pub(crate) fn is_built(&self) -> bool {
        self.inner.lock().built
    }

    /// The last seal delivered for `file`, if any.
    pub(crate) fn seal_of(&self, file: FileId) -> Option<String> {
        self.inner.lock().seals.get(&file).cloned()
    }

    /// Records the seal of diagnostics just delivered for `file`.
    pub(crate) fn set_seal(&self, file: FileId, seal: String) {
        self.inner.lock().seals.insert(file, seal);
    }

    // ----- reverse-index maintenance ----------------------------------------

    /// The source files the index covers (the working set of the reverse maps).
    pub(crate) fn source_files(&self) -> Vec<FileId> {
        self.inner.lock().source_files.clone()
    }

    /// Applies freshly computed `(file, deps, refs)` triples. When `update` is
    /// set, the entries are diffed against the stored ones so stale reverse
    /// rows are drained; otherwise (initial build) rows are simply populated.
    fn apply_entries(&self, entries: Vec<IndexEntry>, update: bool) {
        let mut data = self.inner.lock();
        for (file, deps, refs) in entries {
            if update {
                if let Some(old) = data.deps_of.get(&file).cloned() {
                    let removed: Vec<FileId> = old.difference(&deps).copied().collect();
                    for dep in removed {
                        if let Some(set) = data.type_reverse.get_mut(&dep) {
                            set.remove(&file);
                            if set.is_empty() {
                                data.type_reverse.remove(&dep);
                            }
                        }
                    }
                }
                if let Some(old) = data.refs_of.get(&file).cloned() {
                    let to_remove: Vec<&Name> = old.iter().filter(|n| !refs.contains(*n)).collect();
                    for name in to_remove {
                        let Some(key) = trim(name.as_str()) else {
                            continue;
                        };
                        if let Some(set) = data.name_reverse.get_mut(key) {
                            set.remove(&file);
                            if set.is_empty() {
                                data.name_reverse.remove(key);
                            }
                        }
                    }
                }
            }
            for dep in deps.iter() {
                data.type_reverse.entry(*dep).or_default().insert(file);
            }
            for name in refs.iter() {
                if let Some(key) = trim(name.as_str()) {
                    data.name_reverse
                        .entry(SmolStr::new(key))
                        .or_default()
                        .insert(file);
                }
            }
            data.deps_of.insert(file, Arc::clone(&deps));
            data.refs_of.insert(file, Arc::clone(&refs));
        }
        data.built = true;
    }

    /// The files that may be affected by editing every file in `changed`,
    /// probed through the reverse index. Excludes the edited files themselves.
    fn candidates(
        &self,
        changed: &FxHashSet<FileId>,
        analysis: &Analysis,
    ) -> Cancellable<FxHashSet<FileId>> {
        // Exported names are salsa reads; collect them before locking the index.
        let mut probes: Vec<(FileId, Vec<SmolStr>)> = Vec::with_capacity(changed.len());
        for &a in changed {
            probes.push((a, exported_names(analysis, a)?));
        }

        let data = self.inner.lock();
        let mut out = FxHashSet::default();
        for (a, names) in &probes {
            if let Some(dependents) = data.type_reverse.get(a) {
                out.extend(dependents.iter().copied());
            }
            for name in names {
                if let Some(users) = data.name_reverse.get(name) {
                    out.extend(users.iter().copied());
                }
            }
        }
        for &a in changed {
            out.remove(&a);
        }
        Ok(out)
    }

    // ----- pull channel -----------------------------------------------------

    /// The `related_documents` of a document-diagnostic pull for `self_file`:
    /// every candidate that is affected by edits to `self_file`, sealed with
    /// its diagnostic digest. Candidates whose seal is already delivered are
    /// reported as unchanged (tiny payloads on steady state).
    pub(crate) fn related_for(
        &self,
        snapshot: &GlobalStateSnapshot,
        self_file: FileId,
    ) -> Cancellable<Option<FxHashMap<Uri, RelatedDocument>>> {
        let changed: FxHashSet<FileId> = FxHashSet::from_iter([self_file]);
        // A pull can still attach related documents before the background pass
        // ever ran, as long as the index is built.
        if !self.is_built() {
            return Ok(None);
        }
        let candidates = self.candidates(&changed, &snapshot.analysis)?;

        let mut out: FxHashMap<Uri, RelatedDocument> = FxHashMap::default();
        let mut new_seals: Vec<(FileId, String)> = Vec::new();
        for file in candidates {
            if file == self_file {
                continue;
            }
            let Ok(uri) = snapshot.file_id_to_url(file) else {
                continue;
            };
            let fallback = fallback_kind(&uri);
            let seal = diagnostic_seal(&snapshot.analysis, file, fallback)?;
            let report = if self.seal_of(file).as_deref() == Some(seal.as_str()) {
                RelatedDocument::UnchangedDocumentDiagnosticReport(
                    lsp_types::UnchangedDocumentDiagnosticReport { result_id: seal },
                )
            } else {
                if out.len() >= MAX_RELATED_DOCS {
                    continue;
                }
                let items = diagnostics_items(snapshot, file, fallback)?;
                new_seals.push((file, seal.clone()));
                RelatedDocument::FullDocumentDiagnosticReport(
                    lsp_types::FullDocumentDiagnosticReport {
                        result_id: Some(seal),
                        items,
                    },
                )
            };
            out.insert(uri, report);
        }
        for (file, seal) in new_seals {
            self.set_seal(file, seal);
        }
        Ok(Some(out))
    }
}

/// The diagnostic pipeline of a single background pass over a set of changed
/// files: (build | maintain) the reverse index, derive the candidate set,
/// verify each candidate's seal, and produce full push payloads for the files
/// whose diagnostics actually moved.
///
/// `force` holds files to re-verify directly (dependents touched by a source
/// file deletion) in addition to the candidate dependents probed from the
/// index; their own reports are emitted when their seal moved.
///
/// All salsa reads run through [`ide::Analysis`], whose query boundary returns
/// [`Cancelled::PendingWrite`] when a concurrent write lands; the caller
/// re-enqueues the same changed set once the write is applied.
pub(crate) fn run_cross_file_pass(
    snapshot: &GlobalStateSnapshot,
    changed: &FxHashSet<FileId>,
    force: &FxHashSet<FileId>,
) -> Cancellable<Vec<PublishPayload>> {
    let analysis = &snapshot.analysis;
    let cross = &snapshot.cross_file;

    // 1. Build the index on first use; afterwards only re-derive the edited
    //    files' own entries.
    let build_all = !cross.is_built();
    let mut entries: Vec<IndexEntry> = Vec::new();
    if build_all {
        for file in cross.source_files() {
            let deps = analysis.file_resolved_deps(file)?;
            let refs = analysis.file_dependency_refs(file)?;
            entries.push((file, deps, refs));
        }
    } else {
        let mut refresh: FxHashSet<FileId> = changed.clone();
        refresh.extend(force.iter().copied());
        for file in refresh {
            let deps = analysis.file_resolved_deps(file)?;
            let refs = analysis.file_dependency_refs(file)?;
            entries.push((file, deps, refs));
        }
    }
    cross.apply_entries(entries, !build_all);

    // 2. Candidate files.
    let mut verify = cross.candidates(changed, analysis)?;
    // 3. Files to verify directly (dependents touched by a deletion), in
    //    addition to the probes' candidates.
    verify.extend(force.iter().copied());

    // 4. Verify each file's seal and build push payloads for the ones that
    //    actually changed.
    tracing::debug!(changed = ?changed, candidates = ?verify, "cross-file pass candidates");
    let mut out = Vec::new();
    for file in verify {
        let Ok(uri) = snapshot.file_id_to_url(file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        let seal = diagnostic_seal(analysis, file, fallback)?;
        if cross.seal_of(file).as_deref() == Some(seal.as_str()) {
            tracing::debug!(?file, "cross-file candidate unchanged, skipping");
            continue;
        }
        let diagnostics = diagnostics_items(snapshot, file, fallback)?;
        out.push(PublishPayload {
            file,
            uri,
            seal,
            diagnostics,
        });
    }
    tracing::debug!(pushes = out.len(), "cross-file pass finished");
    Ok(out)
}

/// The `workspace/diagnostic` pull: one full or unchanged report per source
/// file, sealed with the file's diagnostic digest. A document whose previous
/// result id (*reported per URI*) matches its current seal is echoed as
/// `Unchanged` (a `resultId` only); everything else is a full report. Delivered
/// full states are sealed, so subsequent document pulls and pushes deduplicate
/// against them. Items are sorted by URI for determinism.
pub(crate) fn workspace_diagnostic_reports(
    snapshot: &GlobalStateSnapshot,
    previous_ids: &FxHashMap<lsp_types::Uri, String>,
) -> Cancellable<Vec<lsp_types::WorkspaceDocumentDiagnosticReport>> {
    let mut items = Vec::new();
    for file in snapshot.cross_file.source_files() {
        let Ok(uri) = snapshot.file_id_to_url(file) else {
            continue;
        };
        let fallback = fallback_kind(&uri);
        let seal = diagnostic_seal(&snapshot.analysis, file, fallback)?;
        let version = crate::lsp::from_proto::vfs_path(&uri)
            .ok()
            .and_then(|path| snapshot.open_document_version(&path));
        if previous_ids.get(&uri) == Some(&seal) {
            items.push(
                lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
                    lsp_types::WorkspaceUnchangedDocumentDiagnosticReport {
                        uri,
                        version,
                        unchanged_document_diagnostic_report: lsp_types::UnchangedDocumentDiagnosticReport {
                            result_id: seal,
                        },
                    },
                ),
            );
        } else {
            let diagnostics = diagnostics_items(snapshot, file, fallback)?;
            snapshot.cross_file.set_seal(file, seal.clone());
            items.push(
                lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(
                    lsp_types::WorkspaceFullDocumentDiagnosticReport {
                        uri,
                        version,
                        full_document_diagnostic_report: lsp_types::FullDocumentDiagnosticReport {
                            result_id: Some(seal),
                            items: diagnostics,
                        },
                    },
                ),
            );
        }
    }
    let mut keyed: Vec<(String, lsp_types::WorkspaceDocumentDiagnosticReport)> = items
        .into_iter()
        .map(|item| {
            let key = match &item {
                lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(
                    report,
                ) => report.uri.as_str().to_owned(),
                lsp_types::WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
                    report,
                ) => report.uri.as_str().to_owned(),
            };
            (key, item)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(keyed.into_iter().map(|(_, item)| item).collect())
}

fn trim(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
