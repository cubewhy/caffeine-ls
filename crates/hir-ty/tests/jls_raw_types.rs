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

// -- §4.8: a *raw* receiver erases *inherited* instance members ----------------
// A raw `CheckContainer` extends `ArrayList<Check<T>>`; its inherited
// `add(Check<T>)` erases to `add(Object)` and accepts the `Check<?>` actual.
// The per-class erasure only covers declared members — without the
// receiver-wide erasure the parameterized supertype keeps `Check<T>` and
// rejects the call.
snapshot!(
    raw_inherited_members_erased,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.ArrayList;

class Raw {
    static class Check<T> {
    }

    static class CheckContainer<T> extends ArrayList<Check<T>> {
    }

    void m(Check<?> c) {
        CheckContainer cc = new CheckContainer();
        cc.add(c);
    }
}
",
    )])
);

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

// -- JLS §4.8: "The type of an inherited instance method or non-static field
// of a raw type C, where the member was declared in a class or interface D,
// is the type of the member in the supertype of C that names D."
//
// The receiving supertype is the *declared one*: `Base` is non-generic, so
// its `items()` keeps `List<String>` even though the raw `Sub<T>` receiver
// erased the edges on the way to it. javac: green.
snapshot!(
    raw_receiver_non_generic_super_method_keeps_type,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

class Base {
    List<String> items() { return null; }
}

class Sub<T> extends Base {
}

class Body {
    void m(Sub s) {
        for (String x : s.items()) {
        }
    }
}
",
    )])
);

// -- JLS §4.8: "The superclass types (respectively, superinterface types) of
// a raw type are the erasures of the superclass types (superinterface types)
// of the named class or interface."
//
// `Gen` is declared generic, so the raw `Sub<T>` receiver names `Gen` (the
// erasure, not `Gen<T>`) and `items()` erases to `Object`. This pins that the
// walk above did not lose the erasure of members inherited from a generic
// supertype. javac: error.
snapshot!(
    raw_receiver_generic_super_method_erased,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

class Gen<T> {
    List<String> items() { return null; }
}

class Sub<T> extends Gen<T> {
}

class Body {
    void m(Sub s) {
        for (String x : s.items()) {
        }
    }
}
",
    )])
);

// -- JLS §4.8, supertype sentence, behind a non-generic intermediate:
// `Mid extends Gen<String>` — the raw `Sub<T>` receiver erases `Sub`'s edge
// to `Mid`, and the erasure context stays on (erasure is monotone), so
// `Mid`'s edge to the generic `Gen` is the erasure `Gen` too. `items()` is
// declared in `Gen`, so it erases to `Object`. javac: error.
snapshot!(
    raw_receiver_generic_super_behind_non_generic_intermediate,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

class Gen<T> {
    List<String> items() { return null; }
}

class Mid extends Gen<String> {
}

class Sub<T> extends Mid {
}

class Body {
    void m(Sub s) {
        for (String x : s.items()) {
        }
    }
}
",
    )])
);

// -- JLS §4.8, inherited-member sentence, generics on the path but not in the
// declarer: `Mid<T> extends Base` and the raw `Sub<T>` receiver reaches
// `Base` through the erased `Mid`; `Base` is non-generic, so `items()` keeps
// its declared `List<String>` and the loop is assignable. javac: green.
snapshot!(
    raw_receiver_non_generic_declarer_behind_generic_intermediate,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

class Base {
    List<String> items() { return null; }
}

class Mid<T> extends Base {
}

class Sub<T> extends Mid<T> {
}

class Body {
    void m(Sub s) {
        for (String x : s.items()) {
        }
    }
}
",
    )])
);

// -- JLS §4.8 via §4.4: a type variable whose bound is a raw type is itself
// raw for member lookup — the bound `RawSub` is a raw use, so the member
// lookup through it erases the inherited `items()` from `Gen`. Both the
// bound-receiver call and the direct raw `new RawSub()` receiver report.
// javac: error on each line.
snapshot!(
    raw_type_variable_bound_method_erased,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

class Gen<T> {
    List<String> items() { return null; }
}

class Mid extends Gen<String> {
}

class RawSub<T> extends Mid {
}

class Body {
    <X extends RawSub> void m(X x) {
        for (String y : x.items()) {
        }
    }

    void directRaw() {
        for (String y : new RawSub().items()) {
        }
    }
}
",
    )])
);

// -- JLS §4.8, inherited-member sentence for a non-static field: the raw
// `Sub<T>` receiver inherits `items` from the non-generic `Base`, so the field
// keeps its declared `List<String>` and the loop is assignable. javac: green.
snapshot!(
    raw_receiver_non_generic_super_field_keeps_type,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

class Base {
    List<String> items;
}

class Sub<T> extends Base {
}

class Body {
    void m(Sub s) {
        for (String x : s.items) {
        }
    }
}
",
    )])
);

// -- JLS §4.8, supertype sentence for a non-static field: the field is
// declared in the generic `Gen`, so the raw `Sub<T>` receiver names the
// erased `Gen` and `items` becomes the raw `List`. javac: error.
snapshot!(
    raw_receiver_generic_super_field_erased,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

class Gen<T> {
    List<String> items;
}

class Sub<T> extends Gen<T> {
}

class Body {
    void m(Sub s) {
        for (String x : s.items) {
        }
    }
}
",
    )])
);

// -- JLS §4.8 + §4.6 + §9.8: the single abstract method of the raw functional
// interface `Sub` is the erased `F` descriptor. `T`'s first bound is
// `List<String>`, so `void accept(T)` erases to `void accept(List)` — the raw
// `List`, not `Object`. The lambda parameter `p` is therefore a raw `List`
// and `p.get(0)` returns `Object`, which cannot initialize `String`. javac:
// `incompatible types: Object cannot be converted to String`.
snapshot!(
    raw_functional_interface_sam_erased,
    check_body_types(&[(
        "/src/com/example/Raw.java",
        "\
package com.example;

import java.util.List;

interface F<T extends List<String>> {
    void accept(T t);
}

interface Sub<T extends List<String>> extends F<T> {
}

class Body {
    void m(Sub s) {
        Sub f = (p) -> {
            String a = p.get(0);
        };
    }
}
",
    )])
);
