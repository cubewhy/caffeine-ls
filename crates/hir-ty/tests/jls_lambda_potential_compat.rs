//! Snapshots of potential applicability for lambda arguments
//! ([JLS §15.12.2.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.1),
//! [§15.27.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27.2),
//! [§14.22](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.22)).
//!
//! A lambda argument is potentially compatible with a candidate's function
//! type only when its body's shape matches the result kind: a `void` result
//! requires a statement expression ([§14.8]) or a void-compatible block, a
//! value result an expression or a value-compatible block. A block is
//! void-compatible iff every `return` in it is a bare `return;`, and
//! value-compatible iff it cannot complete normally ([§14.22]) and every
//! `return` carries a value — so `{ try { … } catch (Throwable t) { } }`
//! (can complete normally) targets only the `void` functional interface while
//! `{ throw … }` targets only the value-returning one. `ExecutorService.submit`
//! has exactly this `<T> Future<T> submit(Callable<T>)` / `Future<?>
//! submit(Runnable)` overload pair.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- §15.12.2.1/§15.27.2: block shape decides the applicable overload ---------
// javac's picks for this fixture (verified with `javap -c`): `Runnable`-shaped
// (`Task`) for the empty, `try`/`catch`, `while (flag)`, `try`/`finally` and
// bare-`return` bodies; `Callable`-shaped (`ValueTask`) for the `throw`-only,
// `while (true)` and expression bodies. `{ if (b) return 1; }` matches neither
// and is javac's `no suitable method found`.

snapshot!(
    lambda_potential_compat_body_types,
    check_body_types(&[(
        "/src/com/example/Gate.java",
        "\
package com.example;

class Gate {
    interface Task {
        void run();
    }

    interface ValueTask<T> {
        T call() throws Exception;
    }

    static String submit(Task task) {
        return null;
    }

    static <T> T submit(ValueTask<T> task) {
        return null;
    }

    static void sink() {
    }

    static void emptyBlock() {
        submit(() -> {
        });
    }

    static void tryCatchBlock() {
        submit(() -> {
            try {
                sink();
            } catch (Throwable throwable) {
            }
        });
    }

    static void whileBlock(boolean flag) {
        submit(() -> {
            while (flag) {
            }
        });
    }

    static void tryFinallyBlock() {
        submit(() -> {
            try {
            } finally {
            }
        });
    }

    static void bareReturnBlock(boolean flag) {
        submit(() -> {
            if (flag) {
                return;
            }
        });
    }

    static Object throwOnlyBlock() {
        return submit(() -> {
            throw new RuntimeException();
        });
    }

    static Object constantTrueWhile() {
        return submit(() -> {
            while (true) {
            }
        });
    }

    static Integer expressionBody() {
        return submit(() -> 42);
    }
}
",
    )])
);

// -- green: no diagnostic for any of the eight invocations --------------------

snapshot!(
    lambda_potential_compat_diagnostics,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Gate.java",
        "\
package com.example;

class Gate {
    interface Task {
        void run();
    }

    interface ValueTask<T> {
        T call() throws Exception;
    }

    static String submit(Task task) {
        return null;
    }

    static <T> T submit(ValueTask<T> task) {
        return null;
    }

    static void sink() {
    }

    static void emptyBlock() {
        submit(() -> {
        });
    }

    static void tryCatchBlock() {
        submit(() -> {
            try {
                sink();
            } catch (Throwable throwable) {
            }
        });
    }

    static void whileBlock(boolean flag) {
        submit(() -> {
            while (flag) {
            }
        });
    }

    static void tryFinallyBlock() {
        submit(() -> {
            try {
            } finally {
            }
        });
    }

    static void bareReturnBlock(boolean flag) {
        submit(() -> {
            if (flag) {
                return;
            }
        });
    }

    static Object throwOnlyBlock() {
        return submit(() -> {
            throw new RuntimeException();
        });
    }

    static Object constantTrueWhile() {
        return submit(() -> {
            while (true) {
            }
        });
    }

    static Integer expressionBody() {
        return submit(() -> 42);
    }
}
",
    )])
);

// -- §15.27.2: a body that is neither void- nor value-compatible --------------
// `{ if (flag) return 1; }` carries a valued `return` (so it is not
// void-compatible) and *can* complete normally (so it is not value-compatible
// either): it is not potentially compatible with the `Task` result
// ([§15.12.2.1]) and not congruent with the `ValueTask<T>` result
// ([§15.27.2]), so no candidate is applicable. javac reports `no suitable
// method found for submit(...)` naming both overloads (`bad return type in
// lambda expression: unexpected return value` against `Task`, `missing return
// value` against `ValueTask`); the invocation is reported here as
// `wrong-argument-count` from the same applicability outcome. Under the
// deleted approximation the valued `return` alone made the body
// value-compatible, so `ValueTask` stayed applicable — and because the body
// can also complete normally, the diagnostic was `missing-return-value`.

snapshot!(
    lambda_potential_compat_neither_shape,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Gate.java",
        "\
package com.example;

class Gate {
    interface Task {
        void run();
    }

    interface ValueTask<T> {
        T call() throws Exception;
    }

    static String submit(Task task) {
        return null;
    }

    static <T> T submit(ValueTask<T> task) {
        return null;
    }

    static void neither(boolean flag) {
        submit(() -> {
            if (flag) {
                return 1;
            }
        });
    }
}
",
    )])
);
