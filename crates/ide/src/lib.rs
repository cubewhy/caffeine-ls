use std::panic::AssertUnwindSafe;
use std::path::Path;

use ide_db::{RootDatabase, base_db::salsa::Cancelled, line_index};

pub use hir::{
    Classpath, ClasspathEntry, LibraryInfo, LibraryKind, LibrarySources, ProjectGraphData,
    SourceSetId, SourceSymbolKind,
};
pub use ide_db::{
    Severity,
    base_db::LanguageKind,
    line_index::{LineCol, LineIndex},
};
pub use ide_diagnostics::Diagnostic;
pub use project_model::LibraryId;
use rustc_hash::FxHashSet;
pub use syntax::{DiagnosticCode, JavaDiagnosticCode, KotlinDiagnosticCode};
use triomphe::Arc;
use vfs::FileId;

mod change;
pub mod delta;
pub mod docs;
pub mod highlight;
pub mod inlay_hints;
pub mod nav;
pub mod symbols;
pub mod workspace;

pub use change::Change;
pub use highlight::{Highlight, HlMods, HlTag};
pub use inlay_hints::{
    InlayHint, InlayHintDetail, InlayHintEdit, InlayHintKind, InlayHintLabelPart, InlayHintsConfig,
};
pub use nav::{HoverInfo, LibraryFileRef, NavigationTarget, ReferenceTarget};
pub use symbols::{DocumentSymbol, WorkspaceSymbolSummary};
pub use workspace::WorkspaceReport;

pub type Cancellable<T> = Result<T, Cancelled>;

pub struct AnalysisHost {
    db: RootDatabase,
}

impl AnalysisHost {
    pub fn new() -> Self {
        Self {
            db: RootDatabase::new(),
        }
    }

    pub fn snapshot(&self) -> Analysis {
        Analysis {
            db: self.db.clone(),
        }
    }

    /// Applies the change to the database. Outstanding snapshots are canceled.
    pub fn apply_change(&mut self, change: Change) {
        change.apply(&mut self.db);
    }

    /// Enables the persistent (LMDB-backed) stub cache under
    /// `<cache_dir>/stubs/vN`. Returns whether it could be enabled; without it
    /// the stub index stays in memory only.
    pub fn enable_persistent_stub_cache(&self, cache_dir: &Path) -> bool {
        hir::enable_persistent_stub_cache(&self.db, cache_dir)
    }

    /// The registered libraries (those reachable from some source set), in
    /// unspecified order. Empty before the first workspace load. Reads the
    /// host database directly, without a snapshot: the caller needs the ids
    /// only to hand out to [`Self::library_warmup`].
    pub fn registered_libraries(&self) -> Vec<LibraryId> {
        hir::registered_libraries(&self.db)
    }

    /// A cloneable handle to the session's library-index state, for background
    /// warmup that must not hold a database snapshot. Warming one archive can
    /// take seconds (a JDK image), and a snapshot clone held for that long
    /// blocks the next write — and with it the server's main loop.
    pub fn library_warmup(&self) -> LibraryWarmup {
        LibraryWarmup {
            state: self.db.shared_hir_state(),
        }
    }

    pub fn raw_database(&self) -> &RootDatabase {
        &self.db
    }

    pub fn raw_database_mut(&mut self) -> &mut RootDatabase {
        &mut self.db
    }
}

/// Background warmup of the library stub index that holds no database
/// snapshot, so a long archive parse cannot block the server's next write.
#[derive(Clone)]
pub struct LibraryWarmup {
    state: Arc<hir::HirState>,
}

impl LibraryWarmup {
    /// Builds the tier-1 stub index of `id` on the calling thread, storing it
    /// in the session cache so the first query that resolves into the library
    /// does not pay the archive parse. A library a reload has already dropped
    /// is skipped.
    pub fn warm(&self, id: LibraryId) {
        hir::warmup_library(&self.state, id);
    }

    /// Drops persistent stub-cache entries of libraries `live` does not name.
    /// Called once a warmup pass finished.
    pub fn prune(&self, live: &FxHashSet<LibraryId>) {
        hir::prune_stub_cache(&self.state, live);
    }
}

impl Default for AnalysisHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of [AnalysisHost]
#[derive(Clone)]
pub struct Analysis {
    db: RootDatabase,
}

impl std::panic::UnwindSafe for Analysis {}

impl Analysis {
    pub fn raw_database(&self) -> &RootDatabase {
        &self.db
    }

    /// Performs an operation on the database that may be canceled.
    ///
    /// LSP needs to be able to answer semantic questions about the
    /// code while the code is being modified. A common problem is that a
    /// long-running query is being calculated when a new change arrives.
    ///
    /// We can't just apply the change immediately: this will cause the pending
    /// query to see inconsistent state (it will observe an absence of
    /// repeatable read). So what we do is we **cancel** all pending queries
    /// before applying the change.
    ///
    /// Salsa implements cancellation by unwinding with a special value and
    /// catching it on the API boundary.
    fn with_db<F, T>(&self, f: F) -> Cancellable<T>
    where
        F: FnOnce(&RootDatabase) -> T + std::panic::UnwindSafe,
    {
        Cancelled::catch(AssertUnwindSafe(|| f(&self.db)))
    }

    pub fn syntax_diagnostics(&self, file_id: FileId) -> Cancellable<Vec<Diagnostic>> {
        self.with_db(|db| ide_diagnostics::syntax_diagnostics(db, file_id))
    }

    /// The type-layer diagnostics of the file, collected from the inference of
    /// every body it owns (see [`ide_diagnostics::type_diagnostics`]).
    pub fn type_diagnostics(&self, file_id: FileId) -> Cancellable<Vec<Diagnostic>> {
        self.with_db(|db| ide_diagnostics::type_diagnostics(db, file_id))
    }

    /// The declaration-level diagnostics of the file — unknown-type/ambiguity
    /// and import reports ([JLS §6.5.5.1], [§7.5]) and the inheritance check
    /// of every class-like declaration ([§8.4.8.3], [§9.4.1.3], [§9.6.4.4]) —
    /// see [`ide_diagnostics::declaration_diagnostics`].
    pub fn declaration_diagnostics(&self, file_id: FileId) -> Cancellable<Vec<Diagnostic>> {
        self.with_db(|db| ide_diagnostics::declaration_diagnostics(db, file_id))
    }

    /// The type-layer and declaration-level diagnostics of the file, merged
    /// and memoized as one salsa query (see
    /// [`ide_diagnostics::file_diagnostics`]).
    pub fn file_diagnostics(&self, file_id: FileId) -> Cancellable<Vec<Diagnostic>> {
        self.with_db(|db| {
            ide_diagnostics::file_diagnostics(db, file_id)
                .iter()
                .cloned()
                .collect()
        })
    }

    /// The complete report of the file — its syntax diagnostics plus its merged
    /// type and declaration diagnostics — the unit the LSP diagnostics store
    /// tracks and diffs per file (see [`ide_diagnostics::file_report`]). The
    /// memoized `Arc` is returned as it is, so a repeat pull is an O(1) cache
    /// hit.
    pub fn file_report(&self, file_id: FileId) -> Cancellable<triomphe::Arc<[Diagnostic]>> {
        self.with_db(|db| ide_diagnostics::file_report(db, file_id))
    }

    /// The workspace source files whose declarations the file's type outputs
    /// resolve against (see [`hir_ty::java::db::file_resolved_deps`]).
    pub fn file_resolved_deps(
        &self,
        file_id: FileId,
    ) -> Cancellable<triomphe::Arc<FxHashSet<FileId>>> {
        self.with_db(|db| hir_ty::java::db::file_resolved_deps(db, file_id))
    }

    /// The resolution-relevant names of the file, the sound name-level
    /// fallback of the cross-file dependency index (see
    /// [`hir_ty::java::db::file_dependency_refs`]).
    pub fn file_dependency_refs(
        &self,
        file_id: FileId,
    ) -> Cancellable<triomphe::Arc<FxHashSet<hir_expand::name::Name>>> {
        self.with_db(|db| hir_ty::java::db::file_dependency_refs(db, file_id))
    }

    /// Gets the file's `LineIndex`: data structure to convert between absolute
    /// offsets and line/column representation.
    pub fn file_line_index(&self, file_id: FileId) -> Cancellable<Arc<LineIndex>> {
        self.with_db(|db| line_index(db, file_id).clone())
    }

    /// The complete diagnostic report of every workspace source file, computed
    /// in parallel across the memoized per-file salsa queries (see
    /// [`workspace::workspace_reports`]). The `workspace/diagnostic` pull.
    pub fn workspace_reports(&self) -> Cancellable<Vec<WorkspaceReport>> {
        self.with_db(workspace::workspace_reports)
    }

    /// The declared symbols of a file, in declaration order.
    pub fn document_symbols(&self, file_id: FileId) -> Cancellable<Vec<DocumentSymbol>> {
        self.with_db(|db| symbols::document_symbols(db, file_id))
    }

    /// Symbols whose simple name starts with `query`, or whose canonical name
    /// (`pkg.Enclosing.simple`) contains it — case-insensitive — across every
    /// registered source set, or only within `files` when given; sorted by
    /// (canonical name, file, item). Dotted queries (`Class.member`,
    /// `com.example.Foo`) match the canonical name. Member rows carry
    /// `{EnclosingType}.{simple}` names (`Foo.bar`) with the package as
    /// `container_name`, so dotted queries also match the row a client
    /// filters locally. Rows are cheap summaries (no ranges, no signatures);
    /// resolve the range for a single row on demand via
    /// [`Analysis::source_symbol_range`]. An empty query returns everything
    /// in scope.
    pub fn workspace_symbols(
        &self,
        query: &str,
        files: Option<&[FileId]>,
    ) -> Cancellable<Vec<WorkspaceSymbolSummary>> {
        self.with_db(|db| symbols::workspace_symbol_summaries(db, query, files))
    }

    /// The declaration range of one lowered item, for `workspaceSymbol/resolve`.
    /// `item` is the raw `ItemId` arena index. `None` when the `(file, item)`
    /// no longer exists.
    pub fn source_symbol_range(
        &self,
        file_id: FileId,
        item: u32,
    ) -> Cancellable<Option<rowan::TextRange>> {
        self.with_db(|db| symbols::source_symbol_range(db, file_id, item))
    }

    /// The declared type of an item — a field's type, a method's return type,
    /// or the type of a class-like declaration — from the HIR type layer.
    pub fn item_ty(
        &self,
        file_id: FileId,
        item: hir::hir_def::java::item_tree::ItemId,
    ) -> Cancellable<String> {
        self.with_db(|db| symbols::item_ty(db, file_id, item))
    }

    /// The parameter types of a method or constructor, in declaration order.
    pub fn method_params(
        &self,
        file_id: FileId,
        item: hir::hir_def::java::item_tree::ItemId,
    ) -> Cancellable<Arc<[String]>> {
        self.with_db(|db| symbols::method_params(db, file_id, item))
    }

    /// The declaration(s) the reference at `offset` resolves to
    /// ([JLS §6.5]) — the LSP `textDocument/definition` result.
    pub fn goto_definition(
        &self,
        file_id: FileId,
        offset: rowan::TextSize,
    ) -> Cancellable<Vec<NavigationTarget>> {
        self.with_db(|db| nav::definition(db, file_id, offset))
    }

    /// The reference sites of the declaration(s) the reference at `offset`
    /// names — the LSP `textDocument/references` result. `include_declaration`
    /// adds each declaration's own name token.
    pub fn references(
        &self,
        file_id: FileId,
        offset: rowan::TextSize,
        include_declaration: bool,
    ) -> Cancellable<Vec<ReferenceTarget>> {
        self.with_db(|db| nav::references(db, file_id, offset, include_declaration))
    }

    /// The library files the reference at `offset` resolves into but which are
    /// not loaded into the database yet — an archive entry to read, or a class
    /// to decompile. The LSP layer materializes each one and re-runs the
    /// request; the retried [`Self::goto_definition`]/[`Self::hover`] then
    /// answers with the real source location.
    pub fn pending_library_files(
        &self,
        file_id: FileId,
        offset: rowan::TextSize,
    ) -> Cancellable<Vec<nav::LibraryFileRef>> {
        self.with_db(|db| nav::pending_library_files(db, file_id, offset))
    }

    /// The hover at `offset` — the declaration a reference names, the type of
    /// the expression the offset is inside, or the declaration whose own name
    /// the offset is on; `None` when the offset names nothing. Serves the LSP
    /// `textDocument/hover` request.
    pub fn hover(
        &self,
        file_id: FileId,
        offset: rowan::TextSize,
    ) -> Cancellable<Option<HoverInfo>> {
        self.with_db(|db| nav::hover(db, file_id, offset))
    }

    /// The declaration of the class-like type `fqn` names in `file_id`'s scope
    /// — what a click on an inlay hint's type label navigates to. `None` when
    /// the scope resolves the name to no declaration, or only to a library
    /// class whose source is not loaded.
    pub fn class_definition(
        &self,
        file_id: FileId,
        fqn: &str,
    ) -> Cancellable<Option<NavigationTarget>> {
        self.with_db(|db| nav::class_declaration(db, file_id, fqn))
    }

    /// The semantic highlighting of the file — the model behind the LSP
    /// `textDocument/semanticTokens` requests, sorted by range start.
    pub fn highlight(&self, file_id: FileId) -> Cancellable<Vec<Highlight>> {
        self.with_db(|db| highlight::highlight(db, file_id))
    }

    /// The inlay hints of the file whose offset `range` contains, sorted by
    /// offset — the model behind the LSP `textDocument/inlayHint` request (see
    /// [`crate::inlay_hints`]).
    pub fn inlay_hints(
        &self,
        file_id: FileId,
        range: rowan::TextRange,
        config: &InlayHintsConfig,
    ) -> Cancellable<Vec<InlayHint>> {
        self.with_db(|db| inlay_hints::inlay_hints(db, file_id, range, config))
    }

    /// The detail of the one hint a resolve names — the tooltip, the label
    /// parts' declarations and the edits accepting the hint applies. `None`
    /// when no hint is anchored at `(offset, kind)` any more (see
    /// [`crate::inlay_hints::inlay_hint_resolve`]).
    pub fn inlay_hint_resolve(
        &self,
        file_id: FileId,
        offset: rowan::TextSize,
        kind: InlayHintKind,
        config: &InlayHintsConfig,
    ) -> Cancellable<Option<InlayHintDetail>> {
        self.with_db(|db| inlay_hints::inlay_hint_resolve(db, file_id, offset, kind, config))
    }
}
