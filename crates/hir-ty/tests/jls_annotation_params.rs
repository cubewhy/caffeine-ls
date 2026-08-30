//! JLS SE 26 scenario snapshots for annotation *argument* parsing
//! ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7),
//! [§9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)):
//! the element-value pairs of every annotation — declaration annotations and
//! type-use annotations ([§9.7.4]) alike — are lowered into a structured
//! [`AnnotationRef`] carrying each pair's name, value and source range. The
//! single-argument form `@Foo(v)` is the implicit `value` element.
//!
//! The renderer ([`check_annotations`]) prints every item's annotations with
//! their parsed arguments, so nested, array, enum, class-literal and raw
//! (unresolved) values are asserted verbatim.

#[macro_use]
mod common;

use crate::common::check_annotations;

// -- green: the element-value forms of §9.7.1 --------------------------------

snapshot!(
    element_value_forms,
    check_annotations(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@Ann(
    a = 1,
    b = 2L,
    c = 'x',
    d = true,
    e = \"text\",
    f = Foo.BAR,
    g = String.class,
    h = { 1, 2, 3 },
    i = @Nested(x = false),
    j = -5
)
class Anns {}
",
    )])
);
// Every element value form of [§9.7.1]: primitive and string literals, a
// qualified enum constant `Foo.BAR`, a class literal `String.class`, an array
// initializer `{ 1, 2, 3 }`, a nested annotation `@Nested(...)` and — as an
// *unresolved* raw expression — the unary `-5` (not a constant literal).

snapshot!(
    single_argument_form,
    check_annotations(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@SuppressWarnings(\"unchecked\")
@Target(METHOD)
class Anns {}
",
    )])
);
// §9.7.1: the single-argument form `@Foo(v)` is lowered as an implicit
// `value = v` pair. A bare identifier (`METHOD`) is a bare enum constant with
// no qualifier — its declaring type is the element's type.

snapshot!(
    marker_and_empty,
    check_annotations(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@Marker
@Empty()
@Pair(a = 1)
class Anns {
    @Marker
    void run() {}
}
",
    )])
);
// A marker annotation (`@Marker`) and an empty argument list (`@Empty()`) both
// carry no pairs; a single named pair is kept under its element name.

snapshot!(
    nested_annotation_value,
    check_annotations(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@Outer(inner = @Inner(value = 1), arr = { @A, @B(x = \"y\") })
class Anns {}
",
    )])
);
// Nested annotations appear both as a named element value and as array
// elements; a marker `@A` inside the array has no pairs.

// -- green: type-use annotations keep their arguments ([§9.7.4]) -------------

snapshot!(
    type_use_annotations,
    check_annotations(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.util.List;

class Anns {
    @TUA(level = 1) String field;
    List<@TUA(level = 2) String> list;
    int @TUA(level = 3) [] dim;
    @TUA(level = 4) <T> void generic() {}
}
",
    )])
);
// Type-use annotations on a field's type, a generic type argument, an array
// dimension and a method type parameter each carry their parsed arguments —
// the same [`AnnotationRef`] currency as declaration annotations.

// -- green: annotations on record components and modules ----------------------

snapshot!(
    record_and_module_annotations,
    check_annotations(&[
        (
            "/src/com/example/Rec.java",
            "\
package com.example;

@Ann
record Rec(@Comp(a = \"x\") String name) {}
",
        ),
        (
            "/src/module-info.java",
            "\
@Ann(module = true)
module com.example {}
",
        ),
    ])
);
// Record-component annotations and module annotations lower like any
// declaration annotation, with their arguments.
