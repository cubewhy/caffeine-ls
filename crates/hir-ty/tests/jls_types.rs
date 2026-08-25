//! JLS SE 26 scenario snapshots for types, values and variables
//! ([JLS §4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html)):
//! primitive and reference types ([§4.2], [§4.3]), the null type ([§4.1]) and
//! the strict distinctness of `boolean` from the integral types ([§4.2]).
//! Red cases render the diagnostics the type layer must report; green cases
//! confirm the declared forms resolve to the expected types.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: field types across every primitive and the null type --------------

snapshot!(
    primitive_and_null_fields,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    boolean flag = false;
    byte small = 100;
    short mid = 300;
    char letter = 'A';
    int count = 42;
    long big = 42L;
    float approx = 1.5f;
    double precise = 2.0;
    Object any = null;
    String text = null;
}
",
    )])
);

// -- green: char promotes to int in arithmetic --------------------------------

snapshot!(
    char_promotes_to_int,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(char c, int i) {
        return c + i;
    }
}
",
    )])
);

// -- red: boolean is distinct from the integral types --------------------------

snapshot!(
    boolean_primitive_distinctness,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        int a = true;
        boolean b = 0;
    }
}
",
    )])
);

// -- red: the null type is assignable only to reference types ------------------

snapshot!(
    null_to_primitive,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        int n = null;
        Object ok = null;
    }
}
",
    )])
);

// -- green: array builtin members ([§10.7]) -------------------------------------
// Every array type has a public final field `length` and a public method
// `clone` returning the array type itself, overriding `Object.clone`.

snapshot!(
    array_builtin_members,
    check_body_types(&[(
        "/src/com/example/Arrays.java",
        "\
package com.example;

class Arrays {
    int[] copy(int[] a) {
        return a.clone();
    }

    int size(String[] names) {
        return names.length;
    }
}
",
    )])
);
