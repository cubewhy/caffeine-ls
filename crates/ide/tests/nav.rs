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
    // The offset is on the invocation's own name (`add` of `add(1, 2)`) — an
    // argument is a nested expression, whose own resolution is its own.
    assert_snapshot!("goto_method_call", render_nav_at(&fixture, "add(1, 2)", 0));
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
    // The class declaration's signature — hover on its own name (`class Nav`'s
    // `Nav`), which is the token the declaration is named by ([JLS §6.3]); the
    // `class` keyword names nothing.
    assert_snapshot!("hover_class_declaration", render_nav_at(&fixture, "Nav", 0));
}

// -- anchoring: a resolution belongs to the expression the offset is inside --------

/// The shapes an anchor decides: every reference below is nested inside an
/// invocation or a creation, so a walk that ascends past the innermost
/// expression answers the enclosing member — a declaration whose own name is
/// not written where the offset is.
const ANCHOR_SRC: &str = r#"package com.example;

class Holder {
    static Holder label = null;
}

class Example {
    void foo(Runnable r) {}
}

class Entry {}

class Client {
    static void consume(Holder h) {}

    void take(Entry value) {}

    void run(String[] args, Entry other) {
        consume(Holder.label);
        var made = new Example();
        this.take((Entry) other);
        take(other);
        class Local {}
        interface Contract {}
        new Local();
    }
}
"#;

#[test]
fn goto_and_hover_anchor_on_the_innermost_expression() {
    let fixture = test_file(ANCHOR_SRC);
    assert_snapshot!(
        "goto_and_hover_anchor_on_the_innermost_expression",
        render_nav_many(
            &fixture,
            &[
                // §6.5.6.1 written through the *type* the field is declared by
                // ([§15.11.1]): the field read is an argument, and the
                // enclosing invocation names `consume`, not the field — its
                // resolution is about a name that is not written at the offset.
                ("label)", 0),
                // §15.16: a cast's type as an invocation's argument. The cast
                // is the expression at the offset; the invocation's resolution
                // is its method.
                ("Entry) other", 0),
                // §15.12.1/[§6.5.6.1]: an argument written as a name names what
                // that name denotes — here the parameter `other`, not `take`.
                ("other);", 0),
                // The invocation's own name, for contrast.
                ("take(other);", 0),
                // §14.4/[§6.5.2]: a declared type renders its *simple* name,
                // as a declaration's signature does.
                ("made", 0),
                ("args,", 0),
                // §14.3: a local class-like declaration is in no symbol index
                // ([§6.7] gives it no canonical name) — its own name is what a
                // hover there asks about, not the enclosing method's signature.
                ("Local {}", 0),
                ("Contract {}", 0),
                ("new Local()", 0),
            ],
        )
    );
}

// -- a declaration is answered by its own name -------------------------------

/// A declaration is named by its own name token ([JLS §6.3]): a hover on the
/// modifiers or the keywords of a declaration names nothing and answers
/// nothing, and so does a definition — the enclosing class is *not* the
/// declaration an offset on `private static final` names.
const NAME_ONLY_SRC: &str = r#"package com.example;

class Holder {
    private static final Holder field = null;

    static void main(String[] args) {}
}
"#;

#[test]
fn hover_and_definition_on_a_declaration_answer_only_its_name() {
    let fixture = test_file(NAME_ONLY_SRC);
    assert_snapshot!(
        "hover_and_definition_on_a_declaration_answer_only_its_name",
        render_nav_many(
            &fixture,
            &[
                // The modifiers of the field and of the method: no declaration
                // is written there, and the class that encloses them is not the
                // declaration the offset names.
                ("private static final", 0),
                ("static final", 0),
                ("final Holder field", 0),
                ("static void main", 0),
                // The declared type, for contrast: a reference, answered by the
                // class it names.
                ("Holder field", 0),
                // The declaration's own name, for contrast.
                ("field =", 0),
                ("main(String", 0),
            ],
        )
    );
}

// -- documentation: the hovered declaration's doc comment, rendered ---------------

const DOC_SRC: &str = r#"package com.example;

/**
 * A documented class.
 * <p>Second paragraph, with a {@code List<Item>}.
 */
class Docs {
    /** How many items. */
    int count;

    /**
     * Greets someone.
     *
     * @param name who to greet
     * @return the greeting
     * @throws IllegalStateException if closed
     */
    String greet(String name) {
        return name;
    }

    void call() {
        String s = greet("world");
    }
}

/**
 * A documented pair.
 *
 * @param left the left value
 * @param right the right value
 */
record Pair(int left, int right) {}
"#;

/// Renders the hover at the middle of `needle` ([`Fixture::offset`]): the
/// header the tests below pin, and the documentation of the declaration behind
/// it — the rendered doc comment, or `"<none>"` when the declaration has none.
fn render_hover_docs(fixture: &Fixture, needle: &str) -> String {
    render_hover_docs_at(fixture, needle, needle.len() / 2)
}

/// [`render_hover_docs`] with the offset `inner` bytes into the first `needle`.
/// A test that hovers a *declaration* points at its own name token — the middle
/// of `class Docs` is the keyword `class`, which names nothing, and a name is
/// what a declaration hover asks about.
fn render_hover_docs_at(fixture: &Fixture, needle: &str, inner: usize) -> String {
    let offset = fixture.offset_start(needle, 0) + TextSize::new(inner as u32);
    match fixture.analysis().hover(fixture.file, offset) {
        Ok(Some(hover)) => format!(
            "--- hover @{needle:?} ---\n{}\n--- docs ---\n{}",
            hover.value,
            hover.docs.as_deref().unwrap_or("<none>")
        ),
        Ok(None) => format!("--- hover @{needle:?} ---\n<none>"),
        Err(err) => panic!("hover failed: {err:?}"),
    }
}

#[test]
fn hover_docs_on_a_class_declaration() {
    let fixture = test_file(DOC_SRC);
    assert_snapshot!(
        "hover_docs_on_a_class_declaration",
        render_hover_docs_at(&fixture, "class Docs", 6)
    );
}

#[test]
fn hover_docs_on_a_method_declaration() {
    let fixture = test_file(DOC_SRC);
    assert_snapshot!(
        "hover_docs_on_a_method_declaration",
        render_hover_docs_at(&fixture, "String greet(", 7)
    );
}

#[test]
fn hover_docs_on_a_field_declaration() {
    let fixture = test_file(DOC_SRC);
    assert_snapshot!(
        "hover_docs_on_a_field_declaration",
        render_hover_docs(&fixture, "count;")
    );
}

/// A hover on a *reference* answers the declaration it names — the header of
/// the referenced declaration, and its documentation — not the expression's
/// type.
#[test]
fn hover_docs_on_a_reference() {
    let fixture = test_file(DOC_SRC);
    assert_snapshot!(
        "hover_docs_on_a_reference",
        render_hover_docs(&fixture, "= greet(")
    );
}

/// A record component has no doc comment of its own; the record's
/// `@param <component>` text documents it.
#[test]
fn hover_docs_on_a_record_component() {
    let fixture = test_file(DOC_SRC);
    assert_snapshot!(
        "hover_docs_on_a_record_component",
        render_hover_docs(&fixture, "int left")
    );
}

/// A declaration with no doc comment has no documentation to show.
#[test]
fn hover_docs_on_an_undocumented_declaration() {
    let fixture = test_file(DOC_SRC);
    assert_snapshot!(
        "hover_docs_on_an_undocumented_declaration",
        render_hover_docs(&fixture, "void call(")
    );
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
    // `$` is an ordinary identifier character ([JLS §3.8]), so the name the
    // hover answers for is the whole `A$B`.
    assert_snapshot!(
        "hover_over_dollar_class_declaration",
        render_nav_at(&fixture, "A$B", 0)
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

// -- the type qualifier of a qualified name ([JLS §6.5.2]) --------------------------
// An ambiguous name is reclassified — an expression name in scope first, a
// type name otherwise, a package name last ([§6.5.2]). Inference records the
// *member* a qualified access names on the enclosing access expression, so the
// bare leading segment is reached only by the classpath walk: `Main` of
// `Main.FIELD` names the class, while the access's own last segment names the
// member. A variable's name obscures a type of the same name ([§6.4.2]), and
// the expression name wins.

const QUALIFIER_SRC: &str = r#"package com.example;

enum Flag {
    ON,
    OFF
}

class Main {
    static final String FIELD = "";
}

class Use {
    void run(Main shadow, Flag flag) {
        String a = Main.FIELD;
        String b = shadow.FIELD;
        Flag on = Flag.ON;
    }
}
"#;

#[test]
fn goto_type_qualifier_of_qualified_name() {
    let fixture = test_file(QUALIFIER_SRC);

    // §6.5.2: the leading `Main`/`Flag` is no expression name, so it is a type
    // name — the class/enum declaration, not the member the access reads.
    assert_targets_name(&fixture, "Main.FIELD", 0, "class Main", "Main");
    assert_targets_name(&fixture, "Flag.ON", 0, "enum Flag", "Flag");
    // The access's own last segment still names the member.
    assert_targets_name(&fixture, "FIELD;", 0, "static final String FIELD", "FIELD");
    assert_targets_name(&fixture, "ON;", 0, "ON,", "ON");
    // §6.4.2/§6.5.2: a variable's name obscures a type of the same name, and
    // the expression name wins.
    assert_targets_name(&fixture, "shadow.FIELD", 0, "Main shadow", "shadow");

    assert_snapshot!(
        "goto_type_qualifier_of_qualified_name",
        render_nav_many(
            &fixture,
            &[
                ("Main.FIELD", 0),
                ("Flag.ON", 0),
                ("FIELD;", 0),
                ("ON;", 0),
                ("shadow.FIELD", 0),
            ]
        )
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
/// declares, and an argument that is a literal — an expression the offset is
/// *inside* names only itself, so the enclosing invocation, whose name is not
/// written there, does not answer for it. (A local's own declarator is not a
/// *reference* either, but goto-definition on its own name still answers with
/// the declaration — see [`goto_own_declaration_name`].)
const MANY_MEMBER_NONE: &[(&str, usize)] = &[("nope + 1", 0), ("1);", 0)];

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

// -- annotation element-value pairs ([JLS §9.7.1]) ----------------------------------
// A pair's name denotes the annotation interface's element ([§9.6.1]); a name
// inside its value denotes what the same name denotes in the scope of the
// declaration that carries the annotation ([§6.5.5.1]): a class literal's
// type ([§15.8.2]), a nested annotation's interface ([§9.7.1]), or a field
// ([§6.5.6]). The fixture is javac-valid, and every target below is a
// declaration of the file (the source set's classpath is empty).

const ANNOTATION_SRC: &str = r#"package com.example;

import static com.example.Ann.FLAG;
import static com.example.Mode.FAST;

enum Mode {
    FAST,
    SLOW
}

@interface Inner {
    String value();
}

@interface Nums {
    int MAX = 9;
}

@interface Ann {
    int FLAG = 1;

    String name();

    int count() default 0;

    Class<?> type();

    Mode mode();

    Inner inner();

    int[] nums();

    Mode[] modes();

    Nums nums2();
}

class Consts {
    static final int CONST = 7;
}

@Ann(
    name = "x",
    count = FLAG,
    type = Holder.class,
    mode = FAST,
    inner = @Inner(value = "y"),
    nums = { 1, Consts.CONST, 2 * 3 },
    modes = { Mode.SLOW, Mode.FAST },
    nums2 = @Nums()
)
class Holder {
    static final int LOCAL = 7;

    @Ann(
        name = "f",
        count = LOCAL,
        type = Holder.class,
        mode = Mode.FAST,
        inner = @Inner(value = "q"),
        nums = { LOCAL },
        modes = { Mode.FAST },
        nums2 = @Nums()
    )
    int field;

    @Ann(
        name = "z",
        count = LOCAL,
        type = Holder.class,
        mode = Mode.FAST,
        inner = @Inner(value = "w"),
        nums = { LOCAL },
        modes = { Mode.FAST },
        nums2 = @Nums()
    )
    void annotated() {
        @Ann(
            name = "v",
            count = LOCAL,
            type = Holder.class,
            mode = Mode.FAST,
            inner = @Inner(value = "u"),
            nums = { LOCAL },
            modes = { Mode.FAST },
            nums2 = @Nums()
        )
        int local = 0;
    }
}
"#;

/// The references of [`ANNOTATION_SRC`]'s element-value pairs: the needle
/// locates the reference (at its `occurrence`-th occurrence), `declaration` is
/// a needle inside the declaration it resolves to, and `name` is that
/// declaration's own name.
const ANNOTATION_GOTO: &[(&str, usize, &str, &str)] = &[
    // §9.6.1: a pair's name denotes the annotation interface's element — the
    // method the interface declares under it. The class, the member and the
    // body annotation are all covered (the last two carry their values in the
    // expression arena, not in the item tree).
    ("name = \"x\"", 0, "String name();", "name"),
    ("count = FLAG", 0, "int count() default 0;", "count"),
    ("type = Holder.class", 0, "Class<?> type();", "type"),
    ("mode = FAST", 0, "Mode mode();", "mode"),
    ("inner = @Inner", 0, "Inner inner();", "inner"),
    ("nums = { 1", 0, "int[] nums();", "nums"),
    ("modes = { Mode.SLOW", 0, "Mode[] modes();", "modes"),
    ("nums2 = @Nums", 0, "Nums nums2();", "nums2"),
    ("count = LOCAL", 1, "int count() default 0;", "count"),
    ("name = \"f\"", 0, "String name();", "name"),
    // A nested annotation's own pairs are read the same way ([§9.7.1]).
    ("value = \"y\"", 0, "String value();", "value"),
    ("value = \"q\"", 0, "String value();", "value"),
    ("value = \"u\"", 0, "String value();", "value"),
    // §6.5.6.1/§7.5.4: a simple name reads a field of the item's own class, or
    // the member a static import puts in scope.
    ("LOCAL }", 0, "static final int LOCAL = 7;", "LOCAL"),
    ("FLAG,", 0, "int FLAG = 1;", "FLAG"),
    ("FAST,", 0, "FAST,", "FAST"),
    // §6.5.6.2: a qualified name reads a static field of the type its
    // qualifier denotes — and the qualifier itself is a type reference.
    ("Consts.CONST", 0, "class Consts", "Consts"),
    ("CONST,", 0, "static final int CONST = 7;", "CONST"),
    ("Mode.SLOW", 0, "enum Mode", "Mode"),
    ("SLOW,", 0, "SLOW", "SLOW"),
    // §15.8.2: a class literal names its type.
    ("Holder.class", 0, "class Holder", "Holder"),
    // §9.7.1: a nested annotation names its annotation interface.
    ("Inner(value", 0, "@interface Inner", "Inner"),
    ("Nums()", 0, "@interface Nums", "Nums"),
];

/// The references of [`ANNOTATION_SRC`]'s pairs that name no declaration: a
/// value that is a literal. No declaration is written there, so the hover
/// beside each target is `<none>` — an element value is lowered outside any
/// body, so even the literal's own type has no owner to be read from yet.
const ANNOTATION_NONE: &[(&str, usize)] = &[("\"x\"", 0), ("2 * 3", 0)];

/// The pair's name is read through the lexer's Unicode translation ([JLS
/// §3.3]): `\u006eame` writes the element `name`, so the raw text of the
/// token names no declaration of its own.
#[test]
fn goto_annotation_pair_name_through_unicode_escape() {
    let fixture = test_file(
        "package com.example;\n\n@interface Ann { String name(); }\n\n@Ann(\\u006eame = \"x\")\nclass C {}\n",
    );
    assert_targets_name(&fixture, "\\u006eame", 0, "String name();", "name");
}

#[test]
fn goto_annotation_element_pair_matrix() {
    let fixture = test_file(ANNOTATION_SRC);
    for &(needle, occurrence, declaration, name) in ANNOTATION_GOTO {
        assert_targets_name(&fixture, needle, occurrence, declaration, name);
    }
    for &(needle, occurrence) in ANNOTATION_NONE {
        assert!(
            goto_targets(&fixture, needle, occurrence).is_empty(),
            "case {needle:?}#{occurrence}"
        );
    }
    let mut cases: Vec<(&str, usize)> = ANNOTATION_GOTO
        .iter()
        .map(|&(needle, occurrence, ..)| (needle, occurrence))
        .collect();
    cases.extend(ANNOTATION_NONE.iter().copied());
    assert_snapshot!(
        "goto_annotation_element_pair_matrix",
        render_nav_many(&fixture, &cases)
    );
}

// -- type parameters ([JLS §4.4]) ---------------------------------------------------
// A written type variable denotes the *parameter* that declares it — the
// narrowest declaration of the name ([§6.4.1]), never a class of the same
// spelling. A type parameter has no item of its own, so the *hover* rendered
// beside each target is `<none>`: `ide::nav`'s hover notes that gap, and the
// definition is what these cases pin.

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

// -- an invocation that selects no one overload ([JLS §15.12.2]) --------------------
// No two members of one class share an erasure ([§8.4.2]), so a *selected*
// signature names exactly one declaration. When the applicable candidates of an
// invocation are tied — `m(1, 1)` against `m(int, double)` and
// `m(double, int)` — no one of them is most specific and the invocation is
// ambiguous: it denotes *every* applicable declaration. And when no declaration
// is applicable at all ([§15.12.2]) — `m("a", "b")`, `m()` — the reference
// still *names* the declarations of the member set. Goto-definition answers
// every one of them either way: navigation is not a compile check. An overload
// the argument types exclude is no candidate of a *tie* — only the tie is
// narrow, the inapplicable answer is the whole name.

const AMBIGUOUS_SRC: &str = r#"package com.example;

class Pair {
    Pair(int x, double y) {}

    Pair(double x, int y) {}

    Pair(String s) {}
}

class Nav {
    void m(int x, double y) {}

    void m(double x, int y) {}

    void m(String s) {}

    void use() {
        m(1, 1);
        m(1, 1L);
        m("a", "b");
        m();
        new Pair(1, 1);
    }
}
"#;

#[test]
fn goto_ambiguous_overload() {
    let fixture = test_file(AMBIGUOUS_SRC);

    // §15.12.2.5: the two numeric overloads are applicable and tied, so both
    // are definitions of the reference — `m(String)` accepts neither argument
    // list and is no candidate.
    assert_targets(
        &fixture,
        "m(1, 1);",
        0,
        &[("void m(int x", "m"), ("void m(double x", "m")],
    );
    // The ambiguity is the *applicability* tie: `m(1, 1L)` selects
    // `m(int, double)` alone, so one declaration is answered.
    assert_targets(&fixture, "m(1, 1L)", 0, &[("void m(int x", "m")]);
    // §15.12.2: no overload accepts two `String`s and none accepts zero
    // arguments, so the invocation selects no declaration — but it names every
    // overload of `m`, and each of them is a definition.
    assert_targets(
        &fixture,
        "m(\"a\", \"b\")",
        0,
        &[
            ("void m(int x", "m"),
            ("void m(double x", "m"),
            ("void m(String s)", "m"),
        ],
    );
    assert_targets(
        &fixture,
        "m();",
        0,
        &[
            ("void m(int x", "m"),
            ("void m(double x", "m"),
            ("void m(String s)", "m"),
        ],
    );

    // §15.9/[§15.12.2.5]: the same tie among a class's constructors, answered
    // at each constructor's own name.
    assert_targets(
        &fixture,
        "new Pair(1, 1)",
        0,
        &[
            ("Pair(int x, double y)", "Pair"),
            ("Pair(double x, int y)", "Pair"),
        ],
    );

    assert_snapshot!(
        "goto_ambiguous_overload",
        render_nav_many(
            &fixture,
            &[
                ("m(1, 1);", 0),
                ("m(1, 1L)", 0),
                ("m(\"a\", \"b\")", 0),
                ("m();", 0),
                ("new Pair(1, 1)", 0),
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
    let expected = declared_name_range(fixture, declaration, name);
    assert_eq!(
        target.range, expected,
        "case {needle:?}#{occurrence} must name {name:?} in {declaration:?}"
    );
}

/// Asserts that the reference at `needle`'s `occurrence`-th occurrence answers
/// exactly the `declarations`, in order — each a `(declaration, name)` pair in
/// the shape of [`assert_targets_name`]. The multi-target counterpart of
/// [`assert_targets_name`], for a reference the type layer could not resolve to
/// one declaration ([JLS §15.12.2.5]).
fn assert_targets(
    fixture: &Fixture,
    needle: &str,
    occurrence: usize,
    declarations: &[(&str, &str)],
) {
    let targets = goto_targets(fixture, needle, occurrence);
    let expected: Vec<(&str, TextRange)> = declarations
        .iter()
        .map(|&(declaration, name)| (name, declared_name_range(fixture, declaration, name)))
        .collect();
    let observed: Vec<(&str, TextRange)> = targets
        .iter()
        .map(|target| (target.name.as_str(), target.range))
        .collect();
    assert_eq!(observed, expected, "case {needle:?}#{occurrence}");
}

/// The source range of the `name` identifier inside the first occurrence of
/// `declaration` in the fixture — the range a target naming that declaration
/// must cover.
fn declared_name_range(fixture: &Fixture, declaration: &str, name: &str) -> TextRange {
    let declaration_at = fixture
        .text
        .find(declaration)
        .unwrap_or_else(|| panic!("the declaration {declaration:?} is not in the fixture"));
    let name_at = declaration_at
        + fixture.text[declaration_at..]
            .find(name)
            .unwrap_or_else(|| panic!("{name:?} is not in {declaration:?}"));
    TextRange::new(
        TextSize::new(name_at as u32),
        TextSize::new((name_at + name.len()) as u32),
    )
}

/// A fixture over several files, each with its own id and text, all in the main
/// source set of one project: the shape a cross-file property needs, which a
/// single-file [`Fixture`] cannot express.
struct FilesFixture {
    host: AnalysisHost,
    files: Vec<(FileId, String)>,
}

impl FilesFixture {
    fn analysis(&self) -> Analysis {
        self.host.snapshot()
    }

    fn file(&self, index: usize) -> FileId {
        self.files[index].0
    }

    /// The offset of `needle`'s first character in the `index`-th file.
    fn offset_start(&self, index: usize, needle: &str) -> TextSize {
        let text = &self.files[index].1;
        TextSize::new(
            text.find(needle)
                .unwrap_or_else(|| panic!("needle {needle:?} not found in:\n{text}"))
                as u32,
        )
    }
}

/// A host over `files`, each a `(path, text)` pair; the file ids are `1..` in
/// argument order.
fn test_files(files: &[(&str, &str)]) -> FilesFixture {
    let mut host = AnalysisHost::new();
    let mut change = Change::default();
    let mut file_set = FileSet::default();
    let mut sources = Vec::with_capacity(files.len());
    for (index, &(path, text)) in files.iter().enumerate() {
        let file = FileId::from_raw(index as u32 + 1);
        file_set.insert(
            file,
            VfsPath::from(AbsPathBuf::assert_utf8(path.to_owned().into())),
        );
        change.change_file(file, Some(text.to_string()));
        sources.push((file, text.to_owned()));
    }
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

    FilesFixture {
        host,
        files: sources,
    }
}

fn test_file(text: &str) -> Fixture {
    let fixture = test_files(&[("/src/main/java/com/example/Nav.java", text)]);
    let file = fixture.file(0);
    Fixture {
        host: fixture.host,
        file,
        text: text.to_owned(),
    }
}

// -- find-all-references ------------------------------------------------------------
// The reverse of goto-definition: a query on a declaration reports every site
// whose forward resolution is that same declaration — including a name written
// through a Unicode escape, and excluding a same-named declaration elsewhere.

/// The source text of every reported reference, in (file, range) order — the
/// reported tokens themselves, so a missing or extra site reads as one line.
fn reference_texts(
    fixture: &Fixture,
    needle: &str,
    occurrence: usize,
    include_declaration: bool,
) -> Vec<String> {
    let offset = fixture.offset_start(needle, occurrence);
    fixture
        .analysis()
        .references(fixture.file, offset, include_declaration)
        .unwrap()
        .iter()
        .map(|reference| {
            fixture.text[reference.range.start().into()..reference.range.end().into()].to_owned()
        })
        .collect()
}

/// Renders the reference sites at one query as `text @range` lines under a
/// header naming the query — [`render_nav`]'s shape for the reverse direction.
fn render_references(
    fixture: &Fixture,
    needle: &str,
    occurrence: usize,
    include_declaration: bool,
) -> String {
    let offset = fixture.offset_start(needle, occurrence);
    let sites = fixture
        .analysis()
        .references(fixture.file, offset, include_declaration)
        .unwrap()
        .iter()
        .map(|reference| {
            format!(
                "{} @{:?}",
                &fixture.text[reference.range.start().into()..reference.range.end().into()],
                reference.range
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("--- refs @{needle:?}#{occurrence} include={include_declaration} ---\n{sites}")
}

fn render_references_many(fixture: &Fixture, cases: &[(&str, usize, bool)]) -> String {
    cases
        .iter()
        .map(|&(needle, occurrence, include_declaration)| {
            render_references(fixture, needle, occurrence, include_declaration)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The workspace of [`test_references_matrix`]: a base class with a static
/// field, a field and a method that reads and writes it, and a subclass that
/// names all of them plus the class in every declaration-side and body type
/// position.
const REFS_SRC: &str = r#"package com.example;

class Base {
    static int STATIC = 1;
    int count;

    void run() {
        int local = count;
        count = local;
    }
}

class Use extends Base {
    Base make() {
        Base b = new Base();
        b.run();
        b.count = Base.STATIC;
        return b;
    }
}
"#;

/// One query per kind of declaration — method, field, local, static field, a
/// class with references, a class without — each with and without the
/// declaration itself: the site count must be exactly the sites written.
#[test]
fn test_references_matrix() {
    let fixture = test_file(REFS_SRC);
    let cases: &[(&str, usize, &[&str], &[&str])] = &[
        ("run()", 0, &["run", "run"], &["run"]),
        (
            "count;",
            0,
            &["count", "count", "count", "count"],
            &["count", "count", "count"],
        ),
        ("local", 0, &["local", "local"], &["local"]),
        ("STATIC", 0, &["STATIC", "STATIC"], &["STATIC"]),
        ("Base {", 0, &["Base"; 6], &["Base"; 5]),
        ("Use extends", 0, &["Use"], &[]),
    ];
    for &(needle, occurrence, with_declaration, without_declaration) in cases {
        assert_eq!(
            reference_texts(&fixture, needle, occurrence, true),
            with_declaration,
            "include_declaration = true for {needle:?}#{occurrence}"
        );
        assert_eq!(
            reference_texts(&fixture, needle, occurrence, false),
            without_declaration,
            "include_declaration = false for {needle:?}#{occurrence}"
        );
    }
}

/// The same matrix, site by site, as one snapshot.
#[test]
fn test_references_snapshot() {
    let fixture = test_file(REFS_SRC);
    let cases: &[(&str, usize, bool)] = &[
        ("run()", 0, true),
        ("count;", 0, true),
        ("local", 0, true),
        ("STATIC", 0, true),
        ("Base {", 0, true),
        ("Use extends", 0, true),
        ("run()", 0, false),
        ("local", 0, false),
        ("Use extends", 0, false),
    ];
    assert_snapshot!("references_matrix", render_references_many(&fixture, cases));
}

/// A query on a local reports only the declaring file's sites: a field of the
/// same name in another file names a different declaration, so neither its
/// declaration nor its uses are reference sites of the local.
#[test]
fn test_references_local_stays_in_its_file() {
    let fixture = test_files(&[
        (
            "/src/main/java/com/example/Locals.java",
            r#"package com.example;

class Locals {
    void run() {
        int v = 0;
        int w = v;
    }
}
"#,
        ),
        (
            "/src/main/java/com/example/Fields.java",
            r#"package com.example;

class Fields {
    int v;

    void use() {
        v = 2;
    }
}
"#,
        ),
    ]);
    let offset = fixture.offset_start(0, "v = 0;");
    let references = fixture
        .analysis()
        .references(fixture.file(0), offset, true)
        .unwrap();
    let observed: Vec<(FileId, &str)> = references
        .iter()
        .map(|reference| {
            (
                reference.file,
                &fixture.files[reference.file.index() as usize - 1].1
                    [reference.range.start().into()..reference.range.end().into()],
            )
        })
        .collect();
    assert_eq!(
        observed,
        vec![(fixture.file(0), "v"), (fixture.file(0), "v")],
        "the local's own declaration and its one use, both in Locals.java"
    );
}

// -- record components ([JLS §8.10]) ------------------------------------------------
// A record component is a declaration of its own ([JLS §8.10.1]): it declares
// the private final field and — unless the body declares it — the public
// accessor ([§8.10.3]) of the component. The HIR carries it without an item of
// its own (the record's declaration holds the component list), so a reference
// to the field or the accessor, and the component's own name, all navigate to
// the component.

const RECORD_COMPONENT_SRC: &str = r#"package com.example;

record Point(int x, int y) {
    int sum() {
        return x + y;
    }

    int twice() {
        return x() * 2;
    }

    int qualified() {
        return this.x;
    }

    Point shift(int d) {
        return new Point(x + d, y);
    }
}

record Explicit(int e) {
    public int e() {
        return e;
    }
}

class Client {
    int read(Point p) {
        return p.x() + p.y();
    }

    int readExplicit(Explicit q) {
        return q.e();
    }
}
"#;

/// Every reference to a record component — the component's own name, the
/// implicit field read and the implicit accessor call — names the component.
/// An accessor the record's body declares *itself* is a method of its own: it
/// is not the component, even though the body reads the component under the
/// same name.
#[test]
fn goto_record_component() {
    let fixture = test_file(RECORD_COMPONENT_SRC);
    let cases: &[(&str, usize, &str, &str)] = &[
        // The component's own name in the declaration header.
        ("x, ", 0, "int x,", "x"),
        // The implicit field: read unqualified, qualified by `this`, and as an
        // instantiation argument.
        ("x + y;", 0, "int x,", "x"),
        ("x;", 0, "int x,", "x"),
        ("x + d", 0, "int x,", "x"),
        // The implicit accessor: called on the record itself and on a receiver
        // of the record type.
        ("x() * 2", 0, "int x,", "x"),
        ("x()", 1, "int x,", "x"),
        // The same, for the second component.
        ("y)", 0, "int y)", "y"),
        ("y;", 0, "int y)", "y"),
        ("y);", 0, "int y)", "y"),
        ("y()", 0, "int y)", "y"),
        // An accessor declared by the body is the method, not the component —
        // but the field of its component still is.
        ("e()", 1, "public int e()", "e"),
        ("e()", 2, "public int e()", "e"),
        ("e;", 1, "int e)", "e"),
    ];
    for &(needle, occurrence, declaration, name) in cases {
        assert_targets_name(&fixture, needle, occurrence, declaration, name);
    }

    let renders: Vec<(&str, usize)> = cases.iter().map(|&(n, o, ..)| (n, o)).collect();
    assert_snapshot!("goto_record_component", render_nav_many(&fixture, &renders));
}

/// The references of a record component are every site that names it: its own
/// declaration, the implicit field reads (which name the field, §8.10.1) and
/// the implicit accessor calls (§8.10.3) inside and outside the record. The
/// component's *name* is not the accessor's: an accessor the record body
/// declares itself is a separate declaration, and its own references are its
/// own.
#[test]
fn references_record_component() {
    let fixture = test_file(RECORD_COMPONENT_SRC);
    let cases: &[(&str, usize, &[&str], &[&str])] = &[
        // The component `x`: declaration, two field reads (`x + y`, `this.x`),
        // an implicit accessor call (`x()`), a constructor argument and the
        // call on a receiver in another class.
        (
            "x, ",
            0,
            &["x", "x", "x", "x", "x", "x"],
            &["x", "x", "x", "x", "x"],
        ),
        // The same sites, queried from a use and from the accessor call.
        (
            "x + y;",
            0,
            &["x", "x", "x", "x", "x", "x"],
            &["x", "x", "x", "x", "x"],
        ),
        (
            "x()",
            1,
            &["x", "x", "x", "x", "x", "x"],
            &["x", "x", "x", "x", "x"],
        ),
        // The component `y`: declaration, a field read, a constructor argument
        // and the accessor call.
        ("y)", 0, &["y", "y", "y", "y"], &["y", "y", "y"]),
        // The explicit accessor is its own declaration: the component's
        // declaration is not one of its sites.
        ("e()", 1, &["e", "e"], &["e"]),
        // The component of that record: its declaration and the field read.
        ("e;", 1, &["e", "e"], &["e"]),
    ];
    for &(needle, occurrence, with_declaration, without_declaration) in cases {
        assert_eq!(
            reference_texts(&fixture, needle, occurrence, true),
            with_declaration,
            "include_declaration = true for {needle:?}#{occurrence}"
        );
        assert_eq!(
            reference_texts(&fixture, needle, occurrence, false),
            without_declaration,
            "include_declaration = false for {needle:?}#{occurrence}"
        );
    }
}

/// The same matrix, site by site, as one snapshot — the ranges are as much the
/// answer as the count, and a component's target must be its name token (not
/// the record's).
#[test]
fn references_record_component_snapshot() {
    let fixture = test_file(RECORD_COMPONENT_SRC);
    let cases: &[(&str, usize, bool)] = &[
        ("x, ", 0, true),
        ("x, ", 0, false),
        ("y)", 0, true),
        ("e()", 1, true),
        ("e;", 1, true),
    ];
    assert_snapshot!(
        "references_record_component",
        render_references_many(&fixture, cases)
    );
}

/// A record component is reachable from every file: its accessor
/// ([JLS §8.10.3]) is public, so a query on the component must sweep the whole
/// workspace, and the accessor call in another file is one of its sites.
#[test]
fn references_record_component_cross_file() {
    let fixture = test_files(&[
        (
            "/src/main/java/com/example/Point.java",
            r#"package com.example;

record Point(int x) {
    int sum() {
        return x;
    }
}
"#,
        ),
        (
            "/src/main/java/com/example/Client.java",
            r#"package com.example;

class Client {
    int read(Point p) {
        return p.x();
    }
}
"#,
        ),
    ]);
    // The query is the component's own declaration in `Point.java`.
    let offset = fixture.offset_start(0, "x)");
    let sites: Vec<(FileId, String)> = fixture
        .analysis()
        .references(fixture.file(0), offset, true)
        .unwrap()
        .iter()
        .map(|reference| {
            (
                reference.file,
                fixture.files[reference.file.index() as usize - 1].1
                    [reference.range.start().into()..reference.range.end().into()]
                    .to_owned(),
            )
        })
        .collect();
    assert_eq!(
        sites,
        vec![
            (fixture.file(0), "x".to_owned()),
            (fixture.file(0), "x".to_owned()),
            (fixture.file(1), "x".to_owned()),
        ],
        "the declaration, the field read and the accessor call"
    );
    // And the reverse: the accessor call in `Client.java` answers the
    // declaration in the other file.
    let offset = fixture.offset_start(1, "x()");
    let sites: Vec<FileId> = fixture
        .analysis()
        .references(fixture.file(1), offset, true)
        .unwrap()
        .iter()
        .map(|reference| reference.file)
        .collect();
    assert_eq!(
        sites,
        vec![fixture.file(0), fixture.file(0), fixture.file(1)],
        "the declaration and the field read are in `Point.java`"
    );
}

/// The shapes a record component's declaration has to keep answering: a
/// *variable-arity* component ([JLS §8.4.1]) — whose field and accessor carry
/// the array type, and which a compact constructor reads as its implicit
/// parameter ([§8.10.4]) — a component of a record nested in another class,
/// and a local that shadows a component's name ([JLS §6.4.1]), which keeps its
/// own declaration.
const RECORD_COMPONENT_SHAPES_SRC: &str = r#"package com.example;

record Group(String... names) {
    Group {
        this.names = names;
    }

    String first() {
        String names = "local";
        return names;
    }
}

class Outer {
    record Inner(int n) {
        int read() {
            return n;
        }
    }
}
"#;

#[test]
fn goto_record_component_shapes() {
    let fixture = test_file(RECORD_COMPONENT_SHAPES_SRC);
    // A varargs component: its own name, the field `this.names` writes, and the
    // implicit parameter a compact constructor reads.
    assert_targets_name(&fixture, "names", 0, "String... names)", "names");
    assert_targets_name(&fixture, "names", 1, "String... names)", "names");
    assert_targets_name(&fixture, "names", 2, "String... names)", "names");
    // A local of the same name shadows it ([JLS §6.4.1]): only the names that
    // denote the *component* reach it.
    assert_targets_name(&fixture, "names;", 1, "String names = \"local\";", "names");
    // A record nested in another class declares its components the same way.
    assert_targets_name(&fixture, "n)", 0, "n)", "n");
    assert_targets_name(&fixture, "n;", 0, "n)", "n");
}

/// Two records may declare components of the same name: each component is its
/// own declaration, so neither query reports the other record's sites.
#[test]
fn record_components_are_not_conflated() {
    let fixture = test_file(
        r#"package com.example;

record A(int v) {
    int read() {
        return v;
    }
}

record B(int v) {
    int read() {
        return v;
    }
}
"#,
    );
    for occurrence in [0, 1] {
        assert_eq!(
            reference_texts(&fixture, "v)", occurrence, true),
            vec!["v", "v"],
            "occurrence {occurrence}: the declaration and the field read of one record"
        );
        assert_eq!(
            reference_texts(&fixture, "v)", occurrence, false),
            vec!["v"],
            "occurrence {occurrence}: the field read alone"
        );
    }
    // Each record's component is a declaration of its own.
    let first = goto_target(&fixture, "v)", 0);
    let second = goto_target(&fixture, "v)", 1);
    assert_ne!(
        first.range, second.range,
        "two components, two declarations"
    );
    assert_eq!(
        &fixture.text[usize::from(second.range.start())..usize::from(second.range.end())],
        "v"
    );
}
