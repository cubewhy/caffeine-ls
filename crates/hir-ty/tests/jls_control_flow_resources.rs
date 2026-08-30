//! JLS SE 26 scenario snapshots for *control flow, try-with-resources and
//! patterns*: an unlabeled `break` outside a `switch` or loop is
//! `BreakOutsideSwitchOrLoop` ([§14.15]); an unlabeled `continue` outside a
//! loop is `ContinueOutsideLoop` ([§14.16]); a labeled `break`/`continue` to
//! a label that is not in scope is `UndefinedLabel`, and a labeled `continue`
//! to a non-loop label is `NotALoopLabel`; a record pattern with the wrong
//! number of nested patterns is `IncorrectNumberOfPatternComponents`
//! ([§14.30.2]); and a try-with-resources declaration whose type is not
//! `AutoCloseable` is an `IncompatibleTypes` error ([§14.20.3]). Red cases
//! render the diagnostics; green cases confirm legal programs pass cleanly.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- §14.15/[§14.16: break and continue outside a loop ------------------------

snapshot!(
    continue_outside_loop,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Break.java",
        "\
package com.example;

class A {
    void m() {
        continue;
    }
    void n() {
        for (int i = 0; i < 3; i++) {
            if (i == 1) {
                continue;
            }
        }
    }
}
",
    )])
);
// Red: `continue` in `m()` has no enclosing loop; inside the `for` it is fine.

snapshot!(
    break_outside_switch_or_loop,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Break.java",
        "\
package com.example;

class A {
    void m() {
        break;
    }
    void n(int x) {
        switch (x) {
            case 1:
                break;
            default:
                break;
        }
    }
    void p() {
        while (true) {
            break;
        }
    }
}
",
    )])
);
// Red: an unlabeled `break` in `m()` has no enclosing switch or loop; inside a
// switch arm and a `while` it is fine.

// -- §14.15/[§14.16: labeled break/continue ------------------------------------

snapshot!(
    undefined_label,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Label.java",
        "\
package com.example;

class A {
    void m() {
        break nowhere;
    }
    void n() {
        outer: for (;;) {
            continue nowhere;
        }
    }
    void p() {
        outer: for (;;) {
            break outer;
        }
    }
}
",
    )])
);
// Red: `break nowhere` and `continue nowhere` name labels no enclosing
// statement declares; `break outer` inside the labeled loop is fine.

snapshot!(
    not_a_loop_label,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Label.java",
        "\
package com.example;

class A {
    void m() {
        lab: {
            continue lab;
        }
    }
    void n() {
        lab: for (;;) {
            continue lab;
        }
    }
}
",
    )])
);
// Red: `continue lab` where `lab` labels a block — a `continue` can only
// target a labeled loop; the labeled-`for` form is fine.

// -- §14.30.2: record pattern arity -------------------------------------------

snapshot!(
    incorrect_number_of_pattern_components,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Pattern.java",
        "\
package com.example;

record Point(int x, int y) {
}

class A {
    void m(Object o) {
        switch (o) {
            case Point(int x) -> {
            }
            default -> {
            }
        }
    }
    void n(Object o) {
        if (o instanceof Point(int x, int y, int z)) {
        }
    }
    void p(Object o) {
        if (o instanceof Point(int x, int y)) {
        }
    }
}
",
    )])
);
// Red: `Point(int x)` and `Point(int x, int y, int z)` do not match the two
// components of `Point`; the matching `Point(int x, int y)` is fine.

// -- §14.20.3: try-with-resources must be AutoCloseable -----------------------

snapshot!(
    resource_must_be_auto_closeable,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Resource.java",
        "\
package com.example;

class NotCloseable {
}

class R implements AutoCloseable {
    public void close() {
    }
}

class A {
    void m() {
        try (NotCloseable n = new NotCloseable()) {
        }
    }
    void n() {
        try (R r = new R()) {
        }
    }
}
",
    )])
);
// Red: a `NotCloseable` resource is not assignable to `AutoCloseable`; an
// `AutoCloseable` subtype is fine.
