use ide_db::{FileRange, RootDatabase, Severity};
use vfs::FileId;

#[derive(Debug)]
pub struct Diagnostic {
    pub message: String,
    pub range: FileRange,
    pub severity: Severity,
    pub unused: bool,
}

pub fn syntax_diagnostics(db: &RootDatabase, file_id: FileId) -> Vec<Diagnostic> {
    // TODO: implement syntax_diagnostics
    vec![]
}
