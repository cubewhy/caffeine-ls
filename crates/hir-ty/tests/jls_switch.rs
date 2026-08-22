//! JLS SE 26 scenario snapshots for `switch`
//! ([JLS §14.11](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11),
//! [§15.28](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28)):
//! selector typing across the allowed types ([§14.11.1]), statement
//! fall-through, the arm result types of a switch expression and `yield`
//! ([§14.21]), plus type-pattern, guarded and `null` labels ([§14.30]). Red
//! cases render the diagnostics the type layer must report; green cases
//! confirm the switch forms type without errors.
//!
//! The trailing *known divergence* section pins behaviour that still
//! contradicts the spec or `javac` 25 — bare enum constants in case labels
//! resolve as `cannot-resolve-symbol` ([§14.11.1]) and non-switchable selector
//! types such as `double` are not diagnosed ([§14.11.1]). Exhaustiveness
//! checking of switch expressions over sealed or enumerated selectors is not
//! modelled and is not snapshotted. Divergence snapshots are kept pending
//! (`.snap.new`) until the divergences are fixed; a fix must flip them.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: statement selectors over int and String -------------------------------

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

// -- known divergence: bare enum constants as case labels ([§14.11.1]) --------------
// `javac` 25 resolves the bare constant `R` against the enum selector; the
// type layer reports a spurious `cannot-resolve-symbol`.

snapshot!(
    divergence_switch_enum_selector_labels,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

enum Color { R, G, B }

class Sw {
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

// -- known divergence: non-switchable selector type ([§14.11.1]) ---------------------
// Only char/byte/short/int, their boxes, String and enum types may be
// selected; `javac` 25 rejects `double`, but the type layer stays silent.

snapshot!(
    divergence_switch_double_selector,
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
