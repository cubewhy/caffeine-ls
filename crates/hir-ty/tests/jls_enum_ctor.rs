//! JLS SE 26 scenario snapshot for the enum's implicit constructor
//! ([JLS §8.9.2]): a constructor declared in an enum with no access modifier
//! is `private` — the *implicit* constructor carries no modifier, so it is
//! private. The regression pin: `E.values()` / `E.valueOf` remain
//! expression-visible and the private constructor's access is not.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: enum implicit members stay reachable ------------------------------

snapshot!(
    enum_implicit_ctor_private,
    check_body_types(&[(
        "/src/com/example/E.java",
        "\
package com.example;

enum E {
    A
}

class Body {
    E[] all() {
        return E.values();
    }

    E byName() {
        return E.valueOf(\"A\");
    }
}
",
    )])
);
// javac: the implicit enum constructor is private ([§8.9.2]) — invisible to
// `new E()` from any source context — while the implicit `values()` and
// `valueOf` members stay public. No diagnostics on either call.
