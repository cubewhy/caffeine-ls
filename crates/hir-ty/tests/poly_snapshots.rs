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
