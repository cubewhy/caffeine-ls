mod input;

use std::{hash::BuildHasherDefault, sync::atomic::AtomicUsize};

use dashmap::{DashMap, Entry};
use rowan::GreenNode;
use rustc_hash::FxHasher;
use salsa::{Durability, Setter};
use triomphe::Arc;

use crate::input::{ParseResult, parse_node};

#[derive(Debug, Clone)]
pub enum LanguageId {
    Java,
    Kotlin,
    Unknown,
}

impl LanguageId {
    pub fn from_extension(ext: &str) -> Self {
        match ext {
            "java" => Self::Java,
            "kt" | "kts" => Self::Kotlin,
            _ => Self::Unknown,
        }
    }
}

#[salsa::input(debug)]
pub struct FileText {
    #[returns(ref)]
    pub text: Arc<str>,
    pub language: LanguageId,
    pub file_id: vfs::FileId,
}

#[salsa::input(debug)]
pub struct SourceTree {
    pub green_node: GreenNode,
    pub file_id: vfs::FileId,
}

#[salsa::db]
pub trait SourceDatabase: salsa::Database {
    /// Text of the file.
    fn file_text(&self, file_id: vfs::FileId) -> FileText;

    fn set_file(&mut self, file_id: vfs::FileId, text: &str, language: LanguageId);

    fn set_file_with_durability(
        &mut self,
        file_id: vfs::FileId,
        text: &str,
        language: LanguageId,
        durability: Durability,
    );

    /// GreenNode of the file
    fn green_node(&self, file_id: vfs::FileId) -> Option<ParseResult<'_>>
    where
        Self: std::marker::Sized,
    {
        let file_text = self.file_text(file_id);
        parse_node(self, file_text)
    }

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision);
}

static NEXT_NONCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce(usize);

impl Default for Nonce {
    #[inline]
    fn default() -> Self {
        Nonce::new()
    }
}

impl Nonce {
    #[inline]
    pub fn new() -> Nonce {
        Nonce(NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }
}

pub struct Files {
    files: Arc<DashMap<vfs::FileId, FileText, BuildHasherDefault<FxHasher>>>,
}

impl Files {
    pub fn file_text(&self, file_id: vfs::FileId) -> FileText {
        match self.files.get(&file_id) {
            Some(text) => *text,
            None => {
                panic!("Unable to fetch file text for `vfs::FileId`: {file_id:?}; this is a bug")
            }
        }
    }

    pub fn set_file(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: vfs::FileId,
        text: &str,
        language: LanguageId,
    ) {
        match self.files.entry(file_id) {
            Entry::Occupied(mut occupied) => {
                let occupied = occupied.get_mut();
                occupied.set_text(db).to(Arc::from(text));
                occupied.set_language(db).to(language);
            }
            Entry::Vacant(vacant) => {
                let text = FileText::new(db, Arc::from(text), language, file_id);
                vacant.insert(text);
            }
        };
    }

    pub fn set_file_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: vfs::FileId,
        text: &str,
        language: LanguageId,
        durability: salsa::Durability,
    ) {
        match self.files.entry(file_id) {
            Entry::Occupied(mut occupied) => {
                let occupied = occupied.get_mut();
                occupied
                    .set_text(db)
                    .with_durability(durability)
                    .to(Arc::from(text));
                occupied
                    .set_language(db)
                    .with_durability(durability)
                    .to(language);
            }
            Entry::Vacant(vacant) => {
                let text = FileText::builder(Arc::from(text), language, file_id)
                    .durability(durability)
                    .new(db);
                vacant.insert(text);
            }
        };
    }
}

impl Default for Files {
    fn default() -> Self {
        Self {
            files: Arc::new(DashMap::default()),
        }
    }
}
