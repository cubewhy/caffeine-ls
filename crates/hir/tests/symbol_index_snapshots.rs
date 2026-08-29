//! Insta snapshot tests for the source symbol index: per-file symbols, the
//! per-source-set FQN table, classpath-order resolution and incremental
//! invalidation.

use hir::{
    ResolutionScope, Resolved, SourceSymbol, SourceSymbolIndex, file_symbols, fqn_resolve,
    source_set_symbols,
};
use insta::assert_snapshot;
use project_model::LibraryId;
use vfs::FileId;

mod common;
use common::{Root, RootFile, build, fixture, main_source_set, test_source_set};

fn file(id: u32, path: &'static str, text: &'static str) -> RootFile {
    RootFile {
        id: FileId::from_raw(id),
        path,
        text,
    }
}

/// Renders per-file symbols in walk order: kind, qualified name, `public`
/// flag and item id.
fn render_symbols(symbols: &[SourceSymbol]) -> String {
    symbols
        .iter()
        .map(|symbol| {
            format!(
                "{:<10} {} public={} item{}",
                symbol.kind.label(),
                symbol.name,
                symbol.public,
                symbol.item.0.0
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders a source-set symbol index as a sorted `name @fileN itemI (kind)`
/// table.
fn render_index(index: &SourceSymbolIndex) -> String {
    let mut lines: Vec<String> = index
        .iter()
        .map(|reference| {
            format!(
                "{:<10} {} @file{} item{}",
                reference.symbol.kind.label(),
                reference.symbol.name,
                reference.file.index(),
                reference.symbol.item.0.0
            )
        })
        .collect();
    lines.sort();
    lines.join("\n")
}

/// Renders the source file an `fqn_resolve` result points at, or the library
/// path for a library result.
fn render_resolved(db: &common::TestDatabase, resolved: Option<&Resolved>) -> String {
    match resolved {
        Some(Resolved::Source(class)) => {
            format!("source @file{} item{}", class.file.index(), class.item.0.0)
        }
        Some(resolved @ Resolved::Library(_)) => format!("library {}", resolved.fqn(db)),
        None => "<none>".to_owned(),
    }
}

#[test]
fn file_symbols_mixed_declarations() {
    let src = r#"package com.example;

public class Outer {
    private int count;
    public static final String NAME = "outer";

    public void greet(String name) {}

    public static class Nested {
        void inner() {}
    }
}

interface Listener {
    void onEvent();
}

enum Color {
    RED,
    GREEN
}

record Point(int x, int y) {}

@interface Marker {}
"#;
    let db = build(
        &[Root {
            source_set: main_source_set(),
            files: vec![file(1, "/src/main/java/com/example/Outer.java", src)],
            classpath: vec![],
        }],
        &[],
    );
    assert_snapshot!(
        "file_symbols_mixed_declarations",
        render_symbols(&file_symbols(&db, FileId::from_raw(1)))
    );
}

#[test]
fn file_symbols_module_info() {
    let src = r#"module com.example.core {
    requires java.base;
    exports com.example.core;
}
"#;
    let db = build(
        &[Root {
            source_set: main_source_set(),
            files: vec![file(1, "/src/main/java/module-info.java", src)],
            classpath: vec![],
        }],
        &[],
    );
    assert_snapshot!(
        "file_symbols_module_info",
        render_symbols(&file_symbols(&db, FileId::from_raw(1)))
    );
}

#[test]
fn source_set_fqn_table() {
    let db = build(
        &[Root {
            source_set: main_source_set(),
            files: vec![
                file(
                    1,
                    "/src/main/java/com/example/Foo.java",
                    "package com.example;\n\npublic class Foo {\n    public void bar() {}\n}\n",
                ),
                file(
                    2,
                    "/src/main/java/com/example/api/Service.java",
                    "package com.example.api;\n\npublic interface Service {}\n",
                ),
            ],
            classpath: vec![],
        }],
        &[],
    );
    assert_snapshot!(
        "source_set_fqn_table",
        render_index(&source_set_symbols(&db, main_source_set()))
    );
}

#[test]
fn classpath_shadowing_across_source_sets() {
    let main = main_source_set();
    let test = test_source_set();
    let db = build(
        &[
            Root {
                source_set: main.clone(),
                files: vec![file(
                    1,
                    "/src/main/java/com/example/Greeter.java",
                    "package com.example;\n\npublic class Greeter {}\n",
                )],
                classpath: vec![],
            },
            Root {
                source_set: test.clone(),
                files: vec![file(
                    2,
                    "/src/test/java/com/example/Greeter.java",
                    "package com.example;\n\npublic class Greeter {}\n",
                )],
                // The test source set depends on main, which defines the
                // same FQN.
                classpath: vec![hir::ClasspathEntry::SourceSet(main.clone())],
            },
        ],
        &[],
    );

    // A source set's own classes win over its classpath.
    assert_snapshot!(
        "classpath_shadowing_own_source_wins",
        render_resolved(
            &db,
            fqn_resolve(
                &db,
                &ResolutionScope::SourceSet(test),
                "com.example.Greeter"
            )
            .as_ref()
        )
    );

    // The dependency is still visible from a scope that does not define it.
    assert_snapshot!(
        "classpath_shadowing_dependency_visible",
        render_resolved(
            &db,
            fqn_resolve(
                &db,
                &ResolutionScope::SourceSet(main),
                "com.example.Greeter"
            )
            .as_ref()
        )
    );
}

#[test]
fn classpath_scoping_hides_unlisted_source_set() {
    let a = main_source_set();
    let b = test_source_set();
    let c = main_source_set_with_project(1);
    let db = build(
        &[
            Root {
                source_set: a.clone(),
                files: vec![file(
                    1,
                    "/a/src/main/java/com/example/Secret.java",
                    "package com.example;\n\nclass Secret {}\n",
                )],
                classpath: vec![],
            },
            Root {
                source_set: b.clone(),
                files: vec![file(
                    2,
                    "/b/src/test/java/com/example/Public.java",
                    "package com.example;\n\nclass Public {}\n",
                )],
                classpath: vec![hir::ClasspathEntry::SourceSet(a.clone())],
            },
            // c depends on b but not on a: b's own classes are visible, a's
            // are not re-exported through b.
            Root {
                source_set: c.clone(),
                files: vec![],
                classpath: vec![hir::ClasspathEntry::SourceSet(b.clone())],
            },
        ],
        &[],
    );

    assert_snapshot!(
        "classpath_scoping_direct_dependency",
        render_resolved(
            &db,
            fqn_resolve(&db, &ResolutionScope::SourceSet(b), "com.example.Secret").as_ref()
        )
    );
    assert_snapshot!(
        "classpath_scoping_transitive_hidden",
        render_resolved(
            &db,
            fqn_resolve(
                &db,
                &ResolutionScope::SourceSet(c.clone()),
                "com.example.Secret"
            )
            .as_ref()
        )
    );
    assert_snapshot!(
        "classpath_scoping_own_classes_visible",
        render_resolved(
            &db,
            fqn_resolve(&db, &ResolutionScope::SourceSet(c), "com.example.Public").as_ref()
        )
    );
}

#[test]
fn fqn_resolve_source_beats_library() {
    let main = main_source_set();
    let (_dir, path) = fixture();
    let jar = path.join("greeter.jar");
    common::build_jar(&jar, "com/example/Greeter");
    let library = LibraryId::from_file_path(jar.as_std_path()).unwrap();
    let library_info = hir::LibraryInfo::new(hir::LibraryKind::Jar, common::abs_path(&jar));

    let db = build(
        &[Root {
            source_set: main.clone(),
            files: vec![file(
                1,
                "/src/main/java/com/example/Greeter.java",
                "package com.example;\n\npublic class Greeter {}\n",
            )],
            classpath: vec![hir::ClasspathEntry::Library(library)],
        }],
        &[(library, library_info)],
    );

    // The classpath carries the same FQN, but the source set's own source
    // declaration shadows it (javac classpath semantics).
    assert_snapshot!(
        "fqn_resolve_source_beats_library",
        render_resolved(
            &db,
            fqn_resolve(
                &db,
                &ResolutionScope::SourceSet(main),
                "com.example.Greeter"
            )
            .as_ref()
        )
    );
}

#[test]
fn edit_invalidates_only_changed_file() {
    let main = main_source_set();
    let a = FileId::from_raw(1);
    let b = FileId::from_raw(2);
    let mut db = build(
        &[Root {
            source_set: main.clone(),
            files: vec![
                file(
                    1,
                    "/src/main/java/com/example/A.java",
                    "package com.example;\n\nclass A {}\n",
                ),
                file(
                    2,
                    "/src/main/java/com/example/B.java",
                    "package com.example;\n\nclass B {}\n",
                ),
            ],
            classpath: vec![],
        }],
        &[],
    );

    let before_b = render_symbols(&file_symbols(&db, b));
    db.edit_file(a, "package com.example;\n\nclass A2 {}\n");

    // Editing A leaves B's symbols untouched.
    assert_eq!(render_symbols(&file_symbols(&db, b)), before_b);

    // The index reflects the new declaration of A only.
    assert_snapshot!(
        "edit_invalidates_only_changed_file_index",
        render_index(&source_set_symbols(&db, main))
    );
}

/// A body-only edit (whitespace inside a method body) must leave the file's
/// symbols and the whole source-set index unchanged: the item tree and the
/// symbols carry no body content, so salsa backdates them and resolution
/// (supertypes, member sets, abstract methods) is not re-run workspace-wide.
#[test]
fn body_edit_keeps_symbols_and_index_stable() {
    let main = main_source_set();
    let a = FileId::from_raw(1);
    let mut db = build(
        &[Root {
            source_set: main.clone(),
            files: vec![file(
                1,
                "/src/main/java/com/example/A.java",
                "package com.example;\nclass A {\n    void m() {\n        int x = 1;\n    }\n}\n",
            )],
            classpath: vec![],
        }],
        &[],
    );

    let before_symbols = render_symbols(&file_symbols(&db, a));
    let before_index = render_index(&source_set_symbols(&db, main.clone()));

    // A body-only edit: a space inside the method body.
    db.edit_file(
        a,
        "package com.example;\nclass A {\n    void m() {\n        int x = 1; \n    }\n}\n",
    );

    assert_eq!(render_symbols(&file_symbols(&db, a)), before_symbols);
    assert_eq!(
        render_index(&source_set_symbols(&db, main)),
        before_index,
        "a body-only edit must not change the source-set symbol index"
    );
}

/// The default main source set of an arbitrary project (for multi-project
/// fixtures).
fn main_source_set_with_project(project: u32) -> hir::SourceSetId {
    hir::SourceSetId {
        project: project_model::ProjectId(project),
        kind: project_model::SourceSetKind::Main,
    }
}
