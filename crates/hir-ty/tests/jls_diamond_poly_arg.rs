//! Diamond class instance creation as a *poly argument* of a generic method
//! ([JLS §15.9.3], [§18.5.2.4]): `register(new Impl<>(c, id))` — the diamond
//! `new Impl<>(...)` is a poly expression whose constructor type parameters
//! are inferred jointly with the enclosing method's. The constructor argument
//! (`Class<T> c` with `T` the enclosing method's type variable) binds the
//! diamond's variable (`β := T`), and the lower bound `Impl<β> <: α_Y`
//! against the upper bound `α_Y <: Impl<α_T>` of `register`'s declared
//! `Y extends Impl<T>` relates `β = α_T` through the declared supertype
//! chain ([§18.2.2]/[§18.3.1]) — `Numeric<β> <: Impl<β>` — so the method's
//! own type parameters receive the diamond's constraints and resolve within
//! their bounds ([§18.4.1]). Every scenario is verified against `javac`
//! before the snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §15.9.3/[§18.5.2.4]: the diamond `new Impl<>(c, id)` joins the
// enclosing `register`'s inference — `c : Class<T>` binds the diamond's `β`
// to the enclosing method's own `T`, and `β = α_T` flows through the
// `Y extends Impl<T>` bound, so both the plain `Impl` and the *subclass*
// `Numeric` (whose declared `extends Impl<β>` chain carries the link) resolve
// to `Impl<T>`-typed arguments. Pre-hoisting to a local
// (`Impl<T> direct = new Impl<>(c, id); return register(direct);`) already
// worked; the inline form is the gap.
snapshot!(
    diamond_into_generic_method,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static class BinaryTag {}
    static class Impl<T extends BinaryTag> {
        Impl(Class<T> c, byte id) {}
    }
    static class Numeric<T extends BinaryTag> extends Impl<T> {
        Numeric(Class<T> c, byte id) { super(c, id); }
    }
    private static <T extends BinaryTag, Y extends Impl<T>> Y register(Y y) {
        return y;
    }
    static <T extends BinaryTag> Impl<T> register(Class<T> c, byte id) {
        return register(new Impl<>(c, id));
    }
    static <T extends BinaryTag> Impl<T> registerNumeric(Class<T> c, byte id) {
        return register(new Numeric<>(c, id));
    }
}
",
    )])
);
