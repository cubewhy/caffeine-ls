//! JLS SE 26 scenario snapshots for *records*: the compact constructor
//! ([JLS §8.10.4]) is a constructor whose parameter list is the record's
//! component list — the compact body assigns the components as blank `final`
//! fields ([§8.10.1], [§8.10.4], [§8.3.1.2]/[§16]), so those writes are not
//! `CannotAssignToFinalVariable`; a delegating `this(...)` from an explicit
//! constructor must resolve against the *component list* arity, not the compact
//! form's empty formal-parameter list, so legal delegation is not a
//! `RecursiveConstructorInvocation` ([§8.8.7.1]); and a genuine delegation
//! cycle is still reported. Red cases render the diagnostics; green cases
//! confirm legal programs pass cleanly.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_class_diagnostics};

// -- §8.10.4/[§8.3.1.2]/[§16]: compact constructor assigns components ----------

snapshot!(
    compact_ctor_component_assignment,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Rec.java",
        "\
package com.example;

record Rec(int x, String s) {
    Rec {
        x = 1;
        s = \"a\";
    }
}
",
    )])
);
// Green: a compact constructor may assign its record's components — they are
// blank final fields whose one legal initialization this is.

snapshot!(
    compact_ctor_partial_assignment,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Rec.java",
        "\
package com.example;

record Rec(int x, String s) {
    Rec {
        if (x < 0) {
            x = 0;
        }
        s = s == null ? \"\" : s;
    }
}
",
    )])
);
// Green: the components are blank finals, so branchy one-time assignment stays
// legal.

snapshot!(
    compact_ctor_foreign_final_write,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Rec.java",
        "\
package com.example;

class Other {
    final int f = 1;
    Other() {
        f = 2;
    }
}

record Rec(int x) {
    Rec {
        this.x = 1;
        final int z = 1;
        z = 2;
        Other o = new Other();
    }
}
",
    )])
);
// Red: a compact constructor is still a constructor — writing an initialized
// `final` field (`Other.f`) or a non-blank `final` local (`z`) stays an error.
// Only the blank component write (`this.x`) is legalized.

// -- §8.8.7.1/[§8.10.4]: this(...) delegation resolves by component arity -----

snapshot!(
    explicit_ctor_delegates_to_compact,
    check_class_diagnostics(&[(
        "/src/com/example/Rec.java",
        "\
package com.example;

record Rec(String a, String b, String c, String d, String e, String f) {
    Rec {
    }
    Rec(String a, String b, String c, String d, boolean e, String f) {
        this(a, b, c, d, e ? \"L\" : \"R\", f);
    }
}
",
    )])
);
// Green: the six-argument `this(...)` resolves against the compact
// constructor's *component list* arity, so this legal delegation is not a
// recursive-constructor invocation.

snapshot!(
    record_ctor_delegation_chain,
    check_class_diagnostics(&[(
        "/src/com/example/Rec.java",
        "\
package com.example;

record Rec(int x, String s) {
    Rec {
        if (s == null) {
            s = \"\";
        }
    }
    Rec(int x) {
        this(x, \"a\");
    }
    Rec(String s) {
        this(0, s);
    }
}
",
    )])
);
// Green: explicit constructors delegate to the compact constructor (or to each
// other) through the component-list arity — the chain bottoms out in the
// compact body, so nothing is recursive.

snapshot!(
    record_ctor_cycle,
    check_class_diagnostics(&[(
        "/src/com/example/Rec.java",
        "\
package com.example;

record Rec(int x) {
    Rec {
        this();
    }
    Rec() {
        this(1);
    }
}
",
    )])
);
// Red: the compact constructor's `this()` and the explicit `Rec()` `this(1)`
// close a delegation cycle that never reaches the supertype constructor — the
// arity-aware resolution still reports it.
