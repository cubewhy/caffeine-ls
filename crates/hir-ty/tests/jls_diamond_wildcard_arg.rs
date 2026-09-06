//! Diamond constructor inference with wildcard-typed constructor arguments
//! ([JLS §15.9.3], [§18.2.2], [§5.1.10]): `new NBTList<>(NBTType.common(),
//! n)` — the argument `NBTType.common() : NBTType<?>` is a wildcarded source
//! whose capture conversion (§5.1.10) stands for it during inference, so the
//! diamond's `T` picks up the capture (`CAP <: NBT`) rather than the bare
//! wildcard's degenerate minimum. The constructor that decided the diamond's
//! variables is the chosen declaration; it is not re-resolved against the
//! instantiated class, which javac never re-checks. Every scenario is
//! verified against `javac` before the snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §15.9.3/[§18.2.2]/[§5.1.10]: `NBTType.common()` returns `NBTType<?>`; a
// bare `?` in a parameterized source captures to a fresh variable bounded by
// the *type parameter's* declared bound (`CAP <: NBT`), and the per-argument
// equality `CAP = α` binds the diamond's `α` to it — the created
// `NBTList<CAP>` satisfies the constructor's `NBTType<T>` formal and the
// wildcard variable target `NBTList<?>`.
snapshot!(
    diamond_wildcard_ctor_arg,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    static class NBT {}
    static class NBTType<T extends NBT> {
        static NBTType<?> common() { return null; }
    }
    static class NBTList<T extends NBT> {
        NBTList(NBTType<T> t, int size) {}
    }
    static void f(List<? extends NBT> tags) {
        NBTList<?> list = new NBTList<>(NBTType.common(), tags.size());
    }
}
",
    )])
);
