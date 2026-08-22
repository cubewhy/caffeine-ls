//! JLS SE 26 scenario snapshots for conversions and contexts
//! ([JLS §5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html)):
//! widening and narrowing primitive conversion ([§5.1.2], [§5.1.3]), boxing
//! and unboxing ([§5.1.7], [§5.1.8]) and casting contexts ([§5.5]). Red cases
//! render the diagnostics the type layer must report; green cases confirm
//! legal conversions apply.
//!
//! The trailing *known divergence* section pins behaviour that still
//! contradicts the spec or `javac` 25 — constant narrowing of `int` literals
//! in assignment context ([§5.2], accepted by `javac`, reported as an error)
//! and inconvertible-reference-cast detection ([§5.5], rejected by `javac`,
//! not diagnosed). Their snapshots are kept pending (`.snap.new`) until the
//! divergences are fixed; a fix must flip them.

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

// -- known divergence: constant narrowing ([§5.2]) ------------------------------
// `javac` 25 accepts every assignment below (an `int` constant narrows in
// assignment context); the type layer wrongly reports `incompatible-types`.

snapshot!(
    divergence_constant_narrowing,
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

// -- known divergence: inconvertible cast ([§5.5]) -------------------------------
// `String` and `Integer` are unrelated final classes, so `javac` 25 rejects
// this cast; the type layer stays silent.

snapshot!(
    divergence_inconvertible_cast,
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
