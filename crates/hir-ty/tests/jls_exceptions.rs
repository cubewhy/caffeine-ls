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

// -- §11.2.3: a catch clause whose checked exception the try block cannot
// throw is a compile-time error; unchecked types are always allowed.

snapshot!(
    catch_never_thrown,
    check_body_types(&[(
        "/src/com/example/Never.java",
        "\
package com.example;

import java.io.IOException;

class Never {
    void t() {
        try {
            int quiet = 1;
        } catch (IOException e) {
        }
    }
}
",
    ),])
);

snapshot!(
    catch_thrown_is_fine,
    check_body_types(&[(
        "/src/com/example/Thrown.java",
        "\
package com.example;

import java.io.IOException;

class Thrown {
    void t() throws IOException {
        try {
            if (Math.max(1, 2) > 1) {
                throw new IOException(\"real\");
            }
        } catch (IOException e) {
        }
    }
}
",
    ),])
);

// -- green: a multi-catch whose *second* alternative is the checked exception --
// §14.20: every alternative of a multi-catch parameter discharges
// independently — `catch (RuntimeException | IOException e)` handles an
// `IOException` thrown by the try block even though `IOException` is not the
// first alternative.

snapshot!(
    multi_catch_second_alternative_discharges,
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
        } catch (RuntimeException | IOException e) {
        }
    }
}
",
    )])
);

// -- red: a multi-catch with a dead alternative ([§11.2.3]) ----------------------
// The try block can only throw `IOException`; the `ClassNotFoundException`
// alternative is checked and can never be thrown, while the live alternatives
// still discharge their share of the liability.

snapshot!(
    multi_catch_dead_alternative,
    check_body_types(&[(
        "/src/com/example/Exc.java",
        "\
package com.example;

import java.io.IOException;

class Exc {
    void io() throws IOException { }

    void m() {
        try {
            io();
        } catch (IOException | ClassNotFoundException e) {
        }
    }
}
",
    ),])
);

// -- green: precise rethrow ([§11.2.2]) -----------------------------------------
// A `throw e` of an (effectively final) catch parameter throws precisely the
// checked exceptions the try block can throw and no earlier clause caught —
// here exactly `CheckstyleException`, which the method declares.

snapshot!(
    precise_rethrow_accepted,
    check_body_types(&[(
        "/src/com/example/Rethrow.java",
        "\
package com.example;

class Rethrow {
    class CheckstyleException extends Exception { }

    int process(String file) throws CheckstyleException {
        try {
            work(file);
        } catch (java.io.IOException ioe) {
            report(ioe);
        } catch (Exception exc) {
            throw exc;
        }
        return 1;
    }

    void work(String f) throws CheckstyleException, java.io.IOException { }
    void report(java.io.IOException ioe) { }
}
",
    ),])
);

// -- red: a precise rethrow of a type the method does not declare -----------------
// The precise set is `{ IOException }` — narrowed from the catch parameter's
// declared `Exception` by §11.2.2 — which `m` does not declare.

snapshot!(
    precise_rethrow_unreported,
    check_body_types(&[(
        "/src/com/example/Rethrow.java",
        "\
package com.example;

import java.io.IOException;

class Rethrow {
    void m() {
        try {
            io();
        } catch (Exception exc) {
            throw exc;
        }
    }

    void io() throws IOException { }
}
",
    ),])
);

// -- green: unchecked-compatible catch clauses ([§11.2.3]) ----------------------
// `catch (Exception)` is legal even when the try block provably throws
// nothing checked: `Exception` covers the unchecked `RuntimeException`/`Error`,
// so a catch-all never becomes a dead clause.

snapshot!(
    catch_unchecked_carve_outs,
    check_body_types(&[(
        "/src/com/example/Catch.java",
        "\
package com.example;

class Catch {
    void plain() { }

    void ok() {
        try {
            plain();
        } catch (Exception e) {
        } catch (Throwable t) {
        }
    }
}
",
    )])
);

// -- green: catching a *subclass* of a thrown checked exception ([§11.2.3]) -----
// A defensive catch of a thrown exception's subclass stays legal even though
// it covers nothing new; the thrown supertype's liability is not discharged.

snapshot!(
    catch_subtype_of_thrown,
    check_body_types(&[(
        "/src/com/example/Sax.java",
        "\
package com.example;

class Sax {
    void parent() throws Exception { }

    void io() throws java.io.IOException { }

    // The catch covers the thrown type; nothing is left pending.
    void covered() {
        try {
            parent();
        } catch (Exception e) {
        }
    }

    // A defensive catch of a *subclass* of a thrown checked exception is
    // legal ([§11.2.3]) even though it discharges nothing — the supertype's
    // liability remains and must still be declared.
    void defensive() throws java.io.IOException {
        try {
            io();
        } catch (java.io.FileNotFoundException e) {
        }
    }

    // A precise rethrow of the caught subclass keeps its own liability.
    void rethrowSubclass() throws java.io.FileNotFoundException {
        try {
            throw new java.io.FileNotFoundException(\"missing\");
        } catch (java.io.FileNotFoundException e) {
            throw e;
        }
    }
}
",
    )])
);

// -- red: a checked alternative unrelated to anything thrown ([§11.2.3]) --------
// The catch clause can never run: no thrown class is assignable to it and it
// is not assignable to any thrown class, nor does it cover the unchecked
// hierarchy.

snapshot!(
    catch_unrelated_checked,
    check_body_types(&[(
        "/src/com/example/CatchUnrelated.java",
        "\
package com.example;

import java.io.FileNotFoundException;

class CatchUnrelated {
    void sax() throws ClassNotFoundException { }

    void dead() {
        try {
            sax();
        } catch (FileNotFoundException e) {
        }
    }
}
",
    )])
);
