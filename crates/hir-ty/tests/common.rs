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
use hir::{HirDatabase, HirState, LibraryId, LibraryKind};
use hir_expand::item_tree::{ItemData, ItemId, ItemTree};
use hir_ty::{Ty, is_assignable, is_subtype, supertypes};
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
}

impl TestDatabase {
    pub fn new() -> Self {
        Self {
            storage: salsa::Storage::default(),
            files: Arc::default(),
            deps_map: Arc3::default(),
            nonce: Nonce::new(),
            hir_state: Arc::default(),
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
    fn file_language_kind(&self, file_id: FileId) -> Option<LanguageKind> {
        self.files.file_language_kind(self, file_id)
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
/// descriptors are JVM field/method descriptors.
pub struct ClassSpec<'a> {
    pub fqn: &'a str,
    pub super_class: Option<&'a str>,
    pub interfaces: &'a [&'a str],
    pub access: u16,
    pub fields: &'a [(&'a str, &'a str)],
    pub methods: &'a [(&'a str, &'a str)],
}

pub fn class(
    fqn: &'static str,
    super_class: Option<&'static str>,
    interfaces: &'static [&'static str],
) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class,
        interfaces,
        access: 0x0021, // ACC_PUBLIC | ACC_SUPER
        fields: &[],
        methods: &[],
    }
}

pub fn interface(fqn: &'static str) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class: None,
        interfaces: &[],
        access: 0x0601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
        fields: &[],
        methods: &[],
    }
}

pub fn interface_ext(fqn: &'static str, interfaces: &'static [&'static str]) -> ClassSpec<'static> {
    ClassSpec {
        fqn,
        super_class: None,
        interfaces,
        access: 0x0601,
        fields: &[],
        methods: &[],
    }
}

/// The small JDK subset the tests resolve and subtype against.
pub fn jdk_classes() -> Vec<ClassSpec<'static>> {
    vec![
        class("java/lang/Object", None, &[]),
        interface("java/lang/CharSequence"),
        class(
            "java/lang/String",
            Some("java/lang/Object"),
            &["java/lang/CharSequence"],
        ),
        class("java/lang/Number", Some("java/lang/Object"), &[]),
        class("java/lang/Integer", Some("java/lang/Number"), &[]),
        interface("java/lang/Cloneable"),
        interface("java/io/Serializable"),
        interface("java/util/Collection"),
        interface_ext("java/util/List", &["java/util/Collection"]),
        class(
            "java/util/AbstractList",
            Some("java/lang/Object"),
            &["java/util/List"],
        ),
        class(
            "java/util/ArrayList",
            Some("java/util/AbstractList"),
            &[
                "java/util/List",
                "java/lang/Cloneable",
                "java/io/Serializable",
            ],
        ),
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
    for (name, desc) in methods {
        out.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
        out.extend_from_slice(&name.to_be_bytes());
        out.extend_from_slice(&desc.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // attributes
    }

    out.extend_from_slice(&0u16.to_be_bytes()); // class attributes
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
                lines.push(format!("field {}: {ty}", field.name));
            }
            ItemData::Method(method) => {
                let ret = hir_ty::item_ty(&db, file_id, id);
                let ret = if method.sig.ret.is_none() {
                    "<none>".to_owned()
                } else {
                    ret.to_string()
                };
                let params: Vec<String> = hir_ty::method_params(&db, file_id, id)
                    .iter()
                    .map(|ty| ty.to_string())
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
pub fn check_ty_model(samples: &[(&str, Ty)]) -> String {
    samples
        .iter()
        .map(|(label, ty)| {
            let element = ty
                .element()
                .map(ToString::to_string)
                .unwrap_or_else(|| "<none>".to_owned());
            format!(
                "--- {label} ---\nDISPLAY: {ty}\nERASURE: {}\nFLAGS: {}\nELEMENT: {element}\n",
                ty.erasure(),
                type_flags(ty),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the result of [`Relation`] for each `(sub, sup)` sample.
pub fn check_relations(samples: &[(&str, Ty, Ty, Relation)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let scope = hir::ResolutionScope::Classpath(&[fixture.lib]);

    samples
        .iter()
        .map(|(label, sub, sup, relation)| {
            let result = match relation {
                Relation::Subtype => is_subtype(&db, &scope, sub, sup),
                Relation::Assignable => is_assignable(&db, &scope, sub, sup),
            };
            format!("{label}: {result}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the direct supertypes of each FQN sample.
pub fn check_supertypes(samples: &[&str]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let scope = hir::ResolutionScope::Classpath(&[fixture.lib]);

    samples
        .iter()
        .map(|name| {
            let ty = Ty::reference(*name, Vec::new());
            let supers: Vec<String> = supertypes(&db, &scope, &ty)
                .iter()
                .map(ToString::to_string)
                .collect();
            format!("{name} -> {}", supers.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The enabled classification flags of a [`Ty`].
fn type_flags(ty: &Ty) -> String {
    let mut out = Vec::new();
    if ty.is_void() {
        out.push("void");
    }
    if ty.is_primitive() {
        out.push("primitive");
    }
    if ty.is_reference() {
        out.push("reference");
    }
    if ty.is_type_var() {
        out.push("type-var");
    }
    if ty.is_array() {
        out.push("array");
    }
    if ty.is_wildcard() {
        out.push("wildcard");
    }
    if ty.is_error() {
        out.push("error");
    }
    if ty.is_object() {
        out.push("object");
    }
    if out.is_empty() {
        "<none>".to_owned()
    } else {
        out.join(" ")
    }
}
