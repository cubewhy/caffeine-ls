//! JLS SE 26 scenario snapshots for *override-equivalence without
//! variable-arity-ness* ([JLS §8.4.2], [§8.4.8.1], [§9.4.1.2]): a method's
//! signature is its parameter types in array-lowered form, so a `String...`
//! and a `String[]` declaration have the same signature and one may override
//! the other; an abstract method of one interface is discharged by an
//! override-equivalent abstract of another.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- green: a varargs override of an array super method -----------------------

snapshot!(
    varargs_array_override_equivalent,
    check_class_diagnostics(&[(
        "/src/com/example/Base.java",
        "\
package com.example;

class Base {
    void m(String[] a) {}
}

class Sub extends Base {
    @Override
    void m(String... a) {}
}
",
    )])
);
// javac: `m(String...)` declares the same signature as `m(String[])`
// ([§8.4.2] — variable-arity-ness is not part of the signature), so the
// `@Override` is valid. Pre-fix the varargs-flag comparison reported it as
// not overriding.

// -- green: varargs abstract discharged across interfaces ---------------------

snapshot!(
    varargs_abstract_discharged_across_interfaces,
    check_class_diagnostics(&[(
        "/src/com/example/C.java",
        "\
package com.example;

interface A {
    void m(String... a);
}

interface B {
    void m(String[] a);
}

class C implements A, B {
    public void m(String[] a) {}
}
",
    )])
);
// javac: `C.m(String[])` implements both `A.m(String...)` and
// `B.m(String[])` — the abstract methods are override-equivalent across the
// interfaces ([§9.4.1.2]) and the one concrete declaration discharges them.
// Pre-fix the varargs comparison reported C as not implementing the abstract
// method of one interface.
