//! JLS SE 26 scenario snapshots for *shadowed type parameters*
//! ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4),
//! [§6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4.1),
//! [§8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4)):
//! a method type parameter shadows a class type parameter of the same name,
//! and the two are *distinct types* (javac renders `T#1`/`T#2`) — a member
//! accessed through `this` binds the *class* `T`, never the method's.
//! Verifying against javac 25 that a `class P<T extends Number>` whose
//! generic method `<T extends String>` reads a class-`T`-typed field
//! through `this` is rejected, while a method that uses the class parameter
//! through a distinctly-named local compiles.
//!
//! Every scenario is verified against `javac` before the snapshot is
//! accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §4.4/§6.4.1: `class P<T extends Number>` declaring `<T extends String>
// void m()` — the method `T` shadows the class `T`, and reading the
// class-typed field `this.f` into the method-typed local is a compile-time
// error (`incompatible types: T#1 cannot be converted to T#2`). A name-keyed
// substitution that conflates the pair would silently accept it by typing
// `this.f` as the method's `String`-bound variable.
snapshot!(
    shadowed_method_param_rejects_class_read,
    check_body_types(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

class Shadow<T extends Number> {
    T f;

    Shadow(T f) {
        this.f = f;
    }

    <T extends String> void m() {
        T local = this.f;
    }
}
",
    )])
);

// Green control: the same class with a *distinctly named* method type
// parameter reads the class `T` field fine — `this.f` is the class `T`
// (`Number`-bounded), assignable to the `U extends Object` local.
snapshot!(
    distinct_method_param_reads_class_field,
    check_body_types(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

class Shadow<T extends Number> {
    T f;

    Shadow(T f) {
        this.f = f;
    }

    <U> void m() {
        U local = (U) (Object) this.f;
    }
}
",
    )])
);
