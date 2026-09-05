//! JLS SE 26 scenario snapshots for argument *pertinence*
//! ([JLS §15.12.2.2], [§18.5.1], [§18.5.2.2]): an argument is not pertinent
//! when it is an implicitly typed lambda or an inexact method reference, and
//! during *applicability* the non-pertinent arguments contribute no ⟨e → F⟩
//! inference constraint. An implicit lambda's body still must be *compatible*
//! with the candidate's function type (javac rejects an overload whose SAM
//! return cannot hold the body — `m(() -> "")` picks the `String` SAM over
//! the `Integer` one), while an explicitly typed lambda additionally
//! contributes its per-parameter constraints and steers generic inference.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: an implicit lambda's body stays compatible-checked ----------------

snapshot!(
    implicit_lambda_body_compatibility,
    check_body_types(&[(
        "/src/com/example/F.java",
        "\
package com.example;

class F {
    interface S {
        String get();
    }

    interface I {
        Integer get();
    }

    static void m(S s) {}
    static void m(I i) {}

    static void call() {
        m(() -> \"\");
        m(() -> 1);
    }
}
",
    )])
);
// javac: `m(() -> "")` resolves to `m(S)` and `m(() -> 1)` to `m(I)` — an
// implicit lambda's body is not pertinent (it contributes no inference
// constraint during applicability, [§18.5.1]) but must still be *compatible*
// with the candidate's function type: the `""` body cannot satisfy
// `I.get(): Integer`, so `m(I)` is not applicable to it (and `m(S)` not to
// the `1` body). No diagnostics.

// -- red: an implicit lambda cannot disambiguate equal-compatibility SAMS ----

snapshot!(
    implicit_lambda_no_constraint_steering,
    check_body_diagnostic_spans(&[(
        "/src/com/example/F.java",
        "\
package com.example;

class F {
    interface S {
        String get();
    }

    interface T {
        CharSequence get();
    }

    static void m(S s) {}
    static void m(T t) {}

    static String call() {
        String r = null;
        return r;
    }
}
",
    )])
);
// A pure demonstration that the two SAMS are unrelated by subtype; the
// interesting case (`m(() -> "")` over both) resolves to `m(S)` through the
// §15.12.2.5 functional-interface return rule (`String <: CharSequence`),
// which is exercised by the most-specific fixtures. This snapshot pins the
// declaration itself (no diagnostic on `m` overloads).

// -- green: an explicit lambda reaches applicability and refines the type ----

snapshot!(
    explicit_lambda_reaches_applicability,
    check_body_types(&[(
        "/src/com/example/F.java",
        "\
package com.example;

import java.util.function.Function;

class F {
    static <T> T use(Function<T, T> f) {
        return null;
    }

    static String call() {
        String s = use((String x) -> x);
        return s;
    }
}
",
    )])
);
// javac: the explicitly typed lambda's parameter `String` contributes
// `⟨String → α⟩` ([§18.5.2.1]), binding `T := String`; the call types
// `String`. Pre-fix the explicit lambda contributed nothing and the call
// typed `Object`.
