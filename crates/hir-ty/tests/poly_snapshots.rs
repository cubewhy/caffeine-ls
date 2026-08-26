//! Snapshots of poly-expression typing: lambdas
//! ([JLS §15.27](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27))
//! and method references
//! ([JLS §15.13](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13))
//! against target functional interfaces ([JLS §9.8]).
//!
//! A lambda or method reference is a poly expression ([JLS §15.2]): it has no
//! standalone type, and its type is the target functional interface of its
//! context ([JLS §15.27.3], [JLS §18.5.2.4]) — a declaration initializer
//! ([§14.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.4)),
//! an assignment ([§15.26](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.26)),
//! a return ([§14.17](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.17)),
//! or a method invocation argument — where it is typed from the single
//! abstract method of the functional interface. The lambda's parameters are
//! typed from the SAM's parameters, and its body is inferred against the SAM's
//! return type ([JLS §15.27.3]).

#[macro_use]
mod common;

use crate::common::check_body_types;

snapshot!(
    lambda_initializer,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;
import java.util.function.Predicate;
import java.util.function.Supplier;

class Poly {
    void lambdas() {
        Function<String, Integer> f = s -> s.length();
        Predicate<String> p = s -> s.length() > 0;
        Supplier<Long> getter = () -> 42L;
    }
}
",
    )])
);

snapshot!(
    lambda_assignment,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;

class Poly {
    void assign(Function<String, Integer> f) {
        f = s -> s.length();
        f = s -> 0;
    }
}
",
    )])
);

snapshot!(
    lambda_return,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;

class Poly {
    Function<String, Integer> fun() {
        return s -> s.length();
    }
}
",
    )])
);

snapshot!(
    lambda_block_body,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.lang.Runnable;

class Poly {
    void block() {
        Runnable r = () -> {
            String s = \"hi\";
        };
    }
}
",
    )])
);

snapshot!(
    lambda_declared_param_type,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;

class Poly {
    void declared(Function<String, Integer> f) {
        f = (String s) -> s.length();
    }
}
",
    )])
);

snapshot!(
    method_ref_static,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;

class Poly {
    void refs() {
        Function<String, Integer> f = String::length;
    }
}
",
    )])
);

snapshot!(
    method_ref_constructor,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Supplier;

class Box {
    Box() {}
}

class Poly {
    void ctorRef() {
        Supplier<Box> s = Box::new;
    }
}
",
    )])
);

snapshot!(
    conditional_and_paren,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;

class Poly {
    void cond(boolean flag) {
        Function<String, Integer> f = flag ? (s -> s.length()) : (s -> 0);
        Function<String, Integer> g = (s -> s.length());
    }
}
",
    )])
);

snapshot!(
    lambda_argument,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;

class Poly {
    void consume(Function<String, Integer> f) {}

    void call() {
        consume(s -> s.length());
        consume((String s) -> s.length());
    }
}
",
    )])
);

snapshot!(
    overloaded_poly_argument,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;
import java.lang.Runnable;

class Poly {
    void consume(Function<String, Integer> f) {}
    void consume(Runnable r) {}

    void call() {
        consume(s -> s.length());
        consume(() -> { int x = 1; x = x + 1; });
    }
}
",
    )])
);

snapshot!(
    cast_lambda,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;
import java.lang.Runnable;

class Poly {
    void casts() {
        Runnable r = (Runnable) () -> {};
        Function<String, Integer> f = (Function<String, Integer>) (s -> s.length());
        Function<String, Integer> g = (Function<String, Integer>) s -> s.length();
    }
}
",
    )])
);

snapshot!(
    cast_method_ref,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;
import java.util.function.Supplier;

class Box {
    Box() {}
}

class Poly {
    void refs() {
        Function<String, Integer> f = (Function<String, Integer>) String::length;
        Supplier<Box> s = (Supplier<Box>) Box::new;
    }
}
",
    )])
);

snapshot!(
    isolation_error,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.function.Function;

class Poly {
    void isolation(boolean flag) {
        Object o = s -> s;
        Object r = String::length;
        Object c = flag ? (s -> s.length()) : (s -> 0);
    }
}
",
    )])
);
// A lambda or method reference has no standalone type ([JLS §15.2]): without
// a functional-interface target it infers to an error. A conditional whose
// arms are lambdas is a poly expression only when the target is a functional
// interface ([§15.25.2]); against the non-functional `Object` target both
// arms and the conditional infer to an error.

// -- green: a generic method whose type variable is bounded by an interface ----
// §18.4: with lower bounds `Bowl` and `Eater` (an interface `Bowl` implements),
// the invocation variable instantiates to their least upper bound `Eater`; the
// lambda body then constrains the downstream element type.

snapshot!(
    generic_invocation_interface_lub,
    check_body_types(&[(
        "/src/com/example/Feed.java",
        "\
package com.example;

import java.util.List;

class Feed {
    interface Eater { }

    static class Bowl implements Eater { }

    static <T> List<T> join(List<? extends T> a, List<? extends T> b) {
        return null;
    }

    void feed(List<Bowl> bowls, List<Eater> eaters) {
        join(bowls, eaters);
    }
}
",
    ),])
);

// -- green: a zero-argument generic method as a nested poly argument ------------
// §18.5.2.4: `Collectors.toList()`'s own type variable is inferred from the
// enclosing invocation's formal (`collect`), jointly with the outer target.
// The standalone re-inference must not apply the enclosing context's target to
// it, and its resolution must succeed so `collect` stays applicable.

snapshot!(
    collect_to_list_target_inference,
    check_body_types(&[(
        "/src/com/example/Poly.java",
        "\
package com.example;

import java.util.List;
import java.util.stream.Collectors;
import java.util.stream.Stream;

class Poly {
    List<String> go(Stream<String> s) {
        return s.collect(Collectors.toList());
    }
}
",
    ),])
);

// -- Optional.map/orElse chains with a boolean-producing lambda -------------------
// `Optional<T>.map(v -> cond)` yields `Optional<Boolean>`; the subsequent
// `orElse(Boolean.FALSE)` unboxes to `boolean`. Inference must solve the
// wildcard SAM formal `Function<? super T, ? extends U>` so that `U` binds to
// `Boolean`, not `Object` ([JLS §18.5.2] with §5.1.10 capture).

snapshot!(
    optional_map_or_else_boolean_chain,
    check_body_types(&[(
        "/src/com/example/Repro.java",
        "\
import java.util.Optional;

class Repro {
    boolean m(Optional<String> opt, String s) {
        return opt.map(v -> v == s).orElse(Boolean.FALSE);
    }
}
",
    )])
);

// -- §15.12.2.5: only m2's genericity gates specificity --------------------------
// A generic `<T> Subject that(T[])` is more specific than a non-generic
// `that(Object)` for an array argument (`T[] <: Object`), even though the
// reverse direction holds through §18.5.4 — javac resolves Truth's builder
// chains to the array overload, whose subject carries `.asList()`/`.hasLength()`.

snapshot!(
    builder_array_overload_most_specific,
    check_body_types(&[(
        "/src/com/example/Truth.java",
        "\
package com.example;

class Subject {
    static class Builder {
        <C extends java.lang.Comparable<C>> ComparableSubject<C> that(C c) {
            return null;
        }

        Subject that(Object o) {
            return null;
        }

        <T> ObjectArraySubject<T> that(T[] a) {
            return null;
        }

        PrimitiveIntArraySubject that(int[] a) {
            return null;
        }

        IterableSubject that(java.lang.Iterable<?> a) {
            return null;
        }
    }

    static class ComparableSubject<C> extends Subject {
    }

    static class ObjectArraySubject<T> extends Subject {
        ObjectArraySubject<T> asList() {
            return null;
        }

        ObjectArraySubject<T> hasLength(int n) {
            return null;
        }
    }

    static class PrimitiveIntArraySubject extends Subject {
        PrimitiveIntArraySubject hasLength(int n) {
            return null;
        }
    }

    static class IterableSubject extends Subject {
        IterableSubject asList() {
            return null;
        }
    }
}

class Bean {
    String[] strings = {\"a\"};
    int[] ints = {1};
    java.util.List<String> list = null;
    Object any = null;
}

class Use {
    void chains(Bean bean, Subject.Builder b) {
        b.that(bean.strings).asList().hasLength(1);
        b.that(bean.ints).hasLength(2);
        b.that(bean.list).asList();
        b.that(bean.any);
    }
}
",
    )])
);

// -- §15.27.3: a block body without valued returns only targets void -------------
// `assertDoesNotThrow(exe, msg)` overloads on `(Executable, String)` and
// `(ThrowingSupplier<T>, String)`. The void block lambda is congruent with
// `Executable.accept()` alone; without the value-compatibility applicability
// check it would also stay applicable to `ThrowingSupplier.get()` and leave
// the two overloads ambiguous.

snapshot!(
    void_block_lambda_executable_only,
    check_body_types(&[(
        "/src/com/example/Assert.java",
        "\
package com.example;

interface Executable {
    void accept();
}

interface ThrowingSupplier<T> {
    T get();
}

class Assert {
    static void doesNotThrow(Executable exe, String message) {
    }

    static <T> T doesNotThrow(ThrowingSupplier<T> supplier, String message) {
        return null;
    }
}

class Use {
    void check(String name) {
        Assert.doesNotThrow(() -> {
            int length = name.length();
        }, \"should not throw\");
    }
}
",
    )])
);
// Resolves to the void `Executable` overload; a value-returning target such
// as `ThrowingSupplier` is not applicable to the block body.
