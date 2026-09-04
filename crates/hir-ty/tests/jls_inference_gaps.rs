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
