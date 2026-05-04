use base_db::{Files, LanguageId, Nonce, SourceDatabase};
use hir::HirDatabase;
use std::sync::Arc;

pub mod handlers;

#[salsa::db]
#[derive(Clone)]
pub struct RootDatabase {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
    nonce: Nonce,
}

impl Default for RootDatabase {
    fn default() -> Self {
        Self::new()
    }
}

impl RootDatabase {
    pub fn new() -> Self {
        Self {
            storage: salsa::Storage::new(None),
            files: Default::default(),
            nonce: Nonce::new(),
        }
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

    fn remove_file(&mut self, file_id: vfs::FileId) {
        let files = self.files.clone();
        files.remove_file(self, file_id);
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
