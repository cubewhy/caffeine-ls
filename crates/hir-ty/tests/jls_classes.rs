//! JLS SE 26 scenario snapshots for classes
//! ([JLS §8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html)):
//! covariant overriding ([§8.4.8.1], [§8.4.5]), field hiding and `super`
//! access ([§8.3.3.2]), constructor chaining via `this(...)` ([§8.8.7.1]) and
//! the illegal forward-reference restriction on field initializers
//! ([§8.3.3]). Red cases render the diagnostics the type layer must report;
//! green cases confirm the class forms type without errors, including
//! constructor delegation via `this(...)` ([§8.8.7.1]) and the
//! illegal-forward-reference restriction on field initializers ([§8.3.3]).
//! An incompatible overridden return type ([§8.4.8.3]) is a declaration-level
//! check rendered by `jls_decl_checks.rs`.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: covariant return in an override ([§8.4.5]) -------------------------

snapshot!(
    covariant_override,
    check_body_types(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

class Base {
    Base self() { return this; }
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

class Derived extends Base {
    Derived self() { return this; }
}
",
        ),
    ])
);

// -- green: field hiding resolved with super and a cast -------------------------

snapshot!(
    field_hiding,
    check_body_types(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

class Base {
    int x = 1;
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

class Derived extends Base {
    String x = \"d\";

    String pick() {
        if (((Base) this).x == 1 && super.x == 1) {
            return x;
        }
        return \"\";
    }
}
",
        ),
    ])
);

// -- green: constructor delegation via this(...) ([§8.8.7.1]) -------------------

snapshot!(
    constructor_this_chain,
    check_body_types(&[(
        "/src/com/example/CtorChain.java",
        "\
package com.example;

class CtorChain {
    int v;

    CtorChain() { this(1); }

    CtorChain(int v) { this.v = v; }
}
",
    )])
);

// -- red: illegal forward reference in a field initializer ([§8.3.3]) ------------
// A simple-name read of a same-class field declared textually later, of the
// same static/instance kind, is illegal; `javac` 25 reports "illegal forward
// reference". The qualified form `this.b` stays legal.

snapshot!(
    illegal_forward_reference,
    check_body_types(&[(
        "/src/com/example/InitOrder.java",
        "\
package com.example;

class InitOrder {
    int a = b;
    int b = 1;
}
",
    )])
);
