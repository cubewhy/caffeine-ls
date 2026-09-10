//! Inference conformance snapshots for bound-set gaps
//! ([JLS §4.10.4], [§5.1.7], [§15.27.2], [§18.3.1], [§18.4]): null-tolerant
//! lub, primitive-target boxing for upper-only variables, standalone block
//! statements against void SAMs, and lower/upper dependency propagation.
//! Every scenario is verified against `javac` before the snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §4.10.4/§15.27.3: `lub(T, null)` is `T` — a block lambda returning a
// value on one path and `null` on another keeps the value type.
snapshot!(
    lub_null_tolerant,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.function.Function;

class Body {
    <T> T with(Function<String, T> f) {
        return null;
    }

    void test(boolean c) {
        String s = with(x -> {
            if (c) {
                return \"a\";
            } else {
                return null;
            }
        });
    }
}
",
    )])
);

// JLS §18.1.1/§4.4 with §5.1.7/§18.4: a primitive-only upper boxes —
// `<U> U` targeting `long` instantiates to `Long`, not `Object`.
snapshot!(
    primitive_target_boxing,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    <T> T make() {
        return null;
    }

    void test() {
        long x = make();
    }
}
",
    )])
);

// JLS §15.27.2: a block lambda's statements are standalone — a generic
// statement-expression against a void SAM constrains nothing.
snapshot!(
    void_block_standalone,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    <T> T take(String s) {
        return null;
    }

    void run(java.lang.Runnable r) {}

    void test() {
        run(() -> {
            take(\"x\");
        });
    }
}
",
    )])
);

// JLS §18.3.1: every lower `S` and upper `T` imply `⟨S <: T⟩` —
// `<T, Z extends T>` with `Z` lower `Byte` and upper `T` gives `T` the lower
// bound, so `T` resolves compatibly instead of degrading.
snapshot!(
    incorporation_propagation,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    interface Reader<R> {
        R apply(Body b);
    }

    interface Writer<T> {
        void accept(Body b, T v);
    }

    static class Data<X> {}

    static <T, Z extends T> Data<Z> define(String s, Reader<Z> r, Writer<T> w) {
        return null;
    }

    byte readByte() {
        return 0;
    }

    void writeByte(int b) {}

    void test() {
        Data<Byte> d = define(\"byte\", Body::readByte, Body::writeByte);
    }
}
",
    )])
);

// -- §18.4.1: a self-referential bound is validated, not ignored --------------
// `parse`'s `E` is F-bound (`E extends Enum<E>`); the argument fixes `E = Color`
// and the *target* then demands `String`, so `String <: Enum<Color>` fails and
// the invocation is rejected (javac: "inference variable E has incompatible
// bounds; equality constraints: Color; upper bounds: String, Enum<E>"). The
// erasure fallback for a variable with only a self-referential bound must not
// turn this into a legal call.

snapshot!(
    self_referential_bound_conflict,
    check_body_types(&[(
        "/src/com/example/EnumNeg.java",
        "\
package com.example;

class EnumNeg {
    enum Color { RED }

    static <E extends Enum<E>> E parse(Class<E> cls, String s) {
        return null;
    }

    static Color ok(String s) {
        return parse(Color.class, s);
    }

    static <E extends Enum<E>> void bad(String s) {
        String x = parse(Color.class, s);
    }
}
",
    )])
);

// -- §18.4.1: a variable with only a self-referential bound instantiates to --
// that bound's erasure. `copyOf(Collection<E>)` with `E extends Enum<E>` called
// with a raw `Collection` gives `E` no proper bound at all, so javac falls back
// to the erasure (`Enum`) and admits the bound check by unchecked conversion
// ([§5.1.9]) — the call compiles with its unchecked-usage note. Instantiating
// such a variable to `Object` instead makes `Object <: Enum<E>` unsatisfiable
// and rejects it.

snapshot!(
    self_referential_bound_erasure_fallback,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.Collection;

class Raw {
    static <E extends Enum<E>> Raw copyOf(Collection<E> c) {
        return null;
    }

    void copy(Collection raw) {
        Raw a = copyOf(raw);
    }
}
",
    )])
);
