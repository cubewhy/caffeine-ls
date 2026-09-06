//! Lambda bodies against an inference-variable parameter type
//! ([JLS §15.27.3], [§18.5.2.2], [§5.1.10]): `apply(forward, back)` with
//! `back = e -> { ... e.isOverride() ... e.argument ... }` where `back`'s
//! parameter is the inference variable `Z` (`Function<Z, T>`). The body's
//! member accesses can only resolve after `Z` instantiates — §18.5.2.2 makes
//! the variables mentioned by the function type's parameter types *input
//! variables* of the lambda's constraints, processed only once they resolve —
//! so a member access on a receiver still carrying an inference variable is
//! *deferred*, not an error: it contributes nothing and reports nothing
//! ([§18.5.2.2], [§15.27.3]), and the chosen method's post-resolution
//! re-inference ([§18.5.2.4]) types the parameter by the instantiated formal
//! and resolves the members there (with §5.1.10 capture of a
//! wildcard-parameterized receiver before the lookup). Every scenario is
//! verified against `javac` before the snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §15.27.3/[§18.5.2.2]: `apply`'s `Z` instantiates to `Entry<T, ?>` from
// the other argument and the return target, and the `back` lambda's member
// accesses (`e.isOverride()`, `e.argument`) resolve against the substituted
// parameter — no diagnostics appear even though the first-pass body carries
// an unresolved `Z`.
snapshot!(
    lambda_param_infer_var_member_access,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.function.Function;

class Body {
    static class Entry<T, A> {
        A argument;
        boolean isOverride() { return false; }
    }
    static class Codec<T> {
        <Z> Codec<Z> apply(Function<T, Z> forward, Function<Z, T> back) {
            return null;
        }
    }
    static <X> Function<X, X> identity() {
        return null;
    }
    static <T> Codec<Entry<T, ?>> make() {
        return ((Codec<Entry<T, ?>>) null).apply(
            identity(),
            e -> {
                if (e.isOverride()) {
                    Object arg = e.argument;
                    return e;
                }
                return e;
            }
        );
    }
}
",
    )])
);

// JLS §15.27.3/[§18.5.2.2]: the block-lambda variant — the body's two
// result expressions both reach members of the parameter typed by the
// unresolved `Z`; the first pass defers them, `Z` resolves from the forward
// argument and the return target, and the re-inference checks the block
// against `Entry<T, ?>` with no error lanes.
snapshot!(
    block_body_var_param_member_access,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.function.Function;

class Body {
    static class Entry<T, A> {
        A argument;
        boolean isOverride() { return false; }
    }
    static class Codec<T> {
        <Z> Codec<Z> apply(Function<T, Z> forward, Function<Z, T> back) {
            return null;
        }
    }
    static <X> Function<X, X> identity() {
        return null;
    }
    static <T> Codec<Entry<T, ?>> make() {
        return ((Codec<Entry<T, ?>>) null).apply(
            identity(),
            e -> {
                if (e.isOverride()) {
                    Object arg = e.argument;
                    return arg == null ? e : e;
                }
                return e;
            }
        );
    }
}
",
    )])
);
