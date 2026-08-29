use hir::hir_expand::item_tree::{ItemData, ItemId, ItemTree};
use ide_db::{
    FileRange, RootDatabase, Severity,
    base_db::{self, FileText, LanguageKind, SourceDatabase, salsa},
};
use rowan::TextRange;
use std::collections::HashMap;
use syntax::DiagnosticCode;
use vfs::FileId;

use std::sync::Arc;

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct Diagnostic {
    pub message: String,
    pub range: FileRange,
    pub severity: Severity,
    pub unused: bool,
    /// The stable diagnostic code, when the underlying error kind carries one
    /// (see [`syntax::DiagnosticCode`]); surfaces as the LSP `code` field.
    pub code: Option<DiagnosticCode>,
}

/// The push-based collection point of a diagnostics run, mirroring the
/// rust-analyzer `DiagnosticSink`: every check writes its findings into the
/// sink keyed by the file the finding belongs to, so a single check invocation
/// may produce diagnostics for several files (cross-file checks) and the caller
/// gathers them uniformly.
#[derive(Default)]
pub struct DiagnosticSink {
    pub(crate) per_file: HashMap<FileId, Vec<Diagnostic>>,
}

impl DiagnosticSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a diagnostic against the file it reports on.
    pub fn push(&mut self, file_id: FileId, diagnostic: Diagnostic) {
        self.per_file.entry(file_id).or_default().push(diagnostic);
    }

    /// The diagnostics collected for `file_id` (empty when none were pushed).
    pub fn into_file(mut self, file_id: FileId) -> Vec<Diagnostic> {
        self.per_file.remove(&file_id).unwrap_or_default()
    }
}

pub fn syntax_diagnostics(
    db: &RootDatabase,
    file_id: FileId,
    fallback_language_kind: LanguageKind,
) -> Vec<Diagnostic> {
    let mut sink = DiagnosticSink::new();
    collect_syntax(&mut sink, db, file_id, fallback_language_kind);
    sink.into_file(file_id)
}

/// Pushes the parse-level (syntax) diagnostics of `file_id` into `sink`.
pub(crate) fn collect_syntax(
    sink: &mut DiagnosticSink,
    db: &RootDatabase,
    file_id: FileId,
    fallback_language_kind: LanguageKind,
) {
    // Before the workspace is loaded the file is not part of any source root,
    // so `file_language_kind` can't resolve the language. Fall back to the
    // kind inferred from the file path to keep basic syntax diagnostics.
    let language_kind = db
        .file_language_kind(file_id)
        .filter(|&kind| kind != LanguageKind::Unknown)
        .unwrap_or(fallback_language_kind);
    if language_kind == LanguageKind::Unknown {
        tracing::warn!("unsupported language");
        return;
    }

    let parse = base_db::parse(db, file_id, language_kind);
    for e in parse.errors() {
        sink.push(
            file_id,
            make_diagnostic(file_id, &e.message, e.range, e.code, Severity::Error),
        );
    }
}

/// The type-layer diagnostics of a file ([JLS §6.5], [§14.18], [§15.11],
/// [§15.12]): the `TypeError`s reported while inferring the body of every
/// method, constructor, initializer, field initializer and enum constant
/// argument in the file (see [`hir_ty::body_types`]). Each diagnostic's range
/// is the source range of the offending construct, computed from its body-IR
/// arena id.
pub fn type_diagnostics(db: &dyn hir_ty::TyDatabase, file_id: FileId) -> Vec<Diagnostic> {
    let mut sink = DiagnosticSink::new();
    collect_type_diagnostics(&mut sink, db, file_id);
    sink.into_file(file_id)
}

pub(crate) fn collect_type_diagnostics(
    sink: &mut DiagnosticSink,
    db: &dyn hir_ty::TyDatabase,
    file_id: FileId,
) {
    let tree = hir::file_item_tree(db, file_id);
    let bodies = hir::file_body_tree(db, file_id);
    for (item_id, _) in all_items(&tree) {
        let Some(body_types) = hir_ty::body_types(db, file_id, item_id) else {
            continue;
        };
        for diagnostic in &body_types.diagnostics {
            let Some(range) = diagnostic.range(&bodies) else {
                // A synthetic construct (e.g. a `Missing` expression lowered
                // from broken source) has no range to point at.
                continue;
            };
            sink.push(
                file_id,
                make_diagnostic(
                    file_id,
                    &diagnostic.message(&bodies),
                    range,
                    Some(diagnostic.code()),
                    // Raw-type and unchecked-conversion reports are warnings
                    // ([JLS §4.12.2], [§5.1.9]): legal programs, flagged for
                    // their unsoundness.
                    if diagnostic.is_warning() {
                        Severity::Warning
                    } else {
                        Severity::Error
                    },
                ),
            );
        }
    }
}

/// The declaration-level diagnostics of a file ([JLS §6.5.5.1], [§7.5], [§8],
/// [§9]): the unknown-type/ambiguity/import reports and the override and
/// default-method checks of [`hir_ty::class_diagnostics`]. Each reference
/// carries its own source range; the hierarchy checks are keyed to the
/// offending method's name.
pub fn declaration_diagnostics(db: &dyn hir_ty::TyDatabase, file_id: FileId) -> Vec<Diagnostic> {
    let mut sink = DiagnosticSink::new();
    collect_declaration_diagnostics(&mut sink, db, file_id);
    sink.into_file(file_id)
}

pub(crate) fn collect_declaration_diagnostics(
    sink: &mut DiagnosticSink,
    db: &dyn hir_ty::TyDatabase,
    file_id: FileId,
) {
    for diagnostic in hir_ty::class_diagnostics(db, file_id) {
        let Some(range) = diagnostic.range().or_else(|| {
            // The hierarchy checks (incompatible override, conflicting
            // defaults, missing `@Override`) are keyed to the declaring method
            // name; point at the whole declaration when no reference range is
            // recorded.
            let tree = hir::file_item_tree(db, file_id);
            let method_name = diagnostic.method_name();
            let item = tree
                .top
                .iter()
                .copied()
                .find_map(|top| find_method(&tree, top, method_name));
            item.map(|item| tree.data(item).range())
        }) else {
            continue;
        };
        sink.push(
            file_id,
            make_diagnostic(
                file_id,
                &diagnostic.message(),
                range,
                Some(diagnostic.code()),
                Severity::Error,
            ),
        );
    }
}

/// The type-layer and declaration-level diagnostics of a file, merged into a
/// single report. Unlike syntax diagnostics — which are strictly file-local —
/// these can change when a *different* file, one this file's types resolve
/// against, is edited, so salsa memoizes them per [`FileText`] and the LSP
/// layer compares digests instead of recomputing them. A text edit to another
/// file invalidates only the innermost queries whose inputs actually changed
/// (`body_types_query`, `class_diagnostics_query`), so re-deriving an
/// unaffected file's report here is a hash compare, not a re-inference.
#[salsa::tracked(returns(clone))]
pub(crate) fn file_diagnostics_query(
    db: &dyn hir_ty::TyDatabase,
    file: FileText,
) -> Arc<Vec<Diagnostic>> {
    let file_id = *file.file_id(db);
    let mut sink = DiagnosticSink::new();
    collect_type_diagnostics(&mut sink, db, file_id);
    collect_declaration_diagnostics(&mut sink, db, file_id);
    Arc::new(sink.into_file(file_id))
}

/// The merged type + declaration diagnostics of a file.
pub fn file_diagnostics(db: &dyn hir_ty::TyDatabase, file_id: FileId) -> Arc<Vec<Diagnostic>> {
    file_diagnostics_query(db, db.file_text(file_id))
}

/// The complete report of a file: its syntax diagnostics plus its merged type
/// and declaration diagnostics, collected into the same sink. This is the unit
/// the LSP diagnostics store tracks and diffs per file.
pub fn file_report(
    db: &RootDatabase,
    file_id: FileId,
    fallback_language_kind: LanguageKind,
) -> Arc<Vec<Diagnostic>> {
    let mut sink = DiagnosticSink::new();
    collect_syntax(&mut sink, db, file_id, fallback_language_kind);
    collect_type_diagnostics(&mut sink, db, file_id);
    collect_declaration_diagnostics(&mut sink, db, file_id);
    Arc::new(sink.into_file(file_id))
}

fn find_method(tree: &ItemTree, id: ItemId, name: &str) -> Option<ItemId> {
    match tree.data(id) {
        ItemData::Method(method) if method.name.as_str() == name => return Some(id),
        _ => {}
    }
    for &child in tree.data(id).body() {
        if let Some(found) = find_method(tree, child, name) {
            return Some(found);
        }
    }
    None
}

/// Every `(ItemId, &ItemData)` in the tree, parents before children.
fn all_items(tree: &ItemTree) -> Vec<(ItemId, &ItemData)> {
    fn walk<'a>(tree: &'a ItemTree, id: ItemId, out: &mut Vec<(ItemId, &'a ItemData)>) {
        let data = tree.data(id);
        out.push((id, data));
        for &child in data.body() {
            walk(tree, child, out);
        }
    }
    let mut out = Vec::new();
    for &top in &tree.top {
        walk(tree, top, &mut out);
    }
    out
}

fn make_diagnostic(
    file_id: FileId,
    message: &str,
    range: TextRange,
    code: Option<DiagnosticCode>,
    severity: Severity,
) -> Diagnostic {
    let range = FileRange::new(file_id, range);
    Diagnostic {
        message: message.to_string(),
        range,
        severity,
        unused: false,
        code,
    }
}
