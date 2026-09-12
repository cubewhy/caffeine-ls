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

/// The navigation targets of the reference at `needle`'s `occurrence`-th
/// occurrence, in resolution order.
fn goto_targets(fixture: &Fixture, needle: &str, occurrence: usize) -> Vec<NavigationTarget> {
    let offset = fixture.offset_start(needle, occurrence);
    fixture
        .analysis()
        .goto_definition(fixture.file, offset)
        .unwrap()
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

// -- class instance creation ([JLS §15.9]) ------------------------------------------
// `new C(...)` names the *constructor* the creation selected: the declaration
// the classfile calls `<init>` and the class writes under its own name. A
// class that declares no constructor of its own keeps the class as its
// definition — there is no declaration to point at — and a *method* carrying
// the class's name is not a constructor, so `void Alt(int)` never answers
// `new Alt(1)`.

const NEW_SRC: &str = r#"package com.example;

class Alt {
    void Alt(int n) {}

    Alt(long n) {}

    Alt make() {
        return new Alt(1);
    }
}

class Two {
    Two(int n) {}

    Two(long n) {}

    static Two make() {
        return new Two(1L);
    }

    static Two other() {
        return new Two(1);
    }
}

class Plain {
    Plain make() {
        return new Plain();
    }
}

record Pair(int left, int right) {
    Pair {
    }

    static Pair make() {
        return new Pair(1, 2);
    }
}
"#;

/// The class instance creations of [`NEW_SRC`], in the shape of
/// [`assert_targets_name`]: the creation selects the constructor by its
/// parameter list ([§15.12.2]), and a record's canonical constructor is the
/// compact declaration ([§8.10.4]).
const NEW_GOTO: &[(&str, usize, &str, &str)] = &[
    // The only applicable constructor, not the same-named method that takes
    // the argument's own type.
    ("new Alt(1)", 0, "Alt(long n)", "Alt"),
    // The constructor the argument's type selects, not the first constructor
    // of the same arity.
    ("new Two(1L)", 0, "Two(long n)", "Two"),
    ("new Two(1)", 0, "Two(int n)", "Two"),
    // The compact canonical constructor of a record, declared under the
    // record's own name.
    ("new Pair(1, 2)", 0, "Pair {\n", "Pair"),
];

/// A creation whose class resolves to nothing: the name denotes no
/// declaration, so the offset answers nothing.
const NEW_EMPTY_SRC: &str = r#"package com.example;

class Client {
    Object make() {
        return new Nope();
    }
}
"#;

#[test]
fn goto_class_instance_creation() {
    let fixture = test_file(NEW_SRC);
    for &(needle, occurrence, declaration, name) in NEW_GOTO {
        assert_targets_name(&fixture, needle, occurrence, declaration, name);
    }

    // A class with no constructor declaration of its own has no constructor to
    // point at: the creation names the class.
    assert_targets_name(&fixture, "new Plain()", 0, "class Plain", "Plain");

    assert!(goto_targets(&test_file(NEW_EMPTY_SRC), "new Nope()", 0).is_empty());

    let cases: Vec<(&str, usize)> = NEW_GOTO
        .iter()
        .map(|&(needle, occurrence, ..)| (needle, occurrence))
        .collect();
    assert_snapshot!(
        "goto_class_instance_creation",
        render_nav_many(&fixture, &cases)
    );
}

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
/// occurrence — `declaration` is a needle inside the declaration it resolves
/// to, and `name` is that declaration's own name, the exact identifier the
/// target range must cover.
const MANY_MEMBER_GOTO: &[(&str, usize, &str, &str)] = &[
    // §6.5.6.1: a bare name is a field of the implicit `this`; the qualified
    // form names the same declaration through the receiver.
    ("count = b", 0, "int count;", "count"),
    ("count;", 1, "int count;", "count"),
    ("self = b", 0, "Base self;", "self"),
    ("self;", 1, "Base self;", "self"),
    // §15.12.2: overload selection — the declaration the argument types select,
    // not the first same-named declaration of the file and not one picked by
    // arity alone.
    ("method(1);", 0, "void method(int n) {}", "method"),
    // The `method(1)` of `super.method(1)`: the invocation selects the
    // superclass's declaration. (The offset on the `super` keyword itself
    // names the superclass — see [`goto_this_and_super`].)
    ("method(1)", 1, "void method(int n) {}", "method"),
    ("method(1L)", 0, "void method(long n) {}", "method"),
    ("pick(1L)", 0, "void pick(long n) {}", "pick"),
    // A static field and the enum constants.
    ("STATIC;", 1, "static int STATIC = 1;", "STATIC"),
    ("FIRST;", 0, "FIRST,", "FIRST"),
    ("SECOND:", 0, "SECOND", "SECOND"),
    // §15.27.2: the body IR carries a lambda parameter as a name/range pair,
    // not as a local, so the use names its declarator.
    ("q.count", 0, "(Base q) ->", "q"),
];

/// The references of [`MANY_SRC`] that name no declaration: a name nothing
/// declares. (A local's own declarator is not a *reference* either, but
/// goto-definition on its own name still answers with the declaration — see
/// [`goto_own_declaration_name`].)
const MANY_MEMBER_NONE: &[(&str, usize)] = &[("nope + 1", 0)];

#[test]
fn goto_reference_matrix() {
    let fixture = test_file(MANY_SRC);
    for &(needle, occurrence, declaration, name) in MANY_MEMBER_GOTO {
        assert_targets_name(&fixture, needle, occurrence, declaration, name);
    }
    for &(needle, occurrence) in MANY_MEMBER_NONE {
        assert!(
            goto_targets(&fixture, needle, occurrence).is_empty(),
            "case {needle:?}#{occurrence}"
        );
    }
    let mut cases: Vec<(&str, usize)> = MANY_MEMBER_GOTO
        .iter()
        .map(|&(needle, occurrence, ..)| (needle, occurrence))
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
        assert_targets_name(&fixture, needle, occurrence, declaration, name);
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

// -- `this`/`super` keywords and explicit constructor invocations ---------------
// A `this` keyword names the enclosing class and a `super` keyword the direct
// superclass ([JLS §15.8.3], [§15.8.4]) — the *type* the keyword denotes, not
// the field or method the enclosing `this.f`/`super.m()` reads, whose own
// resolution lies in a different declaration. `this(...)` and `super(...)` are
// explicit constructor invocations ([§8.8.7.1]): like a class instance
// creation, they name the constructor they selected.

const KEYWORD_SRC: &str = r#"package com.example;

class Base {
    int count;

    Base() {}

    Base(int n) {}
}

class Sub extends Base {
    Sub() {
        this(0);
    }

    Sub(int n) {
        super(n);
    }

    Base source() {
        int a = this.count;
        int b = super.count;
        return this;
    }
}

class Outer {
    class Inner {
        Outer outer() {
            return Outer.this;
        }
    }
}

interface Greeter {
    default String greet() {
        return "hi";
    }
}

class Greet implements Greeter {
    String run() {
        return Greeter.super.greet();
    }
}

class Empty {
}

class Thin extends Empty {
    Thin() {
        super();
    }
}
"#;

/// The keyword and constructor-delegation references of [`KEYWORD_SRC`], in the
/// shape of [`assert_targets_name`].
const KEYWORD_GOTO: &[(&str, usize, &str, &str)] = &[
    // §8.8.7.1: the delegated constructor, selected by its parameter list.
    ("this(0)", 0, "Sub(int n)", "Sub"),
    ("super(n)", 0, "Base(int n)", "Base"),
    // A superclass that declares no constructor of its own has none to point
    // at: the delegation names the class, as an instance creation does.
    ("super();", 0, "class Empty", "Empty"),
    // §15.8.3/§15.8.4: the keyword denotes the enclosing class and its direct
    // superclass, never the field the access reads.
    ("this.count", 0, "class Sub", "Sub"),
    ("super.count", 0, "class Base", "Base"),
    // A bare `this` — the second `this;` is the keyword of `Outer.this`.
    ("this;", 0, "class Sub", "Sub"),
    ("this;", 1, "class Outer", "Outer"),
    // §15.11.2: a qualified keyword names the class or interface it writes.
    ("Outer.this", 0, "class Outer", "Outer"),
    ("Greeter.super", 0, "interface Greeter", "Greeter"),
];

#[test]
fn goto_this_and_super() {
    let fixture = test_file(KEYWORD_SRC);
    for &(needle, occurrence, declaration, name) in KEYWORD_GOTO {
        assert_targets_name(&fixture, needle, occurrence, declaration, name);
    }

    // The `super` of a qualified-super invocation is answered by the interface
    // it writes, not by the default method the invocation names.
    assert_targets_name(&fixture, "super.greet", 0, "interface Greeter", "Greeter");

    let cases: Vec<(&str, usize)> = KEYWORD_GOTO
        .iter()
        .map(|&(needle, occurrence, ..)| (needle, occurrence))
        .collect();
    assert_snapshot!("goto_this_and_super", render_nav_many(&fixture, &cases));
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

// -- a declaration's own name ([JLS §6.3]) ------------------------------------------
// A declaration is not a reference to itself: `m` in `Main m`, `Main` in
// `class Main`, `local` in `int local = 0` name a declaration, not a *use* of
// one. Goto-definition on such a name still answers — with the declaration it
// names. This "self" step is taken only after every reference step found
// nothing, so the `Main` of `Main m` is answered by the class, not the field.

const SELF_NAV_SRC: &str = r#"package com.example;

class Main {
    Main m;

    int count;

    <T> T pick(T first, T second) {
        int local = 0;
        Runnable r = (String s) -> s.length();
        return first;
    }
}
"#;

#[test]
fn goto_own_declaration_name() {
    let fixture = test_file(SELF_NAV_SRC);

    // The class's own name, a field's own declarator and a method's own name.
    assert_targets_name(&fixture, "Main {", 0, "class Main", "Main");
    assert_targets_name(&fixture, "m;\n", 0, "Main m", "m");
    assert_targets_name(&fixture, "count;", 0, "int count;", "count");
    assert_targets_name(&fixture, "pick(T first", 0, "T pick(T first", "pick");

    // A variable's own declarator names the variable itself — the identifier,
    // exactly as a use does.
    assert_targets_name(&fixture, "local = 0", 0, "int local = 0", "local");
    assert_targets_name(&fixture, "first, T second", 0, "T first", "first");
    assert_targets_name(&fixture, "s) ->", 0, "(String s)", "s");

    // A type parameter's own declaration in the list that declares it.
    assert_targets_name(&fixture, "T> T pick", 0, "<T> T pick", "T");

    // A declared type that names a same-file class is still read as a
    // *reference*: the `Main` of `Main m` answers with the class.
    assert_targets_name(&fixture, "Main m", 0, "class Main", "Main");

    assert_snapshot!(
        "goto_own_declaration_name",
        render_nav_many(
            &fixture,
            &[
                ("Main {", 0),
                ("m;\n", 0),
                ("count;", 0),
                ("pick(T first", 0),
                ("local = 0", 0),
                ("first, T second", 0),
                ("s) ->", 0),
                ("T> T pick", 0),
                ("Main m", 0),
            ]
        )
    );
}

/// The single navigation target of the reference at `needle`'s `occurrence`-th
/// occurrence.
fn goto_target(fixture: &Fixture, needle: &str, occurrence: usize) -> NavigationTarget {
    let mut targets = goto_targets(fixture, needle, occurrence);
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
