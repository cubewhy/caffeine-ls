//! Conformance snapshots for the raw-type warnings of *declaration*
//! positions — the parameter, field, return, supertype and record-component
//! types that
//! [JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)
//! and [§4.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.2)
//! make a raw use of a generic class — and their `@SuppressWarnings` scope
//! ([§9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5)).
//!
//! Every scenario is verified against `javac -Xlint:rawtypes` before the
//! snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// JLS §4.8/§4.12.2: a raw parameter type. javac:
// `warning: [rawtypes] found raw type: List`.
snapshot!(
    raw_parameter_type,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void m(List xs) {
    }
}
",
    )])
);

// JLS §4.8/§4.12.2: a raw field type, a raw return type and a raw record
// component are the same raw use — javac reports each.
snapshot!(
    raw_field_and_return_types,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;
import java.util.Map;

class Body {
    List field;

    Map m() {
        return null;
    }
}
",
    )])
);

// JLS §4.8: a raw use in an `extends`/`implements` clause. javac reports the
// raw supertype.
snapshot!(
    raw_supertype,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;

class Body extends ArrayList {
}
",
    )])
);

// JLS §4.8/§4.12.2: a parameterized use is *not* raw, and a non-generic class
// has no type arguments to omit — neither is reported.
snapshot!(
    parameterized_and_non_generic_are_not_raw,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void parameterized(List<String> xs) {
    }

    void nonGeneric(String s) {
    }
}
",
    )])
);

// JLS §9.6.4.5: `@SuppressWarnings("rawtypes")` on the declaring method covers
// its signature types — "the annotated declaration or any of its parts" — so
// the raw parameter is not reported. javac reports nothing.
snapshot!(
    rawtypes_scope_covers_signature,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings(\"rawtypes\")
    void m(List xs) {
    }
}
",
    )])
);

// JLS §9.6.4.5: the function-type name of a raw declaration type is keyed to
// the method, so a scope on a *sibling* leaves it in place. javac reports it.
snapshot!(
    rawtypes_scope_does_not_reach_sibling,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings(\"rawtypes\")
    void other(List xs) {
    }

    void m(List xs) {
    }
}
",
    )])
);
