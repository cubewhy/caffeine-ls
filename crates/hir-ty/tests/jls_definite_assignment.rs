//! JLS SE 26 scenario snapshots for definite assignment
//! ([JLS §16](https://docs.oracle.com/javase/specs/jls/se26/html/jls-16.html)):
//! blank finals assigned on every path ([§16.1.8]), loops whose bodies may
//! run zero times ([§16.1.10], [§16.1.14]) and compound assignments that read
//! before writing ([§15.26.2]). Red cases render the diagnostics the type
//! layer must report; green cases confirm legal forms pass without
//! diagnostics.
//!
//! The conditional boolean operators `&&` ([§16.1.2]), `||` ([§16.1.3]),
//! `!` ([§16.1.4]) and `?:` ([§16.1.5]) split the analysis into true/false
//! outcome flows — an assignment in the right operand of a `&&` is definitely
//! assigned in the guarded code (JLS Example 16-1). Constant boolean
//! expressions ([§16.1.1]) drive the loop rules: a constant-`true` `while`/
//! `for(;;)`/`do` escapes only through its `break` paths, whose flows join
//! into the after-loop state.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: a blank final assigned on both branches ([§16.1.8]) ------------------

snapshot!(
    blank_final_assigned_on_all_paths,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(boolean p) {
        final int f;
        if (p) {
            f = 1;
        } else {
            f = 2;
        }
        int use = f + 1;
    }
}
",
    )])
);

// -- green: a conditional return guards one path ----------------------------------
// When the then branch exits, only the fall-through path constrains the join.

snapshot!(
    return_guard,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int m(boolean p) {
        int x;
        if (p) {
            return 0;
        }
        x = 1;
        return x + 1;
    }
}
",
    )])
);

// -- red: a value read before any assignment reaches it ---------------------------

snapshot!(
    use_before_assign,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m() {
        int x;
        int y = x + 1;
    }
}
",
    )])
);

// -- red: a loop body may run zero times ([§16.1.10]) ------------------------------

snapshot!(
    loop_may_not_run,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(boolean p) {
        int x;
        while (p) {
            x = 1;
        }
        int y = x;
    }
}
",
    )])
);

// -- red: a compound assignment reads its left-hand side ([§15.26.2]) ---------------

snapshot!(
    compound_reads_first,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m() {
        int x;
        x += 1;
    }
}
",
    )])
);

// -- red: the try block may exit before its assignment ([§16.2.15]) ---------------
// An exception between the assignment and the end of `try` skips nothing —
// control reaches the catch, so `x` is not definitely assigned after.

snapshot!(
    try_assignment_not_da_after,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    static void step() {}

    void m() {
        int x;
        try {
            x = 1;
            step();
        } catch (RuntimeException e) {
        }
        int y = x;
    }
}
",
    )])
);

// -- green: assigned on every path through try/catch ([§16.2.15]) ------------------
// The assignment happens in the catch too, so after the statement `x` is
// definitely assigned whichever way the try exits.

snapshot!(
    try_catch_all_paths,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m() {
        int x;
        try {
            x = 1;
        } catch (RuntimeException e) {
            x = 2;
        }
        int y = x;
    }
}
",
    )])
);

// -- green: a do-loop body runs at least once ([§16.2.13]) -------------------------
// Unlike a `while`, the condition is checked *after* the body, so an
// assignment in the body holds afterwards.

snapshot!(
    do_while_runs_at_least_once,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(boolean p) {
        int x;
        do {
            x = 1;
        } while (p);
        int y = x;
    }
}
",
    )])
);

// -- §16.1.9 with §14.11.1: assignments inside switch-expression arms ----------
// A local assigned on every arm of a switch *expression* is definitely
// assigned after the expression ([§14.11.1]: each arm block completes
// normally through its `yield`), matching javac's flow analysis for the
// common `startToken = ast; yield ...` pattern.

snapshot!(
    switch_expression_definite_assignment,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(int kind, java.util.List<String> out) {
        String start;
        java.util.List<String> picked = switch (kind) {
            case 1 -> {
                start = \"one\";
                yield out;
            }
            default -> {
                start = \"rest\";
                yield out;
            }
        };
        return start.length() + picked.size();
    }
}
",
    )])
);

// -- red: an arm that skips the assignment keeps it unassigned ------------------

snapshot!(
    switch_expression_missing_arm_assignment,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(int kind, java.util.List<String> out) {
        String start;
        int len = switch (kind) {
            case 1 -> {
                start = \"one\";
                yield out.size();
            }
            default -> out.size();
        };
        return start.length() + len;
    }
}
",
    )])
);

// -- §16.1.2 (JLS Example 16-1): the right operand of `&&` runs only when the
// left matched, so an assignment in it is definitely assigned in the guarded
// code -----------------------------------------

snapshot!(
    and_guard_assignment,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int read() { return 0; }

    int m(int v) {
        int k;
        if (v > 0 && (k = read()) >= 0)
            return k;
        return 0;
    }
}
",
    )])
);
// Green: `k` is definitely assigned inside the guarded `return` — the `&&`
// flows its true outcome (which executed the assignment) into the then arm.

// -- §16.1.3: `a || b` is true when `a` is true *or* (`a` false, `b` true) —
// so a right-operand assignment is not definitely assigned in the guarded code
// unless the left's true flow assigns too ---------------

snapshot!(
    or_guard_assignment_red,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int read() { return 0; }

    int m(int v) {
        int k;
        if (v <= 0 || (k = read()) >= 0)
            return k;
        return 0;
    }
}
",
    )])
);
// Red: the whole condition is true whenever `v <= 0`, on which path `k` was
// never assigned — so the guarded `return k` is not definitely assigned
// ([§16.1.3]: assigned after `a || b` when true iff after `a` true and after
// `b` true).

snapshot!(
    or_both_paths_assign,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int read() { return 0; }

    int m(int v) {
        int k;
        if ((k = read()) > 0 || (k = read()) >= 0)
            return k;
        return 0;
    }
}
",
    )])
);
// Green: both ways the condition is true assign `k` — the left arm's true flow
// and the right arm (under the left's false flow) — so `return k` is guarded.

// -- §16.1.2/§16.1.7: a `&&` whose true flow assigns a local but whose false
// flow does not does *not* make the local assigned after the whole expression --

snapshot!(
    and_partial_flow_not_assigned_after,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int read() { return 0; }

    void m(int v) {
        int k;
        boolean b = v > 0 && (k = read()) >= 0;
        int y = k;
    }
}
",
    )])
);
// Red: after the `&&`, `k` is assigned only on the true flow — the join with
// the false flow keeps it not-definitely-assigned ([§16.1.2] last rule).

// -- §16.1.4: `!` swaps the true and false flows ------------------------------

snapshot!(
    not_swap_guard_assignment,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int read() { return 0; }

    int m(int v) {
        int k;
        if (!(v > 0 && (k = read()) >= 0))
            return 0;
        return k;
    }
}
",
    )])
);
// Green: `!(a && b)` is false exactly when `a && b` is true — which ran the
// assignment to `k` — so the fall-through `return k` sees `k` assigned
// ([§16.1.4] swaps the inner expression's flows).

// -- §16.1.5: a conditional expression joins its two arms' flows --------------

snapshot!(
    conditional_arm_assignment,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int m(boolean p) {
        int k;
        int x = p ? (k = 1) : 2;
        return k;
    }
}
",
    )])
);
// Green: both arms assign `k` (the `1` literal, then the else arm's constant
// 2 does not, but the else arm does not assign `k`) — after the conditional
// only the then arm assigned `k`, so the join leaves it unassigned ([§16.1.5]).

snapshot!(
    conditional_both_arms_assign,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int m(boolean p) {
        int k;
        int x = p ? (k = 1) : (k = 2);
        return k;
    }
}
",
    )])
);
// Green: every arm assigns `k`, so it is definitely assigned after the
// conditional ([§16.1.5] join).

// -- §16.2.10 (JLS Example 16-1): a constant-true loop escapes only through
// its `break`, so the assignments before each `break` carry past the loop -----

snapshot!(
    while_true_break_carries_assignment,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(int n) {
        int k;
        while (true) {
            k = n;
            if (k >= 5) break;
            n = 6;
        }
        int y = k;
    }
}
",
    )])
);
// Green: the only way past `while (true)` is the `break`, and `k` is assigned
// before it (JLS Example 16-1). The non-constant variant below stays red.

snapshot!(
    while_non_constant_does_not_carry,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(int n, boolean p) {
        int k;
        while (p) {
            k = n;
            if (k >= 5) break;
            n = 6;
        }
        int y = k;
    }
}
",
    )])
);
// Red: `p` may be false immediately, so the body may never run (JLS Example
// 16-1's second case).

// -- §16.2.12: a `for(;;)` without a condition is a constant-true loop --------

snapshot!(
    for_no_cond_break_carries_assignment,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int m(int n) {
        int k;
        for (;;) {
            k = n;
            if (k >= 5) break;
        }
        return k;
    }
}
",
    )])
);
// Green: the missing condition never completes through a false value; the
// `break` path carries the assignment.

// -- §16.2.11: a `do { ... } while (true)` with a break ------------------------

snapshot!(
    do_while_true_break_carries_assignment,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int m(int n) {
        int k;
        do {
            k = n;
            if (k >= 5) break;
            n = 6;
        } while (true);
        return k;
    }
}
",
    )])
);
// Green: the condition is a constant `true`, so only the `break` exits the
// loop and the assignment before it is definitely assigned after.

// -- JLS Example 16-2: the values of expressions are not considered -----------
// A compiler must reject `k` after `if (n > 2) k = 3;` even when `n` is a
// constant variable — only boolean *constant expressions* (§15.29) participate
// in the flow analysis.

snapshot!(
    values_of_expressions_not_considered,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(int n) {
        int k;
        int constant = 5;
        if (constant > 2)
            k = 3;
        int y = k;
    }
}
",
    )])
);
// Red: `constant > 2` is not a constant expression (§15.29 names only
// literals, operators over constants and simple names of *constant variables*
// — a `final` initialized by a constant), so the then-assignment is not
// guaranteed and `k` is not definitely assigned (JLS Example 16-2).

snapshot!(
    final_constant_variable_condition,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(int n) {
        int k;
        final int constant = 5;
        if (constant > 2)
            k = 3;
        int y = k;
    }
}
",
    )])
);
// Green: `constant` is a constant variable ([§4.12.4]) — a `final` local
// initialized by a constant — so `constant > 2` *is* a constant expression
// (§15.29, the relational operator over constants) of value `true`: the `if`
// always takes the then arm and `k` is definitely assigned ([§16.1.1]). javac
// accepts this.
