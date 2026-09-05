//! JLS SE 26 scenario snapshots for *SELF-type* member re-pointing
//! ([JLS §8.4.8] fluent-API channel): on a receiver parameterized by a
//! captured wildcard, a member whose result type is the class's own *self*
//! type parameter — the parameter whose bound mentions the class
//! (`Chain<E, SELF extends Chain<E, SELF>>`) — returns the receiver's
//! captured type, keeping assertion chains member-resolution-capable. An
//! ordinary element-typed member (`E element()`) is NOT a SELF channel: its
//! captured return stays assignable to the element type.

#[macro_use]
mod common;

use crate::common::{check_body_types_with_libs, class_with_methods_access_sig};

snapshot!(
    self_type_rewrites_only_self_param,
    check_body_types_with_libs(
        &[class_with_methods_access_sig(
            "com/example/Chain",
            Some("java/lang/Object"),
            &[],
            // `self(): SELF`, `element(): E` — the descriptor uses the raw
            // erasures; the method-level `Signature` attributes carry the
            // type parameters.
            &[
                ("self", "()Ljava/lang/Object;"),
                ("element", "()Ljava/lang/Object;"),
            ],
            &["()TSELF;", "()TE;",],
            &[0x0001, 0x0001],
            // Class signature: `Chain<E, SELF extends Chain<E, SELF>>`.
            Some("<E:Ljava/lang/Object;SELF:Lcom/example/Chain<TE;TSELF;>;>Ljava/lang/Object;"),
        )],
        &[(
            "/src/com/example/Body.java",
            "\
package com.example;

class Body {
    Chain<? extends CharSequence, ? extends Chain<?, ?>> c;

    CharSequence useElement() {
        CharSequence x = c.element();
        return x;
    }

    Chain<?, ?> useSelf() {
        Chain<?, ?> y = c.self();
        return y;
    }
}
",
        )],
    )
);
// javac: `c.element()` returns the captured `E` (a `CharSequence` — its
// declared upper bound), assignable to `CharSequence`; `c.self()` returns the
// captured `SELF` — the receiver's own second argument — assignable to
// `Chain<?, ?>`. Pre-fix the SELF re-pointing keyed on any `CAP#` return, so
// `c.element()` was also rewritten to the receiver type and the
// `CharSequence` assignment errored.
