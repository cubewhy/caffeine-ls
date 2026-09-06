//! JLS SE 26 scenario snapshots for switch, pattern and for-each conformance
//! ([§14.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11.1),
//! [§14.14.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2),
//! [§15.20.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.20.2)):
//! duplicate `case` labels (enum constants included), type-pattern dominance
//! in switch labels, the enhanced-for variable's assignability from the
//! element type, and provably-incompatible `instanceof` tests.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- red/green: duplicate case labels, enum constants included ([§14.11.1]) ----

snapshot!(
    duplicate_case_labels,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

enum E2 { A, B }

class F2 {
    int f(E2 e) {
        return switch (e) {
            case A -> 1;
            case A -> 2;
            case B -> 3;
            default -> 4;
        };
    }

    int g(int x) {
        return switch (x) {
            case 1 -> 1;
            case 2 -> 2;
            default -> 3;
        };
    }
}
",
    )])
);
// Red: the repeated `case A` enum label. Green: the int labels of `g` do not
// repeat.

// -- red: a case pattern dominated by an earlier label ([§14.11.1]) ------------

snapshot!(
    dominated_pattern,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

class F3 {
    int f(Object o) {
        return switch (o) {
            case Number n -> 1;
            case Integer i -> 2;
            default -> 3;
        };
    }
}
",
    )])
);
// Red: `case Integer` is dominated by the earlier `case Number` — every
// `Integer` is a `Number`, so the label can never match
// (javac: `this case label is dominated by a preceding case label`).

// -- red/green: enhanced-for variable assignability ([§14.14.2]) ---------------

snapshot!(
    enhanced_for_assignability,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

import java.util.ArrayList;

class F4 {
    void red() {
        for (Integer x : new ArrayList<String>()) {
        }
    }

    void green() {
        for (String s : new ArrayList<String>()) {
        }
    }
}
",
    )])
);
// Red: the loop variable `Integer x` cannot hold the `String` elements
// (javac: `incompatible types: String cannot be converted to Integer`).
// Green: `String s` over `ArrayList<String>`.

// -- red/green: provably-incompatible instanceof ([§15.20.2]/[§5.5]) -----------

snapshot!(
    incompatible_instanceof,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

class F8b {
    Integer field = new Integer(1);

    void f(Object o) {
        Integer i = field;
        boolean a = i instanceof String;
        boolean b = o instanceof Integer;
        boolean c = \"s\" instanceof Integer;
    }
}
",
    )])
);
// Red: `Integer` and `String` are unrelated final classes — no value of one
// can be an instance of the other, so the `instanceof` is an error, and so is
// the literal `"s" instanceof Integer`. Green: `o instanceof Integer` for an
// `Object` operand stays legal ([§5.5] casts are only rejected when the types
// are provably distinct).
