//! JLS SE 26 scenario snapshots for the record *compact constructor*
//! ([JLS §8.10.4]): `record R(int x) { R { ... } }` declares the canonical
//! constructor in compact form — its implicit formal parameter list is the
//! record's component list. `new R(1)` resolves against the real compact
//! constructor and a parameterless `new R()` does not resolve (the compact
//! form leaves no zero-parameter canonical behind).

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- green/red: compact canonical constructor arity ---------------------------

snapshot!(
    compact_ctor_canonical_signature,
    check_body_diagnostic_spans(&[(
        "/src/com/example/R.java",
        "\
package com.example;

record R(int x) {
    R {
    }
}

class Body {
    void f() {
        R a = new R(1);
        int x = a.x();
        R bad = new R();
    }
}
",
    )])
);
// javac: the compact constructor's canonical parameter list is `(int)`, so
// `new R(1)` resolves and `new R()` is a wrong-arity / no-such-constructor
// error. Pre-fix the compact constructor was emitted with an empty parameter
// list, so `new R()` wrongly resolved against it (and `new R(1)` against the
// duplicate implicit canonical).
