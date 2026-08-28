//! JLS SE 26 scenario snapshots for blocks and statements
//! ([JLS §14](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html)):
//! condition positions ([§14.9]–[§14.11], [§14.16]), the for-each iterable
//! ([§14.14.2]) and return/throw liability ([§14.17], [§14.18]). Red cases
//! render the diagnostics the type layer must report; green cases confirm the
//! statement forms type without errors.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: statement forms ------------------------------------------------

snapshot!(
    statement_forms,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    int m(int a, boolean p, boolean q, List<String> xs, int[] arr, Object o) {
        int acc = 0;
        if (p && q) { acc += 1; } else if (p) { acc += 2; } else { acc += 3; }
        while (p && acc < 10) { acc++; }
        do { acc--; } while (acc > 0 && p);
        for (int i = 0; i < 3 && acc < 5; i++) { acc += i; }
        for (String x : xs) { acc += x.length(); }
        for (int n : arr) { acc += n; }
        if (o instanceof String s) { acc += s.length(); }
        assert p : \"msg\";
        return acc;
    }
}
",
    )])
);

// -- red: non-boolean conditions ---------------------------------------------

snapshot!(
    non_boolean_conditions,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(int x, String s) {
        int acc = 0;
        if (x) { acc++; }
        while (s) { acc++; }
        do { acc++; } while (x);
        for (int i = 0; s; i++) { acc++; }
        for (; x;) { break; }
        int t = x ? 1 : 2;
        assert x;
        return acc;
    }
}
",
    )])
);

// -- red: for-each over a non-iterable --------------------------------------

snapshot!(
    for_each_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(int x, String s, Object o) {
        int acc = 0;
        for (Object v : o) { acc++; }
        for (char c : s) { acc++; }
        return acc;
    }
}
",
    )])
);

// -- red: return liability ---------------------------------------------------

snapshot!(
    return_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(String s) {
        Body b = m2(s);
    }

    int m2(String s) { return 1; }

    void bad() { return 1; }
    String bad2() { return 1; }
    int bad3(Object o) { return o; }
}
",
    )])
);

// -- green: try/catch/finally and try-with-resources (§14.20) ----------------

snapshot!(
    try_catch_forms,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.io.Closeable;
import java.io.IOException;

class Resource implements Closeable {
    public void close() {}
    int read() { return 1; }
}

class Body {
    int risky() throws IOException { return 1; }

    int m(Resource r) {
        int acc = 0;
        try {
            acc += risky();
        } catch (IOException e) {
            acc = -1;
        } finally {
            acc += 2;
        }
        try (Resource r2 = new Resource(); Resource r3 = new Resource()) {
            acc += r2.read();
        }
        return acc;
    }
}
",
    )])
);

// -- §14.22: unreachable statements --------------------------------------------
// The statement after an abruptly-completing statement is a compile-time
// error; §8.4.7 completion analysis still sees the exit.

snapshot!(
    unreachable_statement_after_return,
    check_body_types(&[(
        "/src/com/example/Dead.java",
        "\
package com.example;

class Dead {
    int dead() {
        return 1;
        System.out.println(2);
    }
}
",
    ),])
);

snapshot!(
    missing_return_statement,
    check_body_types(&[(
        "/src/com/example/NoRet.java",
        "\
package com.example;

class NoRet {
    int missing() {
        int y = 1;
    }
}
",
    ),])
);

// §8.4.7: a method with a non-`void` return type must not be able to
// complete normally — reported against the method's closing brace.

// -- green: statements after lambda-valued expressions are reachable -------------
// §14.22: a `return` inside a lambda body returns from the *lambda*
// ([§15.27.2]); it must not mark the enclosing method's following statements
// unreachable.

snapshot!(
    statement_after_lambda_reachable,
    check_body_types(&[(
        "/src/com/example/Lambda.java",
        "\
package com.example;

class Lambda {
    interface IntMaker { int make(); }

    void go() {
        int n = first(() -> {
            side();
            return 1;
        });
        after(n);
    }

    int first(IntMaker maker) { return maker.make(); }
    void side() { }
    void after(int n) { }
}
",
    ),])
);

// -- §14.14.1: basic-for header slots ------------------------------------------
// ForInit may be a statement expression list (`i = 0`, `j--, k++`), not only
// a declaration: the initializer is checked as a statement, the condition —
// and only the condition — must be `boolean`.

snapshot!(
    for_header_statement_expression_lists,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(int i, int j) {
        int acc = 0;
        for (i = 0; i < 3; i++) { acc += i; }
        for (i = 0, j = 9; i < j; i++, j--) { acc++; }
        return acc;
    }
}
",
    )])
);

snapshot!(
    for_header_condition_checked,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(int x) {
        for (; x;) { break; }
        for (; x < 3;) { break; }
    }
}
",
    )])
);

snapshot!(
    finally_always_runs,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.io.IOException;

class Body {
    static class Guard {
        void lock() {}
        void unlock() {}
    }

    void run(Guard lock) throws IOException {
        boolean acquired = false;
        lock.lock();
        try {
            acquired = true;
        } catch (RuntimeException exception) {
            if (acquired) {
                lock.unlock();
            }
            throw new IOException(\"Interrupted\", exception);
        } finally {
            if (acquired) {
                lock.unlock();
            }
        }
    }
}
",
    )])
);
// §14.20.2/§16.1.8: a `finally` block always runs — even when the try block
// returns and every catch clause throws again — so it is entered reachable,
// and only the statement *after* the try takes the try's abrupt-completion
// state. Reporting the finally's first statement as unreachable would be a
// false positive.

snapshot!(
    pattern_var_negation_or,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    record Entry(String name) {}

    int scan(Object value) {
        if (!(value instanceof Entry entry) || entry.name() == null || entry.name().length() == 0) {
            return 0;
        }
        return entry.name().length();
    }
}
",
    )])
);
// §14.30.3: in `a || b || c`, the right operands are reached only through the
// left's *false* flow, so a negated pattern `!(value instanceof Entry entry)`
// binds `entry` in every subsequent operand and in the guarded code.

snapshot!(
    conditional_instanceof_pattern,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static class Entry {
        String stableKey() { return \"k\"; }
        String label() { return \"l\"; }
    }

    static String key(Object value) {
        return value instanceof Entry entry ? entry.stableKey() : \"\";
    }

    static String label(Object value) {
        return !(value instanceof Entry entry) ? \"\" : entry.label();
    }
}
",
    )])
);
// §14.30.3: a pattern in a conditional condition binds its variables in the
// arm where they are definitely matched — the condition's true flow in the
// then-arm, its false flow (via `!`) in the else-arm.
