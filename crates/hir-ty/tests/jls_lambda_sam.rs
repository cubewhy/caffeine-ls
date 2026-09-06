//! JLS SE 26 scenario snapshots for lambda expression / SAM conformance
//! ([§9.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.8),
//! [§15.27.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27.3),
//! [§15.27.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27.2)):
//! a generic method can never be a functional interface's single abstract
//! method (`invalid functional descriptor`), a block lambda against a
//! value-returning SAM must return on every normal-completing path (`missing
//! return value`), lambda parameter count must equal the SAM's declared
//! parameters (a varargs SAM formal is the element array, so a two-parameter
//! lambda against `String...` is `incompatible parameter types`), and
//! unreachable statements inside lambda bodies are reported.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- red: a generic method is not a valid functional descriptor ([§9.8]) ------

snapshot!(
    generic_method_sam,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

interface Gen {
    <T> T id(T t);
}

class L1Test {
    void f() {
        Gen g = x -> x;
    }
}
",
    )])
);
// Red: `Gen`'s only abstract method is generic — a lambda cannot implement
// every instantiation of `<T> T id(T)`, so the interface has no functional
// descriptor (javac: `invalid functional descriptor for lambda expression …
// method (T)T in interface Gen is generic`).

// -- red/green: block lambdas must return on every normal-completing path -----
// (§15.27.3 with §14.17: a value-returning block's every path that completes
// normally must end in a `return` of a value.)

snapshot!(
    block_lambda_missing_return,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

import java.util.function.Supplier;

class L2 {
    boolean c;

    void red() {
        Supplier<Integer> a = () -> {
            if (this.c) {
                return 1;
            }
        };
        Supplier<Integer> d = () -> {
            while (this.c) {
                return 1;
            }
        };
    }

    void green() {
        Supplier<Integer> b = () -> {
            return 1;
        };
        Supplier<Integer> e = () -> {
            while (true) {
                return 1;
            }
        };
    }
}
",
    )])
);
// Red: the `if`-without-else and the condition-`while` bodies each have a
// path that completes normally with no return value (javac: `missing return
// value`). Green: the plain return, and the `while (true)` body that can
// never complete normally, are legal.

// -- red: lambda parameter count vs a varargs SAM ([§15.27.3]/[§8.4.1]) -------

snapshot!(
    varargs_sam_arity,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

interface Var {
    void v(String... args);
}

class L9 {
    void f() {
        Var x = () -> {};
        Var x2 = a -> {};
        Var x3 = (a, b) -> {};
        Var x4 = (String... a) -> {};
    }
}
",
    )])
);
// Red: a zero-parameter lambda against `String...` and a two-parameter one
// both declare a different number of formals than the SAM (whose varargs
// parameter is the single `String[]`, [§8.4.1]) — javac: `incompatible
// parameter types in lambda expression`. Green: the one-parameter form and
// the explicit `String... a` form.

// -- red: unreachable statements inside lambda bodies ([§14.22]) ---------------

snapshot!(
    unreachable_in_lambda,
    check_body_diagnostic_spans(&[(
        "/src/com/example/P.java",
        "\
package com.example;

class L5 {
    void f() {
        Runnable r = () -> {
            return;
            int unreachable = 1;
        };
    }
}
",
    )])
);
// Red: the statement after the `return` inside the lambda block is
// unreachable ([§14.22] applies to the lambda body like any block).
