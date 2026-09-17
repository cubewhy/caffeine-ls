//! Shared fixtures for the hir-ty integration tests: a minimal salsa
//! database implementing [`hir_ty::TyDatabase`] end to end, plus a small
//! classfile/jar builder that hand-encodes the JDK hierarchy the tests
//! resolve against.

#![allow(unused)]

use std::{collections::HashMap, fs::File, io::Write as _};

use rustc_hash::FxHashMap;

use base_db::{
    DepsMap, FileChange, FileSourceRootInput, FileText, Files, LanguageKind, Nonce, SourceDatabase,
    SourceRoot, SourceRootId, SourceRootInput, salsa::Durability,
};
use hir::{
    HirDatabase, HirState, JavaDatabase, JvmDatabase, KotlinDatabase, LibraryId, LibraryInfo,
    LibraryKind, lmdb_store::StubStore,
};
use hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_ty::{DiagLocation, Ty, TyDatabase, is_assignable, is_subtype, supertypes};
pub use ide_diagnostics::{
    body_code, body_message, body_related, decl_code, decl_message, keeps_body_diagnostic,
    keeps_decl_diagnostic,
};
use tempfile::TempDir;
use triomphe::Arc;
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

/// Replaces the text of a single file, keeping the source roots intact (the
/// analogue of the LSP `didChange` path).
pub fn edit_file(db: &mut TestDatabase, file_id: FileId, text: &str) {
    let mut change = FileChange::default();
    change.change_file(file_id, Some(text.to_owned()));
    change.apply(db);
}

/// A temporary JDK-like jar with the class hierarchy used by the tests.
pub struct JdkFixture {
    /// The directory holding the jar. The loader reads the jar *lazily*, so it
    /// must outlive every database the fixture is registered in — a caller
    /// that keeps the database past the fixture's scope leaks this with
    /// [`JdkFixture::keep_alive`] rather than dropping the file out from under
    /// it.
    _dir: TempDir,
    pub jar: camino::Utf8PathBuf,
    pub lib: LibraryId,
}

impl JdkFixture {
    /// Keeps the fixture's directory alive for the rest of the process: the
    /// loaded jar is read on demand, so a database that outlives the fixture
    /// would otherwise resolve nothing from it.
    pub fn keep_alive(self) {
        std::mem::forget(self._dir);
    }

    /// The fixture jar's library registration, as the source set's JDK built-in.
    pub fn library(&self) -> (LibraryId, LibraryInfo) {
        (
            self.lib,
            LibraryInfo::new(
                LibraryKind::Jar,
                AbsPathBuf::assert_utf8(self.jar.as_std_path().to_owned()),
            ),
        )
    }
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

/// A JDK fixture whose SDK also ships a `ct.sym` — the archive javac reads for
/// `javac --release N`
/// ([JEP 247](https://openjdk.org/jeps/247)). The archive is written next to
/// the runtime jar, which is where [`hir::ct_sym`] derives its path from.
pub struct ReleaseFixture {
    pub jdk: JdkFixture,
}

pub fn release_fixture() -> ReleaseFixture {
    let dir = TempDir::new().unwrap();
    let base = camino::Utf8PathBuf::from_path_buf(dir.path().join("fixture")).unwrap();
    std::fs::create_dir_all(&base).unwrap();
    let jar = base.join("jdk.jar");
    build_jar(&jar, &release_jdk_classes());
    let lib = LibraryId::from_file_path(jar.as_std_path()).unwrap();
    build_zip(&base.join("ct.sym"), &release_ct_sym_entries());
    ReleaseFixture {
        jdk: JdkFixture {
            _dir: dir,
            jar,
            lib,
        },
    }
}

/// The runtime classes of the release fixture: the standard fixture, plus the
/// classes the fake `ct.sym` tracks at different releases.
///
/// | class | runtime jar | `ct.sym` `8/` | `ct.sym` `9A/` | `ct.sym` `BCDEFGHIJK/` |
/// |---|---|---|---|---|
/// | `java.util.Api` | all members | `old()`, `<init>()` | `old()`, `<init>()` | `+ newer()`, `<init>(int)`, `FIELD` |
/// | `java.util.Api$Nested` | `<init>()` | — | — | `<init>()` |
/// | `java.util.Later` | `go()` | — | — | `go()` |
/// | `java.util.Sub extends Api` | `<init>()` | `<init>()` | `<init>()` | `<init>()` |
/// | `java.util.Absent` | `<init>()` | — | — | — |
///
/// So `newer`, `FIELD`, `Nested` and `Later` first appear in release 11 (`B`),
/// and `java.util.Absent` is the class the archive never tracks at all — the
/// shape of an internal `jdk.internal.*` class.
pub fn release_jdk_classes() -> Vec<ClassSpec<'static>> {
    let mut classes = jdk_classes();
    let mut api = class_with_methods_access(
        "java/util/Api",
        Some("java/lang/Object"),
        &[],
        &[
            ("old", "()Ljava/lang/String;"),
            ("newer", "()Ljava/lang/String;"),
            ("staticCall", "()V"),
            ("<init>", "()V"),
            ("<init>", "(I)V"),
        ],
        &["", "", "", "", ""],
        // `staticCall` is reached statically, so it must carry ACC_STATIC.
        &[0x0001, 0x0001, 0x0009, 0x0001, 0x0001],
    );
    api.fields = &[("FIELD", "Ljava/lang/String;")];
    classes.push(api);
    classes.push(class_with_methods(
        "java/util/Api$Nested",
        Some("java/lang/Object"),
        &[],
        &[("<init>", "()V")],
        &[""],
    ));
    classes.push(class_with_methods(
        "java/util/Later",
        Some("java/lang/Object"),
        &[],
        &[("go", "()V")],
        &[""],
    ));
    classes.push(class_with_methods(
        "java/util/Sub",
        Some("java/util/Api"),
        &[],
        &[("<init>", "()V")],
        &[""],
    ));
    classes.push(class_with_methods(
        "java/util/Absent",
        Some("java/lang/Object"),
        &[],
        &[("<init>", "()V")],
        &[""],
    ));
    classes
}

/// The entries of the fake `ct.sym`, mirroring the real archive's layout
/// (`<releaseDir>/<module>/<package path>/<SimpleName>.sig`) and its base-36
/// release sets.
fn release_ct_sym_entries() -> Vec<(String, Vec<u8>)> {
    /// A `.sig` for `fqn` carrying `methods` and `fields` — the members that
    /// release's view declares.
    fn sig(
        fqn: &'static str,
        super_class: &'static str,
        methods: &'static [(&'static str, &'static str)],
        method_access: &'static [u16],
        fields: &'static [(&'static str, &'static str)],
    ) -> Vec<u8> {
        class_bytes(&ClassSpec {
            fqn,
            super_class: Some(super_class),
            interfaces: &[],
            access: 0x0021, // ACC_PUBLIC | ACC_SUPER
            fields,
            methods,
            // An empty slice means "no method carries a `Signature`"; the
            // method access defaults to `ACC_PUBLIC` per method.
            method_sigs: &[],
            method_access,
            sig: None,
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            field_access: &[],
            method_deprecations: &[],
            method_defaults: &[],
        })
    }

    const CTOR: &[(&str, &str)] = &[("<init>", "()V")];
    const API_8: &[(&str, &str)] = &[("old", "()Ljava/lang/String;"), ("<init>", "()V")];
    const FIELD_11: &[(&str, &str)] = &[("FIELD", "Ljava/lang/String;")];
    const API_11_STATIC: &[(&str, &str)] = &[
        ("old", "()Ljava/lang/String;"),
        ("newer", "()Ljava/lang/String;"),
        ("staticCall", "()V"),
        ("<init>", "()V"),
        ("<init>", "(I)V"),
    ];
    // `old`, `newer`, the two constructors, then the static method.
    const API_11_ACCESS: &[u16] = &[0x0001, 0x0001, 0x0009, 0x0001, 0x0001];

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    // A real archive lists its directories too; the reader must skip them.
    for dir in ["8", "9A", "BCDEFGHIJK"] {
        entries.push((format!("{dir}/"), Vec::new()));
        entries.push((format!("{dir}/java.base/"), Vec::new()));
    }
    let mut class = |dir: &str, name: &str, bytes: Vec<u8>| {
        entries.push((format!("{dir}/java.base/java/util/{name}.sig"), bytes));
    };
    for dir in ["8", "9A"] {
        class(
            dir,
            "Api",
            sig("java/util/Api", "java/lang/Object", API_8, &[], &[]),
        );
        class(
            dir,
            "Sub",
            sig("java/util/Sub", "java/util/Api", CTOR, &[], &[]),
        );
    }
    class(
        "BCDEFGHIJK",
        "Api",
        sig(
            "java/util/Api",
            "java/lang/Object",
            API_11_STATIC,
            API_11_ACCESS,
            FIELD_11,
        ),
    );
    class(
        "BCDEFGHIJK",
        "Api$Nested",
        sig("java/util/Api$Nested", "java/lang/Object", CTOR, &[], &[]),
    );
    class(
        "BCDEFGHIJK",
        "Later",
        sig(
            "java/util/Later",
            "java/lang/Object",
            &[("go", "()V")],
            &[],
            &[],
        ),
    );
    class(
        "BCDEFGHIJK",
        "Sub",
        sig("java/util/Sub", "java/util/Api", CTOR, &[], &[]),
    );
    // A module descriptor and the surrogate package file: neither is a type a
    // compilation unit can name.
    entries.push((
        "8/java.base/module-info.sig".to_owned(),
        sig("module-info", "java/lang/Object", &[], &[], &[]),
    ));
    entries.push((
        "8/java.base/java/util/package-info.sig".to_owned(),
        sig("java/util/package-info", "java/lang/Object", &[], &[], &[]),
    ));
    entries
}

/// A minimal Kotlin standard library: the classifiers the default imports are
/// claimed to provide, with the shapes kotlinc compiles them to (`kotlin.Int`
/// is a class, `kotlin.collections.List` an interface with one type parameter).
pub fn kotlin_stdlib_classes() -> Vec<ClassSpec<'static>> {
    let class = |fqn: &'static str,
                 super_class: Option<&'static str>,
                 interfaces: &'static [&'static str],
                 access: u16| ClassSpec {
        fqn,
        super_class,
        interfaces,
        access,
        fields: &[],
        field_access: &[],
        methods: &[],
        method_sigs: &[],
        method_access: &[],
        sig: None,
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        method_deprecations: &[],
        method_defaults: &[],
    };
    vec![
        class("kotlin/Any", None, &[], 0x0021),
        class("kotlin/String", Some("kotlin/Any"), &[], 0x0031),
        class("kotlin/Int", Some("kotlin/Number"), &[], 0x0031),
        class("kotlin/Number", Some("kotlin/Any"), &[], 0x0421),
        // The rest of the numeric types and the two other `Char`-adjacent ones:
        // the built-in arithmetic rules are written over them
        // ([KLS
        // `built-in-types-and-their-semantics.html#built-in-integer-arithmetic-operators`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#built-in-integer-arithmetic-operators)).
        class("kotlin/Byte", Some("kotlin/Number"), &[], 0x0031),
        class("kotlin/Short", Some("kotlin/Number"), &[], 0x0031),
        class("kotlin/Long", Some("kotlin/Number"), &[], 0x0031),
        class("kotlin/Float", Some("kotlin/Number"), &[], 0x0031),
        class("kotlin/Double", Some("kotlin/Number"), &[], 0x0031),
        class("kotlin/Char", Some("kotlin/Any"), &[], 0x0031),
        class("kotlin/Boolean", Some("kotlin/Any"), &[], 0x0031),
        class("kotlin/Unit", Some("kotlin/Any"), &[], 0x0031),
        class("kotlin/Nothing", Some("kotlin/Any"), &[], 0x0031),
        // `interface List<out E>` — an interface, hence `ACC_INTERFACE |
        // ACC_ABSTRACT`.
        class("kotlin/collections/List", Some("kotlin/Any"), &[], 0x0601),
        // `interface Function0<out R>`, `Function1<in P1, out R>`,
        // `Function2<in P1, in P2, out R>` — the classifiers a function type
        // `() -> R`, `(P1) -> R`, `(P1, P2) -> R` is.
        class("kotlin/Function0", Some("kotlin/Any"), &[], 0x0601),
        class("kotlin/Function1", Some("kotlin/Any"), &[], 0x0601),
        class("kotlin/Function2", Some("kotlin/Any"), &[], 0x0601),
        // `interface Lazy<out T>`, the classifier a `by lazy { … }` delegate is
        // ([KLS `declarations.html#delegated-property-declaration`] names the
        // form; the classifier rule is the compiler's).
        class("kotlin/Lazy", Some("kotlin/Any"), &[], 0x0601),
        // `abstract class KProperty<V>` (where `kotlin.reflect`'s whole
        // hierarchy is collapsed to the one type a delegated property's
        // `getValue` names).
        class("kotlin/reflect/KProperty", Some("kotlin/Any"), &[], 0x0421),
        // `abstract class Enum<E>`, the implicit supertype of an `enum class`.
        class("kotlin/Enum", Some("kotlin/Any"), &[], 0x0421),
        // `interface Iterable<out T>` with `iterator()`.
        ClassSpec {
            methods: &[("iterator", "()Ljava/util/Iterator;")],
            ..class(
                "kotlin/collections/Iterable",
                Some("kotlin/Any"),
                &[],
                0x0601,
            )
        },
    ]
}

/// A library holding `specs`, plus its id.
pub fn fixture_library(
    dir: &TempDir,
    name: &str,
    specs: &[ClassSpec<'static>],
) -> (hir::LibraryId, AbsPathBuf) {
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().join(name)).unwrap();
    build_jar(&path, specs);
    let abs = AbsPathBuf::assert_utf8(path.as_std_path().to_owned());
    (
        hir::LibraryId::from_file_path(path.as_std_path()).unwrap(),
        abs,
    )
}

/// The Java class the interop tests need and the hand-encoded JDK fixture does
/// not carry: a Swing-shaped class whose members are the shapes kotlinc's
/// interop rules distinguish — a `getDragEnabled()`/`setDragEnabled` pair (the
/// property `dragEnabled`), a `getLayout()`/`setLayout` pair next to a
/// *method* named `layout()` (which is why `container.layout` is the property
/// and `container.layout()` the method), and a `protected` method for the
/// subclass case.
pub fn interop_classes() -> Vec<ClassSpec<'static>> {
    vec![ClassSpec {
        fqn: "javax/swing/JList",
        super_class: Some("java/lang/Object"),
        interfaces: &[],
        access: 0x0021,
        fields: &[],
        field_access: &[],
        methods: &[
            ("<init>", "()V"),
            ("getDragEnabled", "()Z"),
            ("setDragEnabled", "(Z)V"),
            ("getLayout", "()Ljava/lang/Object;"),
            ("setLayout", "(Ljava/lang/Object;)V"),
            ("layout", "()V"),
            ("guarded", "()Ljava/lang/String;"),
        ],
        method_sigs: &["", "", "", "", "", "", ""],
        method_access: &[0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0004],
        sig: None,
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        method_deprecations: &[],
        method_defaults: &[],
    }]
}

/// Registers a source set owning a single source root with `files` (path →
/// text), the JDK fixture as a classpath library. Returns the source set id.
/// The root becomes `SourceRootId(0)` (the first root applied). The source
/// set gets no Java source level, so no source-level check runs for it.
pub fn register_source_set(
    db: &mut TestDatabase,
    fixture: &JdkFixture,
    files: &[(&str, &str)],
) -> hir::SourceSetId {
    register_source_set_at_level(db, fixture, files, None)
}

/// Like [`register_source_set`], but declares the source set's Java source
/// level. A `None` level leaves source-level checks off, which is what every
/// suite but the level tests relies on.
pub fn register_source_set_at_level(
    db: &mut TestDatabase,
    fixture: &JdkFixture,
    files: &[(&str, &str)],
    level: Option<hir::JavaLanguageLevel>,
) -> hir::SourceSetId {
    register_source_set_with(db, fixture.library(), files, level, None)
}

/// Like [`register_source_set_at_level`], but declares the release of the
/// platform API the source set compiles against (`javac --release N`,
/// [JEP 247](https://openjdk.org/jeps/247)) beside its source level. A `None`
/// release leaves the release-view check off.
pub fn register_source_set_at_release(
    db: &mut TestDatabase,
    fixture: &JdkFixture,
    files: &[(&str, &str)],
    level: Option<hir::JavaLanguageLevel>,
    release: Option<u8>,
) -> hir::SourceSetId {
    register_source_set_with(db, fixture.library(), files, level, release)
}

/// Registers `files` as the single source set of `db`, compiled against `jdk`
/// — the library registration of the JDK the source set is built on.
fn register_source_set_with(
    db: &mut TestDatabase,
    jdk: (LibraryId, LibraryInfo),
    files: &[(&str, &str)],
    level: Option<hir::JavaLanguageLevel>,
    release: Option<u8>,
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
    let (jdk_lib, jdk_info) = jdk;
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(jdk_lib, jdk_info);
    data.jdk_libraries.push(jdk_lib);
    data.source_sets.insert(
        source_set.clone(),
        Arc::new(hir::Classpath {
            entries: vec![hir::ClasspathEntry::Library(jdk_lib)],
        }),
    );
    data.source_root_to_source_set
        .insert(SourceRootId(0), source_set.clone());
    if let Some(level) = level {
        data.language_levels.insert(source_set.clone(), level);
    }
    if let Some(release) = release {
        data.releases.insert(source_set.clone(), release);
    }
    hir::set_project_graph(db, data);
    source_set
}

/// Like [`register_source_set`], but additionally records the source root's
/// resolved base directory (as a build system would) in `source_root_dirs`.
/// This lets the package-path diagnostic anchor the file's package directory
/// on the exact base — including for single-file roots, where the file-set
/// heuristic ([`hir::file_package_dir`]) can recover no base.
pub fn register_source_set_with_base(
    db: &mut TestDatabase,
    fixture: &JdkFixture,
    base: &str,
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
    data.source_root_dirs
        .insert(SourceRootId(0), AbsPathBuf::assert_utf8(base.into()));
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
        // A local class-like declaration ([JLS §14.3]) is not a member, so it
        // is not in any `body()`: its own item and its members' are walked
        // from the declaration whose body declares it.
        for local in tree.local_types_of(id) {
            walk(tree, local, out);
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
    let tree = hir_def::java::plugin::tree(db, file);
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
    /// The access flags of each field, parallel to `fields`; an empty slice
    /// means `ACC_PUBLIC` for every field. An enum constant carries
    /// `ACC_ENUM` ([JVMS §4.6]), which is how a classfile declares it.
    pub field_access: &'a [u16],
    pub methods: &'a [(&'a str, &'a str)],
    pub method_sigs: &'a [&'a str],
    /// The access flags of each method, parallel to `methods`; an empty slice
    /// means `ACC_PUBLIC` for every method.
    pub method_access: &'a [u16],
    pub sig: Option<&'a str>,
    /// How the class itself is marked deprecated ([JLS §9.6.4.6]).
    pub deprecation: DeprecationSpec,
    /// How each field is marked deprecated, parallel to `fields`; an empty
    /// slice means no field is.
    pub field_deprecations: &'a [DeprecationSpec],
    /// How each method is marked deprecated, parallel to `methods`; an empty
    /// slice means no method is.
    pub method_deprecations: &'a [DeprecationSpec],
    /// The default value of each method, parallel to `methods`; an empty
    /// slice means no method declares one. javac writes it as the
    /// `AnnotationDefault` attribute ([JVMS §4.7.22]), which makes an
    /// annotation element optional ([JLS §9.7.1]).
    pub method_defaults: &'a [ClassSpecDefault],
}

/// The `AnnotationDefault` of one fixture element ([JVMS §4.7.22]), as the
/// classfile's `element_value` encodes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassSpecDefault {
    /// A `String` default: tag `s` with the `CONSTANT_String` holding the
    /// text.
    String(&'static str),
    /// A `boolean` default: tag `Z` with the `CONSTANT_Integer` holding
    /// `1`/`0` ([JVMS §4.7.22.1]).
    Boolean(bool),
}

/// How a fixture declaration is marked deprecated ([JLS §9.6.4.6]): the
/// classfile `Deprecated` attribute ([JVMS §4.7.15]) and/or a
/// `java.lang.Deprecated` annotation in `RuntimeVisibleAnnotations`, carrying
/// the annotation's `forRemoval` argument when it has one.
///
/// javac writes both markers, but each is independently recognisable, so the
/// fixtures exercise both: a stub built from a *hand-written* classfile may
/// carry either alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeprecationSpec {
    /// Whether to emit the zero-length `Deprecated` attribute.
    pub attribute: bool,
    /// Whether to emit a `java.lang.Deprecated` annotation, and its
    /// `forRemoval` argument: the outer `None` means "no annotation", the
    /// inner one "no argument".
    pub annotation: Option<Option<bool>>,
}

impl DeprecationSpec {
    /// Not deprecated.
    pub const NONE: Self = Self {
        attribute: false,
        annotation: None,
    };
    /// The `Deprecated` attribute alone — the shape a compiler that writes
    /// only the attribute produces.
    pub const ATTRIBUTE: Self = Self {
        attribute: true,
        annotation: None,
    };
    /// A bare `@Deprecated` annotation alone.
    pub const ANNOTATION: Self = Self {
        attribute: false,
        annotation: Some(None),
    };
    /// `@Deprecated(forRemoval = true)`, as a terminal deprecation.
    pub const FOR_REMOVAL: Self = Self {
        attribute: false,
        annotation: Some(Some(true)),
    };
}

pub fn class(
    fqn: &'static str,
    super_class: Option<&'static str>,
    interfaces: &'static [&'static str],
) -> ClassSpec<'static> {
    class_sig(fqn, super_class, interfaces, None)
}

/// Like [`class`], but the class is declared `final` (ACC_FINAL) — the
/// wrapper classes of §4.2.1 are final in the real JDK.
pub fn class_final(
    fqn: &'static str,
    super_class: Option<&'static str>,
    interfaces: &'static [&'static str],
) -> ClassSpec<'static> {
    ClassSpec {
        access: 0x0031, // ACC_PUBLIC | ACC_FINAL | ACC_SUPER
        ..class_sig(fqn, super_class, interfaces, None)
    }
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
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults: &[],
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
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults: &[],
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
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults: &[],
    }
}

/// Like [`class_with_methods_access_sig`], but for a real JDK *interface*
/// whose abstract methods carry `ACC_PUBLIC | ACC_ABSTRACT` ([JLS §9.4]):
/// `ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT` ([JVMS §4.1]).
pub fn interface_with_methods_access_sig(
    fqn: &'static str,
    interfaces: &'static [&'static str],
    methods: &'static [(&'static str, &'static str)],
    method_sigs: &'static [&'static str],
    method_access: &'static [u16],
    sig: Option<&'static str>,
) -> ClassSpec<'static> {
    ClassSpec {
        access: 0x0601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
        ..class_with_methods_access_sig(
            fqn,
            None,
            interfaces,
            methods,
            method_sigs,
            method_access,
            sig,
        )
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
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults: &[],
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
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults: &[],
    }
}

/// An annotation type with element methods, each `(name, descriptor)`
/// ([JLS §9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1)):
/// a real `@SuppressWarnings`-shaped stub whose elements the annotation
/// element-value check can enforce ([JLS §9.7.1]).
pub fn annotation_with_methods(
    fqn: &'static str,
    methods: &'static [(&'static str, &'static str)],
) -> ClassSpec<'static> {
    annotation_with_method_defaults(fqn, methods, &[])
}

/// Like [`annotation_with_methods`], but each element also declares an
/// `AnnotationDefault` ([JVMS §4.7.22]) — the attribute that makes an
/// annotation element optional ([JLS §9.7.1]).
pub fn annotation_with_method_defaults(
    fqn: &'static str,
    methods: &'static [(&'static str, &'static str)],
    method_defaults: &'static [ClassSpecDefault],
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class: None,
        interfaces: &["java/lang/annotation/Annotation"],
        access: 0x2601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT | ACC_ANNOTATION
        fields: &[],
        methods,
        method_sigs: &[],
        method_access: &[],
        sig: None,
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults,
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
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults: &[],
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
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults: &[],
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
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        field_access: &[],
        method_deprecations: &[],
        method_defaults: &[],
    }
}

/// The small JDK subset the tests resolve and subtype against.
pub fn jdk_classes() -> Vec<ClassSpec<'static>> {
    vec![
        // §4.3.2/§8.4.4: `java.lang.Object`'s canonical members — an unbounded
        // type variable's effective upper bound is `Object` ([JLS §4.4]), so
        // its member set (and the `Object` receiver's own) resolves `equals`,
        // `hashCode` and `toString` through this class. The methods mirror the
        // real jimage shapes: `clone` is `protected` and non-final, `finalize`
        // is `protected`, everything else is `public`.
        ClassSpec {
            fqn: "java/lang/Object",
            super_class: None,
            interfaces: &[],
            access: 0x0021,
            methods: &[
                ("getClass", "()Ljava/lang/Class;"),
                ("hashCode", "()I"),
                ("equals", "(Ljava/lang/Object;)Z"),
                ("clone", "()Ljava/lang/Object;"),
                ("toString", "()Ljava/lang/String;"),
                ("notify", "()V"),
                ("notifyAll", "()V"),
                ("wait", "()V"),
                ("wait", "(J)V"),
                ("wait", "(JI)V"),
                ("finalize", "()V"),
            ],
            method_sigs: &["", "", "", "", "", "", "", "", "", "", ""],
            method_access: &[
                0x0001, 0x0001, 0x0001, 0x0004, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001,
                0x0004,
            ],
            sig: None,
            fields: &[],
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            field_access: &[],
            method_deprecations: &[],
            method_defaults: &[],
        },
        // Records have an implicit superclass `java.lang.Record`
        // ([JLS §8.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10)),
        // which declares `equals`, `hashCode` and `toString` abstract
        // ([JLS §8.10.3]); the record's implicit member synthesis implements
        // them. See `jls_records.rs`.
        class_with_methods_access(
            "java/lang/Record",
            Some("java/lang/Object"),
            &[],
            &[
                ("equals", "(Ljava/lang/Object;)Z"),
                ("hashCode", "()I"),
                ("toString", "()Ljava/lang/String;"),
            ],
            &["", "", ""],
            &[0x0401, 0x0401, 0x0401], // ACC_PUBLIC | ACC_ABSTRACT
        ),
        // Real-classfile shape: `java.lang.Class` declares one type parameter
        // (`Class<T>`), which the classfile's `Signature` attribute carries
        // ([JVMS §4.7.9.1]). Without it, a `Class<?>` use would look like
        // arguments on a non-generic class ([§4.5]).
        class_sig(
            "java/lang/Class",
            Some("java/lang/Object"),
            &[],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
        ),
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
        interface_with_methods_access_sig(
            "java/lang/AutoCloseable",
            &[],
            &[("close", "()V")],
            &[""],
            &[0x0411],
            None,
        ),
        interface_with_methods_access_sig(
            "java/io/Closeable",
            &["java/lang/AutoCloseable"],
            &[("close", "()V")],
            &[""],
            &[0x0411],
            None,
        ),
        // `String.CASE_INSENSITIVE_ORDER`: the `Comparator<String>` constant
        // the `thenComparing(Function, Comparator)` chain resolves against.
        ClassSpec {
            fqn: "java/lang/String",
            super_class: Some("java/lang/Object"),
            interfaces: &["java/lang/CharSequence"],
            access: 0x0031,
            fields: &[("CASE_INSENSITIVE_ORDER", "Ljava/util/Comparator;")],
            methods: &[("length", "()I")],
            method_sigs: &[""],
            method_access: &[0x0001],
            sig: None,
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            field_access: &[],
            method_deprecations: &[],
            method_defaults: &[],
        },
        class("java/lang/Number", Some("java/lang/Object"), &[]),
        // §4.2.1: every wrapper class is declared final in the real JDK.
        class_final("java/lang/Integer", Some("java/lang/Number"), &[]),
        class_final("java/lang/Long", Some("java/lang/Number"), &[]),
        class_final("java/lang/Short", Some("java/lang/Number"), &[]),
        class_final("java/lang/Byte", Some("java/lang/Number"), &[]),
        class_final("java/lang/Float", Some("java/lang/Number"), &[]),
        class_final("java/lang/Double", Some("java/lang/Number"), &[]),
        class_final("java/lang/Character", Some("java/lang/Object"), &[]),
        class_final("java/lang/Boolean", Some("java/lang/Object"), &[]),
        functional_interface(
            "java/lang/Runnable",
            "Ljava/lang/Object;",
            &[("run", "()V")],
            &[""],
        ),
        // The real `Function` declares the `<T> Function<T,T> identity()`
        // static factory (ACC_PUBLIC | ACC_STATIC) beside its abstract
        // `apply`; nested-argument inference depends on it, since its own type
        // parameter is fixed by the enclosing formal's *whole* type rather than
        // only by its return position.
        ClassSpec {
            fqn: "java/util/function/Function",
            super_class: None,
            interfaces: &[],
            access: 0x0601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
            fields: &[],
            methods: &[
                ("apply", "(Ljava/lang/Object;)Ljava/lang/Object;"),
                ("identity", "()Ljava/util/function/Function;"),
            ],
            method_sigs: &[
                "(TT;)TR;",
                "<T:Ljava/lang/Object;>()Ljava/util/function/Function<TT;TT;>;",
            ],
            method_access: &[0x0401, 0x0009],
            sig: Some("<T:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;"),
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            field_access: &[],
            method_deprecations: &[],
            method_defaults: &[],
        },
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
        functional_interface(
            "java/util/function/Consumer",
            "<T:Ljava/lang/Object;>Ljava/lang/Object;",
            &[("accept", "(Ljava/lang/Object;)V")],
            &["(TT;)V"],
        ),
        functional_interface(
            "java/util/function/BiFunction",
            "<T:Ljava/lang/Object;U:Ljava/lang/Object;R:Ljava/lang/Object;>Ljava/lang/Object;",
            &[(
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            )],
            &["(TT;TU;)TR;"],
        ),
        // Real `java.util.Comparator` shape: the `compare` SAM is abstract
        // ([JLS §9.4]), the `comparing` factory is static ([§9.4.4]) and the
        // `thenComparing` methods are default ([§9.4.3]). The mixed access
        // flags make `thenComparing(Comparator)` a distinct overload from
        // `thenComparing(Function)` — the pair a method-reference argument must
        // disambiguate by SAM congruence ([§15.13.2]) rather than leave
        // ambiguous ([§15.12.2.5]).
        ClassSpec {
            fqn: "java/util/Comparator",
            super_class: None,
            interfaces: &[],
            access: 0x0601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
            fields: &[],
            methods: &[
                ("compare", "(Ljava/lang/Object;Ljava/lang/Object;)I"),
                (
                    "comparing",
                    "(Ljava/util/function/Function;)Ljava/util/Comparator;",
                ),
                (
                    "thenComparing",
                    "(Ljava/util/function/Function;)Ljava/util/Comparator;",
                ),
                (
                    "thenComparing",
                    "(Ljava/util/Comparator;)Ljava/util/Comparator;",
                ),
                (
                    "thenComparing",
                    "(Ljava/util/function/Function;Ljava/util/Comparator;)Ljava/util/Comparator;",
                ),
                (
                    "comparingInt",
                    "(Ljava/util/function/ToIntFunction;)Ljava/util/Comparator;",
                ),
                (
                    "thenComparingInt",
                    "(Ljava/util/function/ToIntFunction;)Ljava/util/Comparator;",
                ),
            ],
            method_sigs: &[
                "(TT;TT;)I",
                "<T:Ljava/lang/Object;U:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TU;>;)Ljava/util/Comparator<TT;>;",
                "<U:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TU;>;)Ljava/util/Comparator<TT;>;",
                "(Ljava/util/Comparator<-TT;>;)Ljava/util/Comparator<TT;>;",
                "<U:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TU;>;Ljava/util/Comparator<-TU;>;)Ljava/util/Comparator<TT;>;",
                "<T:Ljava/lang/Object;>(Ljava/util/function/ToIntFunction<-TT;>;)Ljava/util/Comparator<TT;>;",
                "(Ljava/util/function/ToIntFunction<-TT;>;)Ljava/util/Comparator<TT;>;",
            ],
            method_access: &[0x0401, 0x0409, 0x0001, 0x0001, 0x0001, 0x0409, 0x0001],
            sig: Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            field_access: &[],
            method_deprecations: &[],
            method_defaults: &[],
        },
        // §9.4.4: the primitive `ToIntFunction` functional interface backing
        // `Comparator.comparingInt`/`thenComparingInt` ([JLS §9.8]).
        functional_interface(
            "java/util/function/ToIntFunction",
            "<T:Ljava/lang/Object;>Ljava/lang/Object;",
            &[("applyAsInt", "(Ljava/lang/Object;)I")],
            &["(TT;)I"],
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
                (
                    "map",
                    "(Ljava/util/function/Function;)Ljava/util/stream/Stream;",
                ),
                ("toList", "()Ljava/util/List;"),
                (
                    "sorted",
                    "(Ljava/util/Comparator;)Ljava/util/stream/Stream;",
                ),
            ],
            &[
                "<R:Ljava/lang/Object;A:Ljava/lang/Object;>(Ljava/util/function/Supplier<TR;>;Ljava/util/function/BiConsumer<TR;-TT;>;Ljava/util/function/BiConsumer<TR;TR;>;)TR;",
                "<R:Ljava/lang/Object;A:Ljava/lang/Object;>(Ljava/util/stream/Collector<-TT;TA;TR;>;)TR;",
                "<R:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TR;>;)Ljava/util/stream/Stream<TR;>;",
                "()Ljava/util/List<TT;>;",
                "(Ljava/util/Comparator<-TT;>;)Ljava/util/stream/Stream<TT;>;",
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
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            field_access: &[],
            method_deprecations: &[],
            method_defaults: &[],
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
            &[
                ("toList", "()Ljava/util/stream/Collector;"),
                // The two factories whose *nested* use relates a
                // `Collector<T, ?, C>` result to the enclosing invocation's
                // own type variables ([§18.2.2]).
                (
                    "toCollection",
                    "(Ljava/util/function/Supplier;)Ljava/util/stream/Collector;",
                ),
                (
                    "collectingAndThen",
                    "(Ljava/util/stream/Collector;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
                ),
            ],
            &[
                // Real classfile shape (JVMS §4.7.9.1): the accumulator
                // position is an unbounded wildcard (`*`, no bound), so
                // inference must contain `α = ?` when reducing
                // ⟨Collector<T,?,List<T>> → Collector<? super T,A,R⟩.
                "<T:Ljava/lang/Object;>()Ljava/util/stream/Collector<TT;*Ljava/util/List<TT;>;>;",
                "<T:Ljava/lang/Object;C::Ljava/util/Collection<TT;>;>(Ljava/util/function/Supplier<TC;>;)Ljava/util/stream/Collector<TT;*TC;>;",
                "<T:Ljava/lang/Object;A:Ljava/lang/Object;R:Ljava/lang/Object;RR:Ljava/lang/Object;>(Ljava/util/stream/Collector<TT;TA;TR;>;Ljava/util/function/Function<-TR;+TRR;>;)Ljava/util/stream/Collector<TT;TA;TRR;>;",
            ],
            &[0x0009, 0x0009, 0x0009], // ACC_PUBLIC | ACC_STATIC
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
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            field_access: &[],
            method_deprecations: &[],
            method_defaults: &[],
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
            Some("<E:Ljava/lang/Enum<TE;>;>Ljava/lang/Object;"),
        ),
        // §8.9.2: an enum's direct superclass is `java.lang.Enum<E>`; the
        // enum collections bound their type parameters by it, so the enum
        // bound-check scenario (`EnumSet<E>`/`EnumMap<K,V>`) needs them.
        class_sig(
            "java/util/EnumSet",
            Some("java/lang/Object"),
            &[],
            Some("<E:Ljava/lang/Enum<TE;>;>Ljava/lang/Object;"),
        ),
        class_sig(
            "java/util/EnumMap",
            Some("java/lang/Object"),
            &[],
            Some("<K:Ljava/lang/Enum<TK;>;V:Ljava/lang/Object;>Ljava/lang/Object;"),
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
        // §9.6.1/§9.7.1: the real `java.lang.Deprecated` declares the
        // optional elements `since` and `forRemoval`, each with an
        // `AnnotationDefault` ([JVMS §4.7.22]) — so a bare `@Deprecated` pairs
        // with nothing and is complete.
        annotation_with_method_defaults(
            "java/lang/Deprecated",
            &[("since", "()Ljava/lang/String;"), ("forRemoval", "()Z")],
            &[
                ClassSpecDefault::String(""),
                ClassSpecDefault::Boolean(false),
            ],
        ),
        annotation("java/lang/Override"),
        // `@SuppressWarnings` elements are enforced by the annotation
        // element-value check ([§9.7.1]); `value()` is `String[]`.
        annotation_with_methods(
            "java/lang/SuppressWarnings",
            &[("value", "()[Ljava/lang/String;")],
        ),
        annotation("java/lang/FunctionalInterface"),
        annotation("java/lang/SafeVarargs"),
        annotation("java/lang/annotation/Annotation"),
        annotation("java/lang/annotation/Documented"),
        // §9.6.1: `Retention.value` and `Target.value` are the real elements
        // of both annotation interfaces, and neither declares a default — an
        // `@Retention(...)`/`@Target(...)` pair must name `value`.
        annotation_with_methods(
            "java/lang/annotation/Retention",
            &[("value", "()Ljava/lang/annotation/RetentionPolicy;")],
        ),
        annotation_with_methods(
            "java/lang/annotation/Target",
            &[("value", "()[Ljava/lang/annotation/ElementType;")],
        ),
        class("java/lang/annotation/RetentionPolicy", None, &[]),
        // §9.6.1/§9.7.1: the `@Target(...)` arguments are enum constants of
        // the real `java.lang.annotation.ElementType`, a public final enum
        // ([JLS §8.9]) whose constants are its `ACC_ENUM`-flagged
        // `public static final` fields ([JVMS §4.1], [§4.6]) — verified with
        // `javap -v java.lang.annotation.ElementType`. Without them the
        // element-value check cannot resolve `ElementType.METHOD` and every
        // `@Target(...)` argument would read as an unknown constant.
        ClassSpec {
            fqn: "java/lang/annotation/ElementType",
            super_class: None,
            interfaces: &[],
            access: 0x4031, // ACC_PUBLIC | ACC_FINAL | ACC_SUPER | ACC_ENUM
            fields: &[
                ("TYPE", "Ljava/lang/annotation/ElementType;"),
                ("FIELD", "Ljava/lang/annotation/ElementType;"),
                ("METHOD", "Ljava/lang/annotation/ElementType;"),
                ("PARAMETER", "Ljava/lang/annotation/ElementType;"),
                ("CONSTRUCTOR", "Ljava/lang/annotation/ElementType;"),
                ("LOCAL_VARIABLE", "Ljava/lang/annotation/ElementType;"),
                ("ANNOTATION_TYPE", "Ljava/lang/annotation/ElementType;"),
                ("PACKAGE", "Ljava/lang/annotation/ElementType;"),
                ("TYPE_PARAMETER", "Ljava/lang/annotation/ElementType;"),
                ("TYPE_USE", "Ljava/lang/annotation/ElementType;"),
                ("MODULE", "Ljava/lang/annotation/ElementType;"),
                ("RECORD_COMPONENT", "Ljava/lang/annotation/ElementType;"),
            ],
            // ACC_PUBLIC | ACC_STATIC | ACC_FINAL | ACC_ENUM
            field_access: &[0x4019; 12],
            methods: &[],
            method_sigs: &[],
            method_access: &[],
            sig: None,
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            method_deprecations: &[],
            method_defaults: &[],
        },
        interface_with_methods(
            "java/lang/Iterable",
            &[],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
            &[
                ("iterator", "()Ljava/util/Iterator;"),
                ("forEach", "(Ljava/util/function/Consumer;)V"),
            ],
            &[
                "()Ljava/util/Iterator<TT;>;",
                "(Ljava/util/function/Consumer<-TT;>;)V",
            ],
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
            &[
                ("iterator", "()Ljava/util/Iterator;"),
                ("stream", "()Ljava/util/stream/Stream;"),
                // The real `Collection<E>` declares `add(E)` itself (as does
                // `List<E>`); without it the stub cannot exercise a write into
                // a `Collection<? super L>` receiver.
                ("add", "(Ljava/lang/Object;)Z"),
            ],
            &[
                "()Ljava/util/Iterator<TE;>;",
                "()Ljava/util/stream/Stream<TE;>;",
                "(TE;)Z",
            ],
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
                ("sort", "(Ljava/util/Comparator;)V"),
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
                "(Ljava/util/Comparator<-TE;>;)V",
                // `List.of(E...)` ([JLS §15.12.2.4] varargs phase): the
                // signature marks the varargs parameter as an array `[TE;`,
                // matching what javac emits for `ACC_VARARGS` methods.
                "<E:Ljava/lang/Object;>([TE;)Ljava/util/List<TE;>;",
            ],
            method_access: &[
                0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001,
                0x0001, // ACC_PUBLIC default `sort(Comparator)`
                0x0409, // ACC_PUBLIC | ACC_STATIC
            ],
            sig: Some("<E:Ljava/lang/Object;>Ljava/lang/Object;Ljava/util/Collection<TE;>;"),
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            field_access: &[],
            method_deprecations: &[],
            method_defaults: &[],
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
                // The generic static factory `Optional.of` a method reference
                // resolves against: `<T> Optional<T> of(T)` ([JLS §15.13.1]
                // inexact reference to a generic method, whose type parameter
                // is only potentially applicable until the enclosing
                // invocation's joint inference instantiates it), and
                // `ofNullable`, whose argument is commonly a nested generic
                // call whose own type variable must be solved before `T` can
                // be ([§18.2.2]).
                ("of", "(Ljava/lang/Object;)Ljava/util/Optional;"),
                ("ofNullable", "(Ljava/lang/Object;)Ljava/util/Optional;"),
            ],
            &[
                "()TT;",
                "<U:Ljava/lang/Object;>(Ljava/util/function/Function<-TT;+TU;>;)Ljava/util/Optional<TU;>;",
                "(TT;)TT;",
                "<T:Ljava/lang/Object;>(TT;)Ljava/util/Optional<TT;>;",
                "<T:Ljava/lang/Object;>(TT;)Ljava/util/Optional<TT;>;",
            ],
            &[0x0001, 0x0001, 0x0001, 0x0009, 0x0009], // of/ofNullable are ACC_PUBLIC | ACC_STATIC
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

    fn integer(&mut self, value: i32) -> u16 {
        let mut entry = Vec::with_capacity(5);
        entry.push(3); // CONSTANT_Integer
        entry.extend_from_slice(&value.to_be_bytes());
        self.alloc(&entry)
    }

    /// A `CONSTANT_String` entry ([JVMS §4.4.3]) over its text's
    /// `CONSTANT_Utf8` — how a classfile spells a `String` element value.
    fn string(&mut self, s: &str) -> u16 {
        let utf8_index = self.utf8(s);
        let mut entry = Vec::with_capacity(3);
        entry.push(8); // CONSTANT_String
        entry.extend_from_slice(&utf8_index.to_be_bytes());
        self.alloc(&entry)
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
pub fn class_bytes(spec: &ClassSpec) -> Vec<u8> {
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
    // The `AnnotationDefault` attribute of each element ([JVMS §4.7.22]),
    // encoded as its single `element_value`: a `String` element as tag `s`
    // with a `CONSTANT_String`, a `boolean` element as tag `Z` with a
    // `CONSTANT_Integer` ([JVMS §4.7.22.1]). Pooled here for the same reason.
    let annotation_default_name = pool.utf8("AnnotationDefault");
    let mut method_defaults: Vec<Vec<u8>> = Vec::with_capacity(spec.methods.len());
    for i in 0..spec.methods.len() {
        let Some(default) = spec.method_defaults.get(i) else {
            method_defaults.push(Vec::new());
            continue;
        };
        let (tag, const_value_index) = match default {
            ClassSpecDefault::String(text) => (b's', pool.string(text)),
            ClassSpecDefault::Boolean(value) => (b'Z', pool.integer(i32::from(*value))),
        };
        let mut attr = Vec::new();
        attr.extend_from_slice(&annotation_default_name.to_be_bytes());
        attr.extend_from_slice(&3u32.to_be_bytes()); // element_value: tag + index
        attr.push(tag);
        attr.extend_from_slice(&const_value_index.to_be_bytes());
        method_defaults.push(attr);
    }

    // Deprecation markers ([JLS §9.6.4.6]): the zero-length `Deprecated`
    // attribute ([JVMS §4.7.15]) and the `java.lang.Deprecated` annotation of
    // a `RuntimeVisibleAnnotations` attribute, whose `forRemoval` argument is
    // an element-value pair with a `boolean` constant.
    let deprecation_attribute = pool.utf8("Deprecated");
    let runtime_visible_annotations = pool.utf8("RuntimeVisibleAnnotations");
    let deprecated_annotation_type = pool.utf8("Ljava/lang/Deprecated;");
    let for_removal_name = pool.utf8("forRemoval");
    let boolean_true = pool.integer(1);
    let boolean_false = pool.integer(0);

    // The attribute block of one declaration: its deprecation markers in
    // source order, prefixed by their count.
    let deprecation_attributes = |marking: DeprecationSpec| -> Vec<u8> {
        let mut attributes: Vec<Vec<u8>> = Vec::new();
        if marking.attribute {
            let mut attr = Vec::new();
            attr.extend_from_slice(&deprecation_attribute.to_be_bytes());
            attr.extend_from_slice(&0u32.to_be_bytes()); // attribute_length
            attributes.push(attr);
        }
        if let Some(for_removal) = marking.annotation {
            let mut attr = Vec::new();
            attr.extend_from_slice(&runtime_visible_annotations.to_be_bytes());
            let mut body = Vec::new();
            body.extend_from_slice(&1u16.to_be_bytes()); // num_annotations
            body.extend_from_slice(&deprecated_annotation_type.to_be_bytes());
            match for_removal {
                Some(value) => {
                    body.extend_from_slice(&1u16.to_be_bytes()); // num_element_value_pairs
                    body.extend_from_slice(&for_removal_name.to_be_bytes());
                    body.push(b'Z'); // boolean
                    let value = if value { boolean_true } else { boolean_false };
                    body.extend_from_slice(&value.to_be_bytes());
                }
                None => body.extend_from_slice(&0u16.to_be_bytes()),
            }
            attr.extend_from_slice(&(body.len() as u32).to_be_bytes());
            attr.extend_from_slice(&body);
            attributes.push(attr);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(attributes.len() as u16).to_be_bytes());
        for attr in attributes {
            out.extend_from_slice(&attr);
        }
        out
    };

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
    for (i, (name, desc)) in fields.iter().enumerate() {
        let field_access = spec.field_access.get(i).copied().unwrap_or(0x0001); // ACC_PUBLIC
        out.extend_from_slice(&field_access.to_be_bytes());
        out.extend_from_slice(&name.to_be_bytes());
        out.extend_from_slice(&desc.to_be_bytes());
        let marking = spec
            .field_deprecations
            .get(i)
            .copied()
            .unwrap_or(DeprecationSpec::NONE);
        out.extend_from_slice(&deprecation_attributes(marking));
    }

    out.extend_from_slice(&(methods.len() as u16).to_be_bytes());
    for (i, (name, desc)) in methods.iter().enumerate() {
        let (sig_name, sig_index) = method_sigs[i];
        let signature = if sig_name == 0 {
            Vec::new()
        } else {
            let mut attr = Vec::new();
            attr.extend_from_slice(&sig_name.to_be_bytes()); // attribute_name_index
            attr.extend_from_slice(&2u32.to_be_bytes()); // attribute_length
            attr.extend_from_slice(&sig_index.to_be_bytes()); // signature_index
            attr
        };
        let marking = spec
            .method_deprecations
            .get(i)
            .copied()
            .unwrap_or(DeprecationSpec::NONE);
        let mut attributes = deprecation_attributes(marking);
        let default = &method_defaults[i];
        let count = u16::from_be_bytes([attributes[0], attributes[1]])
            + u16::from(!signature.is_empty())
            + u16::from(!default.is_empty());
        attributes[..2].copy_from_slice(&count.to_be_bytes());
        attributes.extend_from_slice(&signature);
        attributes.extend_from_slice(default);
        let method_access = spec.method_access.get(i).copied().unwrap_or(0x0001); // ACC_PUBLIC
        out.extend_from_slice(&method_access.to_be_bytes());
        out.extend_from_slice(&name.to_be_bytes());
        out.extend_from_slice(&desc.to_be_bytes());
        out.extend_from_slice(&attributes);
    }

    // class attributes: an optional `Signature` attribute (JVMS §4.7.9.1) and
    // the deprecation markers.
    let mut class_attributes = deprecation_attributes(spec.deprecation);
    if let (Some(sig_name), Some(sig_index)) = (sig_name, sig_index) {
        let count = u16::from_be_bytes([class_attributes[0], class_attributes[1]]) + 1;
        class_attributes[..2].copy_from_slice(&count.to_be_bytes());
        class_attributes.extend_from_slice(&sig_name.to_be_bytes()); // attribute_name_index
        class_attributes.extend_from_slice(&2u32.to_be_bytes()); // attribute_length
        class_attributes.extend_from_slice(&sig_index.to_be_bytes()); // signature_index
    }
    out.extend_from_slice(&class_attributes);
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

/// Builds a ZIP archive with explicit entry names — the shape a `ct.sym`
/// fixture needs, whose entries are `<releaseDir>/<module>/<path>.sig` rather
/// than `<fqn>.class`.
pub fn build_zip(path: &camino::Utf8Path, entries: &[(String, Vec<u8>)]) {
    let file = File::create(path.as_std_path()).unwrap();
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    for (name, bytes) in entries {
        zip.start_file(name.as_str(), options).unwrap();
        zip.write_all(bytes).unwrap();
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
    let tree = hir_def::java::plugin::tree(&db, file_id);

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
    let tree = hir_def::java::plugin::tree(&db, file_id);

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

/// [`check_body_diagnostic_spans`] with an extra third-party library jar
/// (`specs`) on the compile classpath.
pub fn check_body_diagnostic_spans_with_libs(
    specs: &[ClassSpec<'static>],
    files: &[(&str, &str)],
) -> String {
    let (mut db, _extra) = source_set_with_libs(specs, files);
    render_body_diagnostic_spans(&db, files)
}

/// [`check_class_diagnostics`] with an extra third-party library jar (`specs`)
/// on the compile classpath.
pub fn check_class_diagnostics_with_libs(
    specs: &[ClassSpec<'static>],
    files: &[(&str, &str)],
) -> String {
    let (mut db, _extra) = source_set_with_libs(specs, files);
    render_class_diagnostics(&db, files)
}

/// The database of a JDK fixture plus a temporary jar built from `specs`; the
/// returned jar keeps the fixture directory alive.
fn source_set_with_libs(
    specs: &[ClassSpec<'static>],
    files: &[(&str, &str)],
) -> (TestDatabase, TempJar) {
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
    (db, extra)
}

pub fn render_body_types(db: &TestDatabase, files: &[(&str, &str)]) -> String {
    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let tree = hir_def::java::plugin::tree(db, file_id);
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
            let diags: Vec<String> = types
                .diagnostics
                .iter()
                .filter(|diag| {
                    // §9.6.4.5: a warning named by an enclosing
                    // `@SuppressWarnings` is not reported at all.
                    keeps_body_diagnostic(db, file_id, &bodies, diag)
                })
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
                    format!(
                        "{loc}{at}: {}: {}",
                        body_code(diag),
                        body_message(db, diag, &bodies)
                    )
                })
                .collect();
            if !diags.is_empty() {
                lines.push(format!("  diags: {}", diags.join(" | ")));
            }
        }
    }
    normalize_caps(&lines.join("\n"))
}

/// Renders `input` with the capture-variable ids (`CAP#<n>`) renumbered by
/// first-seen order. The ids come from a process-global counter
/// ([`crate::java::ty::capture_conversion`]), so their absolute values depend
/// on test scheduling; normalizing keeps snapshots deterministic.
pub fn normalize_caps(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut map: FxHashMap<String, usize> = FxHashMap::default();
    let mut i = 0;
    while i < input.len() {
        let bytes = input.as_bytes();
        if input[i..].starts_with("CAP#") {
            let mut j = i + 4;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let number = &input[i + 4..j];
            let next = map.len();
            let ordinal = *map.entry(number.to_owned()).or_insert(next);
            out.push_str(&format!("CAP#{ordinal}"));
            i = j;
        } else {
            let ch = input[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Renders the source files and the parsed annotations of every declaration
/// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7),
/// [§9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)):
/// for each item the declaration annotations plus the type-use annotations
/// ([§9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4))
/// of its signature types, with the parsed element-value arguments.
pub fn check_annotations(files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(&mut db, &fixture, files);
    render_annotations(&db, files)
}

/// The source a rendered annotation value reads its *expression* form back
/// from: a value that is not one of the literal forms is lowered as an
/// expression of the file's body tree ([`ItemAnnotationValue::Expr`]), whose
/// source text is the value exactly as written.
struct AnnotationSource<'a> {
    text: &'a str,
    bodies: &'a hir_expand::body::BodyTree,
}

impl AnnotationSource<'_> {
    /// The source text of the expression `expr` spans.
    fn expr_text(&self, expr: hir_expand::body::ExprId) -> String {
        self.bodies
            .expr_range(expr)
            .and_then(|range| {
                let start = u32::from(range.start()) as usize;
                let end = u32::from(range.end()) as usize;
                self.text.get(start..end)
            })
            .unwrap_or_default()
            .to_owned()
    }
}

fn render_annotations(db: &TestDatabase, files: &[(&str, &str)]) -> String {
    use hir_def::java::item_tree::ItemData;
    use hir_def::jvm::decl::{ItemAnnotationRef, ItemAnnotationValue, ItemTypeRef};
    fn render_arg(src: &AnnotationSource<'_>, value: &ItemAnnotationValue) -> String {
        use hir_expand::body::Literal;
        match value {
            ItemAnnotationValue::Literal(Literal::Int(i)) => format!("{i}"),
            ItemAnnotationValue::Literal(Literal::Long(i)) => format!("{i}L"),
            ItemAnnotationValue::Literal(Literal::Char(c)) => format!("'{c}'"),
            ItemAnnotationValue::Literal(Literal::Float) => "f".to_owned(),
            ItemAnnotationValue::Literal(Literal::Double) => "d".to_owned(),
            ItemAnnotationValue::Literal(Literal::Boolean(b)) => format!("{b}"),
            ItemAnnotationValue::Literal(Literal::Str(s)) => format!("\"{s}\""),
            ItemAnnotationValue::EnumConstant { qualifier, member } => match qualifier {
                Some(q) => format!("{}.{}", q.as_str(), member.as_str()),
                None => member.as_str().to_owned(),
            },
            ItemAnnotationValue::ClassLit(ty) => {
                let name = ty
                    .refs
                    .first()
                    .map(|name| name.as_str().to_owned())
                    .unwrap_or_else(|| "<error>".to_owned());
                format!("{name}.class")
            }
            ItemAnnotationValue::Annotation(inner) => render_annotation(src, inner),
            ItemAnnotationValue::Array(values) => format!(
                "{{{}}}",
                values
                    .iter()
                    .map(|v| render_arg(src, v))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ItemAnnotationValue::Expr(expr) => src.expr_text(*expr),
            ItemAnnotationValue::Unresolved { text } => text.clone(),
        }
    }
    fn render_annotation(src: &AnnotationSource<'_>, annotation: &ItemAnnotationRef) -> String {
        let name = annotation.name.as_str();
        if annotation.args.is_empty() {
            format!("@{name}")
        } else {
            let args = annotation
                .args
                .iter()
                .map(|arg| format!("{} = {}", arg.name.as_str(), render_arg(src, &arg.value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("@{name}({args})")
        }
    }
    fn render_annotations_vec(
        src: &AnnotationSource<'_>,
        annotations: &[ItemAnnotationRef],
    ) -> String {
        annotations
            .iter()
            .map(|a| render_annotation(src, a))
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn render_type_annotations(src: &AnnotationSource<'_>, ty: &ItemTypeRef) -> String {
        render_annotations_vec(src, &ty.type_use_annotations)
    }

    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let tree = hir_def::java::plugin::tree(db, file_id);
        let bodies = hir::file_body_tree(db, file_id);
        let src = AnnotationSource {
            text,
            bodies: &bodies,
        };
        for (_id, data) in all_items(&tree) {
            let header = match data {
                ItemData::Class(d) => format!("class {}", d.name.as_str()),
                ItemData::Interface(d) => format!("interface {}", d.name.as_str()),
                ItemData::Enum(d) => format!("enum {}", d.name.as_str()),
                ItemData::Record(d) => format!("record {}", d.name.as_str()),
                ItemData::Annotation(d) => format!("@interface {}", d.name.as_str()),
                ItemData::Method(m) => format!("method {}", m.name.as_str()),
                ItemData::Field(f) => format!("field {}", f.name.as_str()),
                ItemData::EnumConstant(c) => format!("constant {}", c.name.as_str()),
                ItemData::StaticInit(_) => "static {}".to_owned(),
                ItemData::InstanceInit(_) => "instance {}".to_owned(),
                ItemData::Module(d) => format!("module {}", d.name.as_str()),
                _ => continue,
            };
            lines.push(header);
            match data {
                ItemData::Class(d) | ItemData::Interface(d) => {
                    let decl = render_annotations_vec(&src, &d.annotations);
                    if !decl.is_empty() {
                        lines.push(format!("  annotations: {decl}"));
                    }
                    for param in &d.type_params {
                        let anns = render_annotations_vec(&src, &param.annotations);
                        if !anns.is_empty() {
                            lines.push(format!("  type-param {}: {anns}", param.name.as_str()));
                        }
                    }
                }
                ItemData::Enum(d) => {
                    let decl = render_annotations_vec(&src, &d.annotations);
                    if !decl.is_empty() {
                        lines.push(format!("  annotations: {decl}"));
                    }
                }
                ItemData::Record(d) => {
                    let decl = render_annotations_vec(&src, &d.annotations);
                    if !decl.is_empty() {
                        lines.push(format!("  annotations: {decl}"));
                    }
                    for param in &d.type_params {
                        let anns = render_annotations_vec(&src, &param.annotations);
                        if !anns.is_empty() {
                            lines.push(format!("  type-param {}: {anns}", param.name.as_str()));
                        }
                    }
                    for component in &d.components {
                        let anns = render_annotations_vec(&src, &component.annotations);
                        let type_anns = render_type_annotations(&src, &component.ty);
                        let extra = [anns, type_anns]
                            .iter()
                            .filter(|s| !s.is_empty())
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !extra.is_empty() {
                            lines.push(format!("  component {}: {extra}", component.name.as_str()));
                        }
                    }
                }
                ItemData::Annotation(d) => {
                    let decl = render_annotations_vec(&src, &d.annotations);
                    if !decl.is_empty() {
                        lines.push(format!("  annotations: {decl}"));
                    }
                }
                ItemData::Module(d) => {
                    let decl = render_annotations_vec(&src, &d.annotations);
                    if !decl.is_empty() {
                        lines.push(format!("  annotations: {decl}"));
                    }
                }
                ItemData::Method(m) => {
                    let decl = render_annotations_vec(&src, &m.annotations);
                    if !decl.is_empty() {
                        lines.push(format!("  annotations: {decl}"));
                    }
                    for param in &m.sig.type_params {
                        let anns = render_annotations_vec(&src, &param.annotations);
                        if !anns.is_empty() {
                            lines.push(format!("  type-param {}: {anns}", param.name.as_str()));
                        }
                    }
                    let type_anns = m
                        .sig
                        .params
                        .iter()
                        .map(|p| render_type_annotations(&src, &p.ty))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !type_anns.is_empty() {
                        lines.push(format!("  param types: {type_anns}"));
                    }
                    if let Some(ret) = &m.sig.ret {
                        let ret_anns = render_type_annotations(&src, ret);
                        if !ret_anns.is_empty() {
                            lines.push(format!("  return type: {ret_anns}"));
                        }
                    }
                }
                ItemData::Field(f) => {
                    let decl = render_annotations_vec(&src, &f.annotations);
                    if !decl.is_empty() {
                        lines.push(format!("  annotations: {decl}"));
                    }
                    let type_anns = render_type_annotations(&src, &f.ty);
                    if !type_anns.is_empty() {
                        lines.push(format!("  type: {type_anns}"));
                    }
                }
                _ => {}
            }
        }
    }
    lines.join("\n")
}

/// Renders the syntax-layer diagnostics of every file, each as
/// `@line:col..line:col 'covered-text': code: message`, so the friendly
/// ranges of a diagnostic (e.g. the "bad arguments" span of a wrong-arity
/// invocation, [JLS §15.12.2]) are visible verbatim in the snapshot.
pub fn check_syntax_diagnostic_spans(files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(&mut db, &fixture, files);
    render_syntax_spans(&db, files)
}

fn render_syntax_spans(db: &TestDatabase, files: &[(&str, &str)]) -> String {
    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let line_index = line_index::LineIndex::new(text);
        for diag in parse_syntax_errors(db, file_id, text) {
            let span = span_text(&line_index, text, diag.range);
            lines.push(format!(
                "{span}: {}: {}",
                diag.code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "<none>".into()),
                diag.message
            ));
        }
    }
    lines.join("\n")
}

/// Renders the body-type diagnostics of every file with their *full* source
/// ranges (`@line:col..line:col 'covered-text'`) and their
/// `related_information` entries ([JLS §15.12.2], [§5.3]) — the friendly
/// bad-argument spans and the per-argument conversion reasons.
pub fn check_body_diagnostic_spans(files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(&mut db, &fixture, files);
    render_body_diagnostic_spans(&db, files)
}

pub fn render_body_diagnostic_spans(db: &TestDatabase, files: &[(&str, &str)]) -> String {
    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let tree = hir_def::java::plugin::tree(db, file_id);
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
            // §9.6.4.5: a warning named by an enclosing `@SuppressWarnings` is
            // not reported at all, so it neither prints a line nor opens a
            // header for its item.
            let reported: Vec<_> = types
                .diagnostics
                .iter()
                .filter(|diag| keeps_body_diagnostic(db, file_id, &bodies, diag))
                .collect();
            if reported.is_empty() {
                continue;
            }
            lines.push(header);
            for diag in reported {
                let Some(range) = diag.range(&bodies) else {
                    continue;
                };
                let span = span_text(&line_index, text, range);
                lines.push(format!(
                    "  {span}: {}: {}",
                    body_code(diag),
                    body_message(db, diag, &bodies)
                ));
                for (message, rel_range) in body_related(db, diag, &bodies) {
                    let rel_span = span_text(&line_index, text, rel_range);
                    lines.push(format!("    -> {rel_span}: {message}"));
                }
            }
        }
    }
    lines.join("\n")
}

/// Renders a range as `@line:col..line:col 'covered-text'` (0-based columns).
fn span_text(line_index: &line_index::LineIndex, text: &str, range: rowan::TextRange) -> String {
    let start = line_index.line_col(range.start());
    let end = line_index.line_col(range.end());
    let covered = &text[usize::from(range.start())..usize::from(range.end())];
    format!(
        "@{s_line}:{s_col}..{e_line}:{e_col} '{covered}'",
        s_line = start.line,
        s_col = start.col,
        e_line = end.line,
        e_col = end.col
    )
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
    render_class_diagnostics(&db, files)
}

/// Like [`check_class_diagnostics`], but resolving against the *real* JDK at
/// `JAVA_HOME` rather than the fixture jar — the only way a test sees a
/// classfile javac itself compiled, `ConstantValue` attributes
/// ([JVMS §4.7.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.2))
/// and `AnnotationDefault` attributes
/// ([JVMS §4.7.22](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.22))
/// included. `None` with a notice when `JAVA_HOME` is unset or ships no
/// run-time image, so a machine without a JDK still passes.
pub fn check_class_diagnostics_real_jdk(files: &[(&str, &str)]) -> Option<String> {
    let mut db = TestDatabase::new();
    register_source_set_with(&mut db, real_jdk_library()?, files, None, None);
    Some(render_class_diagnostics(&db, files))
}

/// The library registration of the real JDK's run-time image at `JAVA_HOME`,
/// or `None` with a notice when there is no readable image.
fn real_jdk_library() -> Option<(LibraryId, LibraryInfo)> {
    let Ok(java_home) = std::env::var("JAVA_HOME") else {
        eprintln!("skipping: JAVA_HOME is not set");
        return None;
    };
    let archive = camino::Utf8PathBuf::from(java_home)
        .join("lib")
        .join("modules");
    if !archive.as_std_path().is_file() {
        eprintln!("skipping: {archive} does not exist");
        return None;
    }
    let lib = LibraryId::from_file_path(archive.as_std_path()).unwrap();
    Some((
        lib,
        LibraryInfo::new(
            LibraryKind::Jimage,
            AbsPathBuf::assert_utf8(archive.into_std_path_buf()),
        ),
    ))
}

/// Like [`check_class_diagnostics`], but anchors the source root's base
/// directory explicitly (as a build system would) so single-file roots also
/// resolve a package directory.
pub fn check_class_diagnostics_with_base(base: &str, files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set_with_base(&mut db, &fixture, base, files);
    render_class_diagnostics(&db, files)
}

fn render_class_diagnostics(db: &TestDatabase, files: &[(&str, &str)]) -> String {
    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let line_index = line_index::LineIndex::new(text);
        for diag in hir_ty::class_diagnostics(db, file_id) {
            // §9.6.4.5: a warning named by an enclosing `@SuppressWarnings` is
            // not reported at all.
            if !keeps_decl_diagnostic(db, file_id, &diag) {
                continue;
            }
            let at = diag
                .range()
                .map(|r| {
                    let lc = line_index.line_col(r.start());
                    format!("@{line}:{col}", line = lc.line, col = lc.col)
                })
                .unwrap_or_default();
            // A diagnostic keyed to a method (override, @Override, …) is
            // prefixed with its method name; the reference-position and
            // annotation-target diagnostics carry none and render bare.
            let method = diag.method_name();
            if method.is_empty() {
                lines.push(format!(
                    "{at}: {}: {}",
                    decl_code(&diag),
                    decl_message(db, &diag)
                ));
            } else {
                lines.push(format!(
                    "method {}: {}: {}{}",
                    method,
                    decl_code(&diag),
                    at,
                    decl_message(db, &diag)
                ));
            }
        }
    }
    lines.join("\n")
}

/// The module-directive diagnostics ([JLS §7.7]) of every `module-info.java`
/// in the source files, rendered like [`check_class_diagnostics`].
pub fn check_module_diagnostics(files: &[(&str, &str)]) -> String {
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
        for diag in hir_ty::module_diagnostics(&db, file_id) {
            if !keeps_decl_diagnostic(&db, file_id, &diag) {
                continue;
            }
            let at = diag
                .range()
                .map(|r| {
                    let lc = line_index.line_col(r.start());
                    format!("@{line}:{col}", line = lc.line, col = lc.col)
                })
                .unwrap_or_default();
            lines.push(format!(
                "{at}: {}: {}",
                decl_code(&diag),
                decl_message(&db, &diag)
            ));
        }
    }
    lines.join("\n")
}

/// The source-level diagnostics of the source files, rendered like
/// [`check_class_diagnostics`]: one line per construct newer than the
/// declared source level. Files with no level registered report nothing.
pub fn check_level_diagnostics(level: hir::JavaLanguageLevel, files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set_at_level(&mut db, &fixture, files, Some(level));
    render_level_diagnostics(&db, files)
}

/// Like [`check_level_diagnostics`], but with no source level declared for the
/// source set: every file's report must be empty.
pub fn check_level_diagnostics_unknown(files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set_at_level(&mut db, &fixture, files, None);
    render_level_diagnostics(&db, files)
}

fn render_level_diagnostics(db: &TestDatabase, files: &[(&str, &str)]) -> String {
    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let line_index = line_index::LineIndex::new(text);
        for diag in hir_ty::level_diagnostics(db, file_id) {
            if !keeps_decl_diagnostic(db, file_id, &diag) {
                continue;
            }
            let at = diag
                .range()
                .map(|r| {
                    let lc = line_index.line_col(r.start());
                    format!("@{line}:{col}", line = lc.line, col = lc.col)
                })
                .unwrap_or_default();
            lines.push(format!(
                "{at}: {}: {}",
                decl_code(&diag),
                decl_message(db, &diag)
            ));
        }
    }
    lines.join("\n")
}

/// The source-level report of the `RECORD_SOURCE` file after loading the
/// workspace twice at two different levels, to prove a reload re-derives the
/// report rather than serving the first level's memoized answer.
pub fn check_level_diagnostics_across_reloads(
    first: hir::JavaLanguageLevel,
    second: hir::JavaLanguageLevel,
    files: &[(&str, &str)],
) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set_at_level(&mut db, &fixture, files, Some(first));
    let before = render_level_diagnostics(&db, files);
    register_source_set_at_level(&mut db, &fixture, files, Some(second));
    let after = render_level_diagnostics(&db, files);
    format!(
        "--- at -source {}\n{before}\n--- at -source {}\n{after}",
        first.source, second.source
    )
}

/// The source-level report of a file that was first queried *before* the
/// workspace loaded, then again after. Mirrors the driver order an editor
/// produces: a document is opened (its text exists, attributed to the
/// pre-workspace fallback source root) and diagnostics are pulled for it before
/// the build system reports the project; only then do the source root and the
/// project graph arrive. The pre-load answer must not survive the load.
pub fn check_level_diagnostics_across_first_load(
    level: hir::JavaLanguageLevel,
    files: &[(&str, &str)],
) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();

    // Before the load: the file has text and therefore a source-root input (the
    // fallback root), but no root has been registered and no project graph has
    // been set.
    let mut change = FileChange::default();
    for (i, (_, text)) in files.iter().enumerate() {
        change.change_file(FileId::from_raw((i + 1) as u32), Some((*text).to_owned()));
    }
    change.apply(&mut db);
    let before = render_level_diagnostics(&db, files);

    // The workspace load: the source root and the project graph both arrive.
    register_source_set_at_level(&mut db, &fixture, files, Some(level));
    let after = render_level_diagnostics(&db, files);

    format!(
        "--- queried before the workspace loaded\n{before}\n--- queried after the load\n{after}"
    )
}

/// The platform-release report of the source files
/// (`javac --release N`, [JEP 247](https://openjdk.org/jeps/247)), rendered as
/// `@{line}:{col}: {code}: {message}` — one line per declaration diagnostic
/// and per body-inference diagnostic, ordered by file then source order. A
/// `None` release reports nothing at all.
pub fn check_release_diagnostics(release: Option<u8>, files: &[(&str, &str)]) -> String {
    let fixture = release_fixture();
    let mut db = TestDatabase::new();
    register_source_set_at_release(&mut db, &fixture.jdk, files, None, release);
    render_release_diagnostics(&db, files)
}

/// The platform-release report of the source files after loading the
/// workspace twice at two different releases, to prove a reload re-derives the
/// report rather than serving the first release's memoized answer.
pub fn check_release_diagnostics_across_reloads(
    first: u8,
    second: u8,
    files: &[(&str, &str)],
) -> String {
    let fixture = release_fixture();
    let mut db = TestDatabase::new();
    register_source_set_at_release(&mut db, &fixture.jdk, files, None, Some(first));
    let before = render_release_diagnostics(&db, files);
    register_source_set_at_release(&mut db, &fixture.jdk, files, None, Some(second));
    let after = render_release_diagnostics(&db, files);
    format!("--- at release {first}\n{before}\n--- at release {second}\n{after}")
}

fn render_release_diagnostics(db: &TestDatabase, files: &[(&str, &str)]) -> String {
    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let line_index = line_index::LineIndex::new(text);
        let at = |range: rowan::TextRange| {
            let lc = line_index.line_col(range.start());
            format!("@{line}:{col}", line = lc.line, col = lc.col)
        };
        for diag in hir_ty::class_diagnostics(db, file_id) {
            if !keeps_decl_diagnostic(db, file_id, &diag) {
                continue;
            }
            let Some(range) = diag.range() else {
                continue;
            };
            lines.push(format!(
                "{}: {}: {}",
                at(range),
                decl_code(&diag),
                decl_message(db, &diag)
            ));
        }
        let tree = hir_def::java::plugin::tree(db, file_id);
        let bodies = hir::file_body_tree(db, file_id);
        for (id, _) in all_items(&tree) {
            let Some(types) = hir_ty::body_types(db, file_id, id) else {
                continue;
            };
            for diag in &types.diagnostics {
                if !keeps_body_diagnostic(db, file_id, &bodies, diag) {
                    continue;
                }
                let Some(range) = diag.range(&bodies) else {
                    continue;
                };
                lines.push(format!(
                    "{}: {}: {}",
                    at(range),
                    body_code(diag),
                    body_message(db, diag, &bodies)
                ));
            }
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
        let tree = hir_def::java::plugin::tree(&db, file_id);
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
        // The context class renders as the name a diagnostic would use, so
        // the snapshot stays readable whether the class is named or *local*
        // ([JLS §6.7]).
        let enclosing = ctx
            .enclosing_class
            .as_ref()
            .map(|class| class.display_name(&db).as_str().to_owned());
        lines.push(format!(
            "{label}: {rendered} [ctx: class={:?}, package={:?}] [args: {}]",
            enclosing,
            ctx.package,
            arg_types.join(", ")
        ));
    }
    lines.join("\n")
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
