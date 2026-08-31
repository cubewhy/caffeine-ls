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

// -- green: a raw `implements` with a non-generic override ([§4.8], [§8.4.8.1]) --
// Implementing a generic interface *raw* erases its members: the override of
// `<T> T convert(Class<T>, Object)` is `Object convert(Class, Object)`, which
// the @Override check must accept, and calls through a raw-typed value resolve
// against the erased signature.

snapshot!(
    raw_implements_generic_interface,
    check_body_types(&[(
        "/src/com/example/Conv.java",
        "\
package com.example;

class Conv {
    interface Converter<T> {
        T convert(Class<T> type, Object value);
    }

    static class RawImpl implements Converter {
        @Override
        public Object convert(Class type, Object value) {
            return value;
        }
    }

    public static void use() {
        Converter c = new RawImpl();
        Object r = c.convert(String.class, \"x\");
    }
}
",
    ),])
);

// -- green: instance members of a raw generic superclass are erased -------------
// A raw `AbstractList` subclass overrides and invokes erased members without
// diagnostics; unchecked warnings stay out of the way of the resolution.

snapshot!(
    raw_superclass_erased_members,
    check_body_types(&[(
        "/src/com/example/Names.java",
        "\
package com.example;

class Names extends java.util.AbstractList<String> {
    @Override
    public String get(int index) {
        return get(index);
    }

    @Override
    public int size() {
        return 0;
    }
}
",
    ),])
);

// -- §5.1.9: an array of a raw type converts unchecked to a parameterized
// array — `Frame<BasicValue>[] f = new Frame[7]`.

snapshot!(
    raw_array_unchecked_conversion,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static class Frame<T> {
    }
    Frame<String>[] m() {
        Frame<String>[] f = new Frame[7];
        return f;
    }
}
",
    )])
);
// Green: `new Frame[7]` is a raw array; it converts unchecked to
// `Frame<String>[]` ([§5.1.9]).

// -- §5.1.9: a *multi-dimensional* array of a raw type converts unchecked to
// an array of the parameterized element — `ArrayList[][] → List<String>[][]`
// is legal, exactly like the 1-D `Frame[] → Frame<String>[]` case.

snapshot!(
    multi_dim_raw_array_unchecked_conversion,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.List;

class Body {
    List<String>[][] foo() {
        ArrayList[][] arrayListArrayArray = new ArrayList[1][];
        return arrayListArrayArray;
    }
}
",
    )])
);
// Green: the raw element type of every dimension converts unchecked.
