//! Navigation into library sources: goto-definition on a type or member
//! reference that resolves into a dependency jar, the pending state of a
//! library whose source file is not loaded yet, and workspace precedence.

use std::path::PathBuf;

use hir::{Classpath, ClasspathEntry, LibraryInfo, LibraryKind, LibrarySources, ProjectGraphData};
use ide::{Analysis, AnalysisHost};
use ide_db::base_db::{FileChange, SourceRoot, SourceRootId};
use lsp_test::classfile::{build_jar, class_bytes};
use project_model::LibraryId;
use rowan::TextSize;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

const FOO_SRC: &str =
    "package com.example;\n\npublic class Foo {\n    public void greet(int count) {}\n}\n";
const CHILD_SRC: &str = "package com.example;\n\npublic class Child extends Greeter {\n    public void childOnly() {}\n}\n";
const GREETER_SRC: &str = "package com.example;\n\npublic class Greeter extends Root {\n    public void hello(int n) {}\n}\n";
const ROOT_SRC: &str =
    "package com.example;\n\npublic class Root {\n    public void greet(int count) {}\n}\n";
const OVERLOAD_SRC: &str = "package com.example;\n\npublic class Overload {\n    public void run() {}\n\n    public void run(int n) {}\n}\n";
const WORKSPACE_FOO_SRC: &str =
    "package com.example;\n\npublic class Foo {\n    public void greet(int count) {}\n}\n";
const APP_SRC: &str = "package app;\n\nclass App {\n    Object make() {\n        return new com.example.Foo();\n    }\n\n    Object literal() {\n        return com.example.Foo.class;\n    }\n\n    void call(com.example.Child child, com.example.Overload o) {\n        child.greet(1);\n        o.run(1);\n        o.run(1, 2);\n    }\n}\n";

/// The classpath jar's source archive entries, in a fixed order so the file id
/// of a library source is `1000 + index`.
const LIB_SOURCES: &[(&str, &str)] = &[
    ("com/example/Foo.java", FOO_SRC),
    ("com/example/Child.java", CHILD_SRC),
    ("com/example/Greeter.java", GREETER_SRC),
    ("com/example/Root.java", ROOT_SRC),
    ("com/example/Overload.java", OVERLOAD_SRC),
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

fn main_source_set() -> hir::SourceSetId {
    hir::SourceSetId {
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

    let mut change = FileChange::default();
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
    host.apply_change(change);
    hir::set_project_graph(host.raw_database_mut(), data);

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
}

#[test]
fn type_reference_resolves_into_library_sources() {
    let fixture = fixture(&["com/example/Foo.java"], false);

    let targets = fixture.definition("new com.example.Foo()");
    assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
    assert_eq!(targets[0].file, lib_file("com/example/Foo.java"));
    assert!(
        targets[0]
            .range
            .contains(TextSize::new(FOO_SRC.find("class Foo").unwrap() as u32)),
        "the range must cover `class Foo`: {:?}",
        targets[0].range
    );

    // A class literal is a type reference too.
    let targets = fixture.definition("com.example.Foo.class");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].file, lib_file("com/example/Foo.java"));
}

#[test]
fn library_without_sources_yields_no_target() {
    // Nothing is materialized: goto-definition cannot answer without reading
    // the file, and the pending ref names exactly what must be read.
    let fixture = fixture(&[], false);

    assert!(fixture.definition("new com.example.Foo()").is_empty());

    let pending = fixture
        .analysis()
        .pending_library_sources(fixture.app, fixture.offset("new com.example.Foo()"))
        .unwrap();
    assert_eq!(pending.len(), 1, "expected one pending source: {pending:?}");
    assert!(pending[0].path.as_str().ends_with("com/example/Foo.java"));
    assert!(pending[0].archive.as_str().ends_with("deps-sources.jar"));
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
    assert!(targets[0].range.contains(TextSize::new(
        WORKSPACE_FOO_SRC.find("class Foo").unwrap() as u32
    )));
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
    assert!(
        targets[0]
            .range
            .contains(TextSize::new(ROOT_SRC.find("void greet").unwrap() as u32)),
        "the range must cover the `greet` declaration: {:?}",
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
        .pending_library_sources(fixture.app, fixture.offset("child.greet(1)"))
        .unwrap();
    let entries: Vec<&str> = pending.iter().map(|source| source.entry.as_ref()).collect();
    assert_eq!(pending.len(), 2, "expected both owners, got {pending:?}");
    assert!(entries.contains(&"com/example/Greeter.java"), "{entries:?}");
    assert!(entries.contains(&"com/example/Root.java"), "{entries:?}");
}
