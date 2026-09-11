//! Conformance snapshots for the *identity* of a type variable
//! ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4),
//! [§6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3),
//! [§6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4.1),
//! [§8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4)).
//!
//! A type variable is introduced by the declaration that lists it and is
//! scoped to that declaration, so a method type parameter named `T` and a
//! class type parameter named `T` are two *distinct types* (javac's
//! `T#1`/`T#2`). Substitution is declared over one declaration's parameters
//! and is therefore capture-avoiding: instantiating `Either<T, Entry<T, ?>>`'s
//! class parameter must not touch the `T` of a called method, even though the
//! two share a name.
//!
//! Every scenario is verified against `javac` before the snapshot is
//! accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

/// The receiver class and the called method both name a type parameter `T`:
/// `Either<L, R>`'s `<T> T map(...)` against a receiver whose arguments are
/// spelled in terms of the *caller's* `T`. javac compiles it — the two `T`s
/// are distinct types ([§6.4.1]) — and the `UnaryOperator` argument
/// (`Function<Entry<T, ?>, Entry<T, ?>>`) must not be re-typed as the
/// method's own `T` by the receiver instantiation ([§4.4] capture-avoidance).
const SHADOWED_RECEIVER: &str = "\
package com.example;

import java.util.function.Function;

class Either<L, R> {
    <T> T map(Function<L, T> first, Function<R, T> second) {
        return null;
    }
}

class Entry<T, A> {
    A argument;

    static <T> Entry<T, T> createOverride(T value) {
        return null;
    }
}

class Body {
    static <T> Entry<T, ?> codec(Either<T, Entry<T, ?>> either) {
        Function<T, Entry<T, ?>> first = Entry::createOverride;
        Function<Entry<T, ?>, Entry<T, ?>> second = e -> e;
        return either.map(first, second);
    }
}
";

// JLS §4.4/§6.4.1: the receiver's `T` (the class parameter of `Body.codec`'s
// declaration) stays the receiver's `T` — the invocation type is
// `Entry<T, ?>`, and the call is applicable.
snapshot!(
    shadowed_method_type_param_is_distinct,
    check_body_types(&[("/src/com/example/Body.java", SHADOWED_RECEIVER,)])
);

// Green control: renaming the *callee's* parameter to `X` must change nothing
// — the call compiles either way, which is exactly the point (§6.4.1 makes
// the name irrelevant to identity).
snapshot!(
    distinctly_named_callee_param,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.function.Function;

class Either<L, R> {
    <X> X map(Function<L, X> first, Function<R, X> second) {
        return null;
    }
}

class Entry<T, A> {
    A argument;

    static <T> Entry<T, T> createOverride(T value) {
        return null;
    }
}

class Body {
    static <T> Entry<T, ?> codec(Either<T, Entry<T, ?>> either) {
        Function<T, Entry<T, ?>> first = Entry::createOverride;
        Function<Entry<T, ?>, Entry<T, ?>> second = e -> e;
        return either.map(first, second);
    }
}
",
    )])
);

// JLS §4.4/§18.5.2.2: the *invocation* substitution of a generic method
// instantiates the method's own parameters only. Here `Box<T>`'s class
// parameter is named `T` and the invoked method declares its own `T`; the
// argument `Box<T>` (the caller's `T`) must satisfy the method's `Box<T>`
// formal without the two `T`s being equated — javac accepts the call and
// infers the method's `T` as the caller's `T` (a distinct variable that the
// constraint `Box<T#1> <: Box<T#2>` lowers to `T#1 = T#2`, §18.2.2).
snapshot!(
    invocation_instantiates_own_params_only,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Box<T> {
    T value;

    static <T> T unwrap(Box<T> box) {
        return box.value;
    }
}

class Body<T> {
    T read(Box<T> box) {
        return Box.unwrap(box);
    }
}
",
    )])
);
