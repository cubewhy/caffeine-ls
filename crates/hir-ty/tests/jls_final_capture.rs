//! JLS SE 26 scenario snapshots for *final variables, captures and
//! duplicates*: assigning to a `final` variable or field
//! ([§8.3.1.2], [§16]) is `CannotAssignToFinalVariable`; a blank `final`
//! field that no constructor path (or, static, no static initializer)
//! assigns is `FinalFieldNotInitialized`; a local captured by a lambda that
//! is not effectively final is `VariableMustBeEffectivelyFinal`
//! ([§6.5.6.1], [§15.27.2]); and a member, local or parameter whose name is
//! already declared in scope is `DuplicateDeclaration` ([§6.4]). Red cases
//! render the diagnostics; green cases confirm legal programs pass cleanly.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_class_diagnostics};

// -- §8.3.1.2/[§16]: cannot assign to a final variable -----------------------

snapshot!(
    assign_final_local,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    void m() {
        final int x = 1;
        x = 2;
    }
    void n(final int p) {
        p = 3;
    }
}
",
    )])
);
// Red: a `final` local with an initializer, and a `final` parameter, cannot be
// reassigned.

snapshot!(
    assign_final_field,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int x = 1;
    static final int s = 2;
    void m() {
        x = 3;
        this.s = 4;
    }
}
",
    )])
);
// Red: a `final` field with an initializer cannot be reassigned.

snapshot!(
    blank_final_local_assignment,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
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
// Green: a *blank* `final` local (no initializer) may be assigned once on
// every path by definite assignment ([§16]).

snapshot!(
    blank_final_field_ctor_init,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        f = 1;
    }
    Final(int x) {
        this();
    }
}
",
    )])
);
// Green: a blank `final` instance field is initialized by a constructor (or a
// delegating one defers to it) — no assignment error, no
// final-field-not-initialized.

// -- §8.3.1.2/[§16]: final field not initialized -----------------------------

snapshot!(
    final_field_not_initialized,
    check_class_diagnostics(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Broken {
    final int f;
    static final int s;
    Broken() {
    }
    Broken(int x) {
    }
}
",
    )])
);
// Red: `f` is blank and neither constructor assigns it; `s` is blank and no
// static initializer assigns it.

snapshot!(
    final_field_initialized,
    check_class_diagnostics(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Ok {
    final int f;
    final int g = 1;
    static final int s;
    Ok() {
        f = 2;
    }
    static {
        s = 3;
    }
}
",
    )])
);
// Green: `f` is assigned in the (only) constructor, `g` by its initializer and
// `s` in the static initializer.

// -- §6.5.6.1/[§15.27.2]: effectively final captures --------------------------

snapshot!(
    lambda_capture_mutated,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Capture.java",
        "\
package com.example;

class Capture {
    void m() {
        int y = 1;
        Runnable r = () -> { int c = y; };
        y = 2;
    }
}
",
    )])
);
// Red: `y` is captured by the lambda and reassigned afterwards, so it is not
// effectively final — reported at the capturing reference.

snapshot!(
    lambda_capture_effectively_final,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Capture.java",
        "\
package com.example;

class Capture {
    void m() {
        int y = 1;
        Runnable r = () -> { int c = y; };
        int z = y + 1;
    }
}
",
    )])
);
// Green: `y` is never reassigned, so the capture is fine.

// -- §6.4: duplicate declarations ---------------------------------------------

snapshot!(
    duplicate_field,
    check_class_diagnostics(&[(
        "/src/com/example/Dup.java",
        "\
package com.example;

class Dup {
    int a;
    int a;
    int b;
}
",
    )])
);
// Red: the second `a` field is already defined in the class.

snapshot!(
    duplicate_local,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Dup.java",
        "\
package com.example;

class Dup {
    void m(int p) {
        int q = 1;
        int q = 2;
    }
    void n() {
        int r = 1;
        {
            int r = 2;
        }
    }
}
",
    )])
);
// Red: `q` is declared twice in one method scope, and the nested block's `r`
// shadows the enclosing method's `r` — both already defined (§6.4).

// -- §6.4: lambda parameters may not shadow an enclosing declaration ---------
// A lambda parameter ([§15.27.1]) is in scope throughout its body ([§6.3])
// and may not re-declare a name already in scope: an enclosing lambda
// parameter, or a local/parameter of the enclosing body ([§6.4]). A local
// declared textually *after* the lambda is not in scope at it, so the
// shadowing is legal there.

snapshot!(
    lambda_param_shadows_local,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

import java.util.function.Predicate;

class Shadow {
    void go(Predicate<String> p) {}

    void m() {
        String i = \"\";
        go(i -> i == \"\");

        String a;
        go(a -> a == \"\");

        go(x -> x == \"\");
    }
}
",
    )])
);
// Red: the lambda parameter `i` re-declares the enclosing local `i` — reported
// at the parameter name, matching javac; the body reference `i` binds to the
// parameter ([§6.3]), so no effectively-final or not-initialized error is
// reported for the enclosing locals. `a` (declared without an initializer) is
// likewise shadowed — javac reports `already defined`, not `not initialized`.
// Green: `x` clashes with nothing, and the reference binds to the parameter.

snapshot!(
    lambda_param_shadows_method_param,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

import java.util.function.Predicate;

class Shadow {
    void go(Predicate<String> p) {}

    void m(String i) {
        go(i -> i == \"\");
    }
}
",
    )])
);
// Red: a lambda parameter may not shadow a formal parameter of the enclosing
// method (§6.4).

snapshot!(
    lambda_param_after_local,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

import java.util.function.Predicate;

class Shadow {
    void go(Predicate<String> p) {}

    void m() {
        go(i -> i == \"\");
        String i = \"outer\";
    }
}
",
    )])
);
// Green: the local `i` is declared after the lambda, so it is not in scope at
// the lambda — the parameter does not shadow anything (§6.4).

snapshot!(
    lambda_param_sibling_block_local,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

import java.util.function.Predicate;

class Shadow {
    void go(Predicate<String> p) {}

    void m() {
        {
            String i = \"outer\";
        }
        go(i -> i == \"\");
    }
}
",
    )])
);
// Green: the local `i` is declared in a sibling block that has ended, so it is
// not in scope at the lambda (§6.3) — the parameter does not shadow anything.

snapshot!(
    lambda_parameter_list_duplicate,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

import java.util.function.BiConsumer;

class Shadow {
    void h(BiConsumer<String, String> c) {}

    void m() {
        h((x, x) -> {});
    }
}
",
    )])
);
// Red: a lambda parameter list may not repeat a name — the second `x` is
// already defined ([§15.27.1], §6.4).

snapshot!(
    lambda_body_local_shadows_param,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

import java.util.function.Consumer;

class Shadow {
    void h(Consumer<String> c) {}

    void m() {
        h(i -> {
            String i = \"inner\";
        });
    }
}
",
    )])
);
// Red: a local declared inside a lambda body may not shadow the lambda's own
// parameter (§6.4).

snapshot!(
    nested_lambda_param_shadows_outer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

import java.util.function.Consumer;

class Shadow {
    void h(Consumer<String> c) {}
    void sink(String s) {}

    void m() {
        h(i -> h(i -> {
            sink(i);
        }));
    }
}
",
    )])
);
// Red: a nested lambda's parameter may not shadow an enclosing lambda's
// parameter (§6.4).

snapshot!(
    nested_lambda_distinct_params,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Shadow.java",
        "\
package com.example;

import java.util.function.Consumer;

class Shadow {
    void h(Consumer<String> c) {}
    void sink(String s) {}

    void m() {
        h(i -> h(j -> {
            sink(i + j);
        }));
    }
}
",
    )])
);
// Green: nested lambda parameters with distinct names, the inner referencing
// both its own and the outer's parameter — no capture, no duplicate (§6.3,
// §6.4).
