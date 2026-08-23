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
