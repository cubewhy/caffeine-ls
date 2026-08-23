//! JLS SE 26 scenario snapshots for `switch`
//! ([JLS §14.11](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11),
//! [§15.28](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28)):
//! selector typing across the allowed types ([§14.11.1]), statement
//! fall-through, the arm result types of a switch expression and `yield`
//! ([§14.21]), plus type-pattern, guarded and `null` labels ([§14.30]). Red
//! cases render the diagnostics the type layer must report; green cases
//! confirm the switch forms type without errors — including bare enum
//! constant labels resolved against the selector ([§14.11.1], [§8.9.1]) and
//! the rejection of non-selectable primitive selectors ([§14.11.1]).
//! Exhaustiveness checking of switch expressions over sealed or enumerated
//! selectors is not modelled and is not snapshotted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: statement selectors over int, String and an enum ---------------------

snapshot!(
    statement_selector_forms,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

enum Color { R, G, B }

class Sw {
    int classic(int i) {
        switch (i) {
            case 1:
                return 10;
            case 2:
            case 3:
                return 30;
            default:
                return 0;
        }
    }

    int byName(String s) {
        switch (s) {
            case \"a\":
                return 1;
            default:
                return 0;
        }
    }

    int byColor(Color c) {
        switch (c) {
            case R:
                return 1;
            default:
                return 0;
        }
    }
}
",
    )])
);

// -- red: a non-selectable primitive selector ([§14.11.1]) ------------------------
// Only the int-compatible primitives may be selected; `javac` 25 rejects
// `double` (a primitive pattern is required to switch on it).

snapshot!(
    unsupported_selector,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    double m(double d) {
        switch (d) {
            default:
                return 0;
        }
    }
}
",
    )])
);

// -- green: switch expression result types and yield ------------------------------

snapshot!(
    expression_result_types,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    int m(int i) {
        int r = switch (i) {
            case 1 -> 10;
            case 2 -> {
                yield 20;
            }
            default -> 30;
        };
        return r;
    }
}
",
    )])
);

// -- green: type-pattern, guarded and null labels ([§14.30]) ----------------------

snapshot!(
    pattern_and_null_labels,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    int m(Object o) {
        return switch (o) {
            case Integer i when i > 0 -> i;
            case Number n -> 1;
            case null -> -1;
            default -> 0;
        };
    }
}
",
    )])
);

// -- green: an exhaustive switch expression over an enum ([§14.11.1]) -------------
// Every constant is named by some label, so no `default` is required.

snapshot!(
    exhaustive_enum_switch,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

enum Color { R, G, B }

class Sw {
    int m(Color c) {
        return switch (c) {
            case R -> 1;
            case G, B -> 2;
        };
    }
}
",
    )])
);

// -- red: a switch expression that is not exhaustive ([§15.28]) --------------------
// `G` and `B` have no arm and there is no `default`; `javac` 25 reports "the
// switch expression does not cover all possible input values".

snapshot!(
    not_exhaustive,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

enum Color { R, G, B }

class Sw {
    int m(Color c) {
        return switch (c) {
            case R -> 1;
        };
    }
}
",
    )])
);

// -- green: constant-expression labels ([§15.28]) ---------------------------------
// Constant expressions over literals and over a *constant variable* ([§4.12.4])
// are legal labels; the int-typed constant `TWO + 1` also narrows to the
// selector's `byte` in assignment context ([§5.2], [§5.1.3]).

snapshot!(
    constant_expression_labels,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    void m(int i) {
        final int two = 2;
        switch (i) {
            case 1 + 2 * 3 -> f(7);
            case two + 1 -> f(3);
            default -> {}
        }
        String s = \"a\" + \"b\";
    }

    void f(int x) {}
}
",
    )])
);

// -- red: duplicate and non-constant labels ([§14.11.1], [§15.28]) -----------------
// Two arms naming the same constant value make one unreachable, and a plain
// local is not a constant expression — `javac` 25 reports "constant
// expression required".

snapshot!(
    duplicate_and_non_constant_labels,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    void m(int i) {
        int notConstant = 2;
        switch (i) {
            case 1 + 1 -> f(2);
            case 2 -> f(0);
            case notConstant -> f(1);
            default -> {}
        }
    }

    void f(int x) {}
}
",
    )])
);
