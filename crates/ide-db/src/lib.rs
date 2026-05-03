use base_db::{Files, LanguageId, Nonce, SourceDatabase, path_resolver::PathResolver};
use hir::HirDatabase;
use parking_lot::{RwLock, RwLockReadGuard};
use std::sync::Arc;

pub mod handlers;

#[salsa::db]
#[derive(Clone)]
pub struct RootDatabase {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
    vfs: Arc<RwLock<vfs::Vfs>>,
    path_resolver: Arc<PathResolver>,
    nonce: Nonce,
}

impl RootDatabase {
    pub fn new(vfs: Arc<RwLock<vfs::Vfs>>, path_resolver: Arc<PathResolver>) -> Self {
        Self {
            storage: salsa::Storage::new(None),
            files: Default::default(),
            nonce: Nonce::new(),
            path_resolver,
            vfs,
        }
    }

    pub fn read_vfs(&self) -> RwLockReadGuard<'_, vfs::Vfs> {
        self.vfs.read()
    }
}

#[salsa::db]
impl salsa::Database for RootDatabase {}

#[salsa::db]
impl SourceDatabase for RootDatabase {
    fn file_text(&self, file_id: vfs::FileId) -> base_db::FileText {
        self.files.file_text(file_id)
    }

    fn set_file(&mut self, file_id: vfs::FileId, text: &str, language: LanguageId) {
        let files = self.files.clone();
        files.set_file(self, file_id, text, language);
    }

    fn set_file_with_durability(
        &mut self,
        file_id: vfs::FileId,
        text: &str,
        language: LanguageId,
        durability: salsa::Durability,
    ) {
        let files = self.files.clone();
        files.set_file_with_durability(self, file_id, text, language, durability);
    }

    fn read_file_bytes(&self, file_id: vfs::FileId) -> std::io::Result<Vec<u8>> {
        let vfs = self.read_vfs();
        let file_path = vfs.file_path(file_id).clone();
        drop(vfs);

        self.path_resolver.resolve(&file_path)
    }

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision) {
        (
            self.nonce,
            salsa::plumbing::ZalsaDatabase::zalsa(self).current_revision(),
        )
    }
}

#[salsa::db]
impl HirDatabase for RootDatabase {}
