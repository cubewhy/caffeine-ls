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

// §4.12.4: a blank local assigned once on a guarded path is effectively final —
// the `(runnable = ...) != null` condition assigns it for the first time, and
// the capturing lambda runs only after that.

snapshot!(
    lambda_capture_guarded_first_assignment,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Capture.java",
        "\
package com.example;

class Capture {
    static class Row {
        Runnable removeCallback;
    }

    void m(Row row, boolean content) {
        Runnable runnable;
        if (content && (runnable = row.removeCallback) != null) {
            new Thread(() -> System.out.println(runnable)).start();
        }
    }
}
",
    )])
);
// Green: `runnable` is blank-declared and assigned exactly once (its initial
// assignment) inside the condition — javac treats it as effectively final.

// §4.12.4: a blank local assigned once in each `if`/`else` branch — one
// assignment per path — stays effectively final.

snapshot!(
    lambda_capture_if_else_initial_assignment,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Capture.java",
        "\
package com.example;

class Capture {
    void m(boolean b) {
        int x;
        if (b) {
            x = 1;
        } else {
            x = 2;
        }
        Runnable r = () -> System.out.println(x);
    }
}
",
    )])
);
// Green: `x` is assigned once on every path reaching the lambda.

// §6.4/[§15.27.2]: same-named locals in sibling scopes must not collide — one
// local's assignment must not flag a distinct captured local of the same name.

snapshot!(
    lambda_capture_sibling_scope_same_name,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Capture.java",
        "\
package com.example;

class Capture {
    void m(java.util.List<Object> list) {
        for (Object o : list) {
            int n = 1;
            Runnable r = () -> System.out.println(n);
        }
        for (Object o : list) {
            int n = 1;
            n = 2;
            System.out.println(n);
        }
    }
}
",
    )])
);
// Green: the `n` captured in the first loop is never reassigned; the `n`
// reassigned in the second loop is a distinct local and must not flag the
// first loop's capture.

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

// -- §15.26/[§15.11.1]: a qualified field write does not assign the receiver --
// `final Holder h; h.value = 1` writes the *field*, not the local `h`; a
// `final` array reference and index are likewise only read by `a[i] = v`.

snapshot!(
    field_write_through_final_receiver,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static class Holder {
        int value;
        int[] data;
    }
    void m(final Holder h, final int[] arr, final int idx) {
        h.value = 5;
        h.data[0] = 6;
        arr[idx] = 7;
    }
}
",
    )])
);
// Green: writing a field through a `final` receiver reads only the receiver,
// never assigns it — no cannot-assign-to-final-variable.

// -- §8.3.1.2/[§16]: a blank static final field is assigned once in a static
// initializer or a static field initializer — the legal initialization, not a
// cannot-assign error.

snapshot!(
    blank_static_final_in_static_initializer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
    }
}
",
    )])
);
// Green: the static initializer's write is the legal one-time assignment.

snapshot!(
    blank_static_final_in_static_field_initializer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static int copy = sf = 1;
}
",
    )])
);
// Green: a static field initializer assigning the blank final is legal.

// -- §8.3.1.2/[§16]: a blank final instance field is assigned once in a
// constructor or an instance initializer (via a bare simple name or a bare
// `this.field`) — the legal initialization.

snapshot!(
    blank_instance_final_in_constructor,
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
// Green: `f` is assigned in the primary constructor; the delegating
// constructor defers to it.

snapshot!(
    blank_instance_final_in_instance_initializer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    {
        f = 1;
    }
    Final() {
    }
}
",
    )])
);
// Green: the instance initializer's write is the legal one-time assignment.

snapshot!(
    blank_instance_final_qualified_this_in_constructor,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        this.f = 1;
    }
}
",
    )])
);
// Green: the qualified `this.f = 1` is the bare-receiver one-time assignment.

// -- §8.3.1.2/[§16]: a *second* write to a blank final field is the
// already-assigned error — javac: `variable {f} might already have been
// assigned` — reported at the second write's target. Across sibling bodies
// that run before it (another static initializer, a later instance
// initializer, a constructor after the instance initializers) and within one
// body after a branch both of whose paths assigned the field.

snapshot!(
    blank_static_final_double_assignment_same_block,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
        sf = 2;
    }
}
",
    )])
);
// Red: the second write in the same static initializer.

snapshot!(
    blank_static_final_double_assignment_sibling_blocks,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
    }
    static {
        sf = 2;
    }
}
",
    )])
);
// Red: a later sibling static initializer's write.

snapshot!(
    blank_static_final_double_assignment_field_init_then_block,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static int copy = sf = 1;
    static {
        sf = 2;
    }
}
",
    )])
);
// Red: a static field initializer assigns first, a later static initializer
// reassigns.

snapshot!(
    blank_instance_final_ctor_after_instance_initializer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    {
        f = 1;
    }
    Final() {
        f = 2;
    }
}
",
    )])
);
// Red: an instance initializer assigns first, the constructor reassigns.

snapshot!(
    blank_instance_final_double_assignment_two_initializers,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    {
        f = 1;
    }
    {
        f = 2;
    }
    Final() {
    }
}
",
    )])
);
// Red: a later instance initializer's write.

snapshot!(
    blank_final_after_both_branches_assigned,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        if (\"x\".length() > 0) {
            sf = 1;
        } else {
            sf = 2;
        }
        sf = 3;
    }
}
",
    )])
);
// Red: both `if` branches assign, so the trailing write is a second
// assignment on every path.

// -- §8.3.1.2/[§16]: the *legal* one-time assignments that must not be
// flagged: a blank final assigned on both branches of an `if`/`else`, and a
// write after an `if` whose then-arm exits.

snapshot!(
    blank_final_assigned_on_both_branches,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        if (\"x\".length() > 0) {
            sf = 1;
        } else {
            sf = 2;
        }
    }
}
",
    )])
);
// Green: each path assigns exactly once.

snapshot!(
    blank_final_after_exiting_if_branch,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        if (\"x\".length() > 0) {
            sf = 1;
            throw new RuntimeException();
        }
        sf = 2;
    }
}
",
    )])
);
// Green: the then-arm exits, so only the fall-through path reaches the write.

// -- §8.3.1.2/[§16]: a blank final field written from a *method*, an instance
// context writing a static final, a static context writing an instance final,
// a qualified `Type.field`/`Type.this.field` write, and a non-blank final
// reassignment are all errors — the cannot-assign or non-static errors.

snapshot!(
    blank_final_after_this_delegation,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final(int x) {
        f = 1;
    }
    Final() {
        this(1);
        f = 2;
    }
}
",
    )])
);
// Red: the delegating constructor's write is a second assignment (the
// delegated constructor already assigned `f`).

snapshot!(
    blank_final_this_delegation_no_write,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final(int x) {
        f = 1;
    }
    Final() {
        this(1);
    }
}
",
    )])
);
// Green: the delegating constructor writes nothing — the delegated
// constructor's assignment covers it.

snapshot!(
    blank_static_final_written_from_static_method,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
    }
    static void m() {
        sf = 2;
    }
}
",
    )])
);
// Red: a method body is never a legal blank-final assignment point.

snapshot!(
    blank_instance_final_written_from_instance_method,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        f = 1;
    }
    void m() {
        f = 2;
    }
}
",
    )])
);
// Red: the instance method's write is a second assignment.

snapshot!(
    static_final_written_from_instance_context,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
    }
    Final() {
        sf = 2;
    }
}
",
    )])
);
// Red: a static final written from a constructor (instance context) is
// cannot-assign.

snapshot!(
    instance_final_written_from_static_context,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        f = 1;
    }
    static {
        f = 2;
    }
}
",
    )])
);
// Red: an instance final written from a static initializer is both a
// non-static-from-static-context error and a cannot-assign error.

snapshot!(
    blank_final_qualified_type_write,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        Final.sf = 1;
    }
}
",
    )])
);
// Red: the qualified `Final.sf = 1` write is not the bare one-time
// assignment — reported as cannot-assign.

snapshot!(
    blank_final_qualified_this_write,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        Final.this.f = 1;
    }
}
",
    )])
);
// Red: the *qualified* `Final.this.f = 1` write is not the bare
// `this.f = 1` assignment form.

snapshot!(
    initialized_final_field_reassignment,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f = 1;
    static final int sf = 2;
    void m() {
        f = 3;
    }
    static void sm() {
        sf = 4;
    }
}
",
    )])
);
// Red: a `final` field with an initializer is never blank, so both methods'
// writes are cannot-assign.
