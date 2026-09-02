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

// -- green/red: interface fields are implicitly public static final ------------
// ([§9.3]): a static method of the interface reads its constant by simple name
// and through a qualified access, exactly like a class's static field; the
// static-context check must not mistake the constant for an instance field
// (javac's `non-static … cannot be referenced from a static context`).
// A *value* receiver of the interface may not reach the static constant.

snapshot!(
    interface_field_implicit_static,
    check_body_types(&[(
        "/src/com/example/Ctx.java",
        "\
package com.example;

interface Ctx {
    Codec CODEC = (new Codec() {
        public Object decode() { return null; }
    }).codec();

    static Object use() {
        return CODEC;
    }

    static Object useQualified() {
        return Ctx.CODEC;
    }

    interface Codec {
        default Codec codec() { return this; }
    }
}
",
    )])
);

// -- green: interface static method invoked from inside the interface ---------
// ([§9.4.3]): a static method of an interface is in scope inside the interface
// body by its MethodName form — javac resolves `text()` inside the interface
// that declares it, from both static and default methods, and rejects the same
// MethodName from a static context when it names an *instance* method.

snapshot!(
    interface_own_static_method,
    check_body_types(&[(
        "/src/com/example/Factory.java",
        "\
package com.example;

interface Factory {
    static Factory empty() { return null; }

    static Factory make() {
        return empty();
    }

    default Factory reset() {
        return empty();
    }

    void instanceOnly();
}
",
    )])
);

// -- red: a static interface method is not inherited ---------------------------
// ([§9.2]/[§9.4.1]): an interface inherits no static methods, and a class does
// not implement them either. Referencing a super-interface's static method by
// simple name from a subinterface or implementing class is a `cannot find
// symbol`, while the same call inside the declaring interface itself resolves.

snapshot!(
    interface_static_not_inherited,
    check_body_types(&[
        (
            "/src/com/example/I.java",
            "\
package com.example;

interface I {
    static void s() {}
}
",
        ),
        (
            "/src/com/example/Uses.java",
            "\
package com.example;

interface K extends I {
    static void bad() { s(); }
}

class D implements I {
    static void alsoBad() { s(); }
    void alsoBad2() { s(); }
}
",
        ),
    ])
);
