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
    /// The byte offset of `needle`'s first character, for its `occurrence`-th
    /// (0-based) occurrence. The matrix cases anchor on the reference's own
    /// first character, so nothing written before it can shift the offset onto
    /// a receiver or an enclosing declaration.
    fn offset_start(&self, needle: &str, occurrence: usize) -> TextSize {
        let mut from = 0;
        for _ in 0..occurrence {
            let found = self.text[from..]
                .find(needle)
                .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"));
            from += found + needle.len();
        }
        let idx = self.text[from..]
            .find(needle)
            .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"));
        TextSize::new((from + idx) as u32)
    }
}

/// Renders one `needle` section per case — the offset at the start of the
/// case's `occurrence`-th occurrence — in the same shape as [`render_nav`].
fn render_nav_many(fixture: &Fixture, cases: &[(&str, usize)]) -> String {
    cases
        .iter()
        .map(|&(needle, occurrence)| render_nav_at(fixture, needle, occurrence))
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`render_nav`] with the offset of the `occurrence`-th occurrence of
/// `needle`, at the needle's first character.
fn render_nav_at(fixture: &Fixture, needle: &str, occurrence: usize) -> String {
    render(
        fixture,
        fixture.offset_start(needle, occurrence),
        format!("--- goto @{needle:?}#{occurrence} ---"),
    )
}

/// Renders the navigation targets at `offset` and the hover there under
/// `header`, in a deterministic snapshot-friendly form.
fn render(fixture: &Fixture, offset: TextSize, header: String) -> String {
    let analysis = fixture.analysis();
    let targets = analysis.goto_definition(fixture.file, offset).unwrap();
    let hover = analysis.hover(fixture.file, offset).unwrap();
    let targets = targets
        .iter()
        .map(|t| format!("{} @{:?}", t.name, t.range))
        .collect::<Vec<_>>()
        .join("\n");
    let hover = hover.as_ref().map(|h| h.value.as_str()).unwrap_or("<none>");
    format!("{header}\n{targets}\n--- hover ---\n{hover}")
}

/// The declaration text every target of the reference at `needle`'s
/// `occurrence`-th occurrence covers, in the fixture's own source.
fn goto_slices<'a>(fixture: &'a Fixture, needle: &str, occurrence: usize) -> Vec<&'a str> {
    let offset = fixture.offset_start(needle, occurrence);
    fixture
        .analysis()
        .goto_definition(fixture.file, offset)
        .unwrap()
        .iter()
        .map(|target| {
            &fixture.text
                [u32::from(target.range.start()) as usize..u32::from(target.range.end()) as usize]
        })
        .collect()
}
/// Renders the navigation targets of `offset` under the given position, and
/// the hover there, in a deterministic snapshot-friendly form.
fn render_nav(fixture: &Fixture, needle: &str) -> String {
    render(
        fixture,
        fixture.offset(needle),
        format!("--- goto @{needle:?} ---"),
    )
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

// -- a single-file matrix of references and the declarations they denote -------------
// The fixture's source set has an empty classpath, so only same-file names
// resolve: every target below is a declaration of this file.

const MANY_SRC: &str = r#"package com.example;

import com.example.Base;
import static com.example.Base.STATIC;

@interface Marker {}

enum E {
    FIRST,
    SECOND
}

interface Factory {
    int size(Base b);
}

class Box<T extends Base> {}

class Base {
    static int STATIC = 1;

    int count;

    Base self;

    void method(int n) {}

    void method(long n) {}

    void pick(int n) {}

    void pick(long n) {}
}

class Impl implements Factory {
    public int size(Base b) {
        return 0;
    }
}

class Sub extends Base {
    @Marker
    int marked;

    Base make() {
        return null;
    }

    void use(Base b, Sub other, Box<Base> boxed) {
        count = b.count;
        self = b.self;
        method(1);
        super.method(1);
        b.method(1L);
        pick(1L);
        int s = Base.STATIC;
        E e = E.FIRST;
        switch (e) {
            case SECOND:
                break;
        }
        Factory f = (Base q) -> q.count;
        Base cast = (Base) other;
        boolean is = other instanceof Base;
        Class<?> lit = Base.class;
        Base[] arr = new Base[1];
        int local = 0;
        int missing = nope + 1;
    }
}
"#;

/// The body and member references of [`MANY_SRC`]: the needle locates the
/// reference — at the first character of its `occurrence`-th (0-based)
/// occurrence — and the string is the declaration text its single target
/// range covers.
const MANY_MEMBER_GOTO: &[(&str, usize, &str)] = &[
    // §6.5.6.1: a bare name is a field of the implicit `this`; the qualified
    // form names the same declaration through the receiver.
    ("count = b", 0, "count"),
    ("count;", 1, "count"),
    ("self = b", 0, "self"),
    ("self;", 1, "self"),
    // §15.12.2: overload selection — the `long` overload of each name, not the
    // first same-named declaration of the file, and not the arity-equal one.
    ("method(1);", 0, "void method(int n) {}"),
    ("super.method(1)", 0, "void method(int n) {}"),
    ("method(1L)", 0, "void method(long n) {}"),
    ("pick(1L)", 0, "void pick(long n) {}"),
    // A static field and the enum constants.
    ("STATIC;", 1, "STATIC = 1"),
    ("FIRST;", 0, "FIRST"),
    ("SECOND:", 0, "SECOND"),
    // §15.27.2: the body IR carries a lambda parameter as a name/range pair,
    // not as a local, so the use names its declarator.
    ("q.count", 0, "q"),
];

/// The references of [`MANY_SRC`] that name no declaration: a local's own
/// declarator (§6.3 scopes a local from its declarator on, so a declaration is
/// not a reference to itself) and a name nothing declares.
const MANY_MEMBER_NONE: &[(&str, usize)] = &[("local = 0", 0), ("nope + 1", 0)];

#[test]
fn goto_reference_matrix() {
    let fixture = test_file(MANY_SRC);
    for &(needle, occurrence, declaration) in MANY_MEMBER_GOTO {
        assert_eq!(
            goto_slices(&fixture, needle, occurrence),
            vec![declaration],
            "case {needle:?}#{occurrence}"
        );
    }
    for &(needle, occurrence) in MANY_MEMBER_NONE {
        assert_eq!(
            goto_slices(&fixture, needle, occurrence),
            Vec::<&str>::new(),
            "case {needle:?}#{occurrence}"
        );
    }
    let mut cases: Vec<(&str, usize)> = MANY_MEMBER_GOTO
        .iter()
        .map(|&(needle, occurrence, _)| (needle, occurrence))
        .collect();
    cases.extend(MANY_MEMBER_NONE.iter().copied());
    assert_snapshot!("goto_reference_matrix", render_nav_many(&fixture, &cases));
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
