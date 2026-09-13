//! Insta snapshot tests for the per-file doc-comment index: the comment of
//! every documented declaration, the traditional and Markdown comment forms,
//! their boundaries, and the recomputation after an edit.

use hir::hir_def::java::item_tree::{ItemId, ItemTree};
use insta::assert_snapshot;
use vfs::FileId;

mod common;
use common::{Root, RootFile, TestDatabase, build, main_source_set};

fn file(id: u32, path: &'static str, text: &'static str) -> RootFile {
    RootFile {
        id: FileId::from_raw(id),
        path,
        text,
    }
}

/// One file of a fresh database.
fn file_id() -> FileId {
    FileId::from_raw(1)
}

fn database(text: &'static str) -> TestDatabase {
    build(
        &[Root {
            source_set: main_source_set(),
            files: vec![file(1, "/src/main/java/com/example/Doc.java", text)],
            classpath: vec![],
        }],
        &[],
    )
}

/// Renders every declaration of a file in item-tree order: its name, its kind
/// and the text of its doc comment — `"<none>"` when it has none, and the
/// debug-quoted form otherwise so line breaks in the comment stay visible.
fn render_docs(db: &TestDatabase, file: FileId) -> String {
    fn walk(db: &TestDatabase, file: FileId, tree: &ItemTree, id: ItemId, lines: &mut Vec<String>) {
        let data = tree.data(id);
        let name = data
            .name()
            .map_or_else(|| "<anonymous>".to_owned(), |name| name.as_str().to_owned());
        let doc = hir::item_doc(db, file, id)
            .map(|doc| format!("{doc:?}"))
            .unwrap_or_else(|| "<none>".to_owned());
        lines.push(format!("{name} {:?} → {doc}", data.kind()));
        for &child in data.body() {
            walk(db, file, tree, child, lines);
        }
        // A local class-like declaration is not a member, so it is walked from
        // the declaration whose body declares it.
        for local in tree.local_types_of(id) {
            walk(db, file, tree, local, lines);
        }
    }

    let tree = hir::java_item_tree(db, file);
    let mut lines = Vec::new();
    for &top in &tree.top {
        walk(db, file, &tree, top, &mut lines);
    }
    lines.join("\n")
}

const SOURCE: &str = r#"package com.example;

/** Canonical class documentation.
 * <p>Second paragraph.
 */
public class Doc {
    /** The count. */
    private int count;

    /**
     * Greets.
     *
     * @param name who
     * @return the greeting
     */
    public String greet(String name) { return name; }

    /** Both counters. */
    int a, b;

    public void undocumented() {}

    // an ordinary comment, skipped by the scan
    /** The name. */
    private String name;

    /// Markdown line one
    /// line two
    private int markdown;

    /** first */ /** second */
    private int both;

    /** stray */ ;
    int afterStray;

    /** An initializer block is not a documented declaration. */
    static { }

    enum Color {
        /** Red. */
        RED,
        GREEN
    }
}
"#;

#[test]
fn file_docs_declarations() {
    let db = database(SOURCE);
    assert_snapshot!("file_docs_declarations", render_docs(&db, file_id()));
}

/// Two boundaries of a run of Markdown comment lines: a `///` run ends at a
/// blank line, so the comment of `b` is the one closest to it.
#[test]
fn file_docs_comment_run_ends_at_declaration() {
    let source = r#"class C {
    /// one
    /// two
    int a;

    /// three
    /// four

    int b;
}
"#;
    let db = database(source);
    assert_snapshot!(
        "file_docs_comment_run_ends_at_declaration",
        render_docs(&db, file_id())
    );
}

/// Moving a declaration changes which comment documents what: the index is a
/// function of the current file text, not of the item tree.
#[test]
fn file_docs_recompute_after_edit() {
    let mut db = database(SOURCE);
    let before = render_docs(&db, file_id());
    let moved = SOURCE.replace(
        "    /** The count. */\n    private int count;\n",
        "    /** The count. */\n    private int count;\n\n    /** Moved to the front. */\n    private int moved;\n",
    );
    db.edit_file(file_id(), &moved);
    let after = render_docs(&db, file_id());
    assert_snapshot!(
        "file_docs_recompute_after_edit",
        format!("before:\n{before}\n\nafter:\n{after}")
    );
}
