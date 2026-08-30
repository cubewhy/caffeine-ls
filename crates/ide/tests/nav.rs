//! Insta snapshot tests for name-level navigation in `ide`: goto-definition
//! and hover at an offset, resolved through the HIR layer (see [`ide::nav`]).

use triomphe::Arc;

use hir::{Classpath, ProjectGraphData, SourceSetId, set_project_graph};
use ide::{Analysis, AnalysisHost};
use ide_db::base_db::{FileChange, SourceRoot, SourceRootId};
use insta::assert_snapshot;
use rowan::TextSize;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

fn main_source_set(project: u32) -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(project),
        kind: project_model::SourceSetKind::Main,
    }
}

struct Fixture {
    host: AnalysisHost,
    file: FileId,
    text: String,
}

impl Fixture {
    fn analysis(&self) -> Analysis {
        self.host.snapshot()
    }

    /// The byte offset of `needle` (first occurrence), pointing into the
    /// middle so a token boundary never clips it.
    fn offset(&self, needle: &str) -> TextSize {
        let idx = self
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("needle {needle:?} not found in:\n{}", self.text));
        TextSize::new((idx + needle.len() / 2) as u32)
    }
}

/// Renders the navigation targets of `offset` under the given position, and
/// the hover there, in a deterministic snapshot-friendly form.
fn render_nav(fixture: &Fixture, needle: &str) -> String {
    let analysis = fixture.analysis();
    let range = fixture.offset(needle);
    let targets = analysis.goto_definition(fixture.file, range).unwrap();
    let hover = analysis.hover(fixture.file, range).unwrap();
    let targets = targets
        .iter()
        .map(|t| format!("{} @{:?}", t.name, t.range))
        .collect::<Vec<_>>()
        .join("\n");
    let hover = hover.as_ref().map(|h| h.value.as_str()).unwrap_or("<none>");
    format!("--- goto @{needle:?} ---\n{targets}\n--- hover ---\n{hover}")
}

const SRC: &str = r#"package com.example;

class Nav {
    int count;

    int compute() {
        int local = count;
        return local;
    }

    int add(int a, int b) {
        return a + b;
    }

    void call() {
        int r = add(1, 2);
    }

    Nav fresh() {
        return new Nav();
    }
}
"#;

#[test]
fn goto_local_use() {
    let fixture = test_file(SRC);
    assert_snapshot!("goto_local_use", render_nav(&fixture, "local;"));
}

#[test]
fn goto_implicit_field_read() {
    let fixture = test_file(SRC);
    assert_snapshot!("goto_implicit_field_read", render_nav(&fixture, "= count;"));
}

#[test]
fn goto_method_call() {
    let fixture = test_file(SRC);
    assert_snapshot!("goto_method_call", render_nav(&fixture, "add(1, 2)"));
}

#[test]
fn goto_type_reference() {
    let fixture = test_file(SRC);
    assert_snapshot!("goto_type_reference", render_nav(&fixture, "new Nav"));
}

#[test]
fn hover_over_expression() {
    let fixture = test_file(SRC);
    // An expression's type — the local `local` use in `return local;`.
    assert_snapshot!("hover_expression_type", render_nav(&fixture, "local;"));
}

#[test]
fn hover_over_field_declaration() {
    let fixture = test_file(SRC);
    // The field declaration's signature — hover on its declarator name.
    assert_snapshot!("hover_field_declaration", render_nav(&fixture, "count;\n"));
}

#[test]
fn hover_over_method_declaration() {
    let fixture = test_file(SRC);
    // The method declaration's signature — hover on its name.
    assert_snapshot!("hover_method_declaration", render_nav(&fixture, "int add("));
}

#[test]
fn hover_over_class_declaration() {
    let fixture = test_file(SRC);
    // The class declaration's signature.
    assert_snapshot!("hover_class_declaration", render_nav(&fixture, "class Nav"));
}

// -- shadowing ([JLS §6.3]/[§6.4]): the innermost in-scope declarator wins ---------

const SHADOW_SRC: &str = r#"package com.example;

class Nav {
    void m() {
        int dup = 1;
        {
            int dup = 2;
            take(dup);
        }
    }

    void take(int v) {}
}
"#;

#[test]
fn goto_shadowed_local_use() {
    let fixture = test_file(SHADOW_SRC);
    // The use inside the inner block resolves to the *inner* declaration,
    // not the first same-named one of the body.
    assert_snapshot!("goto_shadowed_local_use", render_nav(&fixture, "(dup)"));
}

// -- `$` in identifiers ([JLS §3.8]) ---------------------------------------------
// `$` is an ordinary identifier character, so `A$B`, `x$y` and `m$1` are whole
// names: navigation must not split them into their last `$` segment.

const DOLLAR_SRC: &str = r#"package com.example;

class A$B {
    int x$y;

    void m$1() {}

    void use() {
        new A$B().m$1();
    }
}
"#;

#[test]
fn goto_dollar_named_type() {
    let fixture = test_file(DOLLAR_SRC);
    assert_snapshot!("goto_dollar_named_type", render_nav(&fixture, "new A$B"));
}

#[test]
fn goto_dollar_named_method() {
    let fixture = test_file(DOLLAR_SRC);
    assert_snapshot!("goto_dollar_named_method", render_nav(&fixture, "m$1();"));
}

#[test]
fn hover_over_dollar_class_declaration() {
    let fixture = test_file(DOLLAR_SRC);
    assert_snapshot!(
        "hover_over_dollar_class_declaration",
        render_nav(&fixture, "class A$B")
    );
}

#[test]
fn hover_over_dollar_field_declaration() {
    let fixture = test_file(DOLLAR_SRC);
    assert_snapshot!(
        "hover_over_dollar_field_declaration",
        render_nav(&fixture, "x$y;\n")
    );
}

fn test_file(text: &str) -> Fixture {
    let mut host = AnalysisHost::new();
    let file = FileId::from_raw(1);
    let path = "/src/main/java/com/example/Nav.java";

    let mut change = FileChange::default();
    let mut file_set = FileSet::default();
    file_set.insert(
        file,
        VfsPath::from(AbsPathBuf::assert_utf8(path.to_owned().into())),
    );
    change.change_file(file, Some(text.to_string()));
    change.set_roots(vec![SourceRoot::new(file_set)]);

    let mut data = ProjectGraphData::default();
    data.source_root_to_source_set
        .insert(SourceRootId(0), main_source_set(0));
    data.source_sets.insert(
        main_source_set(0),
        Arc::new(Classpath {
            entries: Vec::new(),
        }),
    );
    host.apply_change(change);
    set_project_graph(host.raw_database_mut(), data);

    Fixture {
        host,
        file,
        text: text.to_owned(),
    }
}
