//! Classpath-aware project model for the resolver.
//!
//! The build-tool layer (`project_model`) produces a [`project_model::WorkspaceGraph`]
//! where every source set carries a flattened, ordered classpath. This module
//! turns that into the DB-facing shapes the HIR resolver searches against:
//! a [`SourceSetId`] (compilation unit), an ordered [`Classpath`], and the
//! per-library [`LibraryInfo`] the stub index is keyed by.

use triomphe::Arc;

use project_model::{LibraryId, ProjectId, SourceSetKind};
use rustc_hash::FxHashMap;
use vfs::AbsPathBuf;

use crate::db::LibraryKind;

/// A compilation unit: one project's one source set (the analog of a Gradle
/// `SourceSet`, a Maven scope, or an IntelliJ module). Every resolve/IDE
/// feature hangs off this id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceSetId {
    pub project: ProjectId,
    pub kind: SourceSetKind,
}

impl std::fmt::Display for SourceSetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.project.0, self.kind)
    }
}

/// An ordered classpath ready for name lookup.
///
/// The build tool has already flattened transitive dependencies, so the
/// entries are deterministic. Order is significant: earlier entries shadow
/// later ones for FQN resolution (javac classpath semantics).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classpath {
    pub entries: Vec<ClasspathEntry>,
}

impl Classpath {
    /// The libraries of this classpath, in classpath order. Internal source
    /// set entries are resolved through the source index (see
    /// [`crate::db::fqn_resolve`]) and are not returned here.
    pub fn libraries(&self) -> impl Iterator<Item = LibraryId> + '_ {
        self.entries.iter().filter_map(|entry| match entry {
            ClasspathEntry::Library(library) => Some(*library),
            ClasspathEntry::SourceSet(_) => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClasspathEntry {
    /// An external jar or a JDK jimage, backed by the stub index.
    Library(LibraryId),
    /// Another workspace source set (an internal project dependency). Resolved
    /// through the source index in classpath order (see
    /// [`crate::db::fqn_resolve`]).
    SourceSet(SourceSetId),
}

/// Metadata of a library registered in the stub index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryInfo {
    pub kind: LibraryKind,
    pub path: AbsPathBuf,
}

impl LibraryInfo {
    pub fn new(kind: LibraryKind, path: AbsPathBuf) -> Self {
        Self { kind, path }
    }
}

/// The DB-facing project model: everything the resolver needs about the
/// workspace. See [`crate::db::ProjectGraph`] for the salsa input.
#[derive(Debug, Clone, Default)]
pub struct ProjectGraphData {
    pub libraries: FxHashMap<LibraryId, LibraryInfo>,
    pub source_sets: FxHashMap<SourceSetId, Arc<Classpath>>,
    pub source_root_to_source_set: FxHashMap<base_db::SourceRootId, SourceSetId>,
    /// build-system source root → its resolved base directory (the directory
    /// a classpath looks the root's packages up under), for the package-path
    /// diagnostic ([JLS §7.2.1]). One entry per `SourceRoot`, aligned with
    /// [`Self::source_root_to_source_set`].
    pub source_root_dirs: FxHashMap<base_db::SourceRootId, AbsPathBuf>,
    /// JDK built-in libraries (jimage / rt.jar), in registration order.
    pub jdk_libraries: Vec<LibraryId>,
}

#[cfg(test)]
mod tests {
    use std::{fs::File, io::Write as _};
    use triomphe::Arc;

    use base_db::{
        DepsMap, FileChange, FileSourceRootInput, FileText, Files, Nonce, SourceDatabase,
        SourceRoot, SourceRootId, SourceRootInput, salsa::Durability,
    };
    use tempfile::TempDir;
    use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};
    use zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;
    use crate::db::{
        ResolutionScope, Resolved, file_item_tree, fqn_resolve, set_project_graph,
        source_set_for_file,
    };
    use crate::{HirDatabase, HirState, JavaDatabase, JvmDatabase, KotlinDatabase, LibraryKind};

    /// Minimal salsa database implementing [`HirDatabase`] plus the source
    /// database plumbing, so the classpath model can be exercised end to end.
    #[salsa::db]
    struct TestDatabase {
        storage: salsa::Storage<Self>,
        files: Arc<Files>,
        deps_map: triomphe::Arc<DepsMap>,
        nonce: Nonce,
        hir_state: Arc<HirState>,
    }

    impl TestDatabase {
        fn new() -> Self {
            Self {
                storage: salsa::Storage::default(),
                files: Arc::default(),
                deps_map: triomphe::Arc::default(),
                nonce: Nonce::new(),
                hir_state: Arc::default(),
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
    impl JvmDatabase for TestDatabase {
        fn hir_state(&self) -> &HirState {
            &self.hir_state
        }
    }

    #[salsa::db]
    impl JavaDatabase for TestDatabase {}

    #[salsa::db]
    impl KotlinDatabase for TestDatabase {}

    #[salsa::db]
    impl HirDatabase for TestDatabase {}

    #[salsa::db]
    impl hir_expand::db::DefDatabase for TestDatabase {}

    #[salsa::db]
    impl hir_def::jvm::db::JvmDatabase for TestDatabase {}

    #[salsa::db]
    impl hir_def::java::db::JavaDatabase for TestDatabase {}

    #[salsa::db]
    impl hir_def::kotlin::db::KotlinDatabase for TestDatabase {}

    #[salsa::db]
    impl hir_def::db::DefDatabase for TestDatabase {}

    /// Hand-encodes a minimal class file for `fqn` (slash-separated, e.g.
    /// `com/example/Greeter`) with an `<init>` method, a `greet` method and a
    /// `name` field, mirroring `loader::tests::greeter_class_bytes`.
    fn class_bytes(fqn: &str) -> Vec<u8> {
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
    fn build_jar(path: &camino::Utf8Path, fqn: &str) {
        let file = File::create(path.as_std_path()).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file(format!("{fqn}.class"), options).unwrap();
        zip.write_all(&class_bytes(fqn)).unwrap();
        zip.finish().unwrap();
    }

    fn fixture() -> (TempDir, camino::Utf8PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fixture");
        std::fs::create_dir_all(&path).unwrap();
        (dir, camino::Utf8PathBuf::from_path_buf(path).unwrap())
    }

    fn abs_path(path: &camino::Utf8PathBuf) -> AbsPathBuf {
        AbsPathBuf::assert_utf8(path.as_std_path().to_owned())
    }

    fn main_source_set(project: ProjectId) -> SourceSetId {
        SourceSetId {
            project,
            kind: SourceSetKind::Main,
        }
    }

    #[test]
    fn classpath_ordering_determines_shadowing() {
        let (_dir, path) = fixture();
        let jar_a = path.join("a.jar");
        let jar_b = path.join("b.jar");
        build_jar(&jar_a, "com/example/Greeter");
        build_jar(&jar_b, "com/example/Greeter");

        let lib_a = LibraryId::from_file_path(jar_a.as_std_path()).unwrap();
        let lib_b = LibraryId::from_file_path(jar_b.as_std_path()).unwrap();

        let mut db = TestDatabase::new();
        let mut data = ProjectGraphData::default();
        data.libraries
            .insert(lib_a, LibraryInfo::new(LibraryKind::Jar, abs_path(&jar_a)));
        data.libraries
            .insert(lib_b, LibraryInfo::new(LibraryKind::Jar, abs_path(&jar_b)));
        set_project_graph(&mut db, data);

        let a_first = fqn_resolve(
            &db,
            &ResolutionScope::Classpath(vec![lib_a, lib_b]),
            "com.example.Greeter",
        );
        let b_first = fqn_resolve(
            &db,
            &ResolutionScope::Classpath(vec![lib_b, lib_a]),
            "com.example.Greeter",
        );

        let library_of = |resolved: &Resolved| match resolved {
            Resolved::Library(r) => Some(r.library),
            Resolved::Source(_) => None,
        };
        assert_eq!(a_first.as_ref().and_then(library_of), Some(lib_a));
        assert_eq!(b_first.as_ref().and_then(library_of), Some(lib_b));
        assert!(
            fqn_resolve(
                &db,
                &ResolutionScope::Classpath(Vec::new()),
                "com.example.Greeter"
            )
            .is_none()
        );
    }

    #[test]
    fn source_set_classpath_is_scoped_and_skips_internal_entries() {
        let (_dir, path) = fixture();
        let jar_a = path.join("a.jar");
        let jar_b = path.join("b.jar");
        build_jar(&jar_a, "com/example/Greeter");
        build_jar(&jar_b, "com/example/Other");

        let lib_a = LibraryId::from_file_path(jar_a.as_std_path()).unwrap();
        let lib_b = LibraryId::from_file_path(jar_b.as_std_path()).unwrap();
        let ss_main = main_source_set(ProjectId(0));
        let ss_test = main_source_set(ProjectId(1));

        let mut db = TestDatabase::new();
        let mut data = ProjectGraphData::default();
        data.libraries
            .insert(lib_a, LibraryInfo::new(LibraryKind::Jar, abs_path(&jar_a)));
        data.libraries
            .insert(lib_b, LibraryInfo::new(LibraryKind::Jar, abs_path(&jar_b)));
        data.source_sets.insert(
            ss_main.clone(),
            Arc::new(Classpath {
                entries: vec![ClasspathEntry::Library(lib_a)],
            }),
        );
        data.source_sets.insert(
            ss_test.clone(),
            Arc::new(Classpath {
                entries: vec![
                    ClasspathEntry::SourceSet(ss_main.clone()),
                    ClasspathEntry::Library(lib_b),
                ],
            }),
        );
        set_project_graph(&mut db, data);

        // ss_main sees its own library, but not ss_test's.
        assert!(
            fqn_resolve(
                &db,
                &ResolutionScope::SourceSet(ss_main.clone()),
                "com.example.Greeter"
            )
            .is_some()
        );
        assert!(
            fqn_resolve(
                &db,
                &ResolutionScope::SourceSet(ss_main.clone()),
                "com.example.Other"
            )
            .is_none()
        );
        // ss_test sees its own library; internal source-set entries are skipped.
        assert!(
            fqn_resolve(
                &db,
                &ResolutionScope::SourceSet(ss_test.clone()),
                "com.example.Other"
            )
            .is_some()
        );
        assert!(
            fqn_resolve(
                &db,
                &ResolutionScope::SourceSet(ss_test.clone()),
                "com.example.Greeter"
            )
            .is_none()
        );
    }

    #[test]
    fn jdk_builtins_scope_only_sees_jdk_libraries() {
        let (_dir, path) = fixture();
        let jar_app = path.join("app.jar");
        let jar_jdk = path.join("jdk.jar");
        build_jar(&jar_app, "com/example/App");
        build_jar(&jar_jdk, "java/lang/Object");

        let lib_app = LibraryId::from_file_path(jar_app.as_std_path()).unwrap();
        let lib_jdk = LibraryId::from_file_path(jar_jdk.as_std_path()).unwrap();

        let mut db = TestDatabase::new();
        let mut data = ProjectGraphData::default();
        data.libraries.insert(
            lib_app,
            LibraryInfo::new(LibraryKind::Jar, abs_path(&jar_app)),
        );
        data.libraries.insert(
            lib_jdk,
            LibraryInfo::new(LibraryKind::Jar, abs_path(&jar_jdk)),
        );
        data.jdk_libraries.push(lib_jdk);
        set_project_graph(&mut db, data);

        assert!(fqn_resolve(&db, &ResolutionScope::JdkBuiltins, "java.lang.Object").is_some());
        assert!(fqn_resolve(&db, &ResolutionScope::JdkBuiltins, "com.example.App").is_none());
    }

    #[test]
    fn source_set_for_file_round_trips_through_source_root() {
        let file_id = FileId::from_raw(1);
        let path = VfsPath::from(AbsPathBuf::assert_utf8(
            "/src/main/java/com/example/A.java".into(),
        ));

        let mut file_set = FileSet::default();
        file_set.insert(file_id, path);
        let root = SourceRoot::new(file_set);

        let mut change = FileChange::default();
        change.set_roots(vec![root]);
        change.change_file(
            file_id,
            Some("package com.example;\nclass A {}\n".to_owned()),
        );

        let ss = main_source_set(ProjectId(0));
        let mut db = TestDatabase::new();
        change.apply(&mut db);

        let mut data = ProjectGraphData::default();
        data.source_root_to_source_set
            .insert(SourceRootId(0), ss.clone());
        set_project_graph(&mut db, data);

        assert_eq!(source_set_for_file(&db, file_id), Some(ss.clone()));
        assert_eq!(source_set_for_file(&db, FileId::from_raw(999)), None);
    }

    #[test]
    fn item_tree_is_keyed_by_file_text_and_invalidates_on_edit() {
        let file_id = FileId::from_raw(7);
        let path = VfsPath::from(AbsPathBuf::assert_utf8(
            "/src/main/java/com/example/A.java".into(),
        ));

        let mut file_set = FileSet::default();
        file_set.insert(file_id, path);
        let root = SourceRoot::new(file_set);

        let mut change = FileChange::default();
        change.set_roots(vec![root]);
        change.change_file(
            file_id,
            Some("package com.example;\npublic class A {\n    int x;\n}\n".to_owned()),
        );

        let mut db = TestDatabase::new();
        change.apply(&mut db);

        let tree = file_item_tree(&db, file_id);
        let rendered = hir_def::java::pretty::pretty_print(&tree);
        assert!(rendered.contains("class A [public]"), "{rendered}");
        assert!(rendered.contains("field x: int"), "{rendered}");
        assert!(rendered.contains("package com.example"), "{rendered}");

        // Edit the file: the item tree must reflect the new content.
        let mut edit = FileChange::default();
        edit.change_file(
            file_id,
            Some("package com.example;\nclass B {}\n".to_owned()),
        );
        edit.apply(&mut db);

        let tree = file_item_tree(&db, file_id);
        let rendered = hir_def::java::pretty::pretty_print(&tree);
        assert!(rendered.contains("class B"), "{rendered}");
        assert!(!rendered.contains("field x: int"), "{rendered}");

        // A file without a mapped path lowers to an empty tree instead of
        // failing (unknown language).
        let other_id = FileId::from_raw(8);
        let mut change = FileChange::default();
        change.change_file(other_id, Some("class Z {}\n".to_owned()));
        change.apply(&mut db);
        let _ = file_item_tree(&db, other_id);
    }
}
