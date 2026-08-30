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
//! `@line:col` diagnostic; the annotation types here all resolve in the same
//! compilation unit, so the elements are read from the annotation type's own
//! source declaration.

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
