use std::fmt;

pub use ::line_index;
pub use base_db;

use base_db::{
    DepsMap, FileSourceRootInput, FileText, Files, Nonce, SourceDatabase, SourceRoot, SourceRootId,
    SourceRootInput,
};
use line_index::LineIndex;
use salsa::Durability;
use triomphe::Arc;
use vfs::FileId;

#[salsa::db]
pub struct RootDatabase {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
    deps_map: Arc<DepsMap>,
    nonce: Nonce,
}

impl Default for RootDatabase {
    fn default() -> RootDatabase {
        RootDatabase::new()
    }
}

impl RootDatabase {
    pub fn new() -> Self {
        Self {
            storage: salsa::Storage::default(),
            files: Default::default(),
            deps_map: Default::default(),
            nonce: Nonce::new(),
        }
    }
}

#[salsa::db]
impl salsa::Database for RootDatabase {}

impl Clone for RootDatabase {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            files: self.files.clone(),
            deps_map: self.deps_map.clone(),
            nonce: self.nonce,
        }
    }
}

impl fmt::Debug for RootDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootDatabase").finish()
    }
}

#[salsa::db]
impl SourceDatabase for RootDatabase {
    fn file_text(&self, file_id: vfs::FileId) -> FileText {
        self.files.file_text(file_id)
    }

    fn set_file_text(&mut self, file_id: vfs::FileId, text: &str) {
        let files = Arc::clone(&self.files);
        files.set_file_text(self, file_id, text);
    }

    fn set_file_text_with_durability(
        &mut self,
        file_id: vfs::FileId,
        text: &str,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_text_with_durability(self, file_id, text, durability);
    }

    /// Source root of the file.
    fn source_root(&self, source_root_id: SourceRootId) -> SourceRootInput {
        self.files.source_root(source_root_id)
    }

    fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_source_root_with_durability(self, source_root_id, source_root, durability);
    }

    fn file_source_root(&self, id: vfs::FileId) -> FileSourceRootInput {
        self.files.file_source_root(self, id)
    }

    fn set_file_source_root_with_durability(
        &mut self,
        id: vfs::FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_source_root_with_durability(self, id, source_root_id, durability);
    }

    fn deps_map(&self) -> Arc<DepsMap> {
        self.deps_map.clone()
    }

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision) {
        (
            self.nonce,
            salsa::plumbing::ZalsaDatabase::zalsa(self).current_revision(),
        )
    }

    fn line_column(&self, file: FileId, offset: rowan::TextSize) -> Result<(u32, u32), ()> {
        line_index(self, file)
            .try_line_col(offset)
            .map(|lc| (lc.line, lc.col))
            .ok_or(())
    }
}

pub fn line_index(db: &dyn SourceDatabase, file_id: FileId) -> &Arc<LineIndex> {
    #[salsa::interned]
    pub struct InternedFileId {
        #[returns(copy)]
        id: FileId,
    }
    #[salsa::tracked(returns(ref))]
    fn line_index<'db>(
        db: &'db dyn SourceDatabase,
        file_id: InternedFileId<'db>,
    ) -> Arc<LineIndex> {
        let text = db.file_text(file_id.id(db)).text(db);
        Arc::new(LineIndex::new(text))
    }
    line_index(db, InternedFileId::new(db, file_id))
}
