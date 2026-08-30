//! JLS SE 26 scenario snapshots for the *friendly bad-arguments* range of an
//! inapplicable invocation ([JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2)):
//! a `wrong-argument-count` diagnostic underlines the offending *arguments* —
//! IntelliJ-style, pre-argument — instead of the method name. When the arities
//! match, exactly the incompatible arguments are highlighted and each carries
//! its own `reason: … cannot be converted to …` `related_information` entry
//! ([§5.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.3)
//! loose conversion); when they differ, the whole argument list is underlined.
//!
//! The renderer ([`check_body_diagnostic_spans`]) prints each diagnostic's
//! full `start..end` source range with the covered text, so the exact spans
//! are asserted verbatim.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- red: arity matches, some arguments are incompatible ----------------------

snapshot!(
    bad_arguments_exact_example,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String arg0, int arg1) {}

    void use() {
        int i = 0;
        func(i, \"string\");
    }
}
",
    )])
);
// `func(String, int)` is called with `(int, String)`: the arities match, so
// the diagnostic underlines exactly the two incompatible arguments — `i` and
// `"string"` — and each surfaces its own conversion-reason entry at its own
// range, alongside the `required:`/`found:` block on the merged span.

snapshot!(
    bad_arguments_only_one,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String arg0, int arg1) {}

    void use() {
        func(\"ok\", \"bad\");
    }
}
",
    )])
);
// `func("ok", "bad")`: only the second argument is incompatible, so only
// `"bad"` is underlined (the first argument stays clean, IntelliJ-style); the
// `reason:` entry points at `"bad"` only.

snapshot!(
    bad_arguments_second_of_three,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String a, String b, int c) {}

    void use() {
        func(\"x\", 42, 1);
    }
}
",
    )])
);
// A three-parameter call with a bad *middle* argument: `42` (an `int` against
// `String`) is the only bad one, so the primary range covers just `42` and
// the two good arguments are left alone.

// -- red: arity mismatch underlines the whole argument list -------------------

snapshot!(
    bad_arguments_arity_mismatch,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String arg0, int arg1) {}

    void use() {
        func(\"only one\");
        func(\"a\", 1, 2);
    }
}
",
    )])
);
// `func("only one")` and `func("a", 1, 2)` have the wrong arity: there is no
// same-arity candidate to compare against, so the whole argument list is
// underlined (from the first argument's start to the last's end) and the
// reason is the argument-list-length text.

// -- red: no-arg invocation keeps pointing at the name ------------------------

snapshot!(
    bad_arguments_no_args,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String arg0, int arg1) {}

    void use() {
        func();
    }
}
",
    )])
);
// `func()` has no arguments at all, so there is nothing to highlight: the
// diagnostic falls back to the method name `func`, like `new Foo()` against a
// parameterized constructor.

// -- green: a nested poly argument keeps its own bad-argument range -----------

snapshot!(
    bad_arguments_nested,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void take(List<String> xs) {}

    void pick(int i) {}

    void use() {
        take(pick(\"nope\"));
    }
}
",
    )])
);
// `pick("nope")` is a nested poly argument of `take(...)`: the nested
// invocation's own diagnostic underlines its argument `"nope"` (its arity
// matches `pick(int)` and the `String` does not convert), and the enclosing
// `take` reports its incompatible poly argument — the whole `pick("nope")`
// call.
