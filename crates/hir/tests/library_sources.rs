//! Library source lookup: archive entry → canonical type name, the path an
//! entry materializes to, and the `Pending`/`Loaded` state of a declaration
//! (see `hir::lib_source`).

// The shared fixture module carries helpers this target does not use (the
// classfile/jar builders belong to the other test targets).
#[allow(dead_code)]
mod common;

use common::{
    LibrarySourcesFixture, Root, RootFile, build_with_library_sources, fixture, main_source_set,
};
use hir::{
    LibrarySourceDecl, library_source_decl, library_source_entry, library_source_for_file,
    library_source_path,
};
use project_model::LibraryId;
use vfs::FileId;

const FOO: &str =
    "package com.example;\n\npublic class Foo {\n    public void greet(int x) {}\n}\n";
const OUTER: &str =
    "package com.example;\n\npublic class Outer {\n    public static class Inner {}\n}\n";
const PENDING: &str =
    "package com.example;\n\npublic class Pending {\n    public static class Nested {}\n}\n";
const STRING: &str = "package java.lang;\n\npublic final class String {}\n";
const WORKSPACE: &str = "package demo;\n\nclass A {}\n";

/// The two libraries the fixture registers, their roots, and the database.
struct Fixture {
    _dir: tempfile::TempDir,
    db: common::TestDatabase,
    library: LibraryId,
    jdk: LibraryId,
    library_root: camino::Utf8PathBuf,
    jdk_root: camino::Utf8PathBuf,
}

fn fixture_db() -> Fixture {
    let (dir, base) = fixture();
    let library = LibraryId(1);
    let jdk = LibraryId(2);
    let library_root = base.join("library");
    let jdk_root = base.join("jdk");

    let sources = [
        LibrarySourcesFixture {
            library,
            archive: base.join("lib-a-sources.jar"),
            root: library_root.clone(),
            entries: vec![
                ("com/example/Foo.java", FOO),
                ("com/example/Outer.java", OUTER),
                ("com/example/Pending.java", PENDING),
            ],
            materialized: vec!["com/example/Foo.java", "com/example/Outer.java"],
        },
        LibrarySourcesFixture {
            library: jdk,
            archive: base.join("src.zip"),
            root: jdk_root.clone(),
            entries: vec![("java.base/java/lang/String.java", STRING)],
            materialized: vec!["java.base/java/lang/String.java"],
        },
    ];

    let workspace = Root {
        source_set: main_source_set(),
        files: vec![RootFile {
            id: FileId::from_raw(1),
            path: "/src/main/java/demo/A.java",
            text: WORKSPACE,
        }],
        classpath: Vec::new(),
    };

    let db = build_with_library_sources(&[workspace], &sources, &[]);
    Fixture {
        _dir: dir,
        db,
        library,
        jdk,
        library_root,
        jdk_root,
    }
}

#[test]
fn entry_lookup_lands_on_the_compilation_unit() {
    let fixture = fixture_db();
    let db = &fixture.db;

    assert_eq!(
        library_source_entry(db, fixture.library, "com.example.Foo").as_deref(),
        Some("com/example/Foo.java")
    );
    // A nested type is declared by its outer compilation unit's file.
    assert_eq!(
        library_source_entry(db, fixture.library, "com.example.Outer$Inner").as_deref(),
        Some("com/example/Outer.java")
    );
    assert_eq!(
        library_source_entry(db, fixture.library, "com.example.Outer.Inner").as_deref(),
        Some("com/example/Outer.java")
    );
    // A class the archive does not contain yields nothing.
    assert_eq!(
        library_source_entry(db, fixture.library, "com.example.Missing"),
        None
    );
    // A library without attached sources yields nothing either.
    assert_eq!(
        library_source_entry(db, LibraryId(99), "com.example.Foo"),
        None
    );
}

#[test]
fn module_prefixed_entries_strip_their_module() {
    let fixture = fixture_db();
    let db = &fixture.db;

    assert_eq!(
        library_source_entry(db, fixture.jdk, "java.lang.String").as_deref(),
        Some("java.base/java/lang/String.java")
    );

    let path = library_source_path(db, fixture.jdk, "java.base/java/lang/String.java")
        .expect("the entry materializes under the library root");
    assert_eq!(
        path.as_str(),
        fixture.jdk_root.join("java/lang/String.java").as_str()
    );
}

#[test]
fn declaration_is_pending_until_materialized() {
    let fixture = fixture_db();
    let db = &fixture.db;

    // `Pending.java` is in the archive but was not materialized: the nested
    // type resolves to the outer compilation unit, which still has to be read.
    match library_source_decl(db, fixture.library, "com.example.Pending.Nested") {
        Some(LibrarySourceDecl::Pending { entry, path }) => {
            assert_eq!(&*entry, "com/example/Pending.java");
            assert_eq!(
                path.as_str(),
                fixture
                    .library_root
                    .join("com/example/Pending.java")
                    .as_str()
            );
        }
        other => panic!("expected Pending, got {other:?}"),
    }

    // `Foo.java` is loaded, so the declaration is answered directly.
    match library_source_decl(db, fixture.library, "com.example.Foo") {
        Some(LibrarySourceDecl::Loaded { file, .. }) => {
            assert_eq!(library_source_for_file(db, file), Some(fixture.library));
        }
        other => panic!("expected Loaded, got {other:?}"),
    }

    // The module-prefixed JDK entry materializes below its stripped path.
    match library_source_decl(db, fixture.jdk, "java.lang.String") {
        Some(LibrarySourceDecl::Loaded { file, .. }) => {
            assert_eq!(library_source_for_file(db, file), Some(fixture.jdk));
        }
        other => panic!("expected Loaded, got {other:?}"),
    }

    // A workspace file belongs to no library.
    assert_eq!(library_source_for_file(db, FileId::from_raw(1)), None);
}

#[test]
fn nested_type_resolves_to_its_own_declaration() {
    let fixture = fixture_db();
    let db = &fixture.db;

    // `Outer.Inner` lands on `Outer.java`, and the loaded file's symbol index
    // answers with the *nested* declaration.
    let inner = match library_source_decl(db, fixture.library, "com.example.Outer.Inner") {
        Some(LibrarySourceDecl::Loaded { item, .. }) => item,
        other => panic!("expected Loaded, got {other:?}"),
    };
    let outer = match library_source_decl(db, fixture.library, "com.example.Outer") {
        Some(LibrarySourceDecl::Loaded { item, .. }) => item,
        other => panic!("expected Loaded, got {other:?}"),
    };
    assert_ne!(inner, outer, "the nested type has its own declaration");
}

#[test]
fn anonymous_class_spelling_has_no_declaration() {
    let fixture = fixture_db();
    let db = &fixture.db;

    // `Outer$1` folds to `Outer.1`, which `Outer.java` does not declare — the
    // walk continues past `Outer` and answers honestly with nothing.
    assert_eq!(
        library_source_decl(db, fixture.library, "com.example.Outer$1"),
        None
    );
}
