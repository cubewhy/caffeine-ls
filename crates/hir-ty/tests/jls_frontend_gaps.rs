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

// JLS §6.4.1: a static generic method in a generic interface shadows the
// interface's type parameter — `<T extends Mapped & Copyable<T>>` in
// `NbtEntryDecoder<T>` resolves its own `T` (with `copy`), not the
// interface's unbounded `T`. Without the innermost-wins lookup the lambda's
// `decode(...).copy(...)` loses `copy`.
snapshot!(
    interface_method_shadowing,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

interface Mapped {
}

interface Copyable<T extends Mapped> {
    T copy(Object d);
}

interface NbtDecoder<T> {
    T decode(Object tag, Object w);
}

interface NbtEntryDecoder<T> {
    static <T extends Mapped & Copyable<T>> NbtEntryDecoder<T> fromDecoder(NbtDecoder<T> decoder) {
        return (tag, wrapper, data) -> decoder.decode(tag, wrapper).copy(data);
    }

    T decode(Object tag, Object w, Object d);
}
",
    )])
);

// JLS §15.27.3: a `return` inside a *nested* lambda belongs to that lambda,
// never to the outer one — an outer void block containing an inner block
// lambda with a valued `return` stays void-compatible and targets `Runnable`.
// Without the isolation the inner `return s;` leaks into the outer probe frame,
// the outer appears value-compatible and constrains `⟨String → void⟩`,
// rejecting the `Runnable` candidate.
snapshot!(
    nested_block_lambda_returns_isolated,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.function.Function;

class Body {
    void run(Runnable r) {
    }

    void test() {
        run(() -> {
            Function<String, String> f = s -> {
                return s;
            };
        });
    }
}
",
    )])
);
