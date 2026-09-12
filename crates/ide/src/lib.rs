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
pub mod nav;
pub mod symbols;
pub mod workspace;

pub use change::Change;
pub use nav::{HoverInfo, NavigationTarget};
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

    pub fn raw_database(&self) -> &RootDatabase {
        &self.db
    }

    pub fn raw_database_mut(&mut self) -> &mut RootDatabase {
        &mut self.db
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

    /// The library source files the reference at `offset` resolves into but
    /// which are not loaded into the database yet. The LSP layer reads each one
    /// out of its archive and re-runs the request; the retried
    /// [`Self::goto_definition`]/[`Self::hover`] then answers with the real
    /// source location.
    pub fn pending_library_sources(
        &self,
        file_id: FileId,
        offset: rowan::TextSize,
    ) -> Cancellable<Vec<nav::LibrarySourceRef>> {
        self.with_db(|db| nav::pending_library_sources(db, file_id, offset))
    }

    /// The hover at `offset` — the type of the expression or the signature of
    /// the declaration, or `None` when nothing is there. Serves the LSP
    /// `textDocument/hover` request.
    pub fn hover(
        &self,
        file_id: FileId,
        offset: rowan::TextSize,
    ) -> Cancellable<Option<HoverInfo>> {
        self.with_db(|db| nav::hover(db, file_id, offset))
    }

    /// The registered libraries (those reachable from some source set), in
    /// unspecified order. Empty before the first workspace load.
    pub fn registered_libraries(&self) -> Vec<LibraryId> {
        hir::registered_libraries(&self.db)
    }

    /// Builds the stub index of one library on the calling thread, so the
    /// first query that resolves into it does not pay the archive parse.
    /// `Err(Cancelled)` when a write landed while it ran.
    pub fn warmup_library(&self, id: LibraryId) -> Cancellable<()> {
        self.with_db(|db| hir::warmup_library(db, id))
    }

    /// Drops the persistent stub-cache entries of libraries the current
    /// project graph no longer registers. Called once a warmup pass finished.
    pub fn prune_stub_cache(&self) {
        hir::prune_stub_cache(&self.db)
    }
}
