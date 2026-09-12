//! Insta snapshot tests for name-level navigation in `ide`: goto-definition
//! and hover at an offset, resolved through the HIR layer (see [`ide::nav`]).

use triomphe::Arc;

use ide::{
    Analysis, AnalysisHost, Change, Classpath, NavigationTarget, ProjectGraphData, SourceSetId,
};
use ide_db::base_db::{SourceRoot, SourceRootId};
use insta::assert_snapshot;
use rowan::{TextRange, TextSize};
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

/// Asserts that the reference at `needle`'s `occurrence`-th occurrence resolves
/// to exactly one target — the declaration named `name`, whose range covers
/// `declaration`, a needle inside that declaration's own text.
fn assert_goto_covers(
    fixture: &Fixture,
    needle: &str,
    occurrence: usize,
    name: &str,
    declaration: &str,
) {
    let offset = fixture.offset_start(needle, occurrence);
    let targets = fixture
        .analysis()
        .goto_definition(fixture.file, offset)
        .unwrap();
    assert_eq!(
        targets.len(),
        1,
        "case {needle:?}#{occurrence}: {targets:?}"
    );
    assert_eq!(targets[0].name, name, "case {needle:?}#{occurrence}");
    let inside = TextSize::new(
        fixture
            .text
            .find(declaration)
            .unwrap_or_else(|| panic!("{declaration:?} is not in the fixture")) as u32,
    );
    assert!(
        targets[0].range.contains(inside),
        "case {needle:?}#{occurrence}: {:?} must cover {declaration:?}",
        targets[0].range
    );
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

    <T> T id(T v) {
        T copy = null;
        return copy;
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
        local = local + 1;
        int missing = nope + 1;
    }
}

class Generic<K, V extends K> {
    K first;

    <K> K same(K v) {
        K copy = v;
        return copy;
    }
}

class Outer<T> {
    class Inner {
        T value;
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
    // §15.12.2: overload selection — the declaration the argument types select,
    // not the first same-named declaration of the file and not one picked by
    // arity alone.
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

/// The declaration-side references of [`MANY_SRC`] — a supertype, a type
/// reference of a declaration or of a body, an annotation, an import: the
/// needle locates the reference (at its `occurrence`-th occurrence), `name` is
/// the declaration's simple name, and the last string a needle inside the
/// target's own range.
const MANY_DECL_GOTO: &[(&str, usize, &str, &str)] = &[
    // §8.1.4/§9.1.2: a supertype clause names the class-like declaration.
    ("Base {", 1, "Base", "class Base"),
    ("Factory {", 1, "Factory", "interface Factory"),
    // §9.7: an annotation names the annotation type.
    ("Marker", 1, "Marker", "@interface Marker"),
    // The declared type of a field, of a parameter and of a return type
    // ([§8.3], [§8.4.1]) — declaration-side references, not expressions.
    ("Base self", 0, "Base", "class Base"),
    ("Base b", 0, "Base", "class Base"),
    ("Base b", 2, "Base", "class Base"),
    ("Base make", 0, "Base", "class Base"),
    // A body's own type references: a local's declared type, a generic
    // argument, a cast, an `instanceof` test, a class literal, an array
    // creation ([§14.4], [§15.16], [§15.20.2], [§15.8.2], [§15.10.1]).
    ("Base cast", 0, "Base", "class Base"),
    ("Base> boxed", 0, "Base", "class Base"),
    ("Base> {}", 0, "Base", "class Base"),
    ("Base) other", 0, "Base", "class Base"),
    ("Base;", 1, "Base", "class Base"),
    ("Base.class", 0, "Base", "class Base"),
    ("Base[1]", 0, "Base", "class Base"),
    // §7.5.1: a single-type import names its type; §7.5.4: the last segment of
    // a static import names the member, the segments before it its type.
    ("Base;", 0, "Base", "class Base"),
    ("STATIC;", 0, "STATIC", "STATIC = 1"),
    ("Base.STATIC", 0, "Base", "class Base"),
];

#[test]
fn goto_declaration_reference_matrix() {
    let fixture = test_file(MANY_SRC);
    for &(needle, occurrence, name, declaration) in MANY_DECL_GOTO {
        assert_goto_covers(&fixture, needle, occurrence, name, declaration);
    }
    let cases: Vec<(&str, usize)> = MANY_DECL_GOTO
        .iter()
        .map(|&(needle, occurrence, ..)| (needle, occurrence))
        .collect();
    assert_snapshot!(
        "goto_declaration_reference_matrix",
        render_nav_many(&fixture, &cases)
    );
}

// -- type parameters ([JLS §4.4]) ---------------------------------------------------
// A written type variable denotes the *parameter* that declares it — the
// narrowest declaration of the name ([§6.4.1]), never a class of the same
// spelling.

#[test]
fn goto_type_parameter_reference() {
    let fixture = test_file(MANY_SRC);

    // A class's own parameter, read as a field's declared type and as another
    // parameter's bound.
    assert_targets_name(&fixture, "K first", 0, "class Generic<K, V extends K>", "K");
    assert_targets_name(&fixture, "K>", 0, "class Generic<K, V extends K>", "K");

    // A method's own parameter — its return type and a local's declared type.
    assert_targets_name(&fixture, "T id(T v)", 0, "<T> T id", "T");
    assert_targets_name(&fixture, "T copy", 0, "<T> T id", "T");

    // §6.4.1: the method's `K` shadows the class's, so both the parameter type
    // and the body's read name the *method's* declaration.
    assert_targets_name(&fixture, "K same", 0, "<K> K same", "K");
    assert_targets_name(&fixture, "K copy", 0, "<K> K same", "K");

    // §6.3: an enclosing class's parameter is in scope inside a nested
    // declaration, so the inner field's type names the *outer* class's `T`.
    assert_targets_name(&fixture, "T value", 0, "class Outer<T>", "T");

    assert_snapshot!(
        "goto_type_parameter_reference",
        render_nav_many(
            &fixture,
            &[
                ("K first", 0),
                ("K>", 0),
                ("T id(T v)", 0),
                ("T copy", 0),
                ("K same", 0),
                ("K copy", 0),
                ("T value", 0),
            ]
        )
    );
}

// -- a variable's target covers its own name ----------------------------------------
// A local, a parameter, a pattern binding and a lambda parameter are
// declarations carried without an item of their own: the target is the
// identifier `Base other` and `int local = 0` were written around, not the
// declarator ([JLS §6.4]).

#[test]
fn goto_variable_target_covers_the_name() {
    let fixture = test_file(MANY_SRC);

    // A parameter, read as the operand of a cast.
    assert_targets_name(&fixture, "other;", 0, "Sub other", "other");
    // A local, read in the statement after its declaration.
    assert_targets_name(&fixture, "local + 1", 0, "int local = 0", "local");
    // A lambda parameter, read in the lambda's body.
    assert_targets_name(&fixture, "q.count", 0, "(Base q)", "q");

    assert_snapshot!(
        "goto_variable_target",
        render_nav_many(&fixture, &[("other;", 0), ("local + 1", 0), ("q.count", 0)])
    );
}

/// The single navigation target of the reference at `needle`'s `occurrence`-th
/// occurrence.
fn goto_target(fixture: &Fixture, needle: &str, occurrence: usize) -> NavigationTarget {
    let offset = fixture.offset_start(needle, occurrence);
    let mut targets = fixture
        .analysis()
        .goto_definition(fixture.file, offset)
        .unwrap();
    assert_eq!(
        targets.len(),
        1,
        "case {needle:?}#{occurrence}: {targets:?}"
    );
    targets.pop().unwrap()
}

/// Asserts that the reference at `needle` resolves to the declaration whose
/// *name* is the first `name` written in `declaration` — the exact identifier,
/// not the declaration it was written in (`Base b` targets `b`, `<T> T id`
/// targets the `T` of the list).
fn assert_targets_name(
    fixture: &Fixture,
    needle: &str,
    occurrence: usize,
    declaration: &str,
    name: &str,
) {
    let target = goto_target(fixture, needle, occurrence);
    assert_eq!(target.name, name, "case {needle:?}#{occurrence}");
    let declaration_at = fixture
        .text
        .find(declaration)
        .unwrap_or_else(|| panic!("the declaration {declaration:?} is not in the fixture"));
    let name_at = declaration_at
        + fixture.text[declaration_at..]
            .find(name)
            .unwrap_or_else(|| panic!("{name:?} is not in {declaration:?}"));
    let expected = TextRange::new(
        TextSize::new(name_at as u32),
        TextSize::new((name_at + name.len()) as u32),
    );
    assert_eq!(
        target.range, expected,
        "case {needle:?}#{occurrence} must name {name:?} in {declaration:?}"
    );
}

fn test_file(text: &str) -> Fixture {
    let mut host = AnalysisHost::new();
    let file = FileId::from_raw(1);
    let path = "/src/main/java/com/example/Nav.java";

    let mut change = Change::default();
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
    change.set_project_graph(data);
    host.apply_change(change);

    Fixture {
        host,
        file,
        text: text.to_owned(),
    }
}
