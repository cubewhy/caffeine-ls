//! JLS SE 26 scenario snapshots for conversions and contexts
//! ([JLS §5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html)):
//! widening and narrowing primitive conversion ([§5.1.2], [§5.1.3]), boxing
//! and unboxing ([§5.1.7], [§5.1.8]) and casting contexts ([§5.5]). Red cases
//! render the diagnostics the type layer must report; green cases confirm
//! legal conversions apply, including the narrowing of `int` constants in
//! assignment context ([§5.2], [§5.1.3]) and the rejection of casts between
//! provably distinct class types ([§5.5.1], [§5.1.6.3]).

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: widening primitive conversion in assignment context ----------------

snapshot!(
    widening_matrix,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(byte b, short s, char c, int i, long l, float f) {
        long l2 = i;
        float f2 = l;
        double d1 = f;
        double d2 = c;
        int back = b + s;
    }
}
",
    )])
);

// -- red: lossy conversions rejected in assignment context ---------------------

snapshot!(
    lossy_assignments,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        byte b = 200;
        char c = -1;
        long l = 3.14f;
    }
}
",
    )])
);

// -- green: boxing and unboxing in assignment context ([§5.2]) -----------------

snapshot!(
    boxing_unboxing_contexts,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        Integer a = 5;
        int un = a;
        Number n = 8;
        Object o = true;
    }
}
",
    )])
);

// -- red: boxing requires a matching primitive ----------------------------------

snapshot!(
    boxing_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(Object o) {
        Integer bad1 = \"str\";
        int bad2 = o;
        Boolean bad3 = 1;
    }
}
",
    )])
);

// -- green: downcasts in casting context ([§5.5]) -------------------------------

snapshot!(
    reference_casting,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(Object o, Number n) {
        String s = (String) o;
        Integer i = (Integer) n;
    }
}
",
    )])
);

// -- green: narrowing of int constants in assignment context ([§5.2]) ----------
// An int-typed constant expression ([§4.12.4], [§15.28]) narrows to `byte`,
// `short` or `char` when its value is representable in the target
// ([§5.1.3]); `javac` 25 accepts every assignment below.

snapshot!(
    constant_narrowing,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        byte b = 100;
        short s = 300;
        char c = 65;
        byte sum = 50 + 50;
    }
}
",
    )])
);

// -- §5.1.3/§4.12.4/§15.28: narrowing through constant variables ------------------
// A `final` local initialized with a constant expression is itself a constant
// variable ([§4.12.4]): the assignment context narrowing of an `int` constant
// ([§5.2], [§5.1.3]) applies to `final int N` reads and `final char` reads
// widen normally — but a value outside the target's range stays an error.
// `javac` 25 accepts the green forms and rejects the lossy one.

snapshot!(
    constant_variable_narrowing,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        final int N = 100;
        byte b = N;
        final int Z = 200;
        char c = Z;
        final char L = 'a';
        int i = L;
        final int BIG = 300;
        byte bad = BIG;
    }
}
",
    )])
);

// -- red: a cast between provably distinct class types ([§5.5.1]) ----------------
// `String` and `Integer` are unrelated final classes, so no common subclass
// can exist and the cast is inconvertible; `javac` 25 rejects it the same way.

snapshot!(
    inconvertible_cast,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        Integer bad = (Integer) \"lit\";
    }
}
",
    )])
);

// -- green: boxing and unboxing casts in casting context ([§15.16], [§5.5]) ------
// A cast performs one of the conversions of [§5.5]: boxing a primitive
// ([§5.1.7]), unboxing a wrapper ([§5.1.8]) and the combined boxing-then-
// widening form (`(Number) i`). `javac` 25 accepts every cast below.

snapshot!(
    cast_context_boxing,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(int i, Integer box) {
        Integer boxed = (Integer) i;
        int unboxed = (int) box;
        Number widened = (Number) i;
        Object up = (Object) i;
    }
}
",
    )])
);

// -- §15.25: conditional expression typing ----------------------------------------
// The numeric promotion of a conditional applies only when at least one
// operand is primitive ([§15.25]): `int ? : Integer` unboxes and promotes,
// two boxed numerics take the least upper bound (`Number`), a
// `boolean`/`Boolean` mix is `boolean`, and `boolean` against an unrelated
// primitive is ill-typed.

snapshot!(
    conditional_numeric_rules,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(java.lang.Integer i, java.lang.Long l) {
        int p = true ? 1 : i;
        java.lang.Number lub = true ? i : l;
        boolean b1 = true ? false : java.lang.Boolean.TRUE;
        int bad = true ? true : 1;
    }
}
",
    )])
);

// -- §4.10.2/§5.2: type variables convert through their bounds --------------------
// An expression typed by a type variable is assignable to its declared upper
// bound (and transitively beyond it), but an unrelated reference does not
// convert *into* the variable's slot: javac rejects `T t = ...` from a plain
// `Number` even when `T extends Number`.

snapshot!(
    type_var_bound_conversion,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body<T extends java.lang.Number> {
    void m(T value, java.lang.Number num) {
        java.lang.Number n = value;
        Object o = value;
        value = num;
    }
}
",
    )])
);

// -- §5.1.10 with §5.2: captured values convert through their capture bounds -----
// Reading from a wildcard-parameterized structure yields a captured type
// variable: it converts to its upper bound (`? extends` captures) but not to
// an unrelated type.

snapshot!(
    capture_assignment_context,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(java.util.List<? extends java.lang.Number> up,
           java.util.List<?> any) {
        java.lang.Number n = up.get(0);
        Object o = any.get(0);
        String bad = up.get(0);
    }
}
",
    )])
);

// -- error degradation stops cascades ---------------------------------------------
// A subexpression that already failed to type (unresolved name or method)
// reports only its own error: the surrounding condition and throw checks must
// not pile on `<error> cannot be converted to boolean/Throwable`.

snapshot!(
    no_error_cascades,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        if (missing) { }
        throw other;
    }
}
",
    )])
);
