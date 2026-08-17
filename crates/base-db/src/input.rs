use vfs::{AnchoredPath, FileId, VfsPath, file_set::FileSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceRootId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRoot {
    file_set: FileSet,
}

impl SourceRoot {
    pub fn new(file_set: FileSet) -> SourceRoot {
        SourceRoot { file_set }
    }

    pub fn path_for_file(&self, file: &FileId) -> Option<&VfsPath> {
        self.file_set.path_for_file(file)
    }

    pub fn file_for_path(&self, path: &VfsPath) -> Option<&FileId> {
        self.file_set.file_for_path(path)
    }

    pub fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
        self.file_set.resolve_path(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = FileId> + '_ {
        self.file_set.iter()
    }
}

#[derive(Debug, Default)]
pub struct DepsMap {}
