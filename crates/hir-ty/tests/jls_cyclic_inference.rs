//! JLS SE 26 scenario snapshots for cyclic inference-variable resolution
//! ([§18.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.4.2),
//! [§18.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2)):
//! invocations whose type variables have no lawful instantiation must be
//! rejected rather than silently Object-estimated.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- red: an equality-vs-lower conflict through a bound ([§18.4.2]) -------------

snapshot!(
    cyclic_variable_conflict,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

class G4 {
    static <T, U extends Comparable<T>> U m3(T t) {
        return null;
    }

    void t3() {
        String s = m3(1);
    }
}
",
    )])
);
// Red: `m3(1)` must type `T := Integer` (from the argument) and the target
// forces `U := String`, but `U extends Comparable<T>` then demands
// `String <: Comparable<Integer>` — no lawful instantiation exists (javac:
// `inference variable T has incompatible bounds`). The invocation is
// rejected instead of fabricating one.
