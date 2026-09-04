//! Frontend conformance snapshots for generic-invocation gaps
//! ([JLS §4.10.4], [§6.4.1], [§8.4.4], [§14.22], [§15.2], [§15.12.1],
//! [§15.12.3], [§15.21], [§15.27.3]): standalone binary/unary/condition
//! operands, unqualified static-interface search, throw-only lambda
//! value-compatibility, identical-type lub and method shadowing. Every
//! scenario is verified against `javac` before the snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §15.2, §15.21: the operands of `==` are standalone — the `boolean`
// result type (and its assignment target) must not constrain a generic
// operand's inference.
snapshot!(
    equality_operand_standalone,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static class Holder<T> {}
    static <T> Holder<T> make() { return null; }

    <V> void test(Holder<V> v) {
        boolean b = v == make();
    }
}
",
    )])
);

// JLS §15.12.1, §15.12.3 (MethodName form): an unqualified call inside an
// interface finds the static method the interface itself declares, not an
// unrelated same-name member of an outer type.
snapshot!(
    unqualified_static_interface_search,
    check_body_types(&[(
        "/src/com/example/Outer.java",
        "\
package com.example;

interface Outer {
    Outer combine();

    interface Inner {
        static Inner of(String a, String b) {
            return combine(a, b);
        }

        static Inner combine(String a, String b) {
            return null;
        }
    }
}
",
    )])
);

// JLS §15.27.3, §14.22: a `throw`-only block never completes normally, so it
// is value-compatible (and void-compatible) with either function type.
snapshot!(
    throw_only_lambda_value_compatible,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.function.Function;

class Body {
    <T> T with(Function<String, T> f) {
        return null;
    }

    void test() {
        String s = with(x -> {
            throw new RuntimeException(\"x\");
        });
    }
}
",
    )])
);

// JLS §4.10.4: the lub of identical types is that type — `lub(Z, Z)` is `Z`,
// not its bound — so duplicate lowers from two same-variable arguments keep
// the variable.
snapshot!(
    lub_identical_types,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    <W> W inner(W[] a, W b) {
        return null;
    }

    <Z> Z outer(Z[] a, Z b) {
        return inner(a, b);
    }
}
",
    )])
);

// JLS §6.4.1, §8.4.4: a method type parameter shadows a class type parameter
// of the same name — `Class<T>` in `<T extends Enum<T>>` is the method's `T`.
snapshot!(
    method_shadowing,
    check_body_types(&[(
        "/src/com/example/Outer.java",
        "\
package com.example;

class Wrapper<T> {
    <T extends java.lang.Enum<T>> java.util.EnumSet<T> read(Class<T> c) {
        return null;
    }
}

class Outer extends Wrapper<Outer> {
    enum Action {
        A,
        B
    }

    void test() {
        Object o = read(Action.class);
    }
}
",
    )])
);
