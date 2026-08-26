//! Snapshots of variable-arity invocation and wildcard-bound inference in
//! generic chains ([JLS §15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4),
//! [§18.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.2),
//! [§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10)).

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- §18.2.1: a *wildcard* source argument is not an instantiation -------------
// `<T> T make(Class<T> c, Object... args)` called with a `Class<?>` bounds the
// method's type variable by containment (§18.2.3), not by equality with the
// bare wildcard: the instantiation picks up the capture's bound (`? extends
// Number` → `Number`), so the invocation stays applicable — including through
// the varargs phase with zero or non-array trailing arguments.

snapshot!(
    varargs_wildcard_class_argument,
    check_body_types(&[(
        "/src/com/example/Factory.java",
        "\
package com.example;

class Factory {
    static <T> T make(Class<T> targetClass, Object... arguments) {
        return null;
    }

    static void zeroTail(Class<?> kind) {
        Object o = Factory.make(kind);
    }

    static void mixedTail(Class<?> kind) {
        Object o = Factory.make(kind, null, true, null);
        Object p = Factory.make(kind, \"x\", 1, 2, 3, 4);
    }

    static void boundedWild(Class<? extends Number> kind) {
        Number n = (Number) Factory.make(kind);
    }
}
",
    )])
);
// Every call resolves through the varargs phase; the inferred return is the
// capture's bound, assignable to `Object`.

// -- §18.5.2.2/§5.1.10: a lambda body infers against the *decaptured* SAM ------
// `Seq<E>.flatMap(Function<? super E, ? extends Seq<? extends R>>)` gives the
// lambda body the target `Seq<? extends R>`; a nested generic invocation inside
// the body constrains `R` through that wildcard. Inferring against the raw
// capture variable instead would reduce ⟨Seq<α> → CAP#n⟩ to a conversion
// between unrelated types and reject every applicable candidate.

snapshot!(
    lambda_body_decaptured_target,
    check_body_types(&[(
        "/src/com/example/Stream.java",
        "\
package com.example;

import java.util.function.Function;

class Seq<E> {
    static <T> Seq<T> of(T a, T b) {
        return null;
    }

    <R> Seq<R> map(Function<? super E, ? extends R> f) {
        return null;
    }

    <R> Seq<R> flatMap(Function<? super E, ? extends Seq<? extends R>> f) {
        return null;
    }
}

class Use {
    Seq<String> go(Seq<String> in) {
        return in.map(p -> p + \"Check\")
            .flatMap(cn -> Seq.of(cn, cn + \"X\"));
    }
}
",
    )])
);
// The nested `Seq.of(cn, cn + "X")` resolves to `Seq<String>`; the outer
// `flatMap`'s element type is inferred from its body through the wildcard.
