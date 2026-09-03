//! JLS SE 26 scenario snapshots for the *illegal modifier pair* check
//! ([JLS §8.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.1),
//! [§8.4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.3)):
//! which combinations `abstract` excludes depends on the *kind* of
//! declaration. On a method (or annotation element) `abstract` cannot
//! co-occur with `static`/`private`/`default`/`native`/`synchronized`/
//! `strictfp` ([§8.4.3]); on a *class-like* declaration those are ordinary —
//! a nested `abstract static class`, a `private abstract class` or an
//! `abstract strictfp class` is legal — and only `abstract final` is
//! rejected ([§8.1.1]). Green cases render legal declarations passing
//! cleanly; red cases render the pair javac reports.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

snapshot!(
    legal_nested_class_abstract_pairs,
    check_class_diagnostics(&[(
        "/src/com/example/Mods.java",
        "\
package com.example;

class Mods {
    public abstract static class A1 {}
    private abstract class A2 {}
    protected abstract static class A3 {}
    abstract static class A4 {}
    static abstract class A5 {}
    abstract strictfp class A6 {}
}
",
    )])
);
// §8.1.1: `abstract` + `static`, `private`, `protected` and `strictfp` are
// legal on nested classes — these are ordinary library shapes
// (`abstract static class Compression` in a utility interface, a private
// `abstract` iterator base), and javac compiles them.

snapshot!(
    illegal_abstract_final_class,
    check_class_diagnostics(&[(
        "/src/com/example/Mods.java",
        "\
package com.example;

class Mods {
    abstract final class Bad1 {}
    final abstract class Bad2 {}
}
",
    )])
);
// §8.1.1: a class cannot be both `abstract` and `final` — an abstract class
// exists only to be subclassed and a final class forbids subclassing. Both
// spellings report the pair in canonical order.

snapshot!(
    legal_class_but_illegal_method_abstract_pairs,
    check_class_diagnostics(&[(
        "/src/com/example/Mods.java",
        "\
package com.example;

abstract class Mods {
    abstract static class Nested {}

    abstract static void bad();
    abstract private void bad2();
    static abstract void bad3();
    abstract native void bad4();
}
",
    )])
);
// §8.4.3: the *method* `abstract static`/`abstract private`/`abstract
// native` pairs are rejected while the same modifiers on the nested class
// `abstract static class Nested` stay legal — the abstract-contradiction set
// is declaration-kind sensitive.
