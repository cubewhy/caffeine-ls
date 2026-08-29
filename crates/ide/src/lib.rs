use std::panic::AssertUnwindSafe;

use ide_db::{
    RootDatabase,
    base_db::{FileChange, salsa::Cancelled},
    line_index,
};

pub use ide_db::{
    Severity,
    base_db::LanguageKind,
    line_index::{LineCol, LineIndex},
};
pub use ide_diagnostics::Diagnostic;
use rustc_hash::FxHashSet;
pub use syntax::{DiagnosticCode, JavaDiagnosticCode, KotlinDiagnosticCode};
use triomphe::Arc;
use vfs::FileId;

pub mod delta;
pub mod nav;
pub mod symbols;

pub use nav::{HoverInfo, NavigationTarget};
pub use symbols::{DocumentSymbol, WorkspaceSymbol};

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

    pub fn apply_change(&mut self, change: FileChange) {
        change.apply(&mut self.db);
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

    pub fn syntax_diagnostics(
        &self,
        file_id: FileId,
        fallback_language_kind: LanguageKind,
    ) -> Cancellable<Vec<Diagnostic>> {
        self.with_db(|db| ide_diagnostics::syntax_diagnostics(db, file_id, fallback_language_kind))
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
    /// tracks and diffs per file (see [`ide_diagnostics::file_report`]).
    pub fn file_report(
        &self,
        file_id: FileId,
        fallback_language_kind: LanguageKind,
    ) -> Cancellable<std::sync::Arc<Vec<Diagnostic>>> {
        self.with_db(|db| ide_diagnostics::file_report(db, file_id, fallback_language_kind))
    }

    /// The workspace source files whose declarations the file's type outputs
    /// resolve against (see [`hir_ty::db::file_resolved_deps`]).
    pub fn file_resolved_deps(
        &self,
        file_id: FileId,
    ) -> Cancellable<std::sync::Arc<FxHashSet<FileId>>> {
        self.with_db(|db| hir_ty::db::file_resolved_deps(db, file_id))
    }

    /// The resolution-relevant names of the file, the sound name-level
    /// fallback of the cross-file dependency index (see
    /// [`hir_ty::db::file_dependency_refs`]).
    pub fn file_dependency_refs(
        &self,
        file_id: FileId,
    ) -> Cancellable<std::sync::Arc<FxHashSet<hir_expand::name::Name>>> {
        self.with_db(|db| hir_ty::db::file_dependency_refs(db, file_id))
    }

    /// Gets the file's `LineIndex`: data structure to convert between absolute
    /// offsets and line/column representation.
    pub fn file_line_index(&self, file_id: FileId) -> Cancellable<Arc<LineIndex>> {
        self.with_db(|db| line_index(db, file_id).clone())
    }

    /// The declared symbols of a file, in declaration order.
    pub fn document_symbols(&self, file_id: FileId) -> Cancellable<Vec<DocumentSymbol>> {
        self.with_db(|db| symbols::document_symbols(db, file_id))
    }

    /// Symbols whose simple name matches `query` (case-insensitive) across
    /// every registered source set, sorted by (name, file, item).
    pub fn workspace_symbols(&self, query: &str) -> Cancellable<Vec<WorkspaceSymbol>> {
        self.with_db(|db| symbols::workspace_symbols(db, query))
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
    ) -> Cancellable<Arc<Vec<String>>> {
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
}
