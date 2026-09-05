//! JLS SE 26 scenario snapshots for the *most-specific* test
//! ([JLS §15.12.2.5]): the functional-interface specificity applies only to
//! lambda/method-reference *argument expressions* (a concrete argument of a
//! type implementing two unrelated interfaces stays ambiguous), and the
//! variable-arity alignment follows the greater declared parameter count (as
//! javac compares) rather than the invocation's argument count.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- red: functional-interface specificity requires a lambda argument --------

snapshot!(
    fi_specificity_requires_lambda_argument,
    check_body_diagnostic_spans(&[(
        "/src/com/example/F.java",
        "\
package com.example;

class F {
    interface S {
        String get();
    }

    interface T {
        Object get();
    }

    static class Both implements S, T {
        public String get() {
            return \"\";
        }
    }

    static void f(S s) {}
    static void f(T t) {}

    static void concrete(Both b) {
        f(b);
    }

    static void lambda() {
        f(() -> \"\");
    }
}
",
    )])
);
// javac: `f(b)` for a concrete `Both implements S, T` argument is ambiguous —
// neither `S` nor `T` is more specific for a non-lambda argument, so the
// §15.12.2.5 functional-interface rule does not apply. The same fixture's
// `f(() -> "")` is fine (the lambda makes `S` more specific — `String` is a
// subtype of `Object`), so only the concrete call is reported. Pre-fix the
// whole-list FI pre-check picked `f(S)` for the concrete argument too.

// -- red/green: varargs specificity uses the greater declared count ----------

snapshot!(
    varargs_specificity_uses_declared_arity,
    check_body_diagnostic_spans(&[(
        "/src/com/example/V.java",
        "\
package com.example;

class V {
    static String m(String x, Integer... rest) {
        return null;
    }

    static String m(CharSequence... cs) {
        return null;
    }

    static String one() {
        return m(\"a\");
    }

    static String two() {
        return m(\"a\", 1);
    }
}
",
    )])
);
// javac: `m("a")` over `m(String, Integer...)` / `m(CharSequence...)` is
// AMBIGUOUS — the comparison aligns at the longer declaration (2 params), so
// the second position compares `Integer` (m1's trailing element) against
// `CharSequence` and fails; a literal invocation-arity reading (k = 1) would
// wrongly pick `m(String, Integer...)`. `m("a", 1)` resolves to the
// fixed-prefix overload.
