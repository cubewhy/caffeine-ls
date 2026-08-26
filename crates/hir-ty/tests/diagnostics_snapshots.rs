//! Snapshots of the type-layer diagnostics surfaced from method-body
//! inference ([`hir_ty::body_types`]). Each diagnostic carries a typed code
//! ([`JavaDiagnosticCode`]), a source range and a message rendered against
//! the body IR ([JLS §14.4], [§14.18], [§15.11], [§15.12]).

#[macro_use]
mod common;

use crate::common::check_body_types;

snapshot!(
    resolve_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(String s, Body b) {
        undefinedName;
        b.missing;
        b.missing();
        s.length(1, 2);
        throw new Object();
    }
}
",
    )])
);
// Each `§` section of the quick wins: a bare name with no local, field, static
// import or implicit-receiver field is a resolution error (§6.5); a field
// access and a method call through a receiver with no such member report
// §15.11/§15.12.1; a call whose members all have the wrong arity reports
// §15.12.2; and a `throw` of a non-`Throwable` reports §14.18.

snapshot!(
    static_context_instance_method,
    check_body_types(&[(
        "/src/Main.java",
        "\
import java.util.ArrayList;
import java.util.List;

public class Main {
    public static void main(String[] args) {
        printStrings(new ArrayList<>());
    }

    private void printStrings(List<String> a) {}
}
",
    )])
);
// JLS §15.12.3: the invocation has the simple `MethodName` form and the chosen
// method is an instance method, so it is a compile-time error from the static
// context ([§8.1.3]) of `main`'s body — javac 25 reports "non-static method
// printStrings(List<String>) cannot be referenced from a static context".

snapshot!(
    static_context_initializers,
    check_body_types(&[(
        "/src/Main.java",
        "\
class Main {
    static String s = helper();

    static {
        helper();
    }

    {
        helper();
    }

    String helper() {
        return null;
    }
}
",
    )])
);
// §8.1.3: a static field initializer and a static initializer are static
// contexts; an instance initializer is not. `helper()` resolves in the
// instance initializer but is rejected from the static ones (§15.12.3).

snapshot!(
    static_context_mixed_overloads,
    check_body_types(&[(
        "/src/Main.java",
        "\
class Main {
    static void m(Object o) {}
    void m(String s) {}

    static void test() {
        m(\"x\");
    }
}
",
    )])
);
// §15.12.3 selects the most specific applicable method from the *full* member
// set before rejecting from a static context: `m(String)` (instance) beats
// `m(Object)` (static) for a `String` actual, so the invocation is an error
// rather than silently resolving to the static overload.

snapshot!(
    static_context_no_error,
    check_body_types(&[(
        "/src/Main.java",
        "\
class Main {
    static void s() {}
    void i() {}

    static void test() {
        s();
        new Main().i();
    }

    void use() {
        i();
        s();
    }
}
",
    )])
);
// Static contexts allow unqualified invocations of *static* methods and of
// instance methods through an explicit receiver (a virtual invocation,
// §15.12.3); a non-static context allows unqualified instance invocations.

// -- §15.8.2: array class literals ---------------------------------------------
// `int[].class`, `String[].class` and `int[][].class` lower through a `TYPE`
// node whose brackets are bare tokens (no `DIMENSIONS` wrapper), so the type
// must fold them into `TypeRef::Array`; dropping them produced
// `Class<<error>>` and rejected every generic call taking the literal as a
// `Class<T>` argument ([JLS §15.8.2], [§10.7]).

snapshot!(
    array_class_literals,
    check_body_types(&[(
        "/src/com/example/Lits.java",
        "\
package com.example;

class Lits {
    static <T> T pick(Class<T> c) {
        return null;
    }

    void literals() {
        Class<int[]> i = int[].class;
        Class<String[]> s = String[].class;
        Class<int[][]> ii = int[][].class;
        int[] a = pick(int[].class);
        String[] b = pick(String[].class);
    }
}
",
    )])
);
