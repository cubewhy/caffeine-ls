//! Shared test fixtures: a minimal salsa database implementing the full
//! `SourceDatabase` → `hir::HirDatabase` trait stack plus a small class-file
//! jar builder, so integration tests can exercise the source symbol index and
//! the classpath-wired resolver end to end.

use std::{fs::File, io::Write as _, sync::Arc};

use base_db::{
    DepsMap, FileChange, FileSourceRootInput, FileText, Files, Nonce, SourceDatabase, SourceRoot,
    SourceRootId, SourceRootInput, salsa::Durability,
};
use hir::{
    ClasspathEntry, HirState, LibraryInfo, ProjectGraphData, SourceSetId, lmdb_store::StubStore,
    set_project_graph,
};
use project_model::LibraryId;
use tempfile::TempDir;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};
use zip::write::{SimpleFileOptions, ZipWriter};

/// A file of a fixture root.
pub struct RootFile {
    pub id: FileId,
    /// The virtual path of the file (used only to populate the file set).
    pub path: &'static str,
    pub text: &'static str,
}

/// A fixture source root, mapped to one source set with an ordered classpath.
pub struct Root {
    pub source_set: SourceSetId,
    pub files: Vec<RootFile>,
    pub classpath: Vec<ClasspathEntry>,
}

/// Minimal salsa database implementing [`HirDatabase`] plus the source
/// database plumbing (mirrors the `#[cfg(test)]` database in
/// `hir/src/project.rs`).
#[salsa::db]
pub struct TestDatabase {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
    deps_map: triomphe::Arc<DepsMap>,
    nonce: Nonce,
    hir_state: Arc<HirState>,
    /// Keeps the per-test stub cache directory alive.
    _stub_cache_dir: tempfile::TempDir,
}

impl TestDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a single-file edit. The file must already belong to a source
    /// root (the roots stay unchanged).
    pub fn edit_file(&mut self, file_id: FileId, text: &str) {
        let mut change = FileChange::default();
        change.change_file(file_id, Some(text.to_owned()));
        change.apply(self);
    }
}

impl Default for TestDatabase {
    fn default() -> Self {
        // Each test database gets its own throwaway LMDB environment, so
        // tier-2 record loads work without touching the user's real cache.
        let stub_cache_dir = tempfile::TempDir::new().unwrap();
        let stub_store = StubStore::default();
        stub_store.open_at(stub_cache_dir.path().to_owned());
        let hir_state = HirState {
            stub_store,
            ..HirState::default()
        };
        Self {
            storage: salsa::Storage::default(),
            files: Arc::default(),
            deps_map: triomphe::Arc::default(),
            nonce: Nonce::new(),
            hir_state: Arc::new(hir_state),
            _stub_cache_dir: stub_cache_dir,
        }
    }
}

#[salsa::db]
impl salsa::Database for TestDatabase {}

#[salsa::db]
impl SourceDatabase for TestDatabase {
    fn file_text(&self, file_id: FileId) -> FileText {
        self.files.file_text(file_id)
    }
    fn set_file_text(&mut self, file_id: FileId, text: &str) {
        let files = Arc::clone(&self.files);
        files.set_file_text(self, file_id, text);
    }
    fn set_file_text_with_durability(
        &mut self,
        file_id: FileId,
        text: &str,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_text_with_durability(self, file_id, text, durability);
    }
    fn source_root(&self, source_root_id: SourceRootId) -> SourceRootInput {
        self.files.source_root(source_root_id)
    }
    fn file_source_root(&self, id: FileId) -> FileSourceRootInput {
        self.files.file_source_root(self, id)
    }
    fn source_root_for_file(&self, file_id: FileId) -> Option<SourceRootId> {
        self.files.file_source_root_id(self, file_id)
    }
    fn set_file_source_root_with_durability(
        &mut self,
        id: FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_source_root_with_durability(self, id, source_root_id, durability);
    }
    fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: triomphe::Arc<SourceRoot>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_source_root_with_durability(self, source_root_id, source_root, durability);
    }
    fn deps_map(&self) -> triomphe::Arc<DepsMap> {
        self.deps_map.clone()
    }
    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision) {
        (
            self.nonce,
            salsa::plumbing::ZalsaDatabase::zalsa(self).current_revision(),
        )
    }
    fn line_column(&self, _file: FileId, _offset: rowan::TextSize) -> Result<(u32, u32), ()> {
        Err(())
    }
}

#[salsa::db]
impl hir::HirDatabase for TestDatabase {
    fn hir_state(&self) -> &HirState {
        &self.hir_state
    }
}

#[salsa::db]
impl hir_expand::db::DefDatabase for TestDatabase {}

#[salsa::db]
impl hir_def::db::DefDatabase for TestDatabase {}

/// Builds a database from fixture roots: every root becomes a
/// `SourceRoot` (ids assigned in order, aligned with the
/// `source_root_to_source_set` map), every source set gets its ordered
/// classpath, and `libraries` are registered in the project graph.
pub fn build(roots: &[Root], libraries: &[(LibraryId, LibraryInfo)]) -> TestDatabase {
    let mut db = TestDatabase::new();
    let mut change = FileChange::default();
    let mut all_roots = Vec::new();
    let mut data = ProjectGraphData {
        libraries: libraries.iter().cloned().collect(),
        ..ProjectGraphData::default()
    };
    for (idx, root) in roots.iter().enumerate() {
        let mut file_set = FileSet::default();
        for file in &root.files {
            file_set.insert(
                file.id,
                VfsPath::from(AbsPathBuf::assert_utf8(file.path.to_owned().into())),
            );
            change.change_file(file.id, Some(file.text.to_owned()));
        }
        all_roots.push(SourceRoot::new(file_set));
        data.source_root_to_source_set
            .insert(SourceRootId(idx as u32), root.source_set.clone());
        data.source_sets.insert(
            root.source_set.clone(),
            Arc::new(hir::Classpath {
                entries: root.classpath.clone(),
            }),
        );
    }
    change.set_roots(all_roots);
    change.apply(&mut db);
    set_project_graph(&mut db, data);
    db
}

/// A fixture helper for the default main source set of project 0.
pub fn main_source_set() -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(0),
        kind: project_model::SourceSetKind::Main,
    }
}

/// A fixture helper for the default test source set of project 0.
pub fn test_source_set() -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(0),
        kind: project_model::SourceSetKind::Test,
    }
}

/// Wraps a std path as an [`AbsPathBuf`] (panicking on non-UTF-8).
pub fn abs_path(path: &camino::Utf8PathBuf) -> AbsPathBuf {
    AbsPathBuf::assert_utf8(path.as_std_path().to_owned())
}

// -- Hand-built jar fixtures (mirror `loader::tests::greeter_class_bytes`).

/// Hand-encodes a minimal class file for `fqn` (slash-separated, e.g.
/// `com/example/Greeter`) with an `<init>` method, a `greet` method and a
/// `name` field.
pub fn class_bytes(fqn: &str) -> Vec<u8> {
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
    bytes.extend_from_slice(&0u16.to_be_bytes()); // minor version
    bytes.extend_from_slice(&52u16.to_be_bytes()); // major version
    bytes.extend_from_slice(&11u16.to_be_bytes()); // constant pool count
    utf8(&mut bytes, fqn);
    class_ref(&mut bytes, 1);
    utf8(&mut bytes, "java/lang/Object");
    class_ref(&mut bytes, 3);
    utf8(&mut bytes, "<init>");
    utf8(&mut bytes, "()V");
    utf8(&mut bytes, "greet");
    utf8(&mut bytes, "()Ljava/lang/String;");
    utf8(&mut bytes, "name");
    utf8(&mut bytes, "Ljava/lang/String;");
    bytes.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
    bytes.extend_from_slice(&2u16.to_be_bytes()); // this_class
    bytes.extend_from_slice(&4u16.to_be_bytes()); // super_class
    bytes.extend_from_slice(&0u16.to_be_bytes()); // interfaces
    bytes.extend_from_slice(&1u16.to_be_bytes()); // fields
    bytes.extend_from_slice(&0x0001u16.to_be_bytes());
    bytes.extend_from_slice(&9u16.to_be_bytes());
    bytes.extend_from_slice(&10u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes());
    bytes.extend_from_slice(&2u16.to_be_bytes()); // methods
    bytes.extend_from_slice(&0x0001u16.to_be_bytes());
    bytes.extend_from_slice(&5u16.to_be_bytes());
    bytes.extend_from_slice(&6u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes());
    bytes.extend_from_slice(&0x0001u16.to_be_bytes());
    bytes.extend_from_slice(&7u16.to_be_bytes());
    bytes.extend_from_slice(&8u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes()); // class attributes
    bytes
}

/// Builds a jar at `path` containing a single class `fqn`.
pub fn build_jar(path: &camino::Utf8Path, fqn: &str) {
    let file = File::create(path.as_std_path()).unwrap();
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    zip.start_file(format!("{fqn}.class"), options).unwrap();
    zip.write_all(&class_bytes(fqn)).unwrap();
    zip.finish().unwrap();
}

/// Creates a temp directory with a `fixture` subdirectory and returns its path.
pub fn fixture() -> (TempDir, camino::Utf8PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("fixture");
    std::fs::create_dir_all(&path).unwrap();
    (dir, camino::Utf8PathBuf::from_path_buf(path).unwrap())
}
