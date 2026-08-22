//! JLS SE 26 scenario snapshots for exceptions
//! ([JLS §11](https://docs.oracle.com/javase/specs/jls/se26/html/jls-11.html)):
//! checked-exception liability ([§11.2]) and catch clauses ([§11.2.3],
//! [§14.20]). Red cases render the diagnostics the type layer must report;
//! green cases confirm the exception forms type without errors.

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

// -- red: an unreported checked exception ([§11.2]) --------------------------------
// A checked exception must be declared or caught; `javac` 25 reports
// "unreported exception IOException".

snapshot!(
    unreported_checked_exception,
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

// -- red: a catch clause shadowed by a superclass ([§11.2.3]) ----------------------
// `javac` 25 rejects the second clause as already caught ("exception
// IOException has already been caught").

snapshot!(
    catch_order_unreachable,
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
