//! JLS SE 26 scenario snapshots for local/field declarations
//! ([JLS §14.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.4),
//! [§8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3)),
//! class instance creation escalation
//! ([§15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9))
//! and array creation
//! ([§15.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.10)).
//! The red cases are the ones `javac` reports: `var` without an initializer,
//! a `var` with an array initializer, an initializer not assignable to the
//! declared type, instantiation of an interface/abstract class/type variable,
//! and generic (non-reifiable) array creation.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: declarations ---------------------------------------------------

snapshot!(
    declaration_forms,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    int count = 10;
    static String greet = \"hi\";

    int m() {
        int a = 1, b = 2;
        long l = 4L + a;
        double d = 1.5 + b;
        boolean ok = true;
        String s = \"x\" + a;
        Object o = null;
        List<String> xs = java.util.Collections.emptyList();
        int[] arr = new int[3];
        int[] nested = new int[][] { { 1 }, { 2, 3 } }[0];
        Integer boxed = a;
        Long wide = (long) a;
        long widened = boxed + 1;
        long unboxed = wide;
        char c = 'x';
        return a + b + (int) l + (int) d + (ok ? 1 : 0) + s.length() + xs.size() + arr.length + nested.length + (int) widened + (int) unboxed + c;
    }
}
",
    )])
);

snapshot!(
    instantiation_forms,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    List<String> a = new java.util.ArrayList<>();
    List<String> b = new java.util.ArrayList<String>();
    Object c = new Object();
    Runnable r = new Runnable() { public void run() {} };

    int m() {
        Body n = new Body();
        java.lang.String s = new java.lang.String(\"x\");
        return n.m2() + s.length();
    }

    int m2() { return 1; }
}
",
    )])
);

// -- red: `var` rules -------------------------------------------------------

snapshot!(
    var_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m() {
        var missing;
        var arrayInit = { 1, 2, 3 };
    }
}
",
    )])
);

// -- red: assignability -----------------------------------------------------

snapshot!(
    assignability_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(String s, long l, int i, Object o) {
        int bad = \"not an int\";
        int narrowed = l;
        i = s;
        int[] arr = o;
        Body b = i;
    }
}
",
    )])
);

// -- red: generic array / non-instantiable ----------------------------------

snapshot!(
    instantiation_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

interface Greeter {
    void greet();
}

abstract class Base {
}

class Body {
    void m() {
        java.util.List<String>[] arr = new java.util.List<String>[3];
        Greeter g = new Greeter();
        Base b = new Base();
    }

    <T> T make() {
        return new T();
    }
}
",
    )])
);

// -- §3.9: restricted identifiers are ordinary method/field names -------------
// `record`, `sealed` and `permits` cannot name a *type*, but a method or field
// of those names is perfectly legal ([JLS §3.9]).

snapshot!(
    restricted_identifier_method_names,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    private void record(int x, int y) {}
    private void sealed() {}
    private void permits() {}
    private int record;
    void m(int a) {
        record(a, a);
        sealed();
        permits();
    }
}
",
    )])
);
// Green: restricted-identifier method and field names type and resolve without
// diagnostics.
