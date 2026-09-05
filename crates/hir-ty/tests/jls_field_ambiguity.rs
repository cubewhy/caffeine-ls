//! JLS SE 26 scenario snapshots for *field-name ambiguity*
//! ([JLS §15.11.1], [§8.3]): a field access selects the *maximal*
//! (non-hidden) declarations of the name across the receiver's supertype
//! closure. A most-derived class field hides its superclass/interface
//! namesakes, but two unrelated supertype declarations (`class C implements
//! A, B` with `int X` in both) leave the reference ambiguous.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- red: two unrelated interface declarations of one field name --------------

snapshot!(
    field_ambiguity_interface_diamond,
    check_body_diagnostic_spans(&[(
        "/src/com/example/C.java",
        "\
package com.example;

interface A {
    int X = 1;
}

interface B {
    int X = 2;
}

class C implements A, B {}

class Body {
    int read(C c) {
        return c.X;
    }
}
",
    )])
);
// javac: `c.X` is ambiguous — `A.X` and `B.X` are both accessible and neither
// hides the other ([§15.11.1]); the reference is reported as ambiguous, not
// as a missing field. Pre-fix the most-derived-first short-circuit silently
// picked whichever interface the walk reached first.

// -- green: a class field still hides its superclass namesake ----------------

snapshot!(
    field_hiding_regression,
    check_body_types(&[(
        "/src/com/example/D.java",
        "\
package com.example;

class D {
    int x;
}

class E extends D {
    int x;
}

class Body {
    int read(E e) {
        return e.x;
    }
}
",
    )])
);
// javac: `e.x` resolves to `E.x` — the most-derived class field hides the
// superclass declaration ([§8.3]); no diagnostic.
