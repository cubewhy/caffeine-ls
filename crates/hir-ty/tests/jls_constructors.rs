//! JLS SE 26 scenario snapshots for constructor resolution
//! ([JLS §15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9)
//! class-instance creation, [§8.8.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7.1)
//! explicit constructor delegation).
//!
//! A `new Foo(...)` selects an applicable constructor exactly like a method
//! invocation ([§15.12.2]): when the class declares no constructor of the
//! name the creation is a `cannot find symbol` error; when the declared
//! constructors exist but none is applicable, the message opens with javac's
//! `constructor {Foo}() cannot be applied to given types;`.

#[macro_use]
mod common;

use crate::common::{check_body_types, check_body_types_with_libs, class_with_methods};

// -- red: `new Foo()` when Foo only declares `Foo(int)` -------------------------

snapshot!(
    new_wrong_arity,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

class Foo {
    Foo(int x) {}
}

class Body {
    void m() {
        new Foo();
        new Foo(1);
    }
}
",
    )])
);
// §15.9/[§15.12.2]: `new Foo()` has members of the name (`Foo(int)`) but none
// applicable — javac's `constructor Foo() cannot be applied to given types;
// required: int found: no arguments`. `new Foo(1)` resolves.

snapshot!(
    new_argument_mismatch,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

class Foo {
    Foo(int x) {}
}

class Body {
    void m() {
        new Foo(\"s\");
        new Foo(1, 2);
    }
}
",
    )])
);
// `new Foo("s")` matches the arity but `String` does not convert to `int` —
// the `reason: incompatible types:` block. `new Foo(1, 2)` differs in arity.

snapshot!(
    new_no_such_constructor,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

class Foo {
    private Foo() {}
}

class Body {
    void m() {
        new Foo();
    }
}
",
    )])
);
// `Foo()` is private ([§6.6.1]): the member set filtered to this caller is
// empty, so the creation reports `cannot find symbol: constructor Foo()`
// ([§15.9]).

// -- red/green: record canonical constructor ([§8.10.4]) ------------------------

snapshot!(
    record_constructor,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

record Rec(int x) {}

class Body {
    void m() {
        new Rec(1);
        new Rec(\"s\");
    }
}
",
    )])
);
// The canonical constructor `Rec(int)` resolves against the matching
// invocation and is rejected against the `String` actual.

// -- red: delegating `this(...)` ([§8.8.7.1]) ----------------------------------

snapshot!(
    this_delegation,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

class Foo {
    Foo(int x) {}

    Foo() {
        this(\"s\");
    }
}
",
    )])
);
// `this("s")` delegates to a constructor the class does not declare that
// signature for; the only candidate is `Foo(int)`, so the delegation is a
// `constructor Foo() cannot be applied` error ([§8.8.7.1]).

// -- green: valid constructions stay silent -------------------------------------

snapshot!(
    valid_constructors,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

class Foo {
    Foo() {}
    Foo(int x) {}
}

class Body {
    void m() {
        new Foo();
        new Foo(7);
        throw new RuntimeException();
    }
}
",
    )])
);
// Every `new` picks an applicable constructor — including the JDK fixture's
// no-arg `RuntimeException` ([JLS §15.9], [§15.12.2]) — so nothing is
// reported.

// -- red: external library class with no applicable constructor ----------------

snapshot!(
    library_class_no_matching_constructor,
    check_body_types_with_libs(
        &[
            class_with_methods("org/objectweb/asm/ClassReader", None, &[], &[], &[],),
            class_with_methods(
                "org/objectweb/asm/ClassWriter",
                Some("java/lang/Object"),
                &[],
                &[
                    ("<init>", "(I)V"),
                    ("<init>", "(Lorg/objectweb/asm/ClassReader;I)V"),
                ],
                &["", ""],
            ),
        ],
        &[(
            "/src/com/example/Body.java",
            "\
package com.example;

class Body {
    void m() {
        org.objectweb.asm.ClassWriter cr = new org.objectweb.asm.ClassWriter();
        org.objectweb.asm.ClassWriter ok = new org.objectweb.asm.ClassWriter(0);
    }
}
",
        )],
    )
);
// §15.9/[§15.12.2]: `new ClassWriter()` with zero arguments must fail — the
// class's constructors are `<init>(I)` and `<init>(ClassReader;I)`, so no
// no-arg constructor exists ([§8.8.9] gives the implicit default only when
// the class declares *no* constructors). The arity-matching `new
// ClassWriter(0)` resolves.
