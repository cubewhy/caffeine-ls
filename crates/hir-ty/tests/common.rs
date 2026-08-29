//! Shared fixtures for the hir-ty integration tests: a minimal salsa
//! database implementing [`hir_ty::TyDatabase`] end to end, plus a small
//! classfile/jar builder that hand-encodes the JDK hierarchy the tests
//! resolve against.

#![allow(unused)]

use std::{collections::HashMap, fs::File, io::Write as _, sync::Arc};

use base_db::{
    DepsMap, FileChange, FileSourceRootInput, FileText, Files, LanguageKind, Nonce, SourceDatabase,
    SourceRoot, SourceRootId, SourceRootInput, salsa::Durability,
};
use hir::{HirDatabase, HirState, LibraryId, LibraryInfo, LibraryKind, lmdb_store::StubStore};
use hir_expand::item_tree::{ItemData, ItemId, ItemTree};
use hir_ty::{DiagLocation, Ty, TyDatabase, is_assignable, is_subtype, supertypes};
use tempfile::TempDir;
use triomphe::Arc as Arc3;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};
use zip::write::{SimpleFileOptions, ZipWriter};

/// Minimal salsa database implementing the full trait stack up to
/// [`hir_ty::TyDatabase`].
#[salsa::db]
pub struct TestDatabase {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
    deps_map: Arc3<DepsMap>,
    nonce: Nonce,
    hir_state: Arc<HirState>,
    /// Keeps the per-test stub cache directory alive.
    _stub_cache_dir: TempDir,
}

impl TestDatabase {
    pub fn new() -> Self {
        // Each test database gets its own throwaway LMDB environment, so
        // tier-2 record loads work without touching the user's real cache.
        let stub_cache_dir = TempDir::new().unwrap();
        let stub_store = StubStore::default();
        stub_store.open_at(stub_cache_dir.path().to_owned());
        let hir_state = HirState {
            stub_store,
            ..HirState::default()
        };
        Self {
            storage: salsa::Storage::default(),
            files: Arc::default(),
            deps_map: Arc3::default(),
            nonce: Nonce::new(),
            hir_state: Arc::new(hir_state),
            _stub_cache_dir: stub_cache_dir,
        }
    }
}

impl Default for TestDatabase {
    fn default() -> Self {
        Self::new()
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
        source_root: Arc3<SourceRoot>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_source_root_with_durability(self, source_root_id, source_root, durability);
    }
    fn deps_map(&self) -> Arc3<DepsMap> {
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
impl HirDatabase for TestDatabase {
    fn hir_state(&self) -> &HirState {
        &self.hir_state
    }
}

#[salsa::db]
impl hir_expand::db::DefDatabase for TestDatabase {}

#[salsa::db]
impl hir_def::db::DefDatabase for TestDatabase {}

#[salsa::db]
impl hir_ty::TyDatabase for TestDatabase {}

/// Registers `text` as the contents of `file_id` at `path` (a `.java` path so
/// the language kind is detected). Each call replaces the source root set.
pub fn add_source(db: &mut TestDatabase, file_id: FileId, path: &str, text: &str) {
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::from(AbsPathBuf::assert_utf8(path.into())));
    let root = SourceRoot::new(file_set);
    let mut change = FileChange::default();
    change.set_roots(vec![root]);
    change.change_file(file_id, Some(text.to_owned()));
    change.apply(db);
}

/// A temporary JDK-like jar with the class hierarchy used by the tests.
pub struct JdkFixture {
    _dir: TempDir,
    pub jar: camino::Utf8PathBuf,
    pub lib: LibraryId,
}

pub fn jdk_fixture() -> JdkFixture {
    let dir = TempDir::new().unwrap();
    let base = camino::Utf8PathBuf::from_path_buf(dir.path().join("fixture")).unwrap();
    std::fs::create_dir_all(&base).unwrap();
    let jar = base.join("jdk.jar");
    build_jar(&jar, &jdk_classes());
    let lib = LibraryId::from_file_path(jar.as_std_path()).unwrap();
    JdkFixture {
        _dir: dir,
        jar,
        lib,
    }
}

/// Registers the fixture jar as both a library and a JDK built-in, and sets
/// the project graph.
pub fn register_jdk(db: &mut TestDatabase, fixture: &JdkFixture) {
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(
        fixture.lib,
        hir::LibraryInfo::new(
            LibraryKind::Jar,
            AbsPathBuf::assert_utf8(fixture.jar.as_std_path().to_owned()),
        ),
    );
    data.jdk_libraries.push(fixture.lib);
    hir::set_project_graph(db, data);
}

/// Registers a source set owning a single source root with `files` (path →
/// text), the JDK fixture as a classpath library. Returns the source set id.
/// The root becomes `SourceRootId(0)` (the first root applied).
pub fn register_source_set(
    db: &mut TestDatabase,
    fixture: &JdkFixture,
    files: &[(&str, &str)],
) -> hir::SourceSetId {
    let mut file_set = FileSet::default();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(
            FileId::from_raw((i + 1) as u32),
            VfsPath::from(AbsPathBuf::assert_utf8((*path).into())),
        );
    }
    let root = SourceRoot::new(file_set);
    let mut change = FileChange::default();
    change.set_roots(vec![root]);
    for (i, (_, text)) in files.iter().enumerate() {
        change.change_file(FileId::from_raw((i + 1) as u32), Some((*text).to_owned()));
    }
    change.apply(db);

    let source_set = hir::SourceSetId {
        project: hir::ProjectId(0),
        kind: hir::SourceSetKind::Main,
    };
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(
        fixture.lib,
        hir::LibraryInfo::new(
            LibraryKind::Jar,
            AbsPathBuf::assert_utf8(fixture.jar.as_std_path().to_owned()),
        ),
    );
    data.jdk_libraries.push(fixture.lib);
    data.source_sets.insert(
        source_set.clone(),
        Arc::new(hir::Classpath {
            entries: vec![hir::ClasspathEntry::Library(fixture.lib)],
        }),
    );
    data.source_root_to_source_set
        .insert(SourceRootId(0), source_set.clone());
    hir::set_project_graph(db, data);
    source_set
}

/// Every `(ItemId, &ItemData)` in the tree, parents before children.
pub fn all_items(tree: &ItemTree) -> Vec<(ItemId, &ItemData)> {
    fn walk<'a>(tree: &'a ItemTree, id: ItemId, out: &mut Vec<(ItemId, &'a ItemData)>) {
        let data = tree.data(id);
        out.push((id, data));
        for &child in data.body() {
            walk(tree, child, out);
        }
    }
    let mut out = Vec::new();
    for &top in &tree.top {
        walk(tree, top, &mut out);
    }
    out
}

/// A temporary jar holding `specs`, plus its registered [`LibraryId`].
pub struct TempJar {
    pub _dir: TempDir,
    pub path: camino::Utf8PathBuf,
    pub lib: LibraryId,
}

/// Builds a temporary library jar from class descriptions.
pub fn temp_jar(name: &str, specs: &[ClassSpec]) -> TempJar {
    let dir = TempDir::new().unwrap();
    let base = camino::Utf8PathBuf::from_path_buf(dir.path().join(name)).unwrap();
    std::fs::create_dir_all(&base).unwrap();
    let path = base.join("lib.jar");
    build_jar(&path, specs);
    let lib = LibraryId::from_file_path(path.as_std_path()).unwrap();
    TempJar {
        _dir: dir,
        path,
        lib,
    }
}

/// Registers a source set owning a single source root with `files`, like
/// [`register_source_set`], but with an explicit ordered classpath and extra
/// libraries (the JDK fixture should be one of the classpath entries).
pub fn register_source_set_classpath(
    db: &mut TestDatabase,
    fixture: &JdkFixture,
    files: &[(&str, &str)],
    classpath: Vec<hir::ClasspathEntry>,
    extra: &[(LibraryId, LibraryInfo)],
) -> hir::SourceSetId {
    let mut file_set = FileSet::default();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(
            FileId::from_raw((i + 1) as u32),
            VfsPath::from(AbsPathBuf::assert_utf8((*path).into())),
        );
    }
    let root = SourceRoot::new(file_set);
    let mut change = FileChange::default();
    change.set_roots(vec![root]);
    for (i, (_, text)) in files.iter().enumerate() {
        change.change_file(FileId::from_raw((i + 1) as u32), Some((*text).to_owned()));
    }
    change.apply(db);

    let source_set = hir::SourceSetId {
        project: hir::ProjectId(0),
        kind: hir::SourceSetKind::Main,
    };
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(
        fixture.lib,
        hir::LibraryInfo::new(
            LibraryKind::Jar,
            AbsPathBuf::assert_utf8(fixture.jar.as_std_path().to_owned()),
        ),
    );
    for (library, info) in extra {
        data.libraries.insert(*library, info.clone());
    }
    data.jdk_libraries.push(fixture.lib);
    data.source_sets.insert(
        source_set.clone(),
        Arc::new(hir::Classpath { entries: classpath }),
    );
    data.source_root_to_source_set
        .insert(SourceRootId(0), source_set.clone());
    hir::set_project_graph(db, data);
    source_set
}

/// The access context ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
/// of a probe call site placed inside the first top-level class of the first
/// source file of `source_set`: the caller is a member of that class and in
/// its package, so package-private and `protected` members of the source set's
/// classes are accessible as from within its own file
/// ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
/// Falls back to an external probe
/// ([`InvocationContext::external`]) when the source set owns no classes.
pub fn source_context(
    db: &TestDatabase,
    source_set: hir::SourceSetId,
) -> hir_ty::InvocationContext {
    // `register_source_set` maps the first source file to `FileId(1)`.
    let file = FileId::from_raw(1);
    let tree = hir::file_item_tree(db, file);
    match tree.top.first().copied() {
        Some(item) => hir_ty::access_context(db, file, item),
        None => hir_ty::InvocationContext::external(&hir::ResolutionScope::SourceSet(source_set)),
    }
}

/// The id of the first field named `name`, if any.
pub fn find_field(tree: &ItemTree, name: &str) -> Option<ItemId> {
    all_items(tree)
        .into_iter()
        .find_map(|(id, data)| match data {
            ItemData::Field(field) if field.name.as_str() == name => Some(id),
            _ => None,
        })
}

/// The id of the first method named `name`, if any.
pub fn find_method(tree: &ItemTree, name: &str) -> Option<ItemId> {
    all_items(tree)
        .into_iter()
        .find_map(|(id, data)| match data {
            ItemData::Method(method) if method.name.as_str() == name => Some(id),
            _ => None,
        })
}

// -- classfile encoding ------------------------------------------------------

/// A hand-encoded classfile description. Names are slash-separated FQNs;
/// descriptors are JVM field/method descriptors. `sig` is the class-level
/// `Signature` attribute ([JVMS §4.7.9.1]) if present, e.g.
/// `<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;Ljava/util/List<TE;>;`.
/// `method_sigs` is the method-level `Signature` attribute of each method
/// (empty string for none), which overrides the descriptor with type
/// variables — e.g. `List.add` has descriptor `(Ljava/lang/Object;)Z` but
/// signature `(TE;)Z`.
pub struct ClassSpec<'a> {
    pub fqn: &'a str,
    pub super_class: Option<&'a str>,
    pub interfaces: &'a [&'a str],
    pub access: u16,
    pub fields: &'a [(&'a str, &'a str)],
    pub methods: &'a [(&'a str, &'a str)],
    pub method_sigs: &'a [&'a str],
    /// The access flags of each method, parallel to `methods`; an empty slice
    /// means `ACC_PUBLIC` for every method.
    pub method_access: &'a [u16],
    pub sig: Option<&'a str>,
}

pub fn class(
    fqn: &'static str,
    super_class: Option<&'static str>,
    interfaces: &'static [&'static str],
) -> ClassSpec<'static> {
    class_sig(fqn, super_class, interfaces, None)
}

/// Like [`class`], but carrying a class-level `Signature` attribute so the
/// supertypes are parameterized.
pub fn class_sig(
    fqn: &'static str,
    super_class: Option<&'static str>,
    interfaces: &'static [&'static str],
    sig: Option<&'static str>,
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class,
        interfaces,
        access: 0x0021, // ACC_PUBLIC | ACC_SUPER
        fields: &[],
        methods: &[],
        method_sigs: &[],
        method_access: &[],
        sig,
    }
}

/// A class with methods, each `(name, descriptor)` plus its method-level
/// `Signature` attribute (empty string for none).
pub fn class_with_methods(
    fqn: &'static str,
    super_class: Option<&'static str>,
    interfaces: &'static [&'static str],
    methods: &'static [(&'static str, &'static str)],
    method_sigs: &'static [&'static str],
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class,
        interfaces,
        access: 0x0021, // ACC_PUBLIC | ACC_SUPER
        fields: &[],
        methods,
        method_sigs,
        method_access: &[],
        sig: None,
    }
}

/// Like [`class_with_methods_access`], with a class-level `Signature`
/// attribute so the supertypes are parameterized.
pub fn class_with_methods_access_sig(
    fqn: &'static str,
    super_class: Option<&'static str>,
    interfaces: &'static [&'static str],
    methods: &'static [(&'static str, &'static str)],
    method_sigs: &'static [&'static str],
    method_access: &'static [u16],
    sig: Option<&'static str>,
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class,
        interfaces,
        access: 0x0021, // ACC_PUBLIC | ACC_SUPER
        fields: &[],
        methods,
        method_sigs,
        method_access,
        sig,
    }
}

/// Like [`class_with_methods`], with explicit per-method access flags
/// (parallel to `methods`).
pub fn class_with_methods_access(
    fqn: &'static str,
    super_class: Option<&'static str>,
    interfaces: &'static [&'static str],
    methods: &'static [(&'static str, &'static str)],
    method_sigs: &'static [&'static str],
    method_access: &'static [u16],
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class,
        interfaces,
        access: 0x0021, // ACC_PUBLIC | ACC_SUPER
        fields: &[],
        methods,
        method_sigs,
        method_access,
        sig: None,
    }
}

pub fn interface(fqn: &'static str) -> ClassSpec<'static> {
    interface_sig(fqn, &[], None)
}

pub fn interface_ext(fqn: &'static str, interfaces: &'static [&'static str]) -> ClassSpec<'static> {
    interface_sig(fqn, interfaces, None)
}

/// An annotation type ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7)):
/// `ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT | ACC_ANNOTATION`, which
/// `ClassKind::from_flags` classifies as `Annotation`
/// ([JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1)).
pub fn annotation(fqn: &'static str) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class: None,
        interfaces: &[],
        access: 0x2601,
        fields: &[],
        methods: &[],
        method_sigs: &[],
        method_access: &[],
        sig: None,
    }
}

/// Like [`interface_ext`], but carrying a class-level `Signature` attribute.
pub fn interface_sig(
    fqn: &'static str,
    interfaces: &'static [&'static str],
    sig: Option<&'static str>,
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class: None,
        interfaces,
        access: 0x0601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
        fields: &[],
        methods: &[],
        method_sigs: &[],
        method_access: &[],
        sig,
    }
}

/// An interface with methods, each `(name, descriptor)` plus its method-level
/// `Signature` attribute (empty string for none).
pub fn interface_with_methods(
    fqn: &'static str,
    interfaces: &'static [&'static str],
    sig: Option<&'static str>,
    methods: &'static [(&'static str, &'static str)],
    method_sigs: &'static [&'static str],
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class: None,
        interfaces,
        access: 0x0601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
        fields: &[],
        methods,
        method_sigs,
        method_access: &[],
        sig,
    }
}

/// A functional interface: an interface whose methods are all
/// `ACC_PUBLIC | ACC_ABSTRACT` ([JLS §9.8]), so it has a single abstract
/// method for lambda and method-reference compatibility.
pub fn functional_interface(
    fqn: &'static str,
    sig: &'static str,
    methods: &'static [(&'static str, &'static str)],
    method_sigs: &'static [&'static str],
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class: None,
        interfaces: &[],
        access: 0x0601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
        fields: &[],
        methods,
        method_sigs,
        method_access: &[0x0401u16; 8],
        sig: Some(sig),
    }
}

/// The small JDK subset the tests resolve and subtype against.
pub fn jdk_classes() -> Vec<ClassSpec<'static>> {
    vec![
        class("java/lang/Object", None, &[]),
        class("java/lang/Class", Some("java/lang/Object"), &[]),
        interface("java/lang/CharSequence"),
        interface_sig(
            "java/lang/Comparable",
            &[],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
        ),
        // Real-classfile shape: an interface's abstract methods carry
        // ACC_PUBLIC | ACC_ABSTRACT ([JLS §9.4]), so `Closeable.close`
        // redeclaring `AutoCloseable.close` makes both override-equivalent
        // abstracts ([§9.4.1.2]) — one SAM.
        class_with_methods_access_sig(
            "java/lang/AutoCloseable",
            None,
            &[],
            &[("close", "()V")],
            &[""],
            &[0x0411],
            None,
        ),
        class_with_methods_access_sig(
            "java/io/Closeable",
            None,
            &["java/lang/AutoCloseable"],
            &[("close", "()V")],
            &[""],
            &[0x0411],
            None,
        ),
        class_with_methods(
            "java/lang/String",
            Some("java/lang/Object"),
            &["java/lang/CharSequence"],
            &[("length", "()I")],
            &[""],
        ),
        class("java/lang/Number", Some("java/lang/Object"), &[]),
        class("java/lang/Integer", Some("java/lang/Number"), &[]),
        class("java/lang/Long", Some("java/lang/Number"), &[]),
        class("java/lang/Short", Some("java/lang/Number"), &[]),
        class("java/lang/Byte", Some("java/lang/Number"), &[]),
        class("java/lang/Float", Some("java/lang/Number"), &[]),
        class("java/lang/Double", Some("java/lang/Number"), &[]),
        class("java/lang/Character", Some("java/lang/Object"), &[]),
        class("java/lang/Boolean", Some("java/lang/Object"), &[]),
        functional_interface(
            "java/lang/Runnable",
            "Ljava/lang/Object;",
            &[("run", "()V")],
            &[""],
        ),
        functional_interface(
            "java/util/function/Function",
            "<T:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
            &[("apply", "(Ljava/lang/Object;)Ljava/lang/Object;")],
            &["(TT;)TR;"],
        ),
        functional_interface(
            "java/util/function/Predicate",
            "<T:Ljava/lang/Object;>Ljava/lang/Object;",
            &[("test", "(Ljava/lang/Object;)Z")],
            &["(TT;)Z"],
        ),
        functional_interface(
            "java/util/function/Supplier",
            "<T:Ljava/lang/Object;>Ljava/lang/Object;",
            &[("get", "()Ljava/lang/Object;")],
            &["()TT;"],
        ),
        // Both `collect` overloads in real classfile order (the 3-arg form is
        // emitted first by javac), so overload resolution exercises the same
        // member set a real jimage produces ([JLS §15.12.2]).
        interface_with_methods(
            "java/util/stream/Stream",
            &[],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[
                (
                    "collect",
                    "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;",
                ),
                (
                    "collect",
                    "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
                ),
            ],
            &[
                "<R:Ljava/lang/Object;A:Ljava/lang/Object;>(Ljava/util/function/Supplier<TR;>;Ljava/util/function/BiConsumer<TR;-TT;>;Ljava/util/function/BiConsumer<TR;TR;>;)TR;",
                "<R:Ljava/lang/Object;A:Ljava/lang/Object;>(Ljava/util/stream/Collector<-TT;TA;TR;>;)TR;",
            ],
        ),
        // commons-beanutils-shaped raw-implementable interface used by the
        // override tests ([JLS §4.8] erasure of members).
        interface_with_methods(
            "org/apache/commons/beanutils/Converter",
            &[],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[(
                "convert",
                "(Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/Object;",
            )],
            &["<R:Ljava/lang/Object;>(Ljava/lang/Class<TR;>;Ljava/lang/Object;)TR;"],
        ),
        ClassSpec {
            fqn: "java/util/Map",
            super_class: None,
            interfaces: &[],
            access: 0x0601,
            fields: &[],
            methods: &[
                ("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
                (
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                ),
                ("size", "()I"),
            ],
            method_sigs: &["(TO;)TV;", "(TK;TV;)TV;", ""],
            method_access: &[0x0401, 0x0401, 0x0401],
            sig: Some("<K:Ljava/lang/Object;V:Ljava/lang/Object;>Ljava/lang/Object;"),
        },
        class_sig(
            "java/util/HashMap",
            Some("java/lang/Object"),
            &["java/util/Map"],
            Some(
                "<K:Ljava/lang/Object;V:Ljava/lang/Object;>java/lang/Object;\
                 Ljava/util/Map<TK;TV;>;",
            ),
        ),
        functional_interface(
            "java/util/function/BiConsumer",
            "<T:Ljava/lang/Object;U:Ljava/lang/Object;>Ljava/lang/Object;",
            &[("accept", "(Ljava/lang/Object;Ljava/lang/Object;)V")],
            &["(TT;TU;)V"],
        ),
        interface_sig(
            "java/util/stream/Collector",
            &[],
            Some(
                "<T:Ljava/lang/Object;A:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
            ),
        ),
        class_with_methods_access(
            "java/util/stream/Collectors",
            Some("java/lang/Object"),
            &[],
            &[("toList", "()Ljava/util/stream/Collector;")],
            &[
                // Real classfile shape (JVMS §4.7.9.1): the accumulator
                // position is an unbounded wildcard (`*`, no bound), so
                // inference must contain `α = ?` when reducing
                // ⟨Collector<T,?,List<T>> → Collector<? super T,A,R⟩.
                "<T:Ljava/lang/Object;>()Ljava/util/stream/Collector<TT;*Ljava/util/List<TT;>;>;",
            ],
            &[0x0009], // ACC_PUBLIC | ACC_STATIC
        ),
        // The primitive-array `equals` overloads plus the generic and
        // primitive `copyOf` forms, in real-classfile order, so overload
        // resolution over nested invocation arguments exercises the same
        // candidate set the jimage produces ([JLS §15.12.2]).
        ClassSpec {
            fqn: "java/util/Arrays",
            super_class: Some("java/lang/Object"),
            interfaces: &[],
            access: 0x0021,
            methods: &[
                ("equals", "([Ljava/lang/Object;[Ljava/lang/Object;)Z"),
                ("equals", "([I[I)Z"),
                ("equals", "([J[J)Z"),
                ("equals", "([B[B)Z"),
                ("equals", "([S[S)Z"),
                ("equals", "([C[C)Z"),
                ("equals", "([Z[Z)Z"),
                ("equals", "([F[F)Z"),
                ("equals", "([D[D)Z"),
                ("copyOf", "([Ljava/lang/Object;I)[Ljava/lang/Object;"),
                ("copyOf", "([II)[I"),
                ("copyOf", "([JI)[J"),
            ],
            method_sigs: &[
                "",
                "",
                "",
                "",
                "",
                "",
                "",
                "",
                "",
                "<T:Ljava/lang/Object;>([TT;I)[TT;",
                "",
                "",
            ],
            method_access: &[0x0009; 12], // ACC_PUBLIC | ACC_STATIC
            sig: None,
            fields: &[],
        },
        class("java/lang/Throwable", Some("java/lang/Object"), &[]),
        class("java/lang/Exception", Some("java/lang/Throwable"), &[]),
        class("java/io/IOException", Some("java/lang/Exception"), &[]),
        class(
            "java/io/FileNotFoundException",
            Some("java/io/IOException"),
            &[],
        ),
        class(
            "java/lang/ClassNotFoundException",
            Some("java/lang/Exception"),
            &[],
        ),
        class_with_methods_access_sig(
            "java/lang/Enum",
            Some("java/lang/Object"),
            &["java/io/Serializable", "java/lang/Comparable"],
            &[("name", "()Ljava/lang/String;"), ("ordinal", "()I")],
            &["", ""],
            &[0x0001, 0x0001], // ACC_PUBLIC (methods)
            Some("<E:Ljava/lang/Enum<TE;>;Ljava/lang/Object;"),
        ),
        class_with_methods_access(
            "java/lang/Math",
            Some("java/lang/Object"),
            &[],
            &[("max", "(II)I"), ("min", "(II)I"), ("sqrt", "(D)D")],
            &["", "", ""],
            &[0x0009, 0x0009, 0x0009], // ACC_PUBLIC | ACC_STATIC
        ),
        interface("java/lang/Cloneable"),
        interface("java/io/Serializable"),
        // Annotations resolved by the declaration checks
        // ([JLS §9.7], [§9.6.4.4]) and by the annotation fixtures.
        annotation("java/lang/Deprecated"),
        annotation("java/lang/Override"),
        annotation("java/lang/SuppressWarnings"),
        annotation("java/lang/FunctionalInterface"),
        annotation("java/lang/SafeVarargs"),
        annotation("java/lang/annotation/Annotation"),
        annotation("java/lang/annotation/Documented"),
        annotation("java/lang/annotation/Retention"),
        annotation("java/lang/annotation/Target"),
        class("java/lang/annotation/RetentionPolicy", None, &[]),
        class("java/lang/annotation/ElementType", None, &[]),
        interface_with_methods(
            "java/lang/Iterable",
            &[],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[("iterator", "()Ljava/util/Iterator;")],
            &["()Ljava/util/Iterator<TT;>;"],
        ),
        interface_with_methods(
            "java/util/Iterator",
            &[],
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[("next", "()Ljava/lang/Object;"), ("hasNext", "()Z")],
            &["()TE;", ""],
        ),
        interface_with_methods(
            "java/util/Collection",
            &["java/lang/Iterable"],
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;Ljava/lang/Iterable<TE;>;"),
            &[("iterator", "()Ljava/util/Iterator;")],
            &["()Ljava/util/Iterator<TE;>;"],
        ),
        // Explicit spec so the varargs factory carries ACC_STATIC
        // ([JLS §9.4.4] interface static methods).
        ClassSpec {
            fqn: "java/util/List",
            super_class: None,
            interfaces: &["java/util/Collection"],
            access: 0x0601,
            fields: &[],
            methods: &[
                ("add", "(Ljava/lang/Object;)Z"),
                ("get", "(I)Ljava/lang/Object;"),
                ("size", "()I"),
                ("isEmpty", "()Z"),
                ("subList", "(II)Ljava/util/List;"),
                ("iterator", "()Ljava/util/Iterator;"),
                // `List.of(E...)` ([JLS §15.12.2.4] varargs phase).
                ("of", "([Ljava/lang/Object;)Ljava/util/List;"),
            ],
            method_sigs: &[
                "(TE;)Z",
                "(I)TE;",
                "",
                "",
                "(II)Ljava/util/List<TE;>;",
                "()Ljava/util/Iterator<TE;>;",
                // `List.of(E...)` ([JLS §15.12.2.4] varargs phase): the
                // signature marks the varargs parameter as an array `[TE;`,
                // matching what javac emits for `ACC_VARARGS` methods.
                "<E:Ljava/lang/Object;>([TE;)Ljava/util/List<TE;>;",
            ],
            method_access: &[
                0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001,
                0x0409, // ACC_PUBLIC | ACC_STATIC
            ],
            sig: Some("<E:Ljava/lang/Object;>Ljava/lang/Object;Ljava/util/Collection<TE;>;"),
        },
        class_sig(
            "java/util/AbstractList",
            Some("java/lang/Object"),
            &["java/util/List"],
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;Ljava/util/List<TE;>;"),
        ),
        class_sig(
            "java/util/ArrayList",
            Some("java/util/AbstractList"),
            &[
                "java/util/List",
                "java/lang/Cloneable",
                "java/io/Serializable",
            ],
            Some(
                "<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;Ljava/util/List<TE;>;\
                 Ljava/lang/Cloneable;Ljava/io/Serializable;",
            ),
        ),
        class_with_methods_access(
            "java/util/Collections",
            None,
            &[],
            &[
                ("emptyList", "()Ljava/util/List;"),
                ("sort", "(Ljava/util/List;)V"),
                ("emptyIterator", "()Ljava/util/Iterator;"),
            ],
            &[
                "<T:Ljava/lang/Object;>()Ljava/util/List<TT;>;",
                "<T:Ljava/lang/Object;>(Ljava/util/List<TT;>;)V",
                "<T:Ljava/lang/Object;>()Ljava/util/Iterator<TT;>;",
            ],
            &[0x0009, 0x0009, 0x0009], // ACC_PUBLIC | ACC_STATIC
        ),
        class_with_methods_access_sig(
            "java/lang/ref/WeakReference",
            Some("java/lang/Object"),
            &[],
            &[("get", "()Ljava/lang/Object;")],
            &["()TT;"],
            &[0x0001],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
        ),
        class_with_methods_access_sig(
            "java/util/Optional",
            Some("java/lang/Object"),
            &[],
            &[
                ("get", "()Ljava/lang/Object;"),
                ("map", "(Ljava/util/function/Function;)Ljava/util/Optional;"),
                ("orElse", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ],
            &[
                "()TT;",
                "<U:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TU;>;)Ljava/util/Optional<TU;>;",
                "(TT;)TT;",
            ],
            &[0x0001, 0x0001, 0x0001],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
        ),
        // Constructors the explicit-`super(args)` tests resolve against
        // ([JLS §8.8.7.1]); library ctors are classfile `<init>` entries.
        class_with_methods_access(
            "java/io/OutputStream",
            Some("java/lang/Object"),
            &[],
            &[],
            &[],
            &[],
        ),
        class_with_methods_access(
            "java/io/ByteArrayOutputStream",
            Some("java/io/OutputStream"),
            &[],
            &[],
            &[],
            &[],
        ),
        class_with_methods_access(
            "java/io/PrintStream",
            Some("java/io/OutputStream"), // real JDK: FilterOutputStream chain
            &[],
            &[("<init>", "(Ljava/io/OutputStream;)V")],
            &[""],
            &[0x0001],
        ),
        class_with_methods_access(
            "java/lang/RuntimeException",
            Some("java/lang/Exception"),
            &[],
            &[("<init>", "(Ljava/lang/String;)V"), ("<init>", "()V")],
            &["", ""],
            &[0x0001, 0x0001],
        ),
        class_with_methods_access(
            "java/io/File",
            Some("java/lang/Object"),
            &[],
            &[("<init>", "(Ljava/lang/String;)V")],
            &[""],
            &[0x0001],
        ),
        class("java/util/regex/Pattern", Some("java/lang/Object"), &[]),
    ]
}

/// A constant-pool builder for the hand-encoded classfiles.
struct Pool {
    bytes: Vec<u8>,
    count: u16,
    utf8: HashMap<String, u16>,
    classes: HashMap<String, u16>,
}

impl Pool {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            count: 1,
            utf8: HashMap::default(),
            classes: HashMap::default(),
        }
    }

    fn alloc(&mut self, entry: &[u8]) -> u16 {
        let idx = self.count;
        self.count += 1;
        self.bytes.extend_from_slice(entry);
        idx
    }

    fn utf8(&mut self, s: &str) -> u16 {
        if let Some(&idx) = self.utf8.get(s) {
            return idx;
        }
        let mut entry = Vec::with_capacity(3 + s.len());
        entry.push(1); // CONSTANT_Utf8
        entry.extend_from_slice(&(s.len() as u16).to_be_bytes());
        entry.extend_from_slice(s.as_bytes());
        let idx = self.alloc(&entry);
        self.utf8.insert(s.to_owned(), idx);
        idx
    }

    fn class(&mut self, name: &str) -> u16 {
        if let Some(&idx) = self.classes.get(name) {
            return idx;
        }
        let name_utf8 = self.utf8(name);
        let mut entry = Vec::with_capacity(3);
        entry.push(7); // CONSTANT_Class
        entry.extend_from_slice(&name_utf8.to_be_bytes());
        let idx = self.alloc(&entry);
        self.classes.insert(name.to_owned(), idx);
        idx
    }
}

/// Encodes a minimal classfile (major 52, no attributes) for `spec`.
fn class_bytes(spec: &ClassSpec) -> Vec<u8> {
    let mut pool = Pool::new();
    let this_class = pool.class(spec.fqn);
    let super_class = match spec.super_class {
        Some(super_name) => pool.class(super_name),
        None => 0,
    };
    let interfaces: Vec<u16> = spec.interfaces.iter().map(|i| pool.class(i)).collect();
    let fields: Vec<(u16, u16)> = spec
        .fields
        .iter()
        .map(|(name, desc)| (pool.utf8(name), pool.utf8(desc)))
        .collect();
    let methods: Vec<(u16, u16)> = spec
        .methods
        .iter()
        .map(|(name, desc)| (pool.utf8(name), pool.utf8(desc)))
        .collect();
    let sig_name = spec.sig.map(|_| pool.utf8("Signature"));
    let sig_index = spec.sig.map(|sig| pool.utf8(sig));
    // Method-level `Signature` attributes must be pooled before the constant
    // pool is flushed into the output below.
    let method_sigs: Vec<(u16, u16)> = spec
        .methods
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let sig = spec.method_sigs.get(i).copied().unwrap_or("");
            if sig.is_empty() {
                (0, 0)
            } else {
                (pool.utf8("Signature"), pool.utf8(sig))
            }
        })
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    out.extend_from_slice(&0u16.to_be_bytes()); // minor version
    out.extend_from_slice(&52u16.to_be_bytes()); // major version
    out.extend_from_slice(&pool.count.to_be_bytes()); // constant pool count
    out.extend_from_slice(&pool.bytes);

    out.extend_from_slice(&spec.access.to_be_bytes());
    out.extend_from_slice(&this_class.to_be_bytes());
    out.extend_from_slice(&super_class.to_be_bytes());
    out.extend_from_slice(&(interfaces.len() as u16).to_be_bytes());
    for idx in interfaces {
        out.extend_from_slice(&idx.to_be_bytes());
    }

    out.extend_from_slice(&(fields.len() as u16).to_be_bytes());
    for (name, desc) in fields {
        out.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
        out.extend_from_slice(&name.to_be_bytes());
        out.extend_from_slice(&desc.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // attributes
    }

    out.extend_from_slice(&(methods.len() as u16).to_be_bytes());
    for (i, (name, desc)) in methods.iter().enumerate() {
        let (sig_name, sig_index) = method_sigs[i];
        let attributes = if sig_name == 0 {
            Vec::new()
        } else {
            let mut attr = Vec::new();
            attr.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
            attr.extend_from_slice(&sig_name.to_be_bytes()); // attribute_name_index
            attr.extend_from_slice(&2u32.to_be_bytes()); // attribute_length
            attr.extend_from_slice(&sig_index.to_be_bytes()); // signature_index
            attr
        };
        let method_access = spec.method_access.get(i).copied().unwrap_or(0x0001); // ACC_PUBLIC
        out.extend_from_slice(&method_access.to_be_bytes());
        out.extend_from_slice(&name.to_be_bytes());
        out.extend_from_slice(&desc.to_be_bytes());
        if attributes.is_empty() {
            out.extend_from_slice(&0u16.to_be_bytes()); // attributes
        } else {
            out.extend_from_slice(&attributes);
        }
    }

    // class attributes: an optional `Signature` attribute (JVMS §4.7.9.1).
    match (sig_name, sig_index) {
        (Some(sig_name), Some(sig_index)) => {
            out.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
            out.extend_from_slice(&sig_name.to_be_bytes()); // attribute_name_index
            out.extend_from_slice(&2u32.to_be_bytes()); // attribute_length
            out.extend_from_slice(&sig_index.to_be_bytes()); // signature_index
        }
        _ => out.extend_from_slice(&0u16.to_be_bytes()), // class attributes
    }
    out
}

/// Builds a jar containing one `.class` per spec.
pub fn build_jar(path: &camino::Utf8Path, specs: &[ClassSpec]) {
    let file = File::create(path.as_std_path()).unwrap();
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    for spec in specs {
        zip.start_file(format!("{}.class", spec.fqn), options)
            .unwrap();
        zip.write_all(&class_bytes(spec)).unwrap();
    }
    zip.finish().unwrap();
}

// -- snapshot helpers --------------------------------------------------------

/// The relation exercised by [`check_relations`].
#[derive(Clone, Copy)]
pub enum Relation {
    Subtype,
    Assignable,
}

/// Builds a [`Ty`] against a database. [`Ty`] values are interned, so a
/// builder closure keeps the construction close to where the database lives.
pub type TyBuilder = for<'a> fn(&'a TestDatabase) -> Ty;

macro_rules! snapshot {
    ($name:ident, $check:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = $check;
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}
pub(crate) use snapshot;

/// Renders the resolved declared type of every field and method signature in
/// a source file, against the JDK fixture. Reference names are resolved per
/// JLS §6.5.5/§7.5.
pub fn check_resolve_src(src: &str) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let file_id = FileId::from_raw(1);
    add_source(&mut db, file_id, "/src/com/example/Box.java", src);
    let tree = hir::file_item_tree(&db, file_id);

    let mut lines = vec![format!("SOURCE:\n{src}"), "RESOLVED:".to_owned()];
    for (id, data) in all_items(&tree) {
        match data {
            ItemData::Field(field) => {
                let ty = hir_ty::item_ty(&db, file_id, id);
                lines.push(format!("field {}: {}", field.name, ty.display(&db)));
            }
            ItemData::Method(method) => {
                let ret = hir_ty::item_ty(&db, file_id, id);
                let ret = if method.sig.ret.is_none() {
                    "<none>".to_owned()
                } else {
                    ret.display(&db).to_string()
                };
                let params: Vec<String> = hir_ty::method_params(&db, file_id, id)
                    .iter()
                    .map(|ty| ty.display(&db).to_string())
                    .collect();
                lines.push(format!(
                    "method {}: {ret}({})",
                    method.name,
                    params.join(", ")
                ));
            }
            _ => {}
        }
    }
    lines.join("\n")
}

/// Renders the resolved declared types of a source file like
/// [`check_resolve_src`], but for every type-var position also prints the
/// declared bounds ([JLS §4.4]) and the erasure ([§4.6]).
pub fn check_bounds_resolve_src(src: &str) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let file_id = FileId::from_raw(1);
    add_source(&mut db, file_id, "/src/com/example/Box.java", src);
    let tree = hir::file_item_tree(&db, file_id);

    let render = |ty: &Ty| {
        let bounds: Vec<String> = ty
            .bounds(&db)
            .iter()
            .map(|b| b.display(&db).to_string())
            .collect();
        format!(
            "{} | bounds: {} | erasure: {}",
            ty.display(&db),
            if bounds.is_empty() {
                "<none>".to_owned()
            } else {
                bounds.join(", ")
            },
            ty.erasure(&db).display(&db),
        )
    };

    let mut lines = vec![format!("SOURCE:\n{src}"), "RESOLVED:".to_owned()];
    for (id, data) in all_items(&tree) {
        match data {
            ItemData::Field(field) => {
                let ty = hir_ty::item_ty(&db, file_id, id);
                lines.push(format!("field {}: {}", field.name, render(&ty)));
            }
            ItemData::Method(method) => {
                let ret = hir_ty::item_ty(&db, file_id, id);
                let ret = if method.sig.ret.is_none() {
                    "<none>".to_owned()
                } else {
                    render(&ret)
                };
                let params: Vec<String> = hir_ty::method_params(&db, file_id, id)
                    .iter()
                    .map(render)
                    .collect();
                lines.push(format!(
                    "method {}: {ret}({})",
                    method.name,
                    params.join(", ")
                ));
            }
            _ => {}
        }
    }
    lines.join("\n")
}

/// Renders each [`Ty`] sample with its display, erasure, classification flags
/// and array element type.
pub fn check_ty_model(samples: &[(&str, TyBuilder)]) -> String {
    let db = TestDatabase::new();
    samples
        .iter()
        .map(|(label, build)| {
            let ty = build(&db);
            let element = ty
                .element(&db)
                .map(|e| e.display(&db).to_string())
                .unwrap_or_else(|| "<none>".to_owned());
            let bounds: Vec<String> = ty
                .bounds(&db)
                .iter()
                .map(|b| b.display(&db).to_string())
                .collect();
            format!(
                "--- {label} ---\nDISPLAY: {}\nERASURE: {}\nBOUNDS: {}\nFLAGS: {}\nELEMENT: {element}\n",
                ty.display(&db),
                ty.erasure(&db).display(&db),
                if bounds.is_empty() {
                    "<none>".to_owned()
                } else {
                    bounds.join(" & ")
                },
                type_flags(&db, &ty),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders each [`Ty`] sample with its simple-name display ([JLS §6.7]
/// simple name — the last `.`-separated segment, `$` kept as an identifier
/// character per [§3.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.8)).
pub fn check_ty_simple(samples: &[(&str, TyBuilder)]) -> String {
    let db = TestDatabase::new();
    samples
        .iter()
        .map(|(label, build)| {
            let ty = build(&db);
            format!(
                "--- {label} ---\nSIMPLE: {}\nFQN:    {}\n",
                ty.display_simple(&db),
                ty.display(&db),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the result of [`Relation`] for each `(sub, sup)` sample.
pub fn check_relations(samples: &[(&str, TyBuilder, TyBuilder, Relation)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let scope = hir::ResolutionScope::Classpath(vec![fixture.lib]);

    samples
        .iter()
        .map(|(label, build_sub, build_sup, relation)| {
            let sub = build_sub(&db);
            let sup = build_sup(&db);
            let result = match relation {
                Relation::Subtype => is_subtype(&db, &scope, &sub, &sup),
                Relation::Assignable => is_assignable(&db, &scope, &sub, &sup),
            };
            format!("{label}: {result}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the direct supertypes of each FQN sample (raw type).
pub fn check_supertypes(samples: &[&str]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let scope = hir::ResolutionScope::Classpath(vec![fixture.lib]);

    samples
        .iter()
        .map(|name| {
            let ty = Ty::reference(&db, *name, Vec::new());
            let supers: Vec<String> = supertypes(&db, &scope, &ty)
                .iter()
                .map(|ty| ty.display(&db).to_string())
                .collect();
            format!("{name} -> {}", supers.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the direct supertypes of each [`Ty`] sample (raw or parameterized).
pub fn check_supertypes_of(samples: &[(&str, TyBuilder)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let scope = hir::ResolutionScope::Classpath(vec![fixture.lib]);

    samples
        .iter()
        .map(|(label, build)| {
            let ty = build(&db);
            let supers: Vec<String> = supertypes(&db, &scope, &ty)
                .iter()
                .map(|ty| ty.display(&db).to_string())
                .collect();
            format!("{label} -> {}", supers.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The enabled classification flags of a [`Ty`].
fn type_flags(db: &dyn TyDatabase, ty: &Ty) -> String {
    let mut out = Vec::new();
    if ty.is_void(db) {
        out.push("void");
    }
    if ty.is_primitive(db) {
        out.push("primitive");
    }
    if ty.is_reference(db) {
        out.push("reference");
    }
    if ty.is_type_var(db) {
        out.push("type-var");
    }
    if ty.is_array(db) {
        out.push("array");
    }
    if ty.is_wildcard(db) {
        out.push("wildcard");
    }
    if ty.is_error(db) {
        out.push("error");
    }
    if ty.is_object(db) {
        out.push("object");
    }
    if out.is_empty() {
        "<none>".to_owned()
    } else {
        out.join(" ")
    }
}

/// Renders the source files and the direct supertypes ([JLS §4.10.2]) of each
/// `"fqn"` sample (raw and parameterized) resolved against the source set's
/// own classes. `files` is `(path, text)`; FQNs refer to the classes declared
/// in them.
pub fn check_source_supertypes(files: &[(&str, &str)], samples: &[(&str, TyBuilder)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let source_set = register_source_set(&mut db, &fixture, files);
    let scope = hir::ResolutionScope::SourceSet(source_set);

    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    lines.push("SUPERTYPES:".to_owned());
    for (label, build) in samples {
        let ty = build(&db);
        let supers: Vec<String> = supertypes(&db, &scope, &ty)
            .iter()
            .map(|ty| ty.display(&db).to_string())
            .collect();
        lines.push(format!("{label} -> {}", supers.join(", ")));
    }
    lines.join("\n")
}

/// Renders the source files and the resolved method call for each
/// `(label, receiver, name, args)` sample, resolved against the source set.
pub fn check_source_methods(
    files: &[(&str, &str)],
    samples: &[(&str, TyBuilder, &str, &[TyBuilder])],
) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let source_set = register_source_set(&mut db, &fixture, files);
    let context = source_context(&db, source_set.clone());
    let scope = hir::ResolutionScope::SourceSet(source_set);

    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    lines.push("METHODS:".to_owned());
    for (label, build_receiver, name, arg_builders) in samples {
        let receiver = build_receiver(&db);
        let args: Vec<hir_ty::PolyArg> = arg_builders
            .iter()
            .map(|build| hir_ty::PolyArg::Concrete(build(&db)))
            .collect();
        let arg_types: Vec<String> = args
            .iter()
            .map(|arg| match arg {
                hir_ty::PolyArg::Concrete(ty) => ty.display(&db).to_string(),
                hir_ty::PolyArg::Poly(_, _) => "<poly>".to_owned(),
            })
            .collect();
        let picked = hir_ty::pick_method(&db, &scope, &receiver, name, &args, &context, None);
        let rendered = match picked {
            Some(method) => format!("{} -> {}", method.display(&db), method.ret.display(&db)),
            None => "<none>".to_owned(),
        };
        lines.push(format!(
            "{label}: {rendered} [args: {}]",
            arg_types.join(", ")
        ));
    }
    lines.join("\n")
}

/// Renders the source files and the result of [`Relation`] for each
/// `(sub, sup)` sample resolved against the source set.
pub fn check_source_relations(
    files: &[(&str, &str)],
    samples: &[(&str, TyBuilder, TyBuilder, Relation)],
) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let source_set = register_source_set(&mut db, &fixture, files);
    let scope = hir::ResolutionScope::SourceSet(source_set);

    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    lines.push("RELATIONS:".to_owned());
    for (label, build_sub, build_sup, relation) in samples {
        let sub = build_sub(&db);
        let sup = build_sup(&db);
        let result = match relation {
            Relation::Subtype => is_subtype(&db, &scope, &sub, &sup),
            Relation::Assignable => is_assignable(&db, &scope, &sub, &sup),
        };
        lines.push(format!("{label}: {result}"));
    }
    lines.join("\n")
}

/// Renders the source files and the inferred types of every method body
/// ([JLS §15], [§14.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.4)):
/// for each method the parameter types, the return type, and the inferred type
/// of every local and expression of its body, ordered by arena id.
pub fn check_body_types(files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(&mut db, &fixture, files);

    render_body_types(&db, files)
}

/// [`check_body_types`] with an extra third-party library jar (`specs`) on
/// the compile classpath, next to the JDK fixture — the shape a maven
/// workspace produces for external dependencies.
pub fn check_body_types_with_libs(specs: &[ClassSpec<'static>], files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let extra = temp_jar("widgets", specs);
    let mut db = TestDatabase::new();
    let info = LibraryInfo::new(
        LibraryKind::Jar,
        AbsPathBuf::assert_utf8(extra.path.as_std_path().to_owned()),
    );
    let classpath = vec![
        hir::ClasspathEntry::Library(fixture.lib),
        hir::ClasspathEntry::Library(extra.lib),
    ];
    register_source_set_classpath(&mut db, &fixture, files, classpath, &[(extra.lib, info)]);

    render_body_types(&db, files)
}

fn render_body_types(db: &TestDatabase, files: &[(&str, &str)]) -> String {
    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let tree = hir::file_item_tree(db, file_id);
        let bodies = hir::file_body_tree(db, file_id);
        let line_index = line_index::LineIndex::new(text);
        for (id, data) in all_items(&tree) {
            let header = match data {
                ItemData::Method(method) => {
                    let ret = if method.sig.ret.is_none() {
                        "<init>".to_owned()
                    } else {
                        hir_ty::item_ty(db, file_id, id).display(db).to_string()
                    };
                    let params: Vec<String> = hir_ty::method_params(db, file_id, id)
                        .iter()
                        .map(|ty| ty.display(db).to_string())
                        .collect();
                    format!("method {}({}): {ret}", method.name, params.join(", "))
                }
                ItemData::Field(field) => format!("field {}", field.name),
                ItemData::EnumConstant(constant) => format!("constant {}", constant.name),
                ItemData::StaticInit(_) => "static {}".to_owned(),
                ItemData::InstanceInit(_) => "instance {}".to_owned(),
                _ => continue,
            };
            let Some(types) = hir_ty::body_types(db, file_id, id) else {
                continue;
            };
            lines.push(header);
            let mut locals: Vec<_> = types.locals.iter().collect();
            locals.sort_by_key(|(id, _)| id.0.0);
            lines.push(format!(
                "  locals: {}",
                locals
                    .iter()
                    .map(|(id, ty)| format!("{id}: {}", ty.display(db)))
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
            let mut exprs: Vec<_> = types.exprs.iter().collect();
            exprs.sort_by_key(|(id, _)| id.0.0);
            lines.push(format!(
                "  exprs: {}",
                exprs
                    .iter()
                    .map(|(id, ty)| format!("{id}: {}", ty.display(db)))
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
            if !types.diagnostics.is_empty() {
                lines.push(format!(
                    "  diags: {}",
                    types
                        .diagnostics
                        .iter()
                        .map(|diag| {
                            let loc = match diag.location() {
                                DiagLocation::Expr(id) => format!("{id}"),
                                DiagLocation::Local(id) => format!("{id}"),
                                DiagLocation::Pattern(id) => format!("p{id}"),
                                DiagLocation::Stmt(id) => format!("s{id}"),
                                DiagLocation::Method => "method".to_owned(),
                            };
                            let at = diag
                                .range(&bodies)
                                .map(|r| {
                                    let lc = line_index.line_col(r.start());
                                    format!("@{line}:{col}", line = lc.line, col = lc.col)
                                })
                                .unwrap_or_default();
                            format!("{loc}{at}: {}: {}", diag.code(), diag.message(db, &bodies))
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
        }
    }
    lines.join("\n")
}

/// Renders the declaration-level diagnostics
/// ([JLS §8.4.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.3),
/// [§9.4.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.3))
/// of every class in the source files: one line per diagnostic, ordered by
/// file then source order, as `method <name>: <code>: <message>`.
pub fn check_class_diagnostics(files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(&mut db, &fixture, files);

    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let line_index = line_index::LineIndex::new(text);
        for diag in hir_ty::class_diagnostics(&db, file_id) {
            let at = diag
                .range()
                .map(|r| {
                    let lc = line_index.line_col(r.start());
                    format!("@{line}:{col}", line = lc.line, col = lc.col)
                })
                .unwrap_or_default();
            lines.push(format!(
                "method {}: {}: {}{}",
                diag.method_name(),
                diag.code(),
                at,
                diag.message(&db)
            ));
        }
    }
    lines.join("\n")
}

/// Renders the resolved method call for each `(label, receiver, name, args)`
/// sample, against the JDK fixture. The receiver and the arguments are
/// [`TyBuilder`]s rendered after resolution.
pub fn check_methods(samples: &[(&str, TyBuilder, &str, &[TyBuilder])]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let scope = hir::ResolutionScope::Classpath(vec![fixture.lib]);

    samples
        .iter()
        .map(|(label, build_receiver, name, arg_builders)| {
            let receiver = build_receiver(&db);
            let args: Vec<hir_ty::PolyArg> = arg_builders
                .iter()
                .map(|build| hir_ty::PolyArg::Concrete(build(&db)))
                .collect();
            let arg_types: Vec<String> = args
                .iter()
                .map(|arg| match arg {
                    hir_ty::PolyArg::Concrete(ty) => ty.display(&db).to_string(),
                    hir_ty::PolyArg::Poly(_, _) => "<poly>".to_owned(),
                })
                .collect();
            let picked = hir_ty::pick_method(
                &db,
                &scope,
                &receiver,
                name,
                &args,
                &hir_ty::InvocationContext::external(&scope),
                None,
            );
            let rendered = match picked {
                Some(method) => format!("{} -> {}", method.display(&db), method.ret.display(&db)),
                None => "<none>".to_owned(),
            };
            format!("{label}: {rendered} [args: {}]", arg_types.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the source files and the resolved method call for each
/// `(label, receiver, name, args)` sample like [`check_source_methods`], but
/// resolving under `ctx`.
pub fn check_source_methods_ctx(
    files: &[(&str, &str)],
    samples: &[(&str, TyBuilder, &str, &[TyBuilder])],
    ctx: Option<&hir_ty::InvocationContext>,
) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let source_set = register_source_set(&mut db, &fixture, files);
    let context = match ctx {
        Some(ctx) => ctx.clone(),
        // `None` places the probe call site inside the first source class, so
        // its package-private and `protected` members resolve as from within
        // the source set ([JLS §6.6]).
        None => source_context(&db, source_set.clone()),
    };
    let scope = hir::ResolutionScope::SourceSet(source_set);

    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    lines.push("METHODS:".to_owned());
    for (label, build_receiver, name, arg_builders) in samples {
        let receiver = build_receiver(&db);
        let args: Vec<hir_ty::PolyArg> = arg_builders
            .iter()
            .map(|build| hir_ty::PolyArg::Concrete(build(&db)))
            .collect();
        let arg_types: Vec<String> = args
            .iter()
            .map(|arg| match arg {
                hir_ty::PolyArg::Concrete(ty) => ty.display(&db).to_string(),
                hir_ty::PolyArg::Poly(_, _) => "<poly>".to_owned(),
            })
            .collect();
        let picked = hir_ty::pick_method(&db, &scope, &receiver, name, &args, &context, None);
        let rendered = match picked {
            Some(method) => format!("{} -> {}", method.display(&db), method.ret.display(&db)),
            None => "<none>".to_owned(),
        };
        lines.push(format!(
            "{label}: {rendered} [args: {}]",
            arg_types.join(", ")
        ));
    }
    lines.join("\n")
}

/// Renders the resolved method call for each `(label, receiver, name, args)`
/// sample against a library built from `classes`, resolved under `ctx`. The
/// classes must include `java.lang.Object`.
pub fn check_methods_lib_ctx(
    classes: &[ClassSpec<'static>],
    samples: &[(&str, TyBuilder, &str, &[TyBuilder])],
    ctx: &hir_ty::InvocationContext,
) -> String {
    let _dir = tempfile::TempDir::new().unwrap();
    let base = camino::Utf8PathBuf::from_path_buf(_dir.path().join("lib")).unwrap();
    std::fs::create_dir_all(&base).unwrap();
    let jar = base.join("lib.jar");
    build_jar(&jar, classes);
    let lib = hir::LibraryId::from_file_path(jar.as_std_path()).unwrap();
    let mut db = TestDatabase::new();
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(
        lib,
        hir::LibraryInfo::new(
            hir::LibraryKind::Jar,
            AbsPathBuf::assert_utf8(jar.as_std_path().to_owned()),
        ),
    );
    data.jdk_libraries.push(lib);
    hir::set_project_graph(&mut db, data);
    let scope = hir::ResolutionScope::Classpath(vec![lib]);

    samples
        .iter()
        .map(|(label, build_receiver, name, arg_builders)| {
            let receiver = build_receiver(&db);
            let args: Vec<hir_ty::PolyArg> = arg_builders
                .iter()
                .map(|build| hir_ty::PolyArg::Concrete(build(&db)))
                .collect();
            let arg_types: Vec<String> = args
                .iter()
                .map(|arg| match arg {
                    hir_ty::PolyArg::Concrete(ty) => ty.display(&db).to_string(),
                    hir_ty::PolyArg::Poly(_, _) => "<poly>".to_owned(),
                })
                .collect();
            let picked = hir_ty::pick_method(&db, &scope, &receiver, name, &args, ctx, None);
            let rendered = match picked {
                Some(method) => format!("{} -> {}", method.display(&db), method.ret.display(&db)),
                None => "<none>".to_owned(),
            };
            format!("{label}: {rendered} [args: {}]", arg_types.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the source files and the resolved method call for each
/// `(label, file_index, method, receiver, name, args)` sample, where the
/// invocation context is derived from the call site's enclosing method
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6), [`hir_ty::access_context`]).
/// `file_index` maps to the i-th `(path, text)` of `files` (files are
/// registered as `FileId::from_raw(i + 1)`); `method` names a method of that
/// file whose body contains the call site.
pub fn check_source_methods_site(
    files: &[(&str, &str)],
    samples: &[(&str, usize, &str, TyBuilder, &str, &[TyBuilder])],
) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let source_set = register_source_set(&mut db, &fixture, files);
    let scope = hir::ResolutionScope::SourceSet(source_set);

    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    lines.push("METHODS:".to_owned());
    for (label, file_index, method, build_receiver, name, arg_builders) in samples {
        let file_id = FileId::from_raw((*file_index + 1) as u32);
        let tree = hir::file_item_tree(&db, file_id);
        let Some(method_id) = find_method(&tree, method) else {
            panic!("method {method} not found in file {file_index}");
        };
        let ctx = hir_ty::access_context(&db, file_id, method_id);
        let receiver = build_receiver(&db);
        let args: Vec<hir_ty::PolyArg> = arg_builders
            .iter()
            .map(|build| hir_ty::PolyArg::Concrete(build(&db)))
            .collect();
        let arg_types: Vec<String> = args
            .iter()
            .map(|arg| match arg {
                hir_ty::PolyArg::Concrete(ty) => ty.display(&db).to_string(),
                hir_ty::PolyArg::Poly(_, _) => "<poly>".to_owned(),
            })
            .collect();
        let picked = hir_ty::pick_method(&db, &scope, &receiver, name, &args, &ctx, None);
        let rendered = match picked {
            Some(method) => format!("{} -> {}", method.display(&db), method.ret.display(&db)),
            None => "<none>".to_owned(),
        };
        lines.push(format!(
            "{label}: {rendered} [ctx: class={:?}, package={:?}] [args: {}]",
            ctx.enclosing_class,
            ctx.package,
            arg_types.join(", ")
        ));
    }
    lines.join("\n")
}

// -- real JDK + javac parity -------------------------------------------------

/// The JDK home of the running toolchain, where the native `lib/modules`
/// image and `javac` live: `CAFFEINE_LS_JAVA_HOME`, then `JAVA_HOME`, then the
/// parent of `readlink -f $(command -v javac)`. `None` when no JDK is around
/// (the parity tests then skip).
pub fn jdk_home() -> Option<camino::Utf8PathBuf> {
    for var in ["CAFFEINE_LS_JAVA_HOME", "JAVA_HOME"] {
        if let Ok(home) = std::env::var(var) {
            let home = std::path::PathBuf::from(home);
            if home.join("lib/modules").exists() || home.join("lib/rt.jar").exists() {
                return camino::Utf8PathBuf::from_path_buf(home).ok();
            }
        }
    }
    let javac = which_javac()?;
    let real = std::fs::canonicalize(&javac).ok()?;
    let home = real.parent()?.parent()?.to_path_buf();
    if home.join("lib/modules").exists() || home.join("lib/rt.jar").exists() {
        camino::Utf8PathBuf::from_path_buf(home).ok()
    } else {
        None
    }
}

fn which_javac() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("javac");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Registers the real `java.base` jimage (or `rt.jar`) as a JDK built-in, so
/// fixtures resolve against the actual JDK — the same one the `javac` parity
/// harness compiles against. Returns the JDK library and home, or `None` when
/// no JDK was found.
pub fn register_real_jdk(db: &mut TestDatabase) -> Option<RealJdk> {
    let home = jdk_home()?;
    let modules = home.join("lib/modules");
    let (archive, kind) = if modules.exists() {
        (modules, LibraryKind::Jimage)
    } else {
        (home.join("lib/rt.jar"), LibraryKind::Jar)
    };
    if !archive.exists() {
        return None;
    }
    let lib = LibraryId::from_file_path(archive.as_std_path()).ok()?;
    let info = hir::LibraryInfo::new(
        kind,
        AbsPathBuf::assert_utf8(archive.as_std_path().to_owned()),
    );
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(lib, info.clone());
    data.jdk_libraries.push(lib);
    hir::set_project_graph(db, data);
    Some(RealJdk { lib, home, info })
}

/// A registered real JDK: its library id, home directory and `LibraryInfo`.
pub struct RealJdk {
    pub lib: LibraryId,
    pub home: camino::Utf8PathBuf,
    pub info: hir::LibraryInfo,
}

/// Registers `files` as a source set resolving against `jdk` (the JDK produced
/// by [`register_real_jdk`]).
pub fn register_source_set_with_jdk(
    db: &mut TestDatabase,
    jdk: &RealJdk,
    files: &[(&str, &str)],
) -> hir::SourceSetId {
    let mut file_set = FileSet::default();
    for (i, (path, _)) in files.iter().enumerate() {
        file_set.insert(
            FileId::from_raw((i + 1) as u32),
            VfsPath::from(AbsPathBuf::assert_utf8((*path).into())),
        );
    }
    let root = SourceRoot::new(file_set);
    let mut change = FileChange::default();
    change.set_roots(vec![root]);
    for (i, (_, text)) in files.iter().enumerate() {
        change.change_file(FileId::from_raw((i + 1) as u32), Some((*text).to_owned()));
    }
    change.apply(db);

    let source_set = hir::SourceSetId {
        project: hir::ProjectId(0),
        kind: hir::SourceSetKind::Main,
    };
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(jdk.lib, jdk.info.clone());
    data.jdk_libraries.push(jdk.lib);
    data.source_sets.insert(
        source_set.clone(),
        Arc::new(hir::Classpath {
            entries: vec![hir::ClasspathEntry::Library(jdk.lib)],
        }),
    );
    data.source_root_to_source_set
        .insert(SourceRootId(0), source_set.clone());
    hir::set_project_graph(db, data);
    source_set
}

/// A syntax-layer diagnostic in the fixture's currency: message, range and
/// code, mirroring `ide_diagnostics::syntax_diagnostics`.
pub struct SyntaxDiag {
    pub message: String,
    pub range: rowan::TextRange,
    pub code: Option<syntax::DiagnosticCode>,
}

/// Parses `file_id` (a `.java` fixture file) and returns its syntax errors.
pub fn parse_syntax_errors(db: &TestDatabase, file_id: FileId, _text: &str) -> Vec<SyntaxDiag> {
    let parse = base_db::parse(db, file_id, LanguageKind::Java);
    parse
        .errors()
        .iter()
        .map(|e| SyntaxDiag {
            message: e.message.clone(),
            range: e.range,
            code: e.code,
        })
        .collect()
}
