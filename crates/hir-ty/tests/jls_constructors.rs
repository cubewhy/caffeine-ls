//! JLS SE 26 scenario snapshots for constructor resolution
//! ([JLS §15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9)
//! class-instance creation, [§8.8.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7.1)
//! explicit constructor delegation).
//!
//! A `new Foo(...)` selects an applicable constructor exactly like a method
//! invocation ([§15.12.2]): when the class declares no constructor of the
//! name the creation is a `cannot resolve symbol` error; when the declared
//! constructors exist but none is applicable, the message reports
//! `Constructor {Foo}() cannot be applied to given types`.

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
// applicable — `Constructor 'Foo()' cannot be applied to given types` (with a
// `required: int` detail). `new Foo(1)` resolves.

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

// -- regression: a stream method-reference chain as a constructor argument -------
// A `list.stream().map(P::of).toList()` argument to a `List<P>` constructor
// formal is a poly expression ([JLS §15.2]): its type is inferred jointly with
// the formal ([JLS §18.5.2.4]). `map`'s type variable `<R>` must be bound by the
// method reference's return type ([JLS §15.13.1] exact/inexact references,
// [§15.13.3]), so the whole chain is `List<P>` and the constructor is applicable
// ([JLS §15.9]/[§15.12.2]). Both `P.of` overloads return `P` in every ordering,
// so the chain must resolve regardless of declaration order.

snapshot!(
    constructor_poly_stream_map_method_ref,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

import java.util.List;

record R(List<P> libs) {}

class P {
    static P of(String first, String... more) { return null; }
    static P of(Object any) { return null; }
}

class Body {
    void m(List<String> names) {
        new R(names.stream().map(P::of).toList());
    }
}
",
    )])
);

// -- regression: a less-specific first overload must not leak -------------------
// The referenced overload is selected by the target functional interface
// ([JLS §15.13.1]): `pack(Integer)` is not even applicable to the SAM's
// `? super String` parameter, so `map`'s `<R>` must be bound by the applicable
// `pack(String)`, which returns `Store`. Taking the *first declared* overload
// instead leaves `<R> := Object`, reports `constructor R() cannot be applied
// to given types; required: List<Store> found: List<Object>`, and diverges from
// javac, which accepts the chain ([JLS §15.12.2.2]/[§15.12.2.3] phase-based
// applicability, [§15.13.3] result compatibility). The wMatcher
// `list.stream().map(Path::of).toList()` constructor-argument regression is the
// same shape.

snapshot!(
    constructor_poly_stream_map_method_ref_applicable_overload,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

import java.util.List;

class Store {
    static Object pack(Integer n) { return null; }
    static Store pack(String s) { return null; }
}

record R(List<Store> libs) {}

class Body {
    void m(List<String> names) {
        new R(names.stream().map(Store::pack).toList());
    }
}
",
    )])
);

// -- regression: interface static methods must be visible to a `T::m` ref -------
// `Path::of` names *static* methods declared on the `Path` *interface*. The
// virtual-invocation member filter of [JLS §15.12.3] excludes static interface
// methods from receiver-expression invocations, but a type-qualified method
// reference `T::m` is not such an invocation ([§15.13.1]): the member set must
// still contain them, or `map`'s `<R>` stays unbound and the chain types
// `List<Object>` — the exact wMatcher report against `List<Path>`. Both
// overloads' result is the referenced interface type, so the chain must
// resolve regardless of which applies.

snapshot!(
    constructor_poly_stream_map_interface_static_method_ref,
    check_body_types(&[(
        "/src/com/example/Constr.java",
        "\
package com.example;

import java.util.List;

interface PathLike {
    static PathLike of(String first, String... more) { return null; }
    static PathLike of(Object any) { return null; }
}

record R(List<PathLike> libs) {}

class Body {
    void m(List<String> names) {
        new R(names.stream().map(PathLike::of).toList());
    }
}
",
    )])
);
