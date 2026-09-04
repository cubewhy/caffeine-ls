//! Member-lookup conformance snapshots for hierarchy gaps
//! ([JLS §4.8], [§4.9], [§6.5.2]): raw supertype erasure, intersection
//! conjuncts and field-vs-type reclassification. Every scenario is verified
//! against `javac` before the snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §4.8: a raw receiver erases its supertypes — a raw `Container`
// extends raw `ArrayList`, so `add(Check<?>)` targets `Object`.
snapshot!(
    raw_supertype_erasure,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;

class Body {
    static class Check<T> {}

    static class Container<T> extends ArrayList<Check<T>> {}

    static void test(Container c, Check<?> x) {
        c.add(x);
    }
}
",
    )])
);

// JLS §4.9: an intersection value finds members through either conjunct.
snapshot!(
    intersection_members,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    interface Mapped {}

    interface Copyable<T extends Mapped> {
        T copy(String s);
    }

    interface Decoder<T> {
        T decode(String s);
    }

    static <T extends Mapped & Copyable<T>> void test(Decoder<T> d) {
        d.decode(\"x\").copy(\"y\");
    }
}
",
    )])
);

// JLS §6.5.2: a simple name naming both a type and a field in scope is an
// expression — `UUID` is the `Codec<UUID>` field, not `java.util.UUID`.
snapshot!(
    field_vs_type,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.UUID;

class Body {
    interface Codec<T> {
        default Codec<T> withAlternative(Decoder<T> d) {
            return null;
        }
    }

    interface Decoder<T> {}

    static final Codec<UUID> UUID = null;
    static final Codec<UUID> STRING_UUID = null;
    static final Codec<UUID> LENIENT = UUID.withAlternative(null);
}
",
    )])
);
