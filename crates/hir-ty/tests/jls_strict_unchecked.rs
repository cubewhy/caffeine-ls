//! JLS SE 26 scenario snapshots for overload resolution across a *raw*
//! argument: a raw value converts to a parameterized formal by *unchecked
//! conversion*
//! ([JLS §5.1.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.9)),
//! which **strict** invocation contexts admit
//! ([§5.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.3)) —
//! so phase 1 of overload resolution
//! ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2))
//! already prefers the parameterized overload over an `Object`-taking
//! alternative (javac's phase-1 applicability test is `isSubtypeUnchecked`).

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: `m(rawList)` selects `m(List<String>)` in phase 1 ------------------

snapshot!(
    strict_unchecked_parameterized_wins_fixed,
    check_body_types(&[(
        "/src/com/example/RawOverload.java",
        "\
package com.example;

import java.util.List;

class RawOverload {
    static List<String> m(List<String> l) {
        return null;
    }

    static Object m(Object o) {
        return null;
    }

    static void use(List raw) {
        Object o = m(raw);
    }
}
",
    )])
);
// javac: the raw `List` converts to `List<String>` by unchecked conversion
// ([§5.1.9]), a strict invocation conversion ([§5.3]), so phase 1
// ([§15.12.2.2]) picks `m(List<String>)` over `m(Object)` — `m(raw)` is typed
// `List<String>`, not `Object`.

// -- green: a raw array converts unchecked to the parameterized array ----------

snapshot!(
    strict_unchecked_array_wins,
    check_body_types(&[(
        "/src/com/example/RawOverload.java",
        "\
package com.example;

class Box<T> {
}

class RawOverload {
    static String m(Box<String>[] xs) {
        return null;
    }

    static Object m(Object[] xs) {
        return null;
    }

    static void use() {
        Object o = m(new Box[0]);
    }
}
",
    )])
);
// javac: `new Box[0]` is an array of the *raw* element `Box`; the element
// unchecked conversion `Box[] → Box<String>[]` ([§5.1.9], [§10]) is a strict
// conversion, so `m(Box<String>[])` is applicable in phase 1 and is more
// specific than `m(Object[])` — the call resolves to the array overload.

// -- green: a *generic* parameterized overload wins over `m(Object)` -----------

snapshot!(
    strict_unchecked_generic_wins,
    check_body_types(&[(
        "/src/com/example/RawOverload.java",
        "\
package com.example;

import java.util.List;

class RawOverload {
    static <T> List<T> m(List<T> l) {
        return null;
    }

    static Object m(Object o) {
        return null;
    }

    static void use(List raw) {
        Object o = m(raw);
    }
}
",
    )])
);
// javac: phase 1 accepts the generic `List<T>` overload for the raw `List`
// actual by unchecked conversion and instantiates `T` (from the upper bound
// `Object` in the absence of a target), so `m(raw)` is typed `List<...>`,
// not `Object` — the generic overload, not the `Object` catch-all, wins.

// -- red: a phase-1-applicable pair with no most specific member ---------------

snapshot!(
    strict_unchecked_pair_stays_ambiguous,
    check_body_diagnostic_spans(&[(
        "/src/com/example/RawOverload.java",
        "\
package com.example;

import java.util.List;
import java.util.ArrayList;

class RawOverload {
    static void m(List<String> l) {
    }

    static void m(ArrayList<Number> a) {
    }

    static void use(ArrayList raw) {
        m(raw);
    }
}
",
    )])
);
// javac: both overloads are applicable in phase 1 (each by unchecked
// conversion — the raw `ArrayList` to `List<String>` and to
// `ArrayList<Number>`), and neither is more specific than the other, so the
// invocation stays ambiguous ("reference to m is ambiguous"). The harness
// renders the ambiguity as a not-applicable diagnostic on the call — the
// same shape as the pre-existing `varargs_ambiguous_pair` snapshot — and it
// must surface with the pair applicable by phase 1, not only from a later
// phase.
