//! Conformance snapshots for the lint-level warnings of the *body* and their
//! suppression: the raw-type use of
//! [JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)
//! and [§4.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.2),
//! the unchecked conversion of
//! [§5.1.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.9),
//! and the `@SuppressWarnings` scope of
//! [§9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5).
//!
//! Every scenario is verified against `javac -Xlint:rawtypes,unchecked` before
//! the snapshot is accepted: a warning javac reports must appear here unless a
//! scope naming its key covers it, and a warning javac suppresses must not
//! appear at all. Declaration-position raw types are covered by
//! `jls_decl_warnings.rs`.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

/// The reference scenario: a raw declared local (`rawtypes`) and a raw value
/// assigned to a parameterized local (`unchecked`). javac:
///
/// ```text
/// Body.java:6: warning: [rawtypes] found raw type: Box
/// Body.java:7: warning: [unchecked] unchecked conversion
/// ```
const RAW_AND_UNCHECKED: &str = "\
package com.example;

import java.util.List;

class Body {
    void m(List<String> xs) {
        List raw = xs;
        List<String> unchecked = raw;
    }
}
";

// JLS §4.12.2/§5.1.9: both warnings are reported when nothing suppresses them.
snapshot!(
    raw_and_unchecked_reported,
    check_body_diagnostic_spans(&[("/src/com/example/Body.java", RAW_AND_UNCHECKED,)])
);

// JLS §9.6.4.5: `@SuppressWarnings("unchecked")` suppresses the unchecked
// warning of the annotated declaration and every part of it, and only that
// one — javac still reports `[rawtypes]`. Before the suppression was modelled
// both warnings were reported, so the annotation was silently ignored.
snapshot!(
    unchecked_key_suppresses_only_unchecked,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings(\"unchecked\")
    void m(List<String> xs) {
        List raw = xs;
        List<String> unchecked = raw;
    }
}
",
    )])
);

// JLS §9.6.4.5: `@SuppressWarnings({"rawtypes", "unchecked"})` names both
// warnings of its declaration, so neither is reported. javac reports none.
snapshot!(
    both_keys_suppress_both,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings({\"rawtypes\", \"unchecked\"})
    void m(List<String> xs) {
        List raw = xs;
        List<String> unchecked = raw;
    }
}
",
    )])
);

// JLS §9.6.4.5: "Any other string specifies a non-standard warning. A Java
// compiler must ignore any such string that it does not recognize." `"all"`
// is not one of the four strings the language defines, so it suppresses
// nothing — matching javac, which reports both warnings for it too.
snapshot!(
    unrecognized_string_suppresses_nothing,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings(\"all\")
    void m(List<String> xs) {
        List raw = xs;
        List<String> unchecked = raw;
    }
}
",
    )])
);

// JLS §9.6.4.5: the vocabulary is case-sensitive, so `"UNCHECKED"` names
// nothing and is ignored. javac reports both warnings.
snapshot!(
    keys_are_case_sensitive,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings(\"UNCHECKED\")
    void m(List<String> xs) {
        List raw = xs;
        List<String> unchecked = raw;
    }
}
",
    )])
);

// JLS §9.6.4.5 with the `java.lang.SuppressWarnings` contract ("the set of
// warnings suppressed in a given element is a union of the warnings suppressed
// in all containing elements"): an annotation on the enclosing class reaches
// the method's warnings, so neither is reported.
snapshot!(
    enclosing_class_scope_covers_method,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

@SuppressWarnings({\"rawtypes\", \"unchecked\"})
class Body {
    void m(List<String> xs) {
        List raw = xs;
        List<String> unchecked = raw;
    }
}
",
    )])
);

// JLS §9.6.4.5: a scope reaches only the annotated declaration's own parts. A
// suppression on a sibling method leaves `m`'s warnings untouched — javac
// reports both.
snapshot!(
    sibling_scope_does_not_reach,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings({\"rawtypes\", \"unchecked\"})
    void other() {
    }

    void m(List<String> xs) {
        List raw = xs;
        List<String> unchecked = raw;
    }
}
",
    )])
);

// JLS §14.4.2 with §9.6.4.5: the annotation's scope is lexical, so one on a
// *local variable declaration* suppresses within that declaration. javac
// reports only `[rawtypes]` here, exactly as for the method-level annotation.
snapshot!(
    local_declaration_scope,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void m(List<String> xs) {
        List raw = xs;
        @SuppressWarnings(\"unchecked\")
        List<String> unchecked = raw;
    }
}
",
    )])
);

// JLS §4.8 with §5.1.9/[§15.12.2.6]: an invocation of a member *declared in a
// raw type* has an erased signature, so the call is unchecked. javac:
// `warning: [unchecked] unchecked call to put(X) as a member of the raw type
// Box`. The receiver's own raw use is reported too ([§4.12.2]).
snapshot!(
    unchecked_call_through_raw_receiver,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Box<X> {
    void put(X value) {
    }
}

class Body {
    void m(Box raw) {
        raw.put(\"x\");
    }
}
",
    )])
);

// JLS §4.8 with §5.1.9: a member whose declared formals are *ground*
// (`void plain(String)`) keeps a fully checked invocation even through a raw
// receiver — javac reports the raw use and no unchecked warning, because
// erasing the signature changed nothing.
snapshot!(
    raw_receiver_ground_signature_stays_checked,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Box<X> {
    void plain(String value) {
    }
}

class Body {
    void m(Box raw) {
        raw.plain(\"x\");
    }
}
",
    )])
);

// JLS §5.1.9/[§15.12.2.2]: an actual argument whose type is raw converting to a
// parameterized formal makes the invocation unchecked. javac: `unchecked
// method invocation: method take in class Body is applied to given types`.
snapshot!(
    unchecked_argument_conversion,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Box<X> {
    X value;
}

class Body {
    static void take(Box<String> box) {
    }

    void m(Box raw) {
        take(raw);
    }
}
",
    )])
);

// JLS §5.5.2: a cast to a parameterized type cannot be checked at run time —
// its type arguments are erased — so it is an *unchecked cast*, whether the
// source is `Object` or an unrelated reference type. javac: `unchecked cast`.
snapshot!(
    unchecked_cast_to_parameterized,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void m(Object o) {
        List<String> strings = (List<String>) o;
    }
}
",
    )])
);

// JLS §4.7: `List<?>` is reifiable — the cast carries no type argument to
// check — so the same cast is fully checked and is not reported. javac reports
// nothing here.
snapshot!(
    cast_to_reifiable_wildcard_is_checked,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void m(Object o) {
        List<?> anys = (List<?>) o;
    }
}
",
    )])
);

// JLS §9.6.4.5: an unchecked call is suppressed by `"unchecked"` on the
// *invoking* declaration — the suppression is keyed to where the warning would
// be generated, not to the member being called.
snapshot!(
    unchecked_call_suppressed_at_call_site,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Box<X> {
    void put(X value) {
    }
}

class Body {
    @SuppressWarnings(\"unchecked\")
    void m(Box raw) {
        raw.put(\"x\");
    }
}
",
    )])
);
