//! JLS SE 26 scenario snapshots for the class-level *modifier, inheritance and
//! override* rules
//! ([§8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1),
//! [§8.4.3.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.3.3),
//! [§8.4.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.3)):
//! a `final` class cannot be inherited from ([§8.1.1.2]), a `final` method
//! cannot be overridden or hidden ([§8.4.3.3]), an override or implementation
//! may not assign weaker access privileges ([§8.4.8.3]), a non-abstract class
//! must implement every inherited abstract method ([§8.1.1.1]), no class may
//! cycle in its inheritance chain ([§8.1.4], [§9.1.3]), and no declaration
//! may combine modifiers the JLS forbids ([§8.1.1], [§8.4.3]). Red cases
//! render the diagnostics the declaration checker must report; green cases
//! confirm legal declarations pass without diagnostics.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- green: legal inheritance and overrides ----------------------------------

snapshot!(
    valid_inheritance,
    check_class_diagnostics(&[(
        "/src/com/example/Base.java",
        "\
package com.example;

class Base {
    public int f() { return 1; }
}

class Mid extends Base {
    @Override
    public int f() { return 2; }
}

class Top extends Mid {
    @Override
    public int f() { return 3; }
}
",
    )])
);
// Green: an ordinary extends chain with covariant overrides is silent.

snapshot!(
    abstract_hierarchy,
    check_class_diagnostics(&[(
        "/src/com/example/G.java",
        "\
package com.example;

abstract class A {
    abstract void m();
    final void done() {}
}

abstract class Mid extends A {
    abstract void n();
}

class Concrete extends Mid {
    void m() {}
    void n() {}
}
",
    )])
);
// Green: an abstract class may leave abstract methods unimplemented; a concrete
// class that implements every inherited abstract method is silent.

snapshot!(
    interface_implementation,
    check_class_diagnostics(&[(
        "/src/com/example/I.java",
        "\
package com.example;

interface Greeter {
    String greet();
}

class EnglishGreeter implements Greeter {
    public String greet() { return \"hello\"; }
}
",
    )])
);
// Green: a concrete class implementing an interface provides the abstract
// method with `public` access (the interface member is implicitly public,
// [§9.4]).

// -- red: cannot inherit from a final class ([§8.1.1.2]) ---------------------

snapshot!(
    inherit_from_final,
    check_class_diagnostics(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

final class Sealed {
    final int x = 1;
}

class Broken extends Sealed {}
",
    )])
);
// Red: `Sealed` is `final`, so `Broken extends Sealed` (javac: `cannot inherit
// from final Sealed`) is an error.

snapshot!(
    inherit_from_final_chain,
    check_class_diagnostics(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class A {}

final class B extends A {}

class C extends B {}
",
    )])
);
// Red: the check follows the direct superclass — `C` extends `B`, whose
// direct superclass chain ends in `final`, so `C` is reported.

snapshot!(
    final_record_enum_super,
    check_class_diagnostics(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

record R(int x) {}
class UsesR extends R {}
",
    )])
);
// Red: a record is implicitly `final` ([§8.10]), so extending it is the same
// §8.1.1.2 error.

// -- red: cannot override/hide a final method ([§8.4.3.3]) -------------------

snapshot!(
    override_final_method,
    check_class_diagnostics(&[(
        "/src/com/example/FinalMethod.java",
        "\
package com.example;

class Base {
    final void seal() {}
    void open() {}
}

class Sub extends Base {
    void seal() {}
    @Override
    void open() {}
}
",
    )])
);
// Red: `Sub.seal()` redeclares the `final` method; `open()` is a legal
// override. javac: `seal() in Sub cannot override seal() in Base; overridden
// method is final`.

snapshot!(
    hide_final_static,
    check_class_diagnostics(&[(
        "/src/com/example/FinalMethod.java",
        "\
package com.example;

class Base {
    static final void stat() {}
}

class Sub extends Base {
    static void stat() {}
}
",
    )])
);
// Red: hiding a `final` static method is also §8.4.3.3.

// -- red: weaker access privileges ([§8.4.8.3]) ------------------------------

snapshot!(
    weaker_access_class,
    check_class_diagnostics(&[(
        "/src/com/example/Access.java",
        "\
package com.example;

class Base {
    public void a() {}
    protected void b() {}
    void c() {}
}

class Sub extends Base {
    void a() {}
    private void b() {}
    public void c() {}
}
",
    )])
);
// Red/Green: `Sub.a()` weakens `public` to package-private and `Sub.b()`
// weakens `protected` to `private`; widening `c()` from package-private to
// public stays legal.

snapshot!(
    weaker_access_interface,
    check_class_diagnostics(&[(
        "/src/com/example/Access.java",
        "\
package com.example;

interface I {
    void m();
}

class Impl implements I {
    void m() {}
}
",
    )])
);
// Red: an interface method is implicitly `public` ([§9.4]), so an
// implementation with package-private access weakens it.

// -- red: unimplemented abstract method ([§8.1.1.1]) -------------------------

snapshot!(
    unimplemented_abstract_class,
    check_class_diagnostics(&[(
        "/src/com/example/Abstract.java",
        "\
package com.example;

abstract class A {
    abstract void m();
}
class B extends A {}
class C extends A {
    void m() {}
}
",
    )])
);
// Red/Green: `B` is non-abstract and inherits `m()` unimplemented; `C`
// provides the concrete method, so it is silent.

snapshot!(
    unimplemented_interface,
    check_class_diagnostics(&[(
        "/src/com/example/Abstract.java",
        "\
package com.example;

interface I {
    boolean add();
}
class L extends java.util.ArrayList implements I {
    public boolean add() { return true; }
}
class Missing implements I {}
",
    )])
);
// Red/Green: `Missing` implements `I` without providing `add()`. `L`
// implements `I` itself (the classpath `ArrayList.add` is unrelated: it does
// not implement `I`), so it stays silent.

snapshot!(
    unimplemented_via_abstract_mid,
    check_class_diagnostics(&[(
        "/src/com/example/Abstract.java",
        "\
package com.example;

abstract class A {
    abstract void m();
}
abstract class Mid extends A {}
class Leaf extends Mid {}
class Done extends Mid {
    void m() {}
}
",
    )])
);
// Red/Green: the abstract method flows through the intermediate abstract
// class; `Leaf` still has to implement it, `Done` does.

snapshot!(
    unimplemented_self_declared,
    check_class_diagnostics(&[(
        "/src/com/example/Abstract.java",
        "\
package com.example;

class Bad {
    abstract void self();
}
",
    )])
);
// Red: a non-abstract class declaring an `abstract` method reports the method
// as unimplemented (javac also reports the `does.not.override.abstract` shape
// `class Bad does not override abstract method self() in Bad`).

// -- red: cyclic inheritance ([§8.1.4], [§9.1.3]) ----------------------------

snapshot!(
    cyclic_class_extends,
    check_class_diagnostics(&[(
        "/src/com/example/Cycle.java",
        "\
package com.example;

class A extends B {}
class B extends A {}
",
    )])
);
// Red: `A extends B` and `B extends A` — both appear in their own inheritance
// chain.

snapshot!(
    cyclic_self_extends,
    check_class_diagnostics(&[(
        "/src/com/example/Cycle.java",
        "\
package com.example;

class Self extends Self {}
",
    )])
);
// Red: a class extending itself.

snapshot!(
    cyclic_interface,
    check_class_diagnostics(&[(
        "/src/com/example/Cycle.java",
        "\
package com.example;

interface X extends Y {}
interface Y extends Z {}
interface Z extends X {}
",
    )])
);
// Red: an interface-extends cycle ([§9.1.3]).

// -- red: illegal modifier combinations ([§8.1.1], [§8.4.3]) -----------------

snapshot!(
    illegal_modifier_pairs,
    check_class_diagnostics(&[(
        "/src/com/example/Mods.java",
        "\
package com.example;

abstract final class AF {}

class C {
    public private void both() {}
    static abstract void sa();
    private abstract void paa();
    final volatile int fv;
    transient volatile int tv;
    native synchronized void ns();
    public int legal;
}
",
    )])
);
// Red: `abstract final` (class), `public private`, `static abstract`,
// `private abstract`, `final volatile`. javac leaves `transient volatile`,
// `native synchronized` and the plain field alone, and so does this check.

snapshot!(
    illegal_modifier_sealed,
    check_class_diagnostics(&[(
        "/src/com/example/Mods.java",
        "\
package com.example;

sealed final class SF {}
",
    )])
);
// Red: `sealed` and `final` are contradictory ([§8.1.1.2]).
