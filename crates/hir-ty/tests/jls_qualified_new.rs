//! JLS SE 26 scenario snapshots for *qualified class instance creation*
//! ([JLS §15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9),
//! [§15.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.1),
//! [§8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3)):
//! `primary.new Inner(...)` creates the *member class* `Inner` of the
//! receiver expression's compile-time type. The grammar names only the bare
//! identifier `Inner` ([JLS §15.9.1]), which the lexical scope never
//! declares, so the created type resolves against the receiver's type —
//! `a.new B()` with `a: A` creates `A.B`. Red cases render the diagnostics;
//! green cases confirm legal programs pass cleanly.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

snapshot!(
    qualified_new_inner_class,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Q.java",
        "\
package com.example;

class Q {
    static class A {
        class B {}
    }

    static A a;

    static Object make() {
        return a.new B();
    }
}
",
    )])
);
// `a.new B()`: `B` is the member class of `A` — the type of `a` — and is
// not in lexical scope inside `Q`. No `cannot resolve symbol 'B'` fires and
// the creation types `Q.A.B`.

snapshot!(
    qualified_new_chained_member,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Q.java",
        "\
package com.example;

class Q {
    static class Outer {
        class Inner {
            Inner(int i) {}
        }
    }

    static Outer newOuter() {
        return null;
    }

    static Object make() {
        return newOuter().new Inner(1);
    }
}
",
    )])
);
// A qualified creation whose receiver is itself an invocation
// (`newOuter().new Inner(1)`): the member class resolves against the
// receiver call's return type and its constructor is applied.

snapshot!(
    qualified_new_member_not_lexical,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Q.java",
        "\
package com.example;

class Q {
    static class A {
        class B {}
    }

    static class B {}

    static A a;

    static Object make() {
        return a.new B();
    }
}
",
    )])
);
// §15.9: the qualified creation names the *member* `A.B`, not the top-level
// `B` that shadows it lexically — the receiver type wins. The result is
// `Q.A.B`, distinct from `Q.B`.

snapshot!(
    qualified_new_member_constructor_args,
    check_body_types(&[(
        "/src/com/example/Q.java",
        "\
package com.example;

class Q {
    static class A {
        class B {}
    }

    static A a;

    static Q.A.B make() {
        return a.new B();
    }
}
",
    )])
);
// The creation's inferred type is the receiver's member class, so it is
// assignable to a `Q.A.B` declared local.
