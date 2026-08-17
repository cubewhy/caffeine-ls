use ide_db::{
    FileRange, RootDatabase, Severity,
    base_db::{LanguageKind, SourceDatabase},
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
    // TODO: caching
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
    let file_text = db.file_text(file_id);
    let text = file_text.text(db);
    match language_kind {
        LanguageKind::Java => {
            let parse = syntax::java::SourceFile::parse(text);
            let (_green, errors) = parse.into();
            errors
                .into_iter()
                .map(|e| make_diagnostic(file_id, e.kind.desc(), e.range))
                .collect()
        }
        LanguageKind::Kotlin => {
            let parse = syntax::kotlin::SourceFile::parse(text);
            let (_green, errors) = parse.into();
            errors
                .into_iter()
                .map(|e| make_diagnostic(file_id, e.kind.desc(), e.range))
                .collect()
        }
        LanguageKind::Unknown => unreachable!("checked above"),
    }
}

fn make_diagnostic(file_id: FileId, message: String, range: TextRange) -> Diagnostic {
    let range = FileRange::new(file_id, range);
    Diagnostic {
        message,
        range,
        severity: Severity::Error,
        unused: false,
    }
}
