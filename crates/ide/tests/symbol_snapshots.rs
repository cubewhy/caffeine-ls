//! Insta snapshot tests for the HIR wiring in `ide`: document/workspace symbol
//! queries and the HIR type layer forwarded through `Analysis`.

use std::{collections::HashMap, sync::Arc};

use hir::{Classpath, ClasspathEntry, ProjectGraphData, SourceSetId, set_project_graph};
use ide::{Analysis, AnalysisHost, DocumentSymbol, WorkspaceSymbol};
use ide_db::base_db::{FileChange, SourceRoot, SourceRootId};
use insta::assert_snapshot;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

fn main_source_set(project: u32) -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(project),
        kind: project_model::SourceSetKind::Main,
    }
}

fn test_source_set(project: u32) -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(project),
        kind: project_model::SourceSetKind::Test,
    }
}

struct Fixture {
    host: AnalysisHost,
    files: HashMap<u32, FileId>,
}

impl Fixture {
    fn analysis(&self) -> Analysis {
        self.host.snapshot()
    }

    fn file(&self, raw: u32) -> FileId {
        self.files[&raw]
    }
}

/// A fixture root: a source set and its files as `(raw_file_id, path, text)`.
type FixtureRoot = (SourceSetId, Vec<(u32, &'static str, &'static str)>);

/// Builds a host from fixture roots.
fn build(roots: &[FixtureRoot]) -> Fixture {
    let mut host = AnalysisHost::new();
    let mut change = FileChange::default();
    let mut all_roots = Vec::new();
    let mut data = ProjectGraphData::default();
    let mut files = HashMap::new();
    for (idx, (source_set, source_files)) in roots.iter().enumerate() {
        let mut file_set = FileSet::default();
        for (raw_id, path, text) in source_files {
            let file_id = FileId::from_raw(*raw_id);
            files.insert(*raw_id, file_id);
            file_set.insert(
                file_id,
                VfsPath::from(AbsPathBuf::assert_utf8(path.to_owned().into())),
            );
            change.change_file(file_id, Some(text.to_string()));
        }
        all_roots.push(SourceRoot::new(file_set));
        data.source_root_to_source_set
            .insert(SourceRootId(idx as u32), source_set.clone());
        data.source_sets.insert(
            source_set.clone(),
            Arc::new(Classpath {
                entries: Vec::<ClasspathEntry>::new(),
            }),
        );
    }
    change.set_roots(all_roots);
    host.apply_change(change);
    set_project_graph(host.raw_database_mut(), data);
    Fixture { host, files }
}

fn render_document_symbols(symbols: &[DocumentSymbol]) -> String {
    symbols
        .iter()
        .map(|symbol| {
            format!(
                "{:<10} {} @{:?}",
                symbol.kind.label(),
                symbol.display_name,
                symbol.range
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_workspace_symbols(symbols: &[WorkspaceSymbol]) -> String {
    let mut lines = symbols
        .iter()
        .map(|symbol| {
            format!(
                "{:<10} {} @file{}",
                symbol.symbol.kind.label(),
                symbol.symbol.display_name,
                symbol.file.index()
            )
        })
        .collect::<Vec<_>>();
    lines.sort();
    lines.join("\n")
}

#[test]
fn document_symbols_snapshot() {
    let fixture = build(&[(
        main_source_set(0),
        vec![(
            1,
            "/src/main/java/com/example/Foo.java",
            "package com.example;\n\npublic class Foo {\n    private int count;\n\n    public void greet() {}\n\n    public void many(int... xs) {}\n\n    public static class Nested {}\n}\n",
        )],
    )]);
    let symbols = fixture
        .analysis()
        .document_symbols(fixture.file(1))
        .unwrap();
    assert_snapshot!(
        "document_symbols_snapshot",
        render_document_symbols(&symbols)
    );
}

#[test]
fn document_symbols_varargs_and_simple_names() {
    let fixture = build(&[(
        main_source_set(0),
        vec![(
            1,
            "/src/com/example/Box.java",
            "package com.example;\n\nclass Box {\n    int value;\n\n    Box(int... xs) {}\n\n    String join(java.util.List<String> items) { return null; }\n\n    class Inner {}\n}\n",
        )],
    )]);
    let analysis = fixture.analysis();
    let symbols = analysis.document_symbols(fixture.file(1)).unwrap();

    let by_name = |name: &str| {
        symbols
            .iter()
            .find(|symbol| symbol.display_name == name)
            .expect("symbol")
    };
    // The simple name is the last `.`-segment, generics simple-rendered.
    assert_eq!(by_name("Box").display_name, "Box");
    assert_eq!(by_name("value: int").display_name, "value: int");
    // Constructor: parameters only, no return type.
    assert_eq!(by_name("Box(int...)").display_name, "Box(int...)");
    // Varargs render with `...`; generic parameter/return types simple.
    assert_eq!(
        by_name("join(List<String>): String").display_name,
        "join(List<String>): String"
    );
    assert_eq!(by_name("Inner").display_name, "Inner");
}

#[test]
fn document_symbol_package_item_and_fqn_detail() {
    let fixture = build(&[
        (
            main_source_set(0),
            vec![(1, "/src/Foo.java", "class Foo {}\n")],
        ),
        (
            main_source_set(1),
            vec![(
                2,
                "/src/com/example/Bar.java",
                "package com.example;\n\nclass Bar {}\n",
            )],
        ),
    ]);
    let analysis = fixture.analysis();

    // Default package: an explicit `<default package>` item above the type,
    // whose detail is its (unqualified) name.
    let foo = analysis.document_symbols(fixture.file(1)).unwrap();
    let package = foo
        .iter()
        .find(|symbol| symbol.kind == hir::SourceSymbolKind::Package)
        .unwrap();
    assert_eq!(package.name, "<default package>");
    assert_eq!(package.display_name, "<default package>");
    assert_eq!(package.item, None);
    let foo_class = foo.iter().find(|symbol| symbol.name == "Foo").unwrap();
    assert_eq!(foo_class.display_name, "Foo");
    assert_eq!(foo_class.detail.as_deref(), Some("Foo"));

    // A declared package gets an item with the qualified name, and the
    // top-level type's detail is its fully qualified name.
    let bar = analysis.document_symbols(fixture.file(2)).unwrap();
    let package = bar
        .iter()
        .find(|symbol| symbol.kind == hir::SourceSymbolKind::Package)
        .unwrap();
    assert_eq!(package.name, "com.example");
    assert_eq!(package.display_name, "com.example");
    let bar_class = bar
        .iter()
        .find(|symbol| symbol.name == "com.example.Bar")
        .unwrap();
    assert_eq!(bar_class.display_name, "Bar");
    assert_eq!(bar_class.detail.as_deref(), Some("com.example.Bar"));
}

#[test]
fn workspace_symbols_snapshot() {
    let fixture = build(&[
        (
            main_source_set(0),
            vec![(
                1,
                "/a/src/main/java/com/example/Foo.java",
                "package com.example;\n\nclass Foo {\n    void many(int... xs) {}\n}\n",
            )],
        ),
        (
            test_source_set(0),
            vec![
                (
                    2,
                    "/b/src/test/java/com/example/api/Bar.java",
                    "package com.example.api;\n\nclass Bar {}\n",
                ),
                (
                    3,
                    "/b/src/test/java/com/example/api/Other.java",
                    "package com.example.api;\n\nclass Other {}\n",
                ),
            ],
        ),
    ]);
    let analysis = fixture.analysis();

    // Case-insensitive prefix search on the simple name.
    let foo = analysis.workspace_symbols("foo").unwrap();
    assert_snapshot!("workspace_symbols_prefix", render_workspace_symbols(&foo));

    // Substring search finds `Bar` and `Other`.
    let bar = analysis.workspace_symbols("ar").unwrap();
    assert_snapshot!(
        "workspace_symbols_substring",
        render_workspace_symbols(&bar)
    );

    // An empty query returns every registered symbol.
    let all = analysis.workspace_symbols("").unwrap();
    assert_snapshot!("workspace_symbols_all", render_workspace_symbols(&all));
}

#[test]
fn item_ty_wired() {
    let fixture = build(&[(
        main_source_set(0),
        vec![(
            1,
            "/src/main/java/com/example/Calc.java",
            "package com.example;\n\nclass Calc {\n    int count;\n    int compute() { return count; }\n}\n",
        )],
    )]);
    let analysis = fixture.analysis();
    let symbols = analysis.document_symbols(fixture.file(1)).unwrap();

    let mut lines = Vec::new();
    for symbol in &symbols {
        // The package symbol has no lowered item.
        let Some(item) = symbol.item else { continue };
        let ty = analysis.item_ty(fixture.file(1), item).unwrap();
        lines.push(format!(
            "{:<10} {} : {ty}",
            symbol.kind.label(),
            symbol.name
        ));
    }
    assert_snapshot!("item_ty_wired", lines.join("\n"));
}

#[test]
fn document_symbols_recompute_when_roots_attach() {
    // A document opened and queried before any source root is applied lowers
    // as Unknown (empty item tree). Attaching the workspace roots afterwards
    // must invalidate the memoized result: the same query then returns the
    // file's symbols instead of replaying the pre-load emptiness (a stale
    // cache is exactly the bug that produced `[]` for id 4 in the field log).
    let mut host = AnalysisHost::new();
    let file_id = FileId::from_raw(1);
    let text = "package org.example;\n\npublic class Foo {\n    public void bar() {}\n}\n";

    let mut change = FileChange::default();
    change.change_file(file_id, Some(text.to_string()));
    host.apply_change(change);
    assert!(
        host.snapshot()
            .document_symbols(file_id)
            .unwrap()
            .is_empty(),
        "pre-load document symbols should be empty (no source root yet)"
    );

    let mut file_set = FileSet::default();
    file_set.insert(
        file_id,
        VfsPath::from(AbsPathBuf::assert_utf8(
            "/src/main/java/org/example/Foo.java".to_owned().into(),
        )),
    );
    let mut data = ProjectGraphData::default();
    data.source_root_to_source_set
        .insert(SourceRootId(0), main_source_set(0));
    data.source_sets.insert(
        main_source_set(0),
        Arc::new(Classpath {
            entries: Vec::<ClasspathEntry>::new(),
        }),
    );
    let mut change = FileChange::default();
    change.set_roots(vec![SourceRoot::new(file_set)]);
    host.apply_change(change);
    set_project_graph(host.raw_database_mut(), data);

    let symbols = host.snapshot().document_symbols(file_id).unwrap();
    assert!(
        !symbols.is_empty(),
        "symbols must recompute once the source root is applied"
    );
    assert_snapshot!(
        "document_symbols_after_roots_attach",
        render_document_symbols(&symbols)
    );
}
