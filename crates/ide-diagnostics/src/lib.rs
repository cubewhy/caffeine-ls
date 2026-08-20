use hir::hir_expand::item_tree::{ItemData, ItemId, ItemTree};
use ide_db::{
    FileRange, RootDatabase, Severity,
    base_db::{self, LanguageKind, SourceDatabase},
};
use rowan::TextRange;
use syntax::DiagnosticCode;
use vfs::FileId;

#[derive(Debug)]
pub struct Diagnostic {
    pub message: String,
    pub range: FileRange,
    pub severity: Severity,
    pub unused: bool,
    /// The stable diagnostic code, when the underlying error kind carries one
    /// (see [`syntax::DiagnosticCode`]); surfaces as the LSP `code` field.
    pub code: Option<DiagnosticCode>,
}

pub fn syntax_diagnostics(
    db: &RootDatabase,
    file_id: FileId,
    fallback_language_kind: LanguageKind,
) -> Vec<Diagnostic> {
    // Before the workspace is loaded the file is not part of any source root,
    // so `file_language_kind` can't resolve the language. Fall back to the
    // kind inferred from the file path to keep basic syntax diagnostics.
    let language_kind = db
        .file_language_kind(file_id)
        .filter(|&kind| kind != LanguageKind::Unknown)
        .unwrap_or(fallback_language_kind);
    if language_kind == LanguageKind::Unknown {
        tracing::warn!("unsupported language");
        return vec![];
    }

    let parse = base_db::parse(db, file_id, language_kind);
    parse
        .errors()
        .iter()
        .map(|e| make_diagnostic(file_id, &e.message, e.range, e.code))
        .collect()
}

/// The type-layer diagnostics of a file ([JLS §6.5], [§14.18], [§15.11],
/// [§15.12]): the `TypeError`s reported while inferring the body of every
/// method, constructor, initializer, field initializer and enum constant
/// argument in the file (see [`hir_ty::body_types`]). Each diagnostic's range
/// is the source range of the offending construct, computed from its body-IR
/// arena id.
pub fn type_diagnostics(db: &RootDatabase, file_id: FileId) -> Vec<Diagnostic> {
    let tree = hir::file_item_tree(db, file_id);
    let mut out = Vec::new();
    for (item_id, _) in all_items(&tree) {
        let Some(body_types) = hir_ty::body_types(db, file_id, item_id) else {
            continue;
        };
        for diagnostic in &body_types.diagnostics {
            let Some(range) = diagnostic.range(&tree.bodies) else {
                // A synthetic construct (e.g. a `Missing` expression lowered
                // from broken source) has no range to point at.
                continue;
            };
            out.push(make_diagnostic(
                file_id,
                &diagnostic.message(&tree.bodies),
                range,
                Some(diagnostic.code()),
            ));
        }
    }
    out
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
) -> Diagnostic {
    let range = FileRange::new(file_id, range);
    Diagnostic {
        message: message.to_string(),
        range,
        severity: Severity::Error,
        unused: false,
        code,
    }
}
