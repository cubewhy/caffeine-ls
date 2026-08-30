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

// -- §9.6.4.4/§8.4.2: a same-arity overload is not an override ----------------------
// Signature matching compares parameter *types*, not just arity: `f(int)` does
// not override `f(long)` — boxing ([§5.1.7]) and widening ([§5.1.2]) apply to
// invocation, never to overriding — so both annotated methods below fail.

snapshot!(
    override_annotation_overload,
    check_class_diagnostics(&[(
        "/src/com/example/Base.java",
        "\
package com.example;

class Base {
    void f(long x) {}
}

class Derived extends Base {
    @Override
    void f(int x) {}

    @Override
    void g(java.lang.String s) {}
}
",
    )])
);

// -- red/green: overriding through a *raw* generic superinterface ([§4.8]) ------
// `implements Converter` (bare) erases the interface's members: the override
// of `<T> T convert(Class<T>, Object)` by `convert(Class, Object)` is exact
// after erasure ([§8.4.8.1], [§8.4.2]).

snapshot!(
    override_raw_generic_interface,
    check_class_diagnostics(&[(
        "/src/com/example/Conv.java",
        "\
package com.example;

import org.apache.commons.beanutils.Converter;

class Conv implements Converter {
    @Override
    public Object convert(Class type, Object value) {
        return value;
    }
}
",
    )])
);

// -- green/red: @Override on an explicitly declared record accessor ([§8.10.3]) -
// The explicit accessor is the implicit accessor mandated by §8.10.3; javac
// accepts @Override on it ([§9.6.4.4]).

snapshot!(
    override_record_accessor,
    check_class_diagnostics(&[(
        "/src/com/example/Meta.java",
        "\
package com.example;

import java.util.List;
import java.util.regex.Pattern;

record Meta(List<Pattern> headerPatterns, List<String> lineContents) {
    @Override
    public List<Pattern> headerPatterns() {
        return headerPatterns;
    }
}
",
    )])
);

// -- green/red: the same override from inside a *static nested* class -----------
snapshot!(
    override_raw_interface_nested,
    check_class_diagnostics(&[(
        "/src/com/example/Outer.java",
        "\
package com.example;

import org.apache.commons.beanutils.Converter;

class Outer {
    private static final class Conv implements Converter {
        @Override
        public Object convert(Class type, Object value) {
            return value;
        }
    }
}
",
    )])
);

// -- §7.6: duplicate top-level classes (same file and cross-file) ----------------

snapshot!(
    duplicate_class_same_file,
    check_class_diagnostics(&[(
        "/src/com/example/Two.java",
        "\
package com.example;

class Foo {}
class Foo {}
",
    )])
);
// §7.6: two top-level declarations of the same simple name in one compilation
// unit share the FQN `com.example.Foo` ([§6.7]); the non-first declaration is
// reported (`duplicate class: com.example.Foo`), like javac.

snapshot!(
    duplicate_class_cross_file,
    check_class_diagnostics(&[
        (
            "/src/com/example/A.java",
            "\
package com.example;

class Foo {}
",
        ),
        (
            "/src/com/example/B.java",
            "\
package com.example;

class Foo {}
",
        ),
    ])
);
// §7.6: the duplicate spans files. `A.java`'s declaration is the first
// occurrence (smallest file), so only `B.java`'s `Foo` is reported.

snapshot!(
    duplicate_class_no_duplicate,
    check_class_diagnostics(&[(
        "/src/com/example/A.java",
        "\
package com.example;

class Foo {}
class Bar {}
",
    )])
);
// Green: distinct top-level declarations in one file share no FQN.

// -- §7.6: a public top-level type must be declared in a file of its name ------

snapshot!(
    single_public_class_ok,
    check_class_diagnostics(&[(
        "/src/com/example/Zed.java",
        "\
package com.example;

public class Zed {}
",
    )])
);
// Green: `Zed.java` declares `public class Zed` — the file stem matches the
// public type's simple name, so no error ([JLS §7.6]).

snapshot!(
    public_class_name_mismatch,
    check_class_diagnostics(&[(
        "/src/com/example/Foo.java",
        "\
package com.example;

public class Zed {}
",
    )])
);
// §7.6: `Foo.java` declares `public class Zed`; a public top-level type must
// be declared in a file named after its simple name, so it is reported
// (`class Zed is public, should be declared in a file named Zed.java`).

snapshot!(
    multiple_public_classes,
    check_class_diagnostics(&[(
        "/src/com/example/A.java",
        "\
package com.example;

public class A {}
public class B {}
",
    )])
);
// §7.6: `A.java` declares two public top-level types. `B` is not declared in a
// file named `B.java`, so it is reported; `A` matches the file and stays
// silent — javac's "at most one public class per file".

snapshot!(
    public_class_wrong_file,
    check_class_diagnostics(&[(
        "/src/com/example/Zed.java",
        "\
package com.example;

public class Zed {}
public class Other {}
",
    )])
);
// §7.6: `Other` is public but the file is `Zed.java`, so it is reported.

snapshot!(
    package_private_class_ok,
    check_class_diagnostics(&[(
        "/src/com/example/Zed.java",
        "\
package com.example;

class Zed {}
class Other {}
",
    )])
);
// Green: package-private top-level types need not name the file ([§7.6]);
// multiple package-private types per file are legal.

// -- §8.8: a constructor must be named after its class --------------------------
// The `SimpleTypeName` in a constructor declaration must be the simple name of
// the class that contains it, or a compile-time error occurs ([§8.8]). javac
// reports such a declaration as `invalid method declaration; return type
// required` (a would-be method with a missing return type, §8.4.5); the
// message rendered here is IntelliJ IDEA's.

snapshot!(
    ctor_mismatch,
    check_class_diagnostics(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Ctors {
    Ctors() {}
    Ctors(int x) {}
    <T> Ctors(String s) {}
    Wrong() {}
    public Bad() {}
    <T> Other(String s) {}
}

class Nested {
    Nested() {}
    class Inner {
        Inner() {}
        Nested() {}
        Inner(int x) {}
        void Wrong() {}
    }
}
",
    )])
);
// Red: `Wrong`, `Bad` and `Other` name something other than their own class —
// each is a constructor-derived method with a missing return type. The nested
// `Inner`'s `Nested()` (which names the *outer* class) is flagged too, matched
// against `Inner`. `void Wrong()` is a real method (`void` is its result,
// §8.4.5) and stays silent, as does every constructor that names its class.

snapshot!(
    ctor_mismatch_record,
    check_class_diagnostics(&[(
        "/src/com/example/Recs.java",
        "\
package com.example;

record Recs(int x) {
    Recs { }
    Recs(int x) { this.x = x; }
    Wrong { }
    Bad(int x) { this.x = x; }
}
",
    )])
);
// §8.10.4: a record's canonical constructor — normal or compact ([§8.10.4.2])
// — must name the record. The compact `Wrong { }` and the normal `Bad(...)`
// mismatch and are flagged; `Recs { }` and `Recs(int)` match.

snapshot!(
    ctor_mismatch_enum,
    check_class_diagnostics(&[(
        "/src/com/example/Seasons.java",
        "\
package com.example;

enum Seasons {
    SPRING, SUMMER;
    Seasons() {}
    Wrong() {}
}
",
    )])
);
// §8.9.2/[§8.8]: enum constructors are constructors of the enum class, so the
// same naming rule applies to the member section after the ';'. (javac
// actually reports the enum spelling as `enum constant not expected here`;
// both agree the declaration is invalid and the mismatch is the visible
// cause.)
