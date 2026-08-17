use std::fmt;

pub use ::line_index;
pub use base_db;
use hir::hir_expand::files::FileRangeWrapper;

use base_db::{
    DepsMap, FileSourceRootInput, FileText, Files, LanguageKind, Nonce, SourceDatabase, SourceRoot,
    SourceRootId, SourceRootInput,
};
use hir::{HirDatabase, HirState};
use line_index::LineIndex;
use salsa::Durability;
use triomphe::Arc;
use vfs::FileId;

pub type FileRange = FileRangeWrapper<FileId>;

#[salsa::db]
pub struct RootDatabase {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
    deps_map: Arc<DepsMap>,
    nonce: Nonce,
    hir_state: Arc<HirState>,
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
            hir_state: Default::default(),
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
            hir_state: self.hir_state.clone(),
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

    fn file_language_kind(&self, file_id: vfs::FileId) -> Option<LanguageKind> {
        self.files.file_language_kind(self, file_id)
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

#[salsa::db]
impl HirDatabase for RootDatabase {
    fn hir_state(&self) -> &HirState {
        &self.hir_state
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

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
    WeakWarning,
    Allow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hir::LibraryKind;
    use std::io::Write as _;

    /// A minimal, hand-encoded `com/example/Greeter` class file with methods
    /// `<init>` and `greet`, and a field `name`. Hand-encoding avoids
    /// depending on the (currently incomplete) rust-asm writer.
    fn greeter_class_bytes() -> Vec<u8> {
        fn utf8(bytes: &mut Vec<u8>, s: &str) {
            bytes.push(1);
            bytes.extend_from_slice(&(s.len() as u16).to_be_bytes());
            bytes.extend_from_slice(s.as_bytes());
        }
        fn class_ref(bytes: &mut Vec<u8>, idx: u16) {
            bytes.push(7);
            bytes.extend_from_slice(&idx.to_be_bytes());
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&52u16.to_be_bytes());
        bytes.extend_from_slice(&11u16.to_be_bytes());
        utf8(&mut bytes, "com/example/Greeter");
        class_ref(&mut bytes, 1);
        utf8(&mut bytes, "java/lang/Object");
        class_ref(&mut bytes, 3);
        utf8(&mut bytes, "<init>");
        utf8(&mut bytes, "()V");
        utf8(&mut bytes, "greet");
        utf8(&mut bytes, "()Ljava/lang/String;");
        utf8(&mut bytes, "name");
        utf8(&mut bytes, "Ljava/lang/String;");
        bytes.extend_from_slice(&0x0021u16.to_be_bytes());
        bytes.extend_from_slice(&2u16.to_be_bytes());
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&0x0001u16.to_be_bytes());
        bytes.extend_from_slice(&9u16.to_be_bytes());
        bytes.extend_from_slice(&10u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&2u16.to_be_bytes());
        bytes.extend_from_slice(&0x0001u16.to_be_bytes());
        bytes.extend_from_slice(&5u16.to_be_bytes());
        bytes.extend_from_slice(&6u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0x0001u16.to_be_bytes());
        bytes.extend_from_slice(&7u16.to_be_bytes());
        bytes.extend_from_slice(&8u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes
    }

    fn test_jar() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        use zip::write::{SimpleFileOptions, ZipWriter};
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.jar");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("com/example/Greeter.class", options)
            .unwrap();
        zip.write_all(&greeter_class_bytes()).unwrap();
        zip.finish().unwrap();
        (dir, camino::Utf8PathBuf::from_path_buf(path).unwrap())
    }

    #[test]
    fn stub_index_end_to_end() {
        let (_dir, jar_path) = test_jar();
        let mut db = RootDatabase::new();

        let library = hir::LibraryId(0x1234);
        hir::register_library(&mut db, library, LibraryKind::Jar, jar_path.as_str().into());

        // Warmup loads the index outside of salsa and populates the per-library
        // cache; a subsequent query must return it without re-parsing.
        hir::warmup_library(&db, library);
        assert!(hir::library_name_index(&db, library).class_count() > 0);

        // Tier-1 lookup.
        let resolved = hir::fqn_resolve(&db, "com.example.Greeter").unwrap();
        assert_eq!(resolved.library, library);
        assert_eq!(
            resolved.entry.super_class,
            Some(db.hir_state().interner.get_or_intern("java.lang.Object"))
        );

        let supers = hir::super_types(&db, &resolved);
        assert_eq!(supers.len(), 1);

        // Tier-2: full member stubs.
        let record = hir::class_record(&db, &resolved).unwrap();
        let hir::stubs::ClassOrModuleStub::Class(class) = record.as_ref() else {
            panic!("expected a class record");
        };
        assert_eq!(class.methods.len(), 2);
        assert_eq!(class.fields.len(), 1);

        // Unknown fqn resolves to nothing.
        assert!(hir::fqn_resolve(&db, "com.example.Missing").is_none());
    }
}
