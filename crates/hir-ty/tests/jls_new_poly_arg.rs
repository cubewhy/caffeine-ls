//! Standalone-first inference for nested generic invocations in the
//! *argument positions of a class instance creation* ([JLS §15.9],
//! [§15.12.2.6], [§15.2], [§18.5.2.4]): a nested method invocation
//! contributed to a `new Foo(...)` constructor resolution is attributed with
//! its *own* actual arguments against its own formal parameter types in a
//! fresh, target-free table before the constructor overloads are considered —
//! exactly as a method-invocation argument is
//! ([§15.12.2.6], [`crate::java::infer::overload`]).
//!
//! Class instance creation is its own invocation context ([JLS §15.9]): the
//! created class's constructor set is resolved by the *same* applicability
//! phases and most-specific rule as a method call ([§15.12.2], [§8.8.7.1]),
//! so the poly-expression rules of its arguments match a method invocation's
//! ([§15.2], [§18.5.2.4]). A nested call that its own formals fully determine
//! — `new Host(pick("x"))` resolving `pick` to `String` against its own
//! formal before `Host(Box<String>)` / `Host(Box<Integer>)` are compared —
//! participates in the constructor selection as a concrete argument of that
//! type. Every scenario is verified against `javac` before the snapshot is
//! accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §15.9/[§15.12.2.6]: the nested `pick("x")` in `new Host(pick("x"))`
// resolves standalone — its own formal `T` fixes `T := String` from the
// `String` actual before the constructor overloads are considered. The
// standalone type `String` is a concrete argument, so `Host(Box<String>)`
// applies and the constructor's own generic type parameter `T` (of the
// enclosing `class Host<T>`) binds to `String`; the `Host(Box<Integer>)`
// overload stays inapplicable. A joint re-inference against each candidate's
// formal would have made both overloads look applicable and ended in
// "cannot apply".
snapshot!(
    new_host_poly_argument,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static class Box<T> {
        Box(T t) {}
    }

    static class Host<T> {
        Host(Box<T> b) {}
    }

    static <T> T pick(T x) {
        return null;
    }

    static void use() {
        new Host<String>(new Box<String>(pick(\"x\")));
    }
}
",
    )])
);

// JLS §15.9/[§18.5.2.4]: a nested *generic* invocation whose type parameter
// its own empty argument list never constrains — `empty()` — stays a poly
// expression whose inference is shared with the constructor's resolution: the
// `List<String>` formal of the applicable constructor drives `E := String`.
// The standalone-first pass must not default the unconstrained parameter to
// `Object` (the fixpoint break the poly deferral exists to avoid), or the
// constructor formal could never reach it.
snapshot!(
    new_empty_list_still_poly,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    static class Host {
        Host(List<String> xs) {}
    }

    static <E> List<E> empty() {
        return null;
    }

    static void use() {
        new Host(empty());
    }
}
",
    )])
);
