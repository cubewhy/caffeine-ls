//! JLS SE 26 scenario snapshots for *sealed hierarchies and interface
//! member shapes*: a direct subclass of a `sealed` supertype that is not
//! named in its `permits` clause is `CantInheritFromSealed`; a permitted
//! direct subclass that is itself neither `final`, `sealed` nor `non-sealed`
//! is `SealedSealedOrFinalExpected`; a `sealed` type with no direct subclass
//! is `SealedClassMustHaveSubclasses` (all §8.1.1.2); a `protected` interface
//! method is `ModifierNotAllowedHere` ([§9.4]); and a non-abstract,
//! non-native method without a body is `MissingMethodBodyOrDeclareAbstract`
//! ([§8.4.5]). Red cases render the diagnostics; green cases confirm legal
//! programs pass cleanly.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- §8.1.1.2: sealed hierarchies ---------------------------------------------

snapshot!(
    cant_inherit_from_sealed,
    check_class_diagnostics(&[(
        "/src/com/example/Sealed.java",
        "\
package com.example;

sealed class Shape permits Circle, Square {
}
final class Circle extends Shape {
}
non-sealed class Square extends Shape {
}
final class Triangle extends Shape {
}
",
    )])
);
// Red: `Triangle` directly extends the sealed `Shape` without being named in
// its `permits` clause; `Circle` (final) and `Square` (non-sealed) are fine.

snapshot!(
    sealed_sealed_or_final_expected,
    check_class_diagnostics(&[(
        "/src/com/example/Sealed.java",
        "\
package com.example;

sealed class Shape permits Circle, Square {
}
final class Circle extends Shape {
}
class Square extends Shape {
}
",
    )])
);
// Red: `Square` is permitted (named in `permits`) but is neither `final`,
// `sealed` nor `non-sealed` — the hierarchy is not closed.

snapshot!(
    sealed_class_must_have_subclasses,
    check_class_diagnostics(&[(
        "/src/com/example/Sealed.java",
        "\
package com.example;

sealed class Lone {
}
",
    )])
);
// Red: the sealed `Lone` has no direct subclass.

snapshot!(
    legal_sealed_hierarchy,
    check_class_diagnostics(&[(
        "/src/com/example/Sealed.java",
        "\
package com.example;

sealed class Shape permits Circle, Square, Rhombus {
}
final class Circle extends Shape {
}
non-sealed class Square extends Shape {
}
sealed class Rhombus extends Shape permits Diamond {
}
final class Diamond extends Rhombus {
}
sealed interface Greets permits A, B {
}
final class A implements Greets {
}
non-sealed class B implements Greets {
}
",
    )])
);
// Green: a fully closed hierarchy — permitted subclasses are final /
// non-sealed / sealed, and a sealed subtype closes in turn.

// -- §9.4: interface member shapes --------------------------------------------

snapshot!(
    modifier_not_allowed_here,
    check_class_diagnostics(&[(
        "/src/com/example/Iface.java",
        "\
package com.example;

interface I {
    protected void m();
}
",
    )])
);
// Red: an interface method may not be `protected` ([§9.4]).

snapshot!(
    missing_method_body_or_declare_abstract,
    check_class_diagnostics(&[(
        "/src/com/example/Iface.java",
        "\
package com.example;

class C {
    void f();
}
interface I {
    static void s();
    void a();
    default void d() {
    }
}
",
    )])
);
// Red: the class `f()` needs `abstract`, and the interface's `static s()`
// needs a body; `a()` is implicitly abstract and `d()` has a body, so both
// are fine.
