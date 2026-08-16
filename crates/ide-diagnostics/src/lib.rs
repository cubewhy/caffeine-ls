use ide_db::{
    FileRange, RootDatabase, Severity,
    base_db::{LanguageKind, SourceDatabase},
};
use vfs::FileId;

#[derive(Debug)]
pub struct Diagnostic {
    pub message: String,
    pub range: FileRange,
    pub severity: Severity,
    pub unused: bool,
}

pub fn syntax_diagnostics(db: &RootDatabase, file_id: FileId) -> Vec<Diagnostic> {
    // TODO: caching, kotlin support
    let Some(language_kind) = db
        .file_language_kind(file_id)
        .filter(|&kind| kind != LanguageKind::Unknown)
    else {
        tracing::warn!("unsupported language");
        // unsupported language
        return vec![];
    };
    let file_text = db.file_text(file_id);
    let text = file_text.text(db);
    let parse = match language_kind {
        LanguageKind::Java => syntax::java::SourceFile::parse(text),
        _ => return vec![],
    };
    let (_green, errors) = parse.into();

    errors
        .into_iter()
        .map(|e| to_diagnostic(file_id, e))
        .collect()
}

fn to_diagnostic(file_id: FileId, syntax_err: syntax::java::SyntaxError) -> Diagnostic {
    let range = FileRange::new(file_id, syntax_err.range);
    Diagnostic {
        message: "WIP".to_string(),
        range,
        severity: Severity::Error,
        unused: false,
    }
}
