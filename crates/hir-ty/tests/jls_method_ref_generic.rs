//! JLS SE 26 scenario snapshots for *inexact method references to generic
//! methods* ([JLS §15.13.1], [§18.5.2.2]): a reference such as
//! `Optional::of` names `<T> Optional<T> of(T)`, whose type parameter is
//! only *potentially* applicable until the enclosing invocation's joint
//! inference instantiates it. When the reference is the argument of another
//! generic invocation — `opt.map(Optional::of)` over
//! `<U> Optional<U> map(Function<? super T, ? extends U>)` — the referenced
//! method's type parameter becomes a fresh inference variable of the shared
//! table, so the parameter constraint `⟨String → α⟩` and the return
//! constraint `Optional<α> <: Optional<U>` solve together from the expected
//! result type `Optional<Optional<String>>`.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: generic static factory reference as the argument of a generic
// -- invocation whose own type variable is target-driven.

snapshot!(
    generic_static_factory_ref_in_map,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Optional;

class Body {
    Optional<Optional<String>> nested() {
        Optional<String> id = Optional.of(\"x\");
        return id.map(Optional::of);
    }

    Optional<Optional<String>> direct() {
        Optional<String> id = Optional.of(\"x\");
        Optional<Optional<String>> r = id.map(Optional::of);
        return r;
    }

    static class RLoc {
        RLoc(String s) {}
    }

    Optional<RLoc> chained(Optional<String> id) {
        return id.map(Optional::of)
            .orElseGet(() -> Optional.of(\"item\"))
            .map(RLoc::new);
    }
}
",
    )])
);
// §15.13.1/§18.5.2.2: `Optional::of` is `<T> Optional<T> of(T)` — a generic
// method, so an inexact reference to it is only potentially applicable by
// arity. Its `T` becomes a fresh variable of `map`'s inference table:
// `id.map(Optional::of)` over an `Optional<String>` receiver contributes
// `⟨String → α⟩` (from the parameter) and `Optional<α> <: Optional<U>`
// (from the referenced return), so the target `Optional<Optional<String>>`
// resolves `U := Optional<String>` and `α := String`. The chained case
// resolves the receiver of `.map(RLoc::new)` — the `Optional<?>` returned
// by `orElseGet` — through the same joint table.

// -- red: the genuinely inapplicable uses still report -------------------------

snapshot!(
    generic_factory_ref_wrong_target,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Optional;
import java.util.function.Function;

class Body {
    Optional<String> wrong(Optional<String> id) {
        return id.map(Optional::of);
    }
}
",
    )])
);
// Red: `id.map(Optional::of)` over `Optional<String>` produces
// `Optional<Optional<String>>` — the enclosing method's `Optional<String>`
// return cannot accept it, so the invocation is reported against the
// expected type ([§18.5.2.4], [§14.17]).
