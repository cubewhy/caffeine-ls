use ide_db::{
    FileRange, RootDatabase, Severity,
    base_db::{self, LanguageKind, SourceDatabase},
};
use rowan::TextRange;
use vfs::FileId;

#[derive(Debug)]
pub struct Diagnostic {
    pub message: String,
    pub range: FileRange,
    pub severity: Severity,
    pub unused: bool,
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
        .map(|e| make_diagnostic(file_id, &e.message, e.range))
        .collect()
}

fn make_diagnostic(file_id: FileId, message: &str, range: TextRange) -> Diagnostic {
    let range = FileRange::new(file_id, range);
    Diagnostic {
        message: message.to_string(),
        range,
        severity: Severity::Error,
        unused: false,
    }
}
