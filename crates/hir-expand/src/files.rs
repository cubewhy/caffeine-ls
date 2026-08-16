use rowan::TextRange;

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct FileRangeWrapper<FileKind> {
    pub file_id: FileKind,
    pub range: TextRange,
}

impl<FileKind> FileRangeWrapper<FileKind> {
    pub fn new(file_id: FileKind, range: TextRange) -> Self {
        Self { file_id, range }
    }
}
