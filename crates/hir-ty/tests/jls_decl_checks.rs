//! JLS SE 26 scenario snapshots for the *declaration-level* checks
//! ([JLS §8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html),
//! [§9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html)) — the
//! checks that need a class's whole inheritance graph rather than one body:
//! the return-type-substitutability of overrides
//! ([§8.4.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.3))
//! and conflicting inherited defaults
//! ([§9.4.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.3)).
//! Red cases render the diagnostics the declaration checker must report;
//! green cases confirm legal declarations pass without diagnostics.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- green: covariant overrides and a disambiguated default ----------------------

snapshot!(
    valid_overrides,
    check_class_diagnostics(&[(
        "/src/com/example/Base.java",
        "\
package com.example;

class Base {
    Base self() { return this; }
}

class Derived extends Base {
    Derived self() { return this; }
}

interface A {
    default String name() { return \"a\"; }
}

interface B {
    default String name() { return \"b\"; }
}

class Diamond implements A, B {
    public String name() { return \"c\"; }
}
",
    )])
);

// -- red: an override whose return type is not substitutable ([§8.4.8.3]) --------

snapshot!(
    incompatible_override,
    check_class_diagnostics(&[(
        "/src/com/example/Base.java",
        "\
package com.example;

class Base {
    int f() { return 1; }
}

class Derived extends Base {
    String f() { return \"\"; }
}
",
    )])
);

// -- red: conflicting defaults from unrelated superinterfaces ([§9.4.1.3]) -------

snapshot!(
    conflicting_default_methods,
    check_class_diagnostics(&[
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

// -- green: related defaults are an override chain, not a conflict ---------------
// ([§9.4.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.1))

snapshot!(
    related_defaults,
    check_class_diagnostics(&[(
        "/src/com/example/Chain.java",
        "\
package com.example;

interface A {
    default String name() { return \"a\"; }
}

interface B extends A {
    default String name() { return \"b\"; }
}

class Chain implements B {}
",
    )])
);

// -- §9.6.4.4: `@Override` validation ---------------------------------------------
// A method annotated `@Override` must override or implement an instance
// supertype method; a `static` method only hides ([§8.4.8.2]), so its
// annotation always fails.

snapshot!(
    override_annotation,
    check_class_diagnostics(&[(
        "/src/com/example/Base.java",
        "\
package com.example;

class Base {
    void run() {}
    static void hide() {}
}

class Derived extends Base {
    @Override
    void run() {}

    @Override
    void missing() {}

    @Override
    static void hide() {}
}
",
    )])
);

// -- §9.7.1/§6.5.5.1: a same-package annotation shadows the JDK one ---------------
// `Override` here resolves to the local annotation type, not
// `java.lang.Override`, so `@Override` on the method carries no override
// requirement and no diagnostic is reported.

snapshot!(
    override_annotation_shadowed,
    check_class_diagnostics(&[(
        "/src/com/example/Base.java",
        "\
package com.example;

@interface Override {}

class Base {
    void run() {}
}

class Derived extends Base {
    @Override
    void missing() {}
}
",
    )])
);
