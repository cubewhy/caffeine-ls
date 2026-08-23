//! JLS SE 26 scenario snapshots for raw types and unchecked conversions
//! ([JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8),
//! [§4.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.2),
//! [§5.1.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.9)):
//! a generic class used without type arguments is a *raw type*, and a raw
//! value converts to any parameterization of its class by *unchecked
//! conversion* ([§5.2]) — both legal, both reported as warnings, unlike the
//! compile-time errors the rest of the type layer reports.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: parameterized declarations convert without warnings -------------------

snapshot!(
    parameterized_declarations,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;
import java.util.ArrayList;

class Raw {
    void m(List<String> xs) {
        List<String> copy = new ArrayList<String>(xs);
        String first = copy.get(0);
    }
}
",
    )])
);

// -- warnings: a raw declared type and an unchecked conversion ([§4.12.2], [§5.1.9])
// `List raw` declares a raw type, and assigning it to `List<String>` succeeds
// by unchecked conversion — legal but unsound; `javac -Xlint:rawtypes,
//unchecked` flags both the same way.

snapshot!(
    raw_type_and_unchecked_conversion,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

class Raw {
    void m(List<String> xs) {
        List raw = xs;
        List<String> unchecked = raw;
        String first = unchecked.get(0);
    }
}
",
    )])
);
