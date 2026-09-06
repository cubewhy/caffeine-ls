//! JLS SE 26 scenario snapshots for *super-invocation* resolution
//! ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3),
//! [§15.11.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.2))
//! and compound assignment legality
//! ([§15.26.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.26.2)):
//! `super.m(...)` and `I.super.m(...)` select the member *as declared in the
//! supertype*, so an abstract member is not invocable (`abstract method {m}
//! in {A} cannot be accessed directly`), and the `I.super` qualifier must
//! name an enclosing superinterface. A compound assignment desugars to
//! `E1 = (T)(E1 op E2)`, whose binary operator must be legal.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- red: super.m on an abstract supertype member ([§15.12.3]/[§15.8.4]) ------

snapshot!(
    abstract_super_access,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

abstract class Abs {
    abstract void m();
}

class Sub extends Abs {
    void m() {}
    void f() {
        super.m();
    }
}

interface IF {
    void go();
}

class C implements IF {
    public void go() {}
    void f() {
        IF.super.go();
    }
}
",
    )])
);
// Red: the subclass's own `m()` does not make `super.m()` legal — the super
// invocation resolves the member *declared in the supertype*, which is
// abstract here ([§15.12.3]). Same for the interface-qualified `IF.super.go()`.

// -- green: concrete superclass / default superinterface members ----------------

snapshot!(
    concrete_super_access,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

class Good {
    void concrete() {}
}

class GoodSub extends Good {
    void f() {
        super.concrete();
    }
}

interface IF2 {
    default void fine() {}
}

class C2 implements IF2 {
    void g() {
        IF2.super.fine();
    }
}
",
    )])
);
// Green: `super.concrete()` resolves the concrete superclass method, and
// `IF2.super.fine()` reaches the inherited interface default ([§15.11.2]).

// -- red: a qualified-super qualifier that is no enclosing superinterface ------

snapshot!(
    qualified_super_not_enclosing,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Q.java",
        "\
package com.example;

interface NotImpl {}

class BadQual {
    void f() {
        NotImpl.super.toString();
    }
}

interface IA {
    default void m() {}
}

interface IB extends IA {
    default void use() {
        IA.super.m();
    }
}

class Impl implements IB {
    void g() {
        IA.super.m();
        IB.super.use();
    }
}
",
    )])
);
// Red: `NotImpl` is not an enclosing superinterface of `BadQual` (javac: `not
// an enclosing class: NotImpl`). Green: `IA`/`IB` *are* enclosing
// superinterfaces of `IB`/`Impl`, so the qualified-super calls resolve.

// -- red/green: compound assignment legality ([§15.26.2]) -----------------------

snapshot!(
    compound_assignment_legality,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Comp.java",
        "\
package com.example;

class Comp {
    void t(Object o) {
        o += 1;
    }
    void u(int[] a) {
        a[0] += 1;
    }
    void v(String s) {
        s += \"x\";
    }
    void w(boolean b) {
        b |= true;
        b &= false;
    }
    void x(long l) {
        l >>= 2;
    }
}
",
    )])
);
// Red: `Object += int` — `Object + int` has no legal binary operator, so the
// compound assignment is an error (javac: `bad operand types for binary
// operator '+'`). Green: array-element, `String` concatenation, `boolean`
// bitwise and `long` shift compound assignments all desugar legally.
