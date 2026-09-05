//! JLS SE 26 scenario snapshots for lambda arguments to a *variable-arity*
//! invocation ([JLS §15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4)):
//! the trailing actuals are compatible with the *i*-th variable arity
//! parameter type — the element `Fn` for `i ≥ n`
//! ([§8.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.1)) —
//! so a trailing lambda's parameter list must have the same arity as that
//! element's single abstract method
//! ([§15.27.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27.3)).
//! An overload whose varargs element does not fit the lambda is not
//! applicable ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)).

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: a 2-param lambda selects the matching varargs element ---------------

snapshot!(
    varargs_lambda_arity_selects_overload,
    check_body_types(&[(
        "/src/com/example/Varargs.java",
        "\
package com.example;

import java.util.function.Function;
import java.util.function.BiFunction;

class Varargs {
    static String m(Function<String, String>... fs) {
        return null;
    }

    static String m(BiFunction<String, String, String>... bs) {
        return null;
    }

    static void use() {
        String s = m((x, y) -> x + y);
    }
}
",
    )])
);
// javac: the trailing lambda's arity (2) must match the *element* type's SAM
// ([§15.27.3]) — `Function<String,String>` has arity 1, so that overload is
// not applicable; the `BiFunction<String,String,String>` overload (arity 2)
// is selected and the lambda parameter types `x`/`y` come from its SAM.

// -- red: a trailing lambda whose arity mismatches the element is rejected -----

snapshot!(
    varargs_lambda_arity_rejects_mismatch,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Varargs.java",
        "\
package com.example;

import java.util.function.Function;

class Varargs {
    static void m(Function<String, String>... fs) {
    }

    static void use() {
        m((x, y) -> x + y);
    }
}
",
    )])
);
// javac: the 2-param lambda is incompatible with the `Function<String,String>`
// element (arity 1) — "varargs mismatch; incompatible parameter types" — so
// the invocation is not applicable and must draw a not-applicable diagnostic
// on the call (pre-fix the overload was wrongly accepted with no diagnostic).
