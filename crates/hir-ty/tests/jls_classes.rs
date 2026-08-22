//! JLS SE 26 scenario snapshots for classes
//! ([JLS §8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html)):
//! covariant overriding ([§8.4.8.1], [§8.4.5]), field hiding and `super`
//! access ([§8.3.3.2]), constructor chaining via `this(...)` ([§8.8.7.1]) and
//! the illegal forward-reference restriction on field initializers
//! ([§8.3.3]). Red cases render the diagnostics the type layer must report;
//! green cases confirm the class forms type without errors.
//!
//! The trailing *known divergence* section pins behaviour that still
//! contradicts the spec or `javac` 25 — constructor delegation via `this(...)`
//! resolves as `<missing>` ([§8.8.7.1]), an incompatible overridden return
//! type is not rejected ([§8.4.8.3]) and the illegal-forward-reference
//! restriction on field initializers is not enforced ([§8.3.3]). Their
//! snapshots are kept pending (`.snap.new`) until the divergences are fixed;
//! a fix must flip them.

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

// -- known divergence: constructor delegation via this(...) ([§8.8.7.1]) ---------
// `javac` 25 accepts the explicit constructor invocation; the type layer
// resolves `this(...)` to a `no-such-method` diagnostic on `<missing>`.

snapshot!(
    divergence_constructor_this_chain,
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

// -- known divergence: incompatible overridden return type ([§8.4.8.3]) ------------
// An override must be return-type-substitutable; `javac` 25 rejects
// `String f()` overriding `int f()`, but the type layer stays silent.

snapshot!(
    divergence_override_incompatible_return,
    check_body_types(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

class Base {
    int f() { return 1; }
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

class Derived extends Base {
    String f() { return \"\"; }
}
",
        ),
    ])
);

// -- known divergence: illegal forward reference ([§8.3.3]) ------------------------
// Reading a field before its declaration in an initializer is illegal;
// `javac` 25 reports "illegal forward reference", but the type layer stays
// silent.

snapshot!(
    divergence_illegal_forward_reference,
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
