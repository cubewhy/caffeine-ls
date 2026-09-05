//! JLS SE 26 scenario snapshots for the implicit constructor of an *inner*
//! class ([JLS §8.8.9], [§8.1.3]): the default constructor of an inner class
//! — a non-static member class, not declared in an interface — has a formal
//! parameter of the immediately enclosing instance type. An unqualified
//! `new Inner()` inside the outer's instance context supplies the enclosing
//! `this`; in a static context (or from outside the outer) the written empty
//! argument list does not fit the one-parameter constructor, exactly as with
//! an explicit one-parameter constructor.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- red/green: `new Inner()` needs the enclosing instance --------------------

snapshot!(
    inner_implicit_ctor_enclosing_instance,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Outer.java",
        "\
package com.example;

class Outer {
    class Inner {}

    void f() {
        new Inner();
    }

    static void s() {
        new Inner();
    }

    static class Body {
        void g() {
            new Outer().new Inner();
        }
    }
}
",
    )])
);
// javac: `f()`'s `new Inner()` passes the enclosing `Outer` instance and
// compiles; `g()`'s `new Outer().new Inner()` is the qualified creation and
// compiles; `s()`'s `new Inner()` has no enclosing instance — the implicit
// constructor takes one parameter, so the creation is a wrong-arity error
// (javac: "an enclosing instance that contains Outer.Inner is required").
// Pre-fix the implicit constructor was zero-parameter and `s()` resolved.
