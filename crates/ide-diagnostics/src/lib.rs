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

pub fn syntax_diagnostics(
    db: &RootDatabase,
    file_id: FileId,
    fallback_language_kind: LanguageKind,
) -> Vec<Diagnostic> {
    // TODO: caching, kotlin support
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
        message: syntax_err.kind.desc(),
        range,
        severity: Severity::Error,
        unused: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_diagnostics_work_without_source_roots() {
        let mut db = RootDatabase::new();
        let file_id = vfs::FileId::from_raw(0);
        db.set_file_text(
            file_id,
            "public class Main {\n    public void m() {\n        int a = 1\n    }\n}",
        );

        let with_fallback = syntax_diagnostics(&db, file_id, LanguageKind::Java);
        assert!(!with_fallback.is_empty());

        let without_fallback = syntax_diagnostics(&db, file_id, LanguageKind::Unknown);
        assert!(without_fallback.is_empty());
    }
}
