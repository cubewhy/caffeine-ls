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

// -- red: string constants compare by value across literal forms -------------------
// ([§15.28], [§5.1.11], [§3.10.6]) An octal escape, `\s`, a text block and
// string concatenation each denote their *value*, so every plain spelling
// below collides with the form above it — `"\101"` is `"A"`, `"a\sb"` is
// `"a b"`, the text block's incidental whitespace strips away, and `"a" + 1`
// converts its int operand to text. None of them may pass as fresh labels.

snapshot!(
    string_constant_forms,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    void m(String s) {
        switch (s) {
            case \"\\101\" -> f();
            case \"A\" -> f();
            case \"a\\sb\" -> f();
            case \"a b\" -> f();
            case \"\"\"
                B
                \"\"\" -> f();
            case \"B\\n\" -> f();
            case \"a\" + 1 -> f();
            default -> {}
        }
    }

    void f() {}
}
",
    )])
);

// -- red: integral constants wrap and shift at their own width ----------------------
// ([§15.19], [§15.18.2]) An `int` shift masks the distance to five bits
// (`1 << 35` shifts by 3, so it is 8), `>>>` zero-fills within 32 bits even
// for a negative operand (`-1 >>> 28` is 15) and arithmetic wraps at 32 bits
// (`65535 * 65535` is -131071). Each pair below therefore collides.

snapshot!(
    shift_and_wrap_widths,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    void m(int sel) {
        switch (sel) {
            case 1 << 35 -> f(0);
            case 8 -> f(0);
            case -1 >>> 28 -> f(1);
            case 15 -> f(1);
            case 65535 * 65535 -> f(2);
            case -131071 -> f(2);
            default -> {}
        }
    }

    void f(int x) {}
}
",
    )])
);

// -- green/red: a constant label narrows to a narrow selector ([§5.2], [§5.1.3]) ----
// A label sits in assignment context, so the int constants below are legal
// for a `byte` selector — but `15 + 1` is still the same value as the masked
// shift above it, and the duplicate is reported.

snapshot!(
    narrow_selector_constant_label,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    void m(byte sel) {
        switch (sel) {
            case 1 << 35 -> f(0);
            case 7 + 1 -> f(0);
            default -> {}
        }
    }

    void f(int x) {}
}
",
    )])
);

// -- red: conditional labels need both branches constant ----------------------------
// ([§15.28]) A conditional expression is constant only when its condition and
// *both* branches are: `false ? x : 2` stays non-constant even though only
// the `else` arm contributes. With a constant-variable condition
// (`true & false` folds once boolean bitwise operators count as constant
// operators, [§15.22.2]) the expression folds to the `else` value and
// duplicates `case 2`.

snapshot!(
    conditional_label_constancy,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    void m(int i) {
        final boolean flag = true & false;
        final int x = i;
        switch (i) {
            case false ? x : 2 -> f(0);
            case flag ? 1 : 2 -> f(1);
            case 2 -> f(1);
            default -> {}
        }
    }

    void f(int x) {}
}
",
    )])
);

snapshot!(
    switch_expr_to_byte,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    enum Kind { ADDED, REMOVED }

    byte code(Kind kind) {
        return switch (kind) {
            case ADDED -> 1;
            case REMOVED -> -1;
        };
    }
}
",
    )])
);
// §5.2 in an assignment/return context with a switch expression as its poly
// ([§15.28]): every arm's result expression must independently be a
// representable `byte` constant — the `-1`/`1` literals narrow, so the switch
// expression as a whole is assignable to `byte`.

// -- green: trailing code after an all-returning switch *statement* ------------
// [JLS §14.21]: a switch *statement* with no `default` label can complete
// normally — the selector may match nothing — even when every arm ends in
// `return`. The trailing `return 0` is therefore reachable ([§14.22]) and the
// method completes normally on the no-match path; neither
// `unreachable-statement` nor `missing-return-statement` may be reported.

snapshot!(
    trailing_return_after_all_returning_switch,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    int m(int i) {
        switch (i) {
            case 1:
                return 10;
            case 2:
                return 20;
        }
        return 0;
    }
}
",
    )])
);

// -- red: trailing code after an all-returning switch *with* default -----------
// With a `default` label every selector value matches some arm, so the switch
// statement cannot complete normally ([§14.21]); the trailing `return` is
// unreachable ([§14.22]).

snapshot!(
    trailing_return_after_switch_with_default,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    int m(int i) {
        switch (i) {
            case 1:
                return 10;
            default:
                return 20;
        }
        return 0;
    }
}
",
    )])
);

// -- green: an all-returning switch without default is not a missing-return ---
// The no-match path falls out of the switch and is completed by the trailing
// `return`, so the method needs no `return` inside the switch at all.

snapshot!(
    fallthrough_tail_completes_method,
    check_body_types(&[(
        "/src/com/example/Sw.java",
        "\
package com.example;

class Sw {
    int m(int i) {
        switch (i) {
            case 1:
                return 10;
        }
        return 0;
    }
}
",
    )])
);
