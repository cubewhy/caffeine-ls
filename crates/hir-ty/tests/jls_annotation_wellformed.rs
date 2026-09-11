//! Conformance snapshots for the two annotations whose *well-formedness* the
//! language fixes beyond their `@Target` applicability — `@SafeVarargs`
//! ([JLS §9.6.4.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.7))
//! and `@FunctionalInterface`
//! ([§9.6.4.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.9))
//! — and the type-argument *arity* of a parameterized type
//! ([§4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5)).
//!
//! Every scenario is verified against `javac` before the snapshot is
//! accepted.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// JLS §9.6.4.7: `@SafeVarargs` is legal on a variable-arity method that is
// `static`, `final` or `private` — the declaration cannot be overridden with a
// different arity, so the annotation's promise holds. javac accepts all four.
snapshot!(
    safe_varargs_legal_declarations,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    @SafeVarargs
    static <T> void a(T... xs) {
    }

    @SafeVarargs
    final <T> void b(T... xs) {
    }

    @SafeVarargs
    private <T> void c(T... xs) {
    }

    @SafeVarargs
    Body(String... xs) {
    }
}
",
    )])
);

// JLS §9.6.4.7: an overridable instance method cannot carry it. javac:
// `Invalid SafeVarargs annotation. Instance method b(T...) is not final`.
snapshot!(
    safe_varargs_instance_method,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    @SafeVarargs
    <T> void b(T... xs) {
    }
}
",
    )])
);

// JLS §9.6.4.7: a non-variable-arity method cannot carry it. javac: `Invalid
// SafeVarargs annotation. Method m(T) is not a varargs method`.
snapshot!(
    safe_varargs_non_varargs,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    @SafeVarargs
    static <T> void m(T xs) {
    }
}
",
    )])
);

// JLS §9.6.4.9: a functional interface declares exactly one abstract method.
// javac accepts this one.
snapshot!(
    functional_interface_legal,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    @FunctionalInterface
    interface F {
        void apply();
    }
}
",
    )])
);

// JLS §9.8: the abstract-method count excludes `default` and `static`
// members, and the public methods of `Object`, which a functional interface
// may freely redeclare. javac accepts all three.
snapshot!(
    functional_interface_object_methods,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    @FunctionalInterface
    interface F {
        void apply();

        default void extra() {
        }

        static void helper() {
        }

        boolean equals(Object other);

        int hashCode();

        String toString();
    }
}
",
    )])
);

// JLS §9.6.4.9: two abstract methods — javac: `Unexpected
// @FunctionalInterface annotation`.
snapshot!(
    functional_interface_two_abstract,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    @FunctionalInterface
    interface F {
        void apply();

        void other();
    }
}
",
    )])
);

// JLS §9.6.4.9: a functional interface may *inherit* its single abstract
// method — the count is over the interface's members, not its declaration.
// javac accepts this.
snapshot!(
    functional_interface_inherited_sam,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    interface Base {
        void apply();
    }

    @FunctionalInterface
    interface F extends Base {
    }
}
",
    )])
);

// JLS §9.6.4.9: an inherited abstract method and a declared one are two
// abstract members. javac: `Unexpected @FunctionalInterface annotation`.
snapshot!(
    functional_interface_inherited_plus_declared,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    interface Base {
        void apply();
    }

    @FunctionalInterface
    interface F extends Base {
        void other();
    }
}
",
    )])
);

// JLS §9.6.4.9: `@FunctionalInterface` on a class. javac: `Unexpected
// @FunctionalInterface annotation`.
snapshot!(
    functional_interface_on_class,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    @FunctionalInterface
    static class NotAnInterface {
    }
}
",
    )])
);

// JLS §4.5: a parameterized type must carry exactly the type arguments its
// class declares. javac: `wrong number of type arguments; required 1`.
snapshot!(
    type_argument_arity_too_many,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    List<String, Integer> broken;
}
",
    )])
);

// JLS §4.8: a *raw* use — no type arguments at all — is legal for any generic
// class, so a declaration naming `List` bare is not an arity error (it is the
// `rawtypes` warning instead, which the lint layer reports).
snapshot!(
    raw_type_is_not_an_arity_error,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    List bare;
}
",
    )])
);

// JLS §4.5 recurses: a *nested* argument is a type reference in its own right,
// so `Map<String>` inside `List` is wrong at the argument even though the
// outer `List` is well-formed. javac: `wrong number of type arguments;
// required 2`.
snapshot!(
    nested_type_argument_arity,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;
import java.util.Map;

class Body {
    List<Map<String>> broken;
}
",
    )])
);

// JLS §4.5.1: a wildcard's bound is a type reference too, so its arity is
// checked the same way.
snapshot!(
    wildcard_bound_arity,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;
import java.util.Map;

class Body {
    List<? extends Map<String>> broken;
}
",
    )])
);

// JLS §6.5.5.2: a *qualified member type* writes its type arguments on the
// qualifier — `Outer<T>.Inner` instantiates `Outer`, and `Inner` takes none of
// its own — so the use is legal even though `Inner` is not generic. javac
// accepts it.
snapshot!(
    qualified_member_type_is_legal,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Map;

class Outer<T> {
    class Inner {
    }

    Map<String, Outer<T>.Inner> members = null;
}
",
    )])
);
