//! Conformance snapshots for the deprecation warnings of
//! [JLS §9.6.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.6).
//!
//! A *deprecated* element is a class or interface, method, constructor or
//! field whose declaration carries `@Deprecated`; with `forRemoval = true` it
//! is *terminally* deprecated, and a reference to it is a warning javac
//! reports without any flag. Every scenario was verified against
//! `javac -Xlint:deprecation,removal` (JDK 25) before its snapshot was
//! accepted, and the javac output is quoted in the scenario's comment.
//!
//! The exempt cases of the same section are covered too: a use inside a
//! declaration that is itself (ordinarily) deprecated, a use and an element
//! within the same outermost class, and an enclosing `@SuppressWarnings`
//! naming the key ([§9.6.4.5]).

#[macro_use]
mod common;

use crate::common::{
    ClassSpec, DeprecationSpec, check_body_diagnostic_spans, check_body_diagnostic_spans_with_libs,
    check_class_diagnostics, check_class_diagnostics_with_libs,
};

/// A package-private class with one ordinary-deprecated method and field, plus
/// a nested class whose method is deprecated — the fixture every scenario
/// below draws on. javac:
///
/// ```text
/// Cases.java:4: warning: [deprecation] Both in q has been deprecated
/// Cases.java:5: warning: [deprecation] sm() in Both has been deprecated
/// Cases.java:6: warning: [deprecation] f in Both has been deprecated
/// Cases.java:8: warning: [deprecation] nm() in Nested has been deprecated
/// ```
const BOTH: &str = "\
package q;

@Deprecated
class Both {
    @Deprecated void sm() {}
    @Deprecated static int f = 1;
    static class Nested {
        @Deprecated void nm() {}
    }
}
";

fn both_fixture(file: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("/src/q/Both.java", BOTH), ("/src/q/Cases.java", file)]
}

// §9.6.4.6: a reference to a deprecated class *and* to a deprecated member of
// it is reported twice — javac reports `Both in q` for the receiver's type and
// `sm() in Both` for the member. The member's owner in the message is the
// declaring class, the class's own operand is its package.
snapshot!(
    source_class_and_member_deprecated,
    check_body_diagnostic_spans(&both_fixture(
        "\
package q;

class Cases {
    void a() { new Both(); }
    void b() { new Both().sm(); }
    void c() { int x = Both.f; }
    void d() { new Both.Nested().nm(); }
}
"
    ))
);

// §9.6.4.6: `@Deprecated(forRemoval = true)` makes the element *terminally*
// deprecated; javac's key is `removal` and its sentence ends
// `and marked for removal`.
snapshot!(
    terminal_deprecation_is_reported,
    check_body_diagnostic_spans(&both_fixture(
        "\
package q;

@Deprecated(forRemoval = true)
class Gone {
    @Deprecated(forRemoval = true) void gm() {}
}

class Cases {
    void a() { new Gone(); }
    void b() { new Gone().gm(); }
}
"
    ))
);

// §9.6.4.6: a use inside a declaration that is itself deprecated is exempt —
// but only from the *ordinary* warning. javac:
// ```text
// R.java:2: warning: [removal] t() in R has been deprecated and marked for removal
// R.java:3: warning: [removal] t() in R has been deprecated and marked for removal
// R.java:4: warning: [removal] t() in R has been deprecated and marked for removal
// R.java:5: warning: [deprecation] o() in R has been deprecated
// ```
// The `@Deprecated` body reports only the terminal call; the
// `@SuppressWarnings("deprecation")` body reports the terminal one too, and
// the `@SuppressWarnings("removal")` body reports only the ordinary one.
snapshot!(
    deprecated_declaration_exempts_ordinary_only,
    check_body_diagnostic_spans(&[
        (
            "/src/q/R.java",
            "\
package q;

class R {
    @Deprecated(forRemoval = true) void t() {}
    @Deprecated void o() {}
}
"
        ),
        (
            "/src/q/UseR.java",
            "\
package q;

class UseR {
    void a() { R r = new R(); r.t(); r.o(); }
    @Deprecated void dep() { R r = new R(); r.t(); r.o(); }
    @SuppressWarnings(\"deprecation\") void sd() { R r = new R(); r.t(); r.o(); }
    @SuppressWarnings(\"removal\") void sr() { R r = new R(); r.t(); r.o(); }
}
"
        )
    ])
);

// §9.6.4.6: a use and a declaration within the same outermost class are
// exempt, nested classes included — `Inner`'s use of `Outer.dm` is silent
// exactly as `Outer`'s own is, while a *different* outermost class using it
// reports. javac reports nothing for this file.
snapshot!(
    same_outermost_class_is_exempt,
    check_body_diagnostic_spans(&[(
        "/src/q/Outer.java",
        "\
package q;

class Outer {
    @Deprecated void dm() {}
    static class Inner {
        void u() { new Outer().dm(); }
    }
    void self() { new Outer().dm(); }
}
"
    )])
);

// §9.6.4.5/§9.6.4.6: `@SuppressWarnings` names the same keys the diagnostics
// carry, so `"deprecation"` hides an ordinary warning and `"removal"` a
// terminal one — a scope naming only one of them leaves the other standing.
// The scope reaches a *declaration*-position reference too: the deprecated
// parameter type of `e` is suppressed by the annotation on the method that
// declares it.
snapshot!(
    suppression_covers_deprecation_and_removal,
    check_body_diagnostic_spans(&[(
        "/src/q/S.java",
        "\
package q;

@Deprecated class D {}

class S {
    @Deprecated void o() {}
    @Deprecated(forRemoval = true) void t() {}
}

class UseS {
    @SuppressWarnings(\"deprecation\") void a() { S s = new S(); s.o(); }
    @SuppressWarnings(\"removal\") void b() { S s = new S(); s.t(); }
    @SuppressWarnings(\"deprecation\") void c() { S s = new S(); s.t(); }
    @SuppressWarnings(\"removal\") void d() { S s = new S(); s.o(); }
    @SuppressWarnings(\"deprecation\") void e(D d) {}
    void f(D d) {}
}
"
    )])
);

// §9.6.4.6: a class of the *unnamed* package has javac's `unnamed package` as
// its "in" operand, while a member of it names the class. javac:
// ```text
// UseTop.java:2: warning: [deprecation] Top in unnamed package has been deprecated
// UseTop.java:3: warning: [deprecation] Top in unnamed package has been deprecated
// UseTop.java:3: warning: [deprecation] m() in Top has been deprecated
// ```
snapshot!(
    unnamed_package_is_named,
    check_body_diagnostic_spans(&[
        (
            "/src/Top.java",
            "\
@Deprecated
class Top {
    @Deprecated void m() {}
}
"
        ),
        (
            "/src/UseTop.java",
            "\
class UseTop {
    void a() { new Top(); }
    void b() { new Top().m(); }
}
"
        )
    ])
);

// §9.6.4.6: the `{0}` operand names the member the way javac does — the
// parameter *types*, their simple names joined by `,`, and a variable-arity
// formal by its element type with `...` (javac: `p(int,String) in M`,
// `v(int...) in M`). A generic method is the one divergence: it is named by
// its invocation-site form rather than javac's `<T>g(List<T>)` (see
// `handlers::deprecation`).
snapshot!(
    parameter_lists_render_like_javac,
    check_body_diagnostic_spans(&[
        (
            "/src/q/M.java",
            "\
package q;

import java.util.List;

class M {
    @Deprecated void p(int a, String b) {}
    @Deprecated <T> void g(List<T> l) {}
    @Deprecated void v(int... xs) {}
}
"
        ),
        (
            "/src/q/UseM.java",
            "\
package q;

class UseM {
    void a(M m) { m.p(1, \"x\"); m.g(null); m.v(1, 2); }
}
"
        )
    ])
);

// §9.6.4.6: a constructor invocation is a use of the constructor, explicit
// `super(...)`/`this(...)` delegation included. javac reports exactly one
// line here — `super(1)` names the deprecated `P(int)`; `super(s)` names an
// undeclared-by-deprecation overload, and `this()` targets a constructor of
// the subclass, not of `P`.
snapshot!(
    constructor_invocations_are_reported,
    check_body_diagnostic_spans(&[
        (
            "/src/q/P.java",
            "\
package q;

class P {
    @Deprecated P(int i) {}
    @Deprecated P() {}
    P(String s) {}
}
"
        ),
        (
            "/src/q/UseP.java",
            "\
package q;

class UseP extends P {
    UseP() { super(1); }
    UseP(int x) { this(); }
    UseP(String s) { super(s); }
}
"
        )
    ])
);

// §9.6.4.5 for a *declaration*-position reference: the scope of the
// annotation belongs to the declaration it annotates, so the deprecated
// parameter type of `e` and the deprecated field type of `g` are suppressed
// while the same references in `f` and `h` are reported.
snapshot!(
    suppression_covers_declaration_references,
    check_class_diagnostics(&[(
        "/src/q/S.java",
        "\
package q;

@Deprecated class D {}

class UseS {
    @SuppressWarnings(\"deprecation\") void e(D d) {}
    void f(D d) {}
    @SuppressWarnings(\"deprecation\") D g;
    D h;
}
"
    )])
);

// §9.6.4.6: a static access writes its qualifier as a *type name*, so the
// qualifier is a reference to the class on its own — javac reports it whether
// or not the member is deprecated. javac:
// ```text
// UseStat.java:2: warning: [deprecation] Stat in q has been deprecated
// UseStat.java:2: warning: [deprecation] f in Stat has been deprecated
// UseStat.java:3: warning: [deprecation] Stat in q has been deprecated
// UseStat.java:4: warning: [deprecation] Stat in q has been deprecated
// UseStat.java:5: warning: [deprecation] Stat in q has been deprecated
// UseStat.java:5: warning: [deprecation] n() in Stat has been deprecated
// UseStat.java:6: warning: [deprecation] Stat in q has been deprecated
// ```
snapshot!(
    static_type_qualifier_is_reported,
    check_body_diagnostic_spans(&[
        (
            "/src/q/Stat.java",
            "\
package q;

@Deprecated
class Stat {
    @Deprecated static int f;
    static int g;
    static void m() {}
    @Deprecated static void n() {}
}
"
        ),
        (
            "/src/q/UseStat.java",
            "\
package q;

class UseStat {
    void a() { int x = Stat.f; }
    void b() { int x = Stat.g; }
    void c() { Stat.m(); }
    void d() { Stat.n(); }
    void e() { new Stat(); }
}
"
        )
    ])
);

// §9.6.4.6: a written type reference is a use, whatever the construct it
// appears in. javac reports one `[deprecation]` line for each, so a missing
// hook shows up here as a missing line. The *declaration*-position references
// first — a supertype, a field/parameter/return type, a type argument and an
// annotation type.
snapshot!(
    declaration_reference_kinds_are_all_reported,
    check_class_diagnostics(&[
        (
            "/src/q/Refs.java",
            "\
package q;

class Refs {
    @Deprecated static class C {}
    @Deprecated static class Base {}
    @Deprecated static interface I {}
    @Deprecated @interface Ann {}
}
"
        ),
        (
            "/src/q/UseRefs.java",
            "\
package q;

import java.util.List;

class UseRefs extends Refs.Base implements Refs.I {
    Refs.C field;
    void param(Refs.C c) {}
    Refs.C ret() { return null; }
    void argument(List<Refs.C> l) {}
    @Refs.Ann void annotated() {}
}
"
        )
    ])
);

// §9.6.4.6: the *body*-position references — a cast, a class literal and a
// local's declared type ([§14.4], [§15.16], [§15.8.2]).
snapshot!(
    body_reference_kinds_are_all_reported,
    check_body_diagnostic_spans(&[
        (
            "/src/q/Refs.java",
            "\
package q;

class Refs {
    @Deprecated static class C {}
}
"
        ),
        (
            "/src/q/UseRefs.java",
            "\
package q;

class UseRefs {
    void cast(Object o) { Refs.C c = (Refs.C) o; }
    Class<?> literal() { return Refs.C.class; }
    void local() { Refs.C c = null; }
}
"
        )
    ])
);

// §9.6.4.6: overriding a deprecated method is a use of it, reported at the
// *overriding* method's name — javac:
// ```text
// UseOv.java:2: warning: [deprecation] dm() in Ov has been deprecated
// ```
snapshot!(
    overriding_a_deprecated_method_is_reported,
    check_class_diagnostics(&[
        (
            "/src/q/Ov.java",
            "\
package q;

class Ov {
    @Deprecated void dm() {}
}
"
        ),
        (
            "/src/q/UseOv.java",
            "\
package q;

class UseOv extends Ov {
    void dm() {}
}
"
        )
    ])
);

/// A library class whose deprecation is carried by the classfile `Deprecated`
/// attribute ([JVMS §4.7.15]) alone, plus a member marked the same way.
fn attribute_deprecated_lib() -> Vec<ClassSpec<'static>> {
    vec![
        ClassSpec {
            deprecation: DeprecationSpec::ATTRIBUTE,
            methods: &[("am", "()V")],
            method_deprecations: &[DeprecationSpec::ATTRIBUTE],
            ..class_spec("com/example/Legacy")
        },
        // A nested library class: its binary name nests with `$`, and javac's
        // "in" operand is the *enclosing* simple name ([§9.6.4.6]).
        ClassSpec {
            deprecation: DeprecationSpec::FOR_REMOVAL,
            methods: &[("nm", "()V")],
            ..class_spec("com/example/Legacy$Nested")
        },
    ]
}

/// A library class deprecated through the `java.lang.Deprecated` annotation of
/// its `RuntimeVisibleAnnotations` attribute — the other way javac writes the
/// marker, and the only one that carries `forRemoval`.
fn annotation_deprecated_lib() -> Vec<ClassSpec<'static>> {
    vec![ClassSpec {
        deprecation: DeprecationSpec::ANNOTATION,
        fields: &[("af", "I")],
        field_deprecations: &[DeprecationSpec::FOR_REMOVAL],
        ..class_spec("com/example/Tagged")
    }]
}

/// The base of the library specs above: a public class extending
/// `java.lang.Object` with no members of its own.
fn class_spec<'a>(fqn: &'a str) -> ClassSpec<'a> {
    ClassSpec {
        fqn,
        super_class: Some("java/lang/Object"),
        interfaces: &[],
        access: 0x0021, // ACC_PUBLIC | ACC_SUPER
        fields: &[],
        field_access: &[],
        methods: &[],
        method_sigs: &[],
        method_access: &[],
        sig: None,
        deprecation: DeprecationSpec::NONE,
        field_deprecations: &[],
        method_deprecations: &[],
        method_defaults: &[],
    }
}

// §9.6.4.6 for a *library*: the classfile `Deprecated` attribute is the marker
// javac reads from a jar, and the `java.lang.Deprecated` annotation's
// `forRemoval` argument turns the report terminal. A deprecated nested library
// class names its enclosing class as the "in" operand (`Nested in Legacy`).
snapshot!(
    library_deprecation_is_reported,
    check_body_diagnostic_spans_with_libs(
        &attribute_deprecated_lib()
            .into_iter()
            .chain(annotation_deprecated_lib())
            .collect::<Vec<_>>(),
        &[(
            "/src/q/UseLib.java",
            "\
package q;

import com.example.Legacy;
import com.example.Tagged;

class UseLib {
    void a() { new Legacy().am(); }
    void b() { new Legacy.Nested().nm(); }
    void c() { int x = new Tagged().af; }
}
"
        ),]
    )
);

// §9.6.4.6: a deprecated library class in a *declaration* position — the
// supertype of a class and the type of a field.
snapshot!(
    library_deprecation_in_declarations_is_reported,
    check_class_diagnostics_with_libs(
        &attribute_deprecated_lib(),
        &[(
            "/src/q/UseDecl.java",
            "\
package q;

import com.example.Legacy;

class UseDecl extends Legacy {
    Legacy field;
}
"
        )]
    )
);
// JLS §9.6.4.5/[§8.9.1]: an enum constant is a declaration, and §9.6.4.5
// scopes a suppression to "the annotated declaration or any of its parts" —
// the constant's argument list is one of them. The grammar gives an
// `ENUM_CONSTANT` no modifier list, so its annotations hang off the constant
// itself; they nonetheless suppress within that constant and no sibling.
// javac:
// ```text
// Cases.java:7: warning: [deprecation] Both in q has been deprecated
//         B(new Both());
//               ^
// ```
snapshot!(
    enum_constant_scope,
    check_body_diagnostic_spans(&both_fixture(
        "\
package q;

class Cases {
    enum E {
        @SuppressWarnings(\"deprecation\")
        A(new Both()),
        B(new Both());

        E(Object o) {
        }
    }
}
"
    ))
);
