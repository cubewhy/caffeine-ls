//! JLS SE 26 scenario snapshots for generics conformance
//! ([§15.9.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.2),
//! [§4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)):
//! the diamond operator on a non-generic class, and the least-upper-bound of
//! array types in conditional expressions.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- red: the diamond on a non-generic class ([§15.9.2]/[§15.9.2.1]) -----------

snapshot!(
    diamond_on_non_generic,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

class Plain {
    void f() {
        Plain p = new Plain<>();
    }

    static class Outer<T> {
        class Inner {
        }

        void g() {
            Outer<String>.Inner x = new Outer<String>().new Inner<>();
        }
    }
}
",
    )])
);
// Red: `new Plain<>()` on a non-generic class, and the diamond on the
// non-generic member class `Inner` of a parameterized outer — javac: `cannot
// use '<>' with non-generic class`.

// -- red/green: the lub of array types in a conditional ([§4.10.4]) ------------
// The lub of two reference-array types is the array of the element lub, not
// the supertype intersection — `Object[] v = c ? new String[]{...} : new
// Integer[1]` must type the conditional as an array.

snapshot!(
    conditional_array_lub,
    check_body_types(&[(
        "/src/com/example/P.java",
        "\
package com.example;

class Arr {
    boolean c;

    void ok() {
        Object[] v = this.c ? new String[] { \"a\" } : new Integer[1];
    }

    void red() {
        Integer[] w = this.c ? new String[] { \"a\" } : new Integer[1];
    }
}
",
    )])
);
// Green: the conditional of `String[]`/`Integer[]` is an array type
// (`Object[]` accepts it). Red: assigning that array to `Integer[]` fails
// (the element lub is not `Integer`).
