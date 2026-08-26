//! JLS SE 26 scenario snapshots for interfaces
//! ([JLS §9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html)):
//! default methods and their overriding ([§9.4.1]), the conflicting-default
//! rule when two superinterfaces declare the same default ([§9.4.1.3]) and
//! the functional-interface restriction on lambda targets ([§9.8], [§15.27.3]).
//! Red cases render the diagnostics the type layer must report; green cases
//! confirm the interface forms type without errors — including the
//! `I.super.m()` qualified-super invocation ([§15.11.2]) and the
//! functional-interface restriction on lambda targets ([§9.8], [§15.27.3]).
//! The conflicting-default rule ([§9.4.1.3]) is a declaration-level check
//! rendered by `jls_decl_checks.rs`.

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

// -- red: a lambda target must be a functional interface ([§9.8]) -----------------
// `javac` 25 rejects the assignment ("NotFI is not a functional interface");
// a target declaring two abstract methods has no single abstract method
// ([§15.27.3]).

snapshot!(
    lambda_needs_functional_interface,
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

// -- green: I.super.m() qualified super invocation ([§15.11.2]) ------------------
// The disambiguating form selects the default method of the named interface;
// `javac` 25 accepts it.

snapshot!(
    qualified_super_invocation,
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

// -- §9.4.1.2 with §9.9: redeclared abstract methods count once -----------------
// `Closeable.close` redeclares the inherited `AutoCloseable.close`; both are
// ACC_ABSTRACT in the classfiles, but override-equivalent abstracts make the
// interface functional with exactly one SAM — a try-with-resources lambda
// target types against it.

snapshot!(
    sam_override_equivalence_closeable,
    check_body_types(&[(
        "/src/com/example/Repro.java",
        "\
import java.io.Closeable;

class Repro {
    void withCloseable() throws Exception {
        try (Closeable c = () -> {}) {
        }
    }
}
",
    )])
);
