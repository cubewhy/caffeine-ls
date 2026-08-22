//! JLS SE 26 scenario snapshots for interfaces
//! ([JLS §9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html)):
//! default methods and their overriding ([§9.4.1]), the conflicting-default
//! rule when two superinterfaces declare the same default ([§9.4.1.3]) and
//! the functional-interface restriction on lambda targets ([§9.8], [§15.27.3]).
//! Red cases render the diagnostics the type layer must report; green cases
//! confirm the interface forms type without errors.
//!
//! The trailing *known divergence* section pins behaviour that still
//! contradicts the spec or `javac` 25 — conflicting inherited defaults are not
//! rejected ([§9.4.1.3]), lambda targets are not checked against the
//! functional-interface restriction ([§9.8], [§15.27.3]) and `I.super.m()`
//! qualified-super invocation resolves as a `no-such-method` diagnostic
//! ([§15.11.2]). Their snapshots are kept pending (`.snap.new`) until the
//! divergences are fixed; a fix must flip them.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: implementing an interface with a default method ---------------------

snapshot!(
    default_method_override,
    check_body_types(&[
        (
            "/src/com/example/Greets.java",
            "\
package com.example;

interface Greets {
    default String name() { return \"g\"; }
    String loud();
}
",
        ),
        (
            "/src/com/example/Impl.java",
            "\
package com.example;

class Impl implements Greets {
    public String name() { return \"i\"; }
    public String loud() { return \"!\"; }
}
",
        ),
    ])
);

// -- green: re-abstracting an inherited default and interface constants ---------
// -- ([§9.4.1.2], [§9.3]) --------------------------------------------------------

snapshot!(
    reabstraction_and_constants,
    check_body_types(&[(
        "/src/com/example/C.java",
        "\
package com.example;

interface A {
    default String name() { return \"a\"; }
    int MAX = 10;
}

interface B extends A {
    String name();
}

class C implements B {
    public String name() { return \"c\" + MAX; }

    int useMax() {
        return B.MAX + C.this.MAX;
    }
}
",
    )])
);

// -- known divergence: conflicting inherited defaults ([§9.4.1.3]) -----------------
// A class inheriting unrelated defaults for the same method is rejected by
// `javac` 25 ("types A and B are incompatible"); the type layer stays silent.

snapshot!(
    divergence_conflicting_default_methods,
    check_body_types(&[
        (
            "/src/com/example/A.java",
            "\
package com.example;

interface A {
    default String name() { return \"a\"; }
}
",
        ),
        (
            "/src/com/example/B.java",
            "\
package com.example;

interface B {
    default String name() { return \"b\"; }
}
",
        ),
        (
            "/src/com/example/Diamond.java",
            "\
package com.example;

class Diamond implements A, B {}
",
        ),
    ])
);

// -- known divergence: lambda target must be a functional interface ([§9.8]) -------
// `javac` 25 rejects a lambda whose target declares two abstract methods; the
// type layer stays silent ([§15.27.3]).

snapshot!(
    divergence_lambda_needs_functional_interface,
    check_body_types(&[
        (
            "/src/com/example/NotFI.java",
            "\
package com.example;

interface NotFI {
    int f(int x);
    int g(int x);
}
",
        ),
        (
            "/src/com/example/UsesFI.java",
            "\
package com.example;

class UsesFI {
    void m() {
        NotFI lam = x -> x;
    }
}
",
        ),
    ])
);

// -- known divergence: I.super.m() qualified super invocation ([§15.11.2]) ----------
// `javac` 25 accepts the qualified-super form used to disambiguate inherited
// defaults; the type layer reports `no-such-method`.

snapshot!(
    divergence_qualified_super_invocation,
    check_body_types(&[(
        "/src/com/example/Greets.java",
        "\
package com.example;

interface Greets {
    default String name() { return \"g\"; }
}

class Impl implements Greets {
    public String name() { return Greets.super.name(); }
}
",
    )])
);
