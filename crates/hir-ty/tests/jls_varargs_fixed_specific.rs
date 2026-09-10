//! Snapshots of most-specific selection between a *variable-arity* and a
//! *fixed-arity* candidate
//! ([JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2),
//! [§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5),
//! [§18.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.4)).
//!
//! A variable-arity method is treated as a fixed-arity method in the first two
//! applicability phases (§15.12.2), so `of(T...)` is applicable as `of(T[])`
//! alongside `of(T one)` for an array argument; §15.12.2.5 bullet 2 and §18.5.4
//! then compare the *declared* formals (`T[] <: T` with `T := E`) and the
//! varargs declaration wins. There is no declared-flag tie-break in §15.12.2.5
//! — the preference for a fixed-arity method over a variable-arity one comes
//! from the phase ordering, never from a flag comparison.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- §15.12.2.5/§18.5.4: an array argument selects the varargs candidate ------
// javac's picks for this fixture (verified with `javap -c`): `of(T...)` with
// `T := String` (`Seq<String>`) for `arrayLocal`, `newArray` and
// `enumValuesChain`; `of(T one)` with `T := Mode` (`Seq<Mode>`) for
// `singleElement`.

snapshot!(
    varargs_fixed_specific_body_types,
    check_body_types(&[(
        "/src/com/example/Seq.java",
        "\
package com.example;

import java.util.function.Function;

class Seq<E> {
    static <T> Seq<T> of(T one) {
        return null;
    }

    @SafeVarargs
    static <T> Seq<T> of(T... many) {
        return null;
    }

    <R> Seq<R> map(Function<? super E, ? extends R> mapper) {
        return null;
    }

    enum Mode {
        ALPHA,
        BETA
    }

    static Seq<String> arrayLocal() {
        String[] values = { \"a\" };
        return Seq.of(values);
    }

    static Seq<String> newArray() {
        return Seq.of(new String[] { \"a\" });
    }

    static Seq<String> enumValuesChain() {
        return Seq.of(Mode.values()).map(Mode::name);
    }

    static Seq<Mode> singleElement() {
        return Seq.of(Mode.ALPHA);
    }
}
",
    )])
);

// -- green: no diagnostic for any of the four invocations ---------------------

snapshot!(
    varargs_fixed_specific_diagnostics,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Seq.java",
        "\
package com.example;

import java.util.function.Function;

class Seq<E> {
    static <T> Seq<T> of(T one) {
        return null;
    }

    @SafeVarargs
    static <T> Seq<T> of(T... many) {
        return null;
    }

    <R> Seq<R> map(Function<? super E, ? extends R> mapper) {
        return null;
    }

    enum Mode {
        ALPHA,
        BETA
    }

    static Seq<String> arrayLocal() {
        String[] values = { \"a\" };
        return Seq.of(values);
    }

    static Seq<String> newArray() {
        return Seq.of(new String[] { \"a\" });
    }

    static Seq<String> enumValuesChain() {
        return Seq.of(Mode.values()).map(Mode::name);
    }

    static Seq<Mode> singleElement() {
        return Seq.of(Mode.ALPHA);
    }
}
",
    )])
);
