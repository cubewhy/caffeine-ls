//! JLS SE 26 scenario snapshots for *static interface member reachability*
//! ([JLS §8.2], [§9.2], [§9.4.1], [§15.12.3]): a static method declared in an
//! interface is a member of that interface only. An interface inherits no
//! static methods from its superinterfaces ([§9.2]) and a class inherits none
//! from its superinterfaces ([§8.2]), so a subinterface or implementing class
//! cannot reach it — neither by invocation nor by method reference.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green/red: interface statics only via the declaring interface ------------

snapshot!(
    static_interface_members_declaring_only,
    check_body_diagnostic_spans(&[(
        "/src/com/example/I.java",
        "\
package com.example;

import java.util.function.Supplier;

interface I {
    static String s() {
        return \"\";
    }
}

interface J extends I {}

class C implements I {}

class Body {
    String own(I i) {
        return I.s();
    }

    Supplier<String> ref() {
        return I::s;
    }

    String subinterface(J j) {
        return J.s();
    }

    Supplier<String> subinterfaceRef() {
        return J::s;
    }

    String implementing(C c) {
        return C.s();
    }
}
",
    )])
);
// javac: `I.s()` and `I::s` resolve on the declaring interface; `J.s()`,
// `J::s`, `C.s()` are all compile-time errors — static interface methods are
// reachable through no receiver/supertype traversal except the declaring
// interface itself ([§9.2], [§8.2], [§15.12.3]). Pre-fix the subinterface and
// implementing-class calls resolved. (`J::s` types as an unresolvable
// reference; the method-reference path reports no diagnostic of its own, so
// the snapshot asserts the invocation forms.)
