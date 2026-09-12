//! Navigation into library sources: goto-definition on a type or member
//! reference that resolves into a dependency jar, the pending state of a
//! library whose source file is not loaded yet, and workspace precedence.

use std::path::PathBuf;

use ide::{
    Analysis, AnalysisHost, Change, Classpath, ClasspathEntry, LibraryFileRef, LibraryId,
    LibraryInfo, LibraryKind, LibrarySources, ProjectGraphData, SourceSetId,
};
use ide_db::base_db::{SourceRoot, SourceRootId};
use lsp_test::classfile::{
    ACC_PUBLIC, ACC_VARARGS, build_jar, class_bytes, class_bytes_with_methods,
};
use rowan::TextSize;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

const FOO_SRC: &str =
    "package com.example;\n\npublic class Foo {\n    public void greet(int count) {}\n}\n";
const CHILD_SRC: &str = "package com.example;\n\npublic class Child extends Greeter {\n    public void childOnly() {}\n}\n";
const GREETER_SRC: &str = "package com.example;\n\npublic class Greeter extends Root {\n    public void hello(int n) {}\n}\n";
const ROOT_SRC: &str =
    "package com.example;\n\npublic class Root {\n    public void greet(int count) {}\n}\n";
const OVERLOAD_SRC: &str = "package com.example;\n\npublic class Overload {\n    public void run() {}\n\n    public void run(int n) {}\n}\n";
/// The classfile of `Pair` declares `combine(int, int)`, the source only
/// `combine(int first)`: the parameter-name merge then takes `first` from the
/// source and falls back to an index name for the second parameter.
const PAIR_SRC: &str =
    "package com.example;\n\npublic class Pair {\n    public void combine(int first) {}\n}\n";
/// Members of one name and parameter *count*, told apart only by their
/// parameter types — each `int` form declared first, so a lookup that knows no
/// more than the arity answers it for every one-argument reference.
const LOADER_SRC: &str = "package com.example;\n\npublic class Loader {\n    public Loader(int size) {}\n\n    public Loader(Root root) {}\n\n    public void load(int size) {}\n\n    public void load(Root root) {}\n\n    public void add(int size) {}\n\n    public void add(Root... roots) {}\n}\n";
/// The classfile of `Widget` declares `<init>(int)`; the source declares that
/// same constructor under the class's own name. A class instance creation has
/// to answer with that declaration — not with the class the classfile's
/// `<init>` belongs to — while the implicit `<init>()V` `class_bytes` always
/// emits names no source declaration.
const WIDGET_SRC: &str =
    "package com.example;\n\npublic class Widget {\n    public Widget(int size) {}\n}\n";
const WORKSPACE_FOO_SRC: &str =
    "package com.example;\n\npublic class Foo {\n    public void greet(int count) {}\n}\n";
const APP_SRC: &str = "package app;\n\nclass App {\n    Object make() {\n        return new com.example.Foo();\n    }\n\n    Object literal() {\n        return com.example.Foo.class;\n    }\n\n    Object widget() {\n        return new com.example.Widget(1);\n    }\n\n    Object loader() {\n        return new com.example.Loader(new com.example.Root());\n    }\n\n    void use(com.example.Loader loader) {\n        loader.load(new com.example.Root());\n        loader.add(new com.example.Root());\n    }\n\n    void call(com.example.Child child, com.example.Overload o, com.example.Foo f, com.example.Pair p) {\n        child.greet(1);\n        o.run(1);\n        o.run(1, 2);\n        f.greet(1);\n        p.combine(1, 2);\n    }\n}\n";

/// The classpath jar's source archive entries, in a fixed order so the file id
/// of a library source is `1000 + index`.
const LIB_SOURCES: &[(&str, &str)] = &[
    ("com/example/Foo.java", FOO_SRC),
    ("com/example/Child.java", CHILD_SRC),
    ("com/example/Greeter.java", GREETER_SRC),
    ("com/example/Root.java", ROOT_SRC),
    ("com/example/Overload.java", OVERLOAD_SRC),
    ("com/example/Pair.java", PAIR_SRC),
    ("com/example/Widget.java", WIDGET_SRC),
    ("com/example/Loader.java", LOADER_SRC),
];

/// The file id the library fixture assigns to `entry`.
fn lib_file(entry: &str) -> FileId {
    let index = LIB_SOURCES
        .iter()
        .position(|(name, _)| *name == entry)
        .unwrap_or_else(|| panic!("{entry} is not a fixture source"));
    FileId::from_raw(1000 + index as u32)
}

fn abs(path: PathBuf) -> AbsPathBuf {
    AbsPathBuf::assert_utf8(path)
}

fn main_source_set() -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(0),
        kind: project_model::SourceSetKind::Main,
    }
}

/// A workspace root plus a dependency jar and its sibling `-sources.jar`.
struct Fixture {
    _dir: tempfile::TempDir,
    host: AnalysisHost,
    app: FileId,
}

/// Builds the fixture. `materialized` lists the library source entries loaded
/// into the library source root now; the rest stay in the archive (or, for
/// entries not listed at all, do not exist). `workspace_foo` adds a same-named
/// class to the workspace.
fn fixture(materialized: &[&str], workspace_foo: bool) -> Fixture {
    let dir = tempfile::TempDir::new().unwrap();
    let base = dir.path().to_path_buf();
    let jar = base.join("lib/deps.jar");
    let sources_jar = base.join("lib/deps-sources.jar");
    let lib_root = base.join("sources/deps");

    let classes: Vec<(String, Vec<u8>)> = vec![
        (
            "com/example/Foo.class".to_owned(),
            class_bytes("com/example/Foo", "java/lang/Object", &[], &[("greet", 1)]),
        ),
        (
            "com/example/Child.class".to_owned(),
            class_bytes("com/example/Child", "com/example/Greeter", &[], &[]),
        ),
        (
            "com/example/Greeter.class".to_owned(),
            class_bytes(
                "com/example/Greeter",
                "com/example/Root",
                &[],
                &[("hello", 1)],
            ),
        ),
        (
            "com/example/Root.class".to_owned(),
            class_bytes("com/example/Root", "java/lang/Object", &[], &[("greet", 1)]),
        ),
        (
            "com/example/Overload.class".to_owned(),
            class_bytes(
                "com/example/Overload",
                "java/lang/Object",
                &[],
                &[("run", 0), ("run", 1)],
            ),
        ),
        (
            "com/example/Pair.class".to_owned(),
            class_bytes(
                "com/example/Pair",
                "java/lang/Object",
                &[],
                &[("combine", 2)],
            ),
        ),
        // `class_bytes` always emits the default `<init>()V`; the explicit
        // `<init>(int)` is the constructor the source declares.
        (
            "com/example/Widget.class".to_owned(),
            class_bytes(
                "com/example/Widget",
                "java/lang/Object",
                &[],
                &[("<init>", 1)],
            ),
        ),
        // One-parameter constructors and methods distinguished only by the
        // descriptor, plus a variable-arity method: its signature erases to
        // the array `Root[]`, which is what the classfile carries.
        (
            "com/example/Loader.class".to_owned(),
            class_bytes_with_methods(
                "com/example/Loader",
                "java/lang/Object",
                &[],
                &[
                    ("<init>", "(I)V", ACC_PUBLIC),
                    ("<init>", "(Lcom/example/Root;)V", ACC_PUBLIC),
                    ("load", "(I)V", ACC_PUBLIC),
                    ("load", "(Lcom/example/Root;)V", ACC_PUBLIC),
                    ("add", "(I)V", ACC_PUBLIC),
                    ("add", "([Lcom/example/Root;)V", ACC_PUBLIC | ACC_VARARGS),
                ],
            ),
        ),
    ];
    let class_entries: Vec<(&str, Vec<u8>)> = classes
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect();
    build_jar(&jar, &class_entries).unwrap();
    build_jar(
        &sources_jar,
        &LIB_SOURCES
            .iter()
            .map(|(name, text)| (*name, text.as_bytes().to_vec()))
            .collect::<Vec<_>>(),
    )
    .unwrap();

    let app = FileId::from_raw(1);
    let workspace_foo_file = FileId::from_raw(2);

    let mut change = Change::default();
    let mut root = FileSet::default();
    root.insert(app, VfsPath::from(abs(base.join("src/app/App.java"))));
    change.change_file(app, Some(APP_SRC.to_owned()));
    if workspace_foo {
        root.insert(
            workspace_foo_file,
            VfsPath::from(abs(base.join("src/com/example/Foo.java"))),
        );
        change.change_file(workspace_foo_file, Some(WORKSPACE_FOO_SRC.to_owned()));
    }

    let mut library_root = FileSet::default();
    for (index, (name, text)) in LIB_SOURCES.iter().enumerate() {
        if !materialized.contains(name) {
            continue;
        }
        let file = FileId::from_raw(1000 + index as u32);
        library_root.insert(file, VfsPath::from(abs(lib_root.join(name))));
        change.change_file(file, Some((*text).to_owned()));
    }
    change.set_roots(vec![
        SourceRoot::new(root),
        SourceRoot::library(library_root),
    ]);

    let library = LibraryId::from_file_path(&jar).unwrap();
    let mut data = ProjectGraphData::default();
    data.libraries.insert(
        library,
        LibraryInfo::new(LibraryKind::Jar, abs(jar.clone())),
    );
    data.source_sets.insert(
        main_source_set(),
        triomphe::Arc::new(Classpath {
            entries: vec![ClasspathEntry::Library(library)],
        }),
    );
    data.source_root_to_source_set
        .insert(SourceRootId(0), main_source_set());
    data.library_sources.insert(
        library,
        LibrarySources {
            archive: abs(sources_jar),
            root: abs(lib_root),
        },
    );
    data.library_source_roots.insert(SourceRootId(1), library);

    let mut host = AnalysisHost::new();
    change.set_project_graph(data);
    host.apply_change(change);

    Fixture {
        _dir: dir,
        host,
        app,
    }
}

impl Fixture {
    fn analysis(&self) -> Analysis {
        self.host.snapshot()
    }

    /// The body offset of the first `needle` in `App.java`, at its last
    /// character so it stays inside the whole expression without landing on its
    /// receiver (which would resolve as a local declarator instead).
    fn offset(&self, needle: &str) -> TextSize {
        let start = APP_SRC.find(needle).expect("needle in App.java");
        TextSize::new((start + needle.len() - 1) as u32)
    }

    fn definition(&self, needle: &str) -> Vec<ide::NavigationTarget> {
        self.analysis()
            .goto_definition(self.app, self.offset(needle))
            .unwrap()
    }

    fn hover(&self, needle: &str) -> Option<String> {
        self.analysis()
            .hover(self.app, self.offset(needle))
            .unwrap()
            .map(|info| info.value)
    }
}

/// The source text a navigation target's range covers: the declared name, not
/// the whole declaration the definition was resolved from.
fn target_text<'a>(text: &'a str, target: &ide::NavigationTarget) -> &'a str {
    &text[u32::from(target.range.start()) as usize..u32::from(target.range.end()) as usize]
}

/// The range of the `name` identifier in the declaration `declaration` of
/// `text` — the identifier a definition at that declaration must cover.
fn declared_name_range(text: &str, declaration: &str, name: &str) -> rowan::TextRange {
    let at = text
        .find(declaration)
        .unwrap_or_else(|| panic!("the declaration {declaration:?} is not in the fixture"));
    let at = at
        + text[at..]
            .find(name)
            .unwrap_or_else(|| panic!("{name:?} is not in {declaration:?}"));
    rowan::TextRange::new(
        TextSize::new(at as u32),
        TextSize::new((at + name.len()) as u32),
    )
}

#[test]
fn type_reference_resolves_into_library_sources() {
    let fixture = fixture(&["com/example/Foo.java"], false);

    let targets = fixture.definition("new com.example.Foo()");
    assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
    assert_eq!(targets[0].file, lib_file("com/example/Foo.java"));
    assert_eq!(
        target_text(FOO_SRC, &targets[0]),
        "Foo",
        "the definition is the class's own name: {:?}",
        targets[0].range
    );
    // §8.8.9: the classfile's implicit `<init>()V` is the constructor this
    // creation resolves to, and it has no source declaration of its own — the
    // class is the answer, and the classfile's name for it is not.
    assert_eq!(targets[0].name, "Foo");

    // A class literal is a type reference too.
    let targets = fixture.definition("com.example.Foo.class");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].file, lib_file("com/example/Foo.java"));
}

/// §15.9: a class instance creation names the constructor it selected — the
/// declaration the class writes its own name at, not the class the classfile's
/// `<init>` belongs to.
#[test]
fn constructor_creation_resolves_to_the_library_constructor() {
    let fixture = fixture(&["com/example/Widget.java"], false);

    let targets = fixture.definition("new com.example.Widget");
    assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
    assert_eq!(targets[0].file, lib_file("com/example/Widget.java"));
    assert_eq!(targets[0].name, "Widget");
    assert_eq!(
        targets[0].range,
        declared_name_range(WIDGET_SRC, "public Widget(int size)", "Widget"),
        "the definition is the constructor's own name, not the class's"
    );
}

/// §15.12.2.2/[§8.4.2]: two constructors of one parameter *count* are told
/// apart by their parameter types — the erasure the classfile declares and the
/// declaration the source writes — so the creation names the constructor its
/// argument selected, never the first of that arity.
#[test]
fn constructor_overloads_of_one_arity_resolve_by_type() {
    let fixture = fixture(&["com/example/Loader.java", "com/example/Root.java"], false);

    let targets = fixture.definition("new com.example.Loader");
    assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
    assert_eq!(targets[0].file, lib_file("com/example/Loader.java"));
    assert_eq!(
        targets[0].range,
        declared_name_range(LOADER_SRC, "public Loader(Root root)", "Loader"),
        "the constructor the argument's type selects, not the one of its arity"
    );
}

/// §15.12.2.2: the same for a method invocation, and for a variable-arity
/// declaration — whose signature erases to the array the classfile declares
/// ([§8.4.1]) while the source writes the `...` form.
#[test]
fn method_overloads_of_one_arity_resolve_by_type() {
    let fixture = fixture(&["com/example/Loader.java", "com/example/Root.java"], false);

    let targets = fixture.definition("loader.load");
    assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
    assert_eq!(targets[0].file, lib_file("com/example/Loader.java"));
    assert_eq!(
        targets[0].range,
        declared_name_range(LOADER_SRC, "public void load(Root root)", "load"),
        "the overload the argument's type selects, not the one of its arity"
    );

    let targets = fixture.definition("loader.add");
    assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
    assert_eq!(
        targets[0].range,
        declared_name_range(LOADER_SRC, "public void add(Root... roots)", "add"),
        "the variable-arity overload's own declaration"
    );
}

#[test]
fn library_without_sources_yields_no_target() {
    // Nothing is materialized: goto-definition cannot answer without reading
    // the file, and the pending ref names exactly what must be read.
    let fixture = fixture(&[], false);

    assert!(fixture.definition("new com.example.Foo()").is_empty());

    let pending = fixture
        .analysis()
        .pending_library_files(fixture.app, fixture.offset("new com.example.Foo()"))
        .unwrap();
    assert_eq!(pending.len(), 1, "expected one pending source: {pending:?}");
    let LibraryFileRef::Source { archive, path, .. } = &pending[0] else {
        panic!("expected a pending source, got {:?}", pending[0]);
    };
    assert!(path.as_str().ends_with("com/example/Foo.java"));
    assert!(archive.as_str().ends_with("deps-sources.jar"));
}

#[test]
fn workspace_declaration_shadows_the_library() {
    let fixture = fixture(&["com/example/Foo.java"], true);

    let targets = fixture.definition("new com.example.Foo()");
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].file,
        FileId::from_raw(2),
        "the workspace declaration must win over the library's"
    );
    assert_eq!(target_text(WORKSPACE_FOO_SRC, &targets[0]), "Foo");
}

#[test]
fn member_declared_on_a_supertype_resolves_there() {
    let fixture = fixture(
        &[
            "com/example/Child.java",
            "com/example/Greeter.java",
            "com/example/Root.java",
        ],
        false,
    );

    // `Child extends Greeter extends Root`; `greet` is declared only on `Root`,
    // so the walk descends the whole hierarchy and lands on the declaring file.
    let targets = fixture.definition("child.greet(1)");
    assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
    assert_eq!(targets[0].file, lib_file("com/example/Root.java"));
    assert_eq!(
        target_text(ROOT_SRC, &targets[0]),
        "greet",
        "the definition is the member's own name: {:?}",
        targets[0].range
    );
}

#[test]
fn overload_arity_prefers_a_match_then_falls_back_to_the_name() {
    let fixture = fixture(&["com/example/Overload.java"], false);

    let targets = fixture.definition("o.run(1)");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].file, lib_file("com/example/Overload.java"));
    let one_parameter = targets[0].range;

    // An arity no overload declares falls back to the name-only match.
    let targets = fixture.definition("o.run(1, 2)");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].file, lib_file("com/example/Overload.java"));
    assert_ne!(
        targets[0].range, one_parameter,
        "the fallback is a name-only match, not the one-parameter overload"
    );
}

#[test]
fn unloaded_hierarchy_reports_every_owner_in_one_round() {
    // Only `Child` is loaded; the walk still collects both unloaded owners of
    // its hierarchy in a single call, so the request needs one round.
    let fixture = fixture(&["com/example/Child.java"], false);

    assert!(fixture.definition("child.greet(1)").is_empty());

    let pending = fixture
        .analysis()
        .pending_library_files(fixture.app, fixture.offset("child.greet(1)"))
        .unwrap();
    let entries: Vec<&str> = pending.iter().map(pending_source_entry).collect();
    assert_eq!(pending.len(), 2, "expected both owners, got {pending:?}");
    assert!(entries.contains(&"com/example/Greeter.java"), "{entries:?}");
    assert!(entries.contains(&"com/example/Root.java"), "{entries:?}");
}

#[test]
fn recorded_member_and_type_reference_agree_on_one_pending_source() {
    let fixture = fixture(&[], false);

    // The offset sits on the `new`'s type name: inference records the
    // constructor's declaring class, the declaration-side walk reads the same
    // written name, and the classpath walk resolves it as a class — all three
    // name `com/example/Foo.java`, and the pending set lists it once.
    let offset =
        TextSize::new((APP_SRC.find("new com.example.Foo()").unwrap() + "new ".len()) as u32);
    assert!(
        fixture
            .analysis()
            .goto_definition(fixture.app, offset)
            .unwrap()
            .is_empty()
    );

    let pending = fixture
        .analysis()
        .pending_library_files(fixture.app, offset)
        .unwrap();
    assert_eq!(
        pending.len(),
        1,
        "expected one deduplicated pending source: {pending:?}"
    );
    let LibraryFileRef::Source { archive, path, .. } = &pending[0] else {
        panic!("expected a pending source, got {:?}", pending[0]);
    };
    assert!(path.as_str().ends_with("com/example/Foo.java"));
    assert!(archive.as_str().ends_with("deps-sources.jar"));
}

#[test]
fn hover_shows_the_merged_signature() {
    let fixture = fixture(&["com/example/Foo.java"], false);

    // The hand-built classfile carries no `MethodParameters` attribute, so the
    // parameter name can only come from the source declaration.
    assert_eq!(
        fixture.hover("f.greet(1)").as_deref(),
        Some("void greet(int count)")
    );
}

#[test]
fn hover_on_an_unloaded_member_reports_the_pending_source() {
    let fixture = fixture(&[], false);

    // Nothing can be rendered before the source is loaded; the LSP layer reads
    // the pending files and re-runs the request.
    assert_eq!(fixture.hover("f.greet(1)"), None);

    let pending = fixture
        .analysis()
        .pending_library_files(fixture.app, fixture.offset("f.greet(1)"))
        .unwrap();
    let entries: Vec<&str> = pending.iter().map(pending_source_entry).collect();
    assert!(
        entries.contains(&"com/example/Foo.java"),
        "the declaring source must be pending: {entries:?}"
    );
}

#[test]
fn parameters_with_no_source_name_fall_back_to_an_index() {
    let fixture = fixture(&["com/example/Pair.java"], false);

    // The stub declares two parameters, the source only names one: the second
    // has no name anywhere and renders as an index.
    assert_eq!(
        fixture.hover("p.combine(1, 2)").as_deref(),
        Some("void combine(int first, int arg1)")
    );
}

/// The archive entry of a pending file the load has to read. A decompile ref is
/// not a source entry and fails the test that asked for one.
fn pending_source_entry(file: &LibraryFileRef) -> &str {
    match file {
        LibraryFileRef::Source { entry, .. } => entry.as_ref(),
        LibraryFileRef::Decompile { class, .. } => {
            panic!("expected a pending source entry, got a decompile of {class}")
        }
    }
}
