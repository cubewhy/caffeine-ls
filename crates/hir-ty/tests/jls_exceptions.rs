//! JLS SE 26 scenario snapshots for exceptions
//! ([JLS §11](https://docs.oracle.com/javase/specs/jls/se26/html/jls-11.html)):
//! checked-exception declaration ([§11.2]) and multi-catch alternatives
//! ([§14.20]). Green cases confirm the exception forms type without errors.
//!
//! The trailing *known divergence* section pins behaviour that still
//! contradicts the spec or `javac` 25 — the compile-time liability check for
//! an unreported checked exception ([§11.2]) and unreachable-catch detection
//! for a clause shadowed by an earlier superclass catch ([§11.2.3]) are both
//! missing. Their snapshots are kept pending (`.snap.new`) until the
//! divergences are fixed; a fix must flip them.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: a checked exception declared with throws -----------------------------

snapshot!(
    checked_exception_declared,
    check_body_types(&[(
        "/src/com/example/Exc.java",
        "\
package com.example;

import java.io.IOException;

class Exc {
    void risky() throws IOException { }

    void caller() throws IOException { risky(); }
}
",
    )])
);

// -- green: multi-catch of unrelated alternatives ([§14.20]) ----------------------

snapshot!(
    multi_catch,
    check_body_types(&[(
        "/src/com/example/Exc.java",
        "\
package com.example;

import java.io.IOException;

class Exc {
    void risky() throws IOException { }

    void m() {
        try {
            risky();
        } catch (IOException | RuntimeException e) {
        }
    }
}
",
    )])
);

// -- known divergence: unreported checked exception ([§11.2]) ----------------------
// A checked exception must be declared or caught; `javac` 25 reports
// "unreported exception IOException", but the type layer stays silent.

snapshot!(
    divergence_unreported_checked_exception,
    check_body_types(&[(
        "/src/com/example/Exc.java",
        "\
package com.example;

import java.io.IOException;

class Exc {
    void risky() throws IOException { }

    void caller() { risky(); }
}
",
    )])
);

// -- known divergence: catch clause shadowed by a superclass ([§11.2.3]) ------------
// `javac` 25 rejects the second clause as already caught ("exception
// IOException has already been caught"); the type layer stays silent.

snapshot!(
    divergence_catch_order_unreachable,
    check_body_types(&[(
        "/src/com/example/Exc.java",
        "\
package com.example;

import java.io.IOException;

class Exc {
    void risky() throws IOException { }

    void m() {
        try {
            risky();
        } catch (Exception e) {
        } catch (IOException e) {
        }
    }
}
",
    )])
);
