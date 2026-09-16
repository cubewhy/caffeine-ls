//! The IDE's diagnostics: what every check reports, and how it is presented.
//!
//! The type layer (`hir-ty`) *detects*: it records structured diagnostics —
//! [`hir_ty::TypeError`] for a body, [`hir_ty::DeclDiagnostic`] for a
//! declaration — and nothing else. This crate owns the *policy and
//! presentation* on top of them:
//!
//! * the `@SuppressWarnings` scopes of
//!   [JLS §9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5)
//!   ([`lint`]), and therefore which warnings an enclosing declaration
//!   suppresses;
//! * each diagnostic's severity ([`Severity::Warning`] exactly for the
//!   diagnostics a lint key names);
//! * each diagnostic's stable [`DiagnosticCode`] and its user-facing message
//!   and secondary detail ([`handlers`]).
//!
//! Every diagnostic a check produces is reported — §9.6.4.5 governs the scope
//! of a suppression, not a set a client may switch off — so the collected
//! report ([`file_report`], [`file_diagnostics`]) is exactly the checks'
//! output minus the warnings an enclosing `@SuppressWarnings` names. It is what
//! the LSP layer pulls per file, and the conformance renderers in `hir-ty`'s
//! integration tests print from the same surface.

use hir::hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use ide_db::{
    FileRange, RootDatabase, Severity,
    base_db::{self, FileText, LanguageKind, SourceDatabase, salsa},
};
use rowan::TextRange;
use rustc_hash::FxHashMap;
use syntax::DiagnosticCode;
use vfs::FileId;

use triomphe::Arc;

mod handlers;
mod lint;
pub use handlers::body::{code as body_code, message as body_message, related as body_related};
pub use handlers::decl::{code as decl_code, message as decl_message};
pub use lint::{keeps_body_diagnostic, keeps_decl_diagnostic};

/// A diagnostic as the IDE layer sees it: its message, its primary and
/// secondary ranges, its severity and its stable code.
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct Diagnostic {
    pub message: String,
    pub range: FileRange,
    pub severity: Severity,
    pub unused: bool,
    /// The stable diagnostic code, when the underlying error kind carries one
    /// (see [`syntax::DiagnosticCode`]); surfaces as the LSP `code` field.
    pub code: Option<DiagnosticCode>,
    /// Secondary ranges and messages, surfaced as LSP `related_information`
    /// (e.g. the `required:`/`found:`/`reason:` detail of an invocation or
    /// assignment mismatch, IntelliJ-style).
    pub related_information: Vec<RelatedInformation>,
}

/// A single item of a diagnostic's [`Diagnostic::related_information`]: a
/// secondary message attached to a range in the same file.
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct RelatedInformation {
    pub message: String,
    pub range: FileRange,
}

/// The push-based collection point of a diagnostics run, mirroring the
/// rust-analyzer `DiagnosticSink`: every check writes its findings into the
/// sink keyed by the file the finding belongs to, so a single check invocation
/// may produce diagnostics for several files (cross-file checks) and the caller
/// gathers them uniformly.
#[derive(Default)]
pub struct DiagnosticSink {
    pub(crate) per_file: FxHashMap<FileId, Vec<Diagnostic>>,
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

pub fn syntax_diagnostics(db: &RootDatabase, file_id: FileId) -> Vec<Diagnostic> {
    let mut sink = DiagnosticSink::new();
    collect_syntax(&mut sink, db, file_id);
    sink.into_file(file_id)
}

/// Pushes the parse-level (syntax) diagnostics of `file_id` into `sink`.
///
/// The language comes from the file's owning source root; a file that is not
/// part of any root yet (e.g. opened before the workspace finished loading)
/// reports no syntax diagnostics until it is attached to one.
pub(crate) fn collect_syntax(sink: &mut DiagnosticSink, db: &dyn SourceDatabase, file_id: FileId) {
    let Some(language_kind) = base_db::file_language_kind(db, file_id) else {
        tracing::debug!(
            ?file_id,
            "file has no source root; skipping syntax diagnostics"
        );
        return;
    };
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
/// arena id, and every one is reported unless an enclosing
/// `@SuppressWarnings` names its key ([JLS §9.6.4.5]).
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
    // A Kotlin file's findings are the Kotlin type layer's: it walks the
    // file's items and reports each with its own severity.
    if let Some(tree) = hir::hir_def::kotlin::plugin::model(&hir::file_item_tree(db, file_id)) {
        for (id, _) in tree.items.iter() {
            let types = hir_ty::kotlin_body_types(db, file_id, hir_expand::ids::ItemId(id));
            for diagnostic in &types.diagnostics {
                let Some(range) = diagnostic.range() else {
                    continue;
                };
                sink.push(
                    file_id,
                    make_diagnostic(
                        file_id,
                        &diagnostic.message(db),
                        range,
                        Some(DiagnosticCode::Kotlin(diagnostic.code())),
                        Severity::Error,
                    ),
                );
            }
        }
        return;
    }
    // Every other file — a Java one, and one no language lowered, which declares
    // no item at all — is walked as Java.
    let tree = hir::java_item_tree(db, file_id);
    for (item_id, _) in all_items(&tree) {
        for diagnostic in item_diagnostics_impl(db, file_id, item_id) {
            sink.push(file_id, diagnostic);
        }
    }
}

fn item_diagnostics_impl(
    db: &dyn hir_ty::TyDatabase,
    file_id: FileId,
    item_id: ItemId,
) -> Vec<Diagnostic> {
    let bodies = hir::file_body_tree(db, file_id);
    let Some(body_types) = hir_ty::body_types(db, file_id, item_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for diagnostic in &body_types.diagnostics {
        // §9.6.4.5: a warning named by an enclosing `@SuppressWarnings` is not
        // reported at all. No string suppresses an error.
        if !lint::keeps_body_diagnostic(db, file_id, &bodies, diagnostic) {
            continue;
        }
        let Some(range) = diagnostic.range(&bodies) else {
            // A synthetic construct (e.g. a `Missing` expression lowered
            // from broken source) has no range to point at.
            continue;
        };
        let related = body_related(db, diagnostic, &bodies)
            .into_iter()
            .map(|(message, range)| RelatedInformation {
                message,
                range: FileRange::new(file_id, range),
            });
        out.push(
            make_diagnostic(
                file_id,
                &body_message(db, diagnostic, &bodies),
                range,
                Some(body_code(diagnostic)),
                // Raw-type and unchecked-conversion reports are warnings
                // ([JLS §4.12.2], [§5.1.9]): legal programs, flagged for
                // their unsoundness.
                lint::severity_of_body(diagnostic),
            )
            .with_related(related),
        );
    }
    out
}

/// The declaration-level diagnostics of a file ([JLS §6.5.5.1], [§7.5], [§8],
/// [§9]): the unknown-type/ambiguity/import reports and the override and
/// default-method checks of [`hir_ty::class_diagnostics`], plus the
/// module-directive checks of [`hir_ty::module_diagnostics`]. Each reference
/// carries its own source range; the hierarchy checks are keyed to the
/// offending method's name.
///
/// [`hir_ty::level_diagnostics`] adds the source-level checks: every construct
/// newer than the level the file's source set is compiled at.
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
        // §9.6.4.5: a warning named by an enclosing `@SuppressWarnings` is not
        // reported at all.
        if !lint::keeps_decl_diagnostic(db, file_id, &diagnostic) {
            continue;
        }
        let Some(range) = diagnostic.range().or_else(|| {
            // The hierarchy checks (incompatible override, conflicting
            // defaults, missing `@Override`) are keyed to the declaring method
            // name; point at the whole declaration when no reference range is
            // recorded.
            let tree = hir::java_item_tree(db, file_id);
            let method_name = diagnostic.method_name();
            let item = tree
                .top
                .iter()
                .copied()
                .find_map(|top| find_method(&tree, top, method_name));
            item.and_then(|item| {
                // The item tree carries no offsets; resolve the declaration
                // range from the file's parse.
                let language = tree.language;
                if language == base_db::LanguageKind::Unknown {
                    return None;
                }
                let source = base_db::parse(db, file_id, language).syntax_node(language);
                let map = hir::hir_def::db::ast_id_map(db, file_id, language);
                hir::hir_def::java::ranges::item_range(map, &source, &tree, item)
            })
        }) else {
            continue;
        };
        sink.push(
            file_id,
            make_diagnostic(
                file_id,
                &decl_message(db, &diagnostic),
                range,
                Some(decl_code(&diagnostic)),
                // A raw-type or deprecation declaration report is a warning
                // ([JLS §4.12.2], [§9.6.4.6]): a legal program, flagged for
                // its unsoundness or its use of a deprecated API.
                lint::severity_of_decl(&diagnostic),
            ),
        );
    }
    // §7.7: the module-directive checks (`requires` of an unknown module,
    // `exports`/`opens` of an empty package, a `provides` implementation not
    // a subtype of its service) of a `module-info.java`. Every module
    // diagnostic carries its own range.
    for diagnostic in hir_ty::module_diagnostics(db, file_id) {
        if !lint::keeps_decl_diagnostic(db, file_id, &diagnostic) {
            continue;
        }
        let Some(range) = diagnostic.range() else {
            continue;
        };
        sink.push(
            file_id,
            make_diagnostic(
                file_id,
                &decl_message(db, &diagnostic),
                range,
                Some(decl_code(&diagnostic)),
                Severity::Error,
            ),
        );
    }
    // A construct newer than the file's project source level (e.g. a record in
    // a `-source 11` module). Always an error: javac rejects it too.
    for diagnostic in hir_ty::level_diagnostics(db, file_id) {
        if !lint::keeps_decl_diagnostic(db, file_id, &diagnostic) {
            continue;
        }
        let Some(range) = diagnostic.range() else {
            continue;
        };
        sink.push(
            file_id,
            make_diagnostic(
                file_id,
                &decl_message(db, &diagnostic),
                range,
                Some(decl_code(&diagnostic)),
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
) -> Arc<[Diagnostic]> {
    let file_id = *file.file_id(db);
    let mut sink = DiagnosticSink::new();
    collect_type_diagnostics(&mut sink, db, file_id);
    collect_declaration_diagnostics(&mut sink, db, file_id);
    Arc::from(sink.into_file(file_id))
}

/// The merged type + declaration diagnostics of a file: every diagnostic the
/// checks produced except the warnings an enclosing `@SuppressWarnings` names
/// ([JLS §9.6.4.5]).
pub fn file_diagnostics(db: &dyn hir_ty::TyDatabase, file_id: FileId) -> Arc<[Diagnostic]> {
    file_diagnostics_query(db, db.file_text(file_id))
}

/// The complete report of a file — syntax plus merged type and declaration
/// diagnostics — memoized per [`FileText`]. Because the underlying sub-queries
/// ([`base_db::parse`], [`file_diagnostics_query`]) are individually memoized
/// and keyed on the file's own inputs, re-deriving an *unaffected* file's
/// report here is an O(1) cache hit returning the same `Arc`, not a re-walk of
/// the item tree: a text edit to another file invalidates only the queries
/// whose inputs actually changed. See [`file_report`].
///
/// `lru = 4096` bounds the retained reports to the most-recently-used files
/// (rust-analyzer-style: evicted at the next revision, recomputed on demand
/// from the still-memoized sub-queries).
#[salsa::tracked(returns(clone), lru = 4096)]
#[tracing::instrument(skip_all, level = "debug")]
pub(crate) fn file_report_query(db: &dyn hir_ty::TyDatabase, file: FileText) -> Arc<[Diagnostic]> {
    let file_id = *file.file_id(db);
    // A library source file is read-only third-party code: neither its syntax
    // nor its type diagnostics are the user's to fix. The tracked
    // file→source-root read inside `library_source_for_file` also keeps this
    // answer re-deriving once the workspace graph exists.
    if hir::library_source_for_file(db, file_id).is_some() {
        return Arc::from(Vec::new());
    }
    let mut sink = DiagnosticSink::new();
    collect_syntax(&mut sink, db, file_id);
    for diagnostic in file_diagnostics_query(db, file).iter() {
        sink.push(file_id, diagnostic.clone());
    }
    Arc::from(sink.into_file(file_id))
}

/// The complete report of a file: its syntax diagnostics plus its merged type
/// and declaration diagnostics ([JLS §9.6.4.5] governing the suppressed
/// warnings only). This is the unit the LSP diagnostics store tracks and diffs
/// per file. Memoized per [`FileText`] by [`file_report_query`].
pub fn file_report(db: &dyn hir_ty::TyDatabase, file_id: FileId) -> Arc<[Diagnostic]> {
    file_report_query(db, db.file_text(file_id))
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
        // A local class-like declaration ([JLS §14.3]) is not a member, so it
        // is not in any `body()`: it and its members are walked from the
        // declaration whose body declares it, so their bodies are inferred
        // and their diagnostics reported too.
        for local in tree.local_types_of(id) {
            walk(tree, local, out);
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
        related_information: Vec::new(),
    }
}

impl Diagnostic {
    /// Replaces the diagnostic's `related_information` with `related`.
    fn with_related(mut self, related: impl IntoIterator<Item = RelatedInformation>) -> Self {
        self.related_information = related.into_iter().collect();
        self
    }
}
