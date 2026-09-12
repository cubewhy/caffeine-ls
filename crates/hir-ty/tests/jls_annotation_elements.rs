//! JLS SE 26 scenario snapshots for the annotation *element-value* argument
//! check ([JLS §9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1),
//! [§9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1)):
//! each `name = value` pair of an annotation's argument list must name an
//! element of the annotation type exactly once, and the value must be
//! assignable to the element's declared type ([§5.2]). The single-argument
//! form `@Foo(v)` is the implicit `value` element; a single non-initializer
//! value against an array-typed element is a one-element array shortcut.
//!
//! The renderer ([`check_class_diagnostics`]) prints one line per
//! `@line:col` diagnostic; the annotation types here either resolve in the
//! same compilation unit — so the elements are read from the annotation
//! type's own source declaration — or are the JDK fixture's
//! classfile-declared ones.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- red: a pair names an element the type does not declare --------------------

snapshot!(
    unknown_member,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int value();
}

class Anns {
    @Ann(foo = 1)
    void run() {}
}
",
    )])
);
// §9.7.1: `Ann` declares only `value`; the pair `foo = 1` names an element the
// annotation type does not have — javac's `no annotation member named foo`.

// -- red: the same element is given a value twice ------------------------------

snapshot!(
    duplicate_member,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int value();
}

@Ann(value = 1, value = 2)
class Anns {}
",
    )])
);
// §9.7.1: `value` is assigned twice; the later pair is the error — javac's
// `duplicate annotation member value`.

// -- red: a literal of the wrong type ------------------------------------------

snapshot!(
    literal_type_mismatch,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    String value();
}

@Ann(value = 1)
class Anns {}
",
    )])
);
// §9.7.1/[§5.2]: an `int` literal is not assignable to the `String` element —
// javac's incompatible-types block.

// -- green: matching literals, incl. the int→byte/short/char narrowing --------

snapshot!(
    matching_literals_and_narrowing,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface AnnI {
    int value();
}

@interface AnnS {
    String value();
}

@interface AnnB {
    byte value();
}

@AnnI(value = 7)
@AnnS(value = \"text\")
@AnnB(value = 100)
class Anns {}
",
    )])
);
// §5.2: `byte` accepts the fitting `int` constant 100 by constant narrowing;
// nothing is reported.

snapshot!(
    narrowing_out_of_range,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface AnnB {
    byte value();
}

@AnnB(value = 1000)
class Anns {}
",
    )])
);
// §5.2: the `int` constant 1000 does not fit `byte`, so the narrowing does
// not apply and the value is rejected.

// -- red: enum-constant element values ([§8.9], [§9.7.1]) ----------------------

snapshot!(
    enum_constant_bare_unknown,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

enum Color { RED, GREEN }

@interface ColorAnn {
    Color value();
}

@ColorAnn(value = BLUE)
class Anns {}
",
    )])
);
// §9.7.1: a bare `BLUE` merges its declaring type from the element's type
// (`Color`); `Color` has no `BLUE` constant, so the symbol is unresolvable.

snapshot!(
    enum_constant_qualified,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

enum Color { RED, GREEN }

enum Shape { CIRCLE }

@interface ColorAnn {
    Color value();
}

@interface ShapeAnn {
    Shape value();
}

@ColorAnn(value = Color.RED)
@ColorAnn(value = Color.BLUE)
@ShapeAnn(value = Color.RED)
class Anns {}
",
    )])
);
// §9.7.1: the qualified `Color.RED` resolves (element type `Color` accepts
// it), `Color.BLUE` is not a constant of `Color`, and `ShapeAnn` cannot take a
// `Color` value.

snapshot!(
    bare_enum_against_non_enum_element,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    String value();
}

@Ann(value = FOO)
class Anns {}
",
    )])
);
// §9.7.1: the element's type (`String`) is not an enum, so the bare `FOO` has
// no declaring type to resolve against — `cannot resolve symbol`.

// -- red/green: class literals ([§15.8.2]) --------------------------------------

snapshot!(
    class_literal_values,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    Class<?> value();
}

@Ann(value = String.class)
@Ann(value = 1)
class Anns {}
",
    )])
);
// §9.7.1: a class literal values `Class`; `String.class` matches the `Class<?>`
// element while the `int` literal does not.

// -- red/green: array element values ([§9.7.1], [§10.6]) -----------------------

snapshot!(
    array_element_values,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int[] value();
}

@Ann(value = { 1, 2, 3 })
@Ann(value = { 1, \"x\" })
@Ann(value = 42)
@Ann(value = \"nope\")
class Anns {}
",
    )])
);
// §9.7.1: each array-initializer element is checked against the component type
// `int` (the `String` fails); a single value against the array element is the
// one-element shortcut — `42` is accepted, the `String` shortcut is rejected.

// -- red: a non-array element receiving an array initializer -------------------

snapshot!(
    array_initializer_for_non_array_element,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int value();
}

@Ann(value = { 1 })
class Anns {}
",
    )])
);
// §10.6/[§9.7.1]: an array initializer where the element is not an array is a
// compile-time error, reported at the initializer.

// -- red/green: nested annotation values ([§9.7.1]) ----------------------------

snapshot!(
    nested_annotation_values,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Inner {
    int value();
}

@interface Outer {
    Inner inner();
}

@Outer(inner = @Inner(value = 1))
@Outer(inner = @Inner(value = \"x\"))
class Anns {}
",
    )])
);
// §9.7.1: a nested annotation values its own annotation type, and its own
// argument list is checked recursively — the `String` value of `Inner.value`
// (an `int` element) is rejected.

// -- red: a JDK library annotation's elements are enforced too -----------------

snapshot!(
    jdk_library_annotation,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@SuppressWarnings(1)
class Anns {}
",
    )])
);
// §9.7.1: `@SuppressWarnings`' elements are read from the JDK classfile —
// its `value()` is `String[]`, against which the `int` literal fails (the
// single-value array shortcut checks the component type `String`).

// -- red: an element without a default value and without a pair ----------------

snapshot!(
    missing_element,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int x();
    int y() default 2;
}

@Ann
class Anns {}

@Ann()
class Empty {}
",
    )])
);
// §9.7.1: a normal annotation must contain a pair for every element of the
// annotation interface except those with a default value, so `@Ann` and
// `@Ann()` — the degenerate case of no pairs at all — are both missing `x`.
// The report is anchored at the annotation's *name*, and `y` (which has a
// default) is not named.

snapshot!(
    missing_element_several,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int x();
    int y();
    Class<?> c();
}

@Ann(c = String.class)
class Anns {}
",
    )])
);
// §9.7.1: every element left without a pair is named, in declaration order,
// in the single report.

snapshot!(
    missing_element_provided,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int value();
}

@Ann(1)
class Single {}
",
    )])
);
// §9.7.1: the single-argument form `@Ann(1)` is the implicit `value` pair, so
// the element is not missing; nothing is reported.

snapshot!(
    missing_element_nested,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Inner {
    int v();
    int w() default 0;
}

@interface Outer {
    Inner in();
}

@Outer(in = @Inner)
class Anns {}
",
    )])
);
// §9.7.1: the rule runs for a nested annotation value too — its own argument
// list is missing a pair for `Inner.v`.

// -- red/green: an element-free annotation type ([§9.6.1], [§9.7.1]) -----------

snapshot!(
    element_free_annotation_type,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

class Anns {
    @Override(target = \"\")
    public String toString() {
        return \"\";
    }
}
",
    )])
);
// §9.6.1/§9.7.1: `java.lang.Override` declares no methods, so its element list
// is empty — no pair of a normal annotation of it can name an element. The
// report is the pair's (at its value), and the method does override
// `Object.toString`, so nothing else is reported.

snapshot!(
    element_free_annotation_elided_value,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

class Anns {
    @Override(\"\")
    public String toString() {
        return \"\";
    }
}
",
    )])
);
// §9.7.1: the single-value form `@Override("")` is the implicit pair for the
// element `value`, and an annotation interface without elements declares no
// `value` either.

snapshot!(
    element_free_annotation_green,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

class Anns {
    @Override
    public String toString() {
        return \"\";
    }
}
",
    )])
);
// §9.6.1/§9.7.1: an element-free annotation interface has nothing that could
// be missing — a bare `@Override` on a real override is complete.

snapshot!(
    empty_library_annotation_type_arguments,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.Documented;

@Documented(nope = 1)
@interface Ann {}
",
    )])
);
// §9.6.1/§9.7.1: the same for a classfile-declared annotation type — the
// fixture's `java.lang.annotation.Documented` declares no methods, so its
// argument list can only name a non-element. `@Documented` is applicable to an
// annotation type declaration (its `@Target` set is empty, [§9.6.4.1]), so the
// pair is the only report.

// -- green: constant expressions ([§15.29]) ------------------------------------

snapshot!(
    constant_expression_values,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int i();
}

@Ann(i = 1 + 2)
@Ann(i = 1 < 2 ? 300 : 1)
@Ann(i = 'a')
@Ann(i = (byte) 300)
@Ann(i = +1)
@Ann(i = ~1)
@Ann(i = true ? 1 : 2)
class Anns {}
",
    )])
);
// §9.7.1/[§15.29]: the arithmetic, shift, relational, conditional, unary and
// cast forms of a constant expression are all admitted — each of these is a
// constant expression of type `int`, so nothing is reported.

snapshot!(
    constant_string_concatenation,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    String s();
}

@Ann(s = \"a\" + 1)
@Ann(s = \"a\" + 'b')
class Anns {}
",
    )])
);
// §15.29/§15.18.1: `+` with a `String` operand is string concatenation, whose
// result is itself a `String` constant expression.

snapshot!(
    abrupt_expression_value,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    int i();
}

@Ann(i = 1 / 0)
class Anns {}
",
    )])
);
// §15.29: a constant expression must not complete abruptly — a division by a
// zero constant divisor does, so the value is not one.

snapshot!(
    null_literal_value,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    String s();
}

@Ann(s = null)
class Anns {}
",
    )])
);
// §9.7.1: "`v` is not `null`" — and §15.29's constant expressions are literals
// of primitive type or of type `String`, so the null literal is not one. It is
// assignable to the `String` element, so the constant-expression rule is what
// reports it.

// -- red/green: names that denote variables ([§6.5.6], [§4.12.4]) --------------

snapshot!(
    non_constant_field_value,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface IntAnn {
    int i();
}

@interface StringAnn {
    String s();
}

class Anns {
    int field = 1;
    final int f1 = 1;

    @IntAnn(i = field)
    @IntAnn(i = this.f1)
    @StringAnn(s = field)
    void run() {}
}
",
    )])
);
// §9.7.1/[§15.29]/[§4.12.4]: `field` is not `final` and `this.f1` is not the
// qualified name `TypeName.Identifier` of [§6.5.6.2], so neither is a constant
// variable — an `int` element reports the missing constant expression. Against
// the `String` element the *type* is what is wrong first (`int` is not
// assignable to `String`), which is the report javac makes too.

snapshot!(
    constant_variable_value,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface IntAnn {
    int i();
}

@interface StringAnn {
    String s();
}

@interface ShortAnn {
    short h();
}

class Anns {
    static final int K = 1;
    static final int L = K;
    static final String TEXT = \"t\";
    static final short SHORT_K = 100;

    @IntAnn(i = K)
    @IntAnn(i = K + K)
    @IntAnn(i = L)
    @IntAnn(i = Anns.K)
    @StringAnn(s = TEXT)
    @StringAnn(s = \"a\" + K)
    @ShortAnn(h = SHORT_K)
    @ShortAnn(h = K)
    @StringAnn(s = K)
    void run() {}
}
",
    )])
);
// §4.12.4: `K` and `L` are constant variables — `final`, of type `int`, with a
// constant expression as initializer — so every value above is a constant
// expression ([§15.29]), and `K`'s value narrows to `short` ([§5.2]). Only the
// last pair fails, and as a *type* mismatch: an `int` constant is not
// assignable to a `String` element.

// -- red/green: the class-literal form ([§15.8.2]) -----------------------------

snapshot!(
    class_literal_form,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Ann {
    Class<?> c();
}

class Anns {
    static final Class<?> THIS_TYPE = String.class;

    @Ann(c = String.class)
    @Ann(c = (String.class))
    @Ann(c = THIS_TYPE)
    void run() {}
}
",
    )])
);
// §9.7.1: a `Class`-typed element takes the class literal itself
// ([§15.8.2]) — `(String.class)` is a parenthesized expression, not a class
// literal, and `THIS_TYPE` is a `Class`-valued constant variable, not a class
// literal; both are reports javac makes.

// -- red/green: the enum-constant form ([§8.9.1]) ------------------------------

snapshot!(
    enum_constant_form,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

enum Color { RED }

@interface Ann {
    Color c();
}

class Anns {
    static final Color ME = Color.RED;
    static final int NOT_ENOUGH = 1;

    @Ann(c = Color.RED)
    @Ann(c = (Color.RED))
    @Ann(c = ME)
    @Ann(c = null)
    @Ann(c = NOT_ENOUGH)
    void run() {}
}
",
    )])
);
// §9.7.1/[§8.9.1]: an enum-typed element takes an enum constant. A
// parenthesized `(Color.RED)` still is one — IntelliJ looks through the
// parentheses, and so does javac — while `ME` (a `Color`-valued constant
// variable) and `null` are not. An `int` value is not assignable to the enum
// element at all, so it is a type mismatch.

// -- green: names written with unicode escapes ([§3.3]) -------------------------

snapshot!(
    unicode_escaped_names,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Anno {
    int \\u0078();
    String v\\u0061lue() default \"\";
}

class Anns {
    static final int my\\u005Fvar = 1;

    @Anno(\\u0078 = my\\u005Fvar)
    @Anno(x = my_var, value = \"v\")
    void run() {}
}
",
    )])
);
// §3.3/[§9.7.1]/[§6.5.6.1]: the names a pair and an element are keyed by are
// the *translated* source text — `\u0078` names the element `x`, and
// `my\u005Fvar` is the constant variable `my_var` — so the escaped and the
// plain spellings name the same things and nothing is reported.

// -- red: a JDK annotation's element without a default -------------------------

snapshot!(
    missing_element_jdk_library,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@SuppressWarnings()
class Anns {}
",
    )])
);
// §9.7.1: the elements of a JDK annotation are read from its classfile —
// `java.lang.SuppressWarnings.value()` carries no `AnnotationDefault`
// attribute ([JVMS §4.7.22]), so the empty argument list is missing its pair.

// -- red/green: the JDK's own classfiles ---------------------------------------

#[test]
fn library_classfile_values() {
    // The JDK's own classfiles: `Integer.MAX_VALUE` and `Math.PI` are
    // *constant variables* ([§4.12.4]) whose `ConstantValue` attributes
    // ([JVMS §4.7.2]) the layer reads; `File.separator` is a `static final
    // String` *without* one, so it is no constant; `Character.MIN_VALUE` and
    // `Character.MAX_VALUE` share a descriptor but not a value, so the
    // constant narrowed into a `byte` element is the field's own. `@Deprecated`'s
    // elements all carry an `AnnotationDefault` ([JVMS §4.7.22]) and
    // `@SuppressWarnings`' one is given a value, so neither reports anything.
    let Some(out) = crate::common::check_class_diagnostics_real_jdk(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.io.File;

@interface IntAnn {
    int i();
}

@interface StrAnn {
    String s();
}

@interface DblAnn {
    double d();
}

@interface ByteAnn {
    byte b();
}

@Deprecated
@SuppressWarnings(\"unchecked\")
class Anns {
    @IntAnn(i = Integer.MAX_VALUE)
    @DblAnn(d = Math.PI)
    @IntAnn(i = Math.PI)
    @StrAnn(s = Math.PI)
    @StrAnn(s = File.separator)
    @ByteAnn(b = Character.MIN_VALUE)
    @ByteAnn(b = Character.MAX_VALUE)
    void run() {}
}
",
    )]) else {
        return;
    };
    insta::assert_snapshot!("library_classfile_values", out);
}

// -- red/green: values written with unicode escapes ([§3.3]) -------------------

snapshot!(
    unicode_escape_values,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface ByteAnn {
    byte b();
}

@interface CharAnn {
    char c();
}

@interface StringAnn {
    String s();
}

@ByteAnn(b = '\\u007f')
@ByteAnn(b = '\\u00ff')
@CharAnn(c = '\\u0061')
@StringAnn(s = \"\\u0041\")
@StringAnn(s = \"\\u005Cn\")
class Anns {}
",
    )])
);
// §3.3/[§15.29]/[§5.2]: a literal's value is read through the Unicode-escape
// translation the lexer tokenizes by, so `'\u007f'` is the character 127 —
// which narrows to `byte` — while `'\u00ff'` is 255, which does not; javac
// reports exactly the second. The strings are constant expressions either way,
// the newline a `'\u005C'` + `n` spells included.
