//! JLS SE 26 scenario snapshots for *parameterized types whose type
//! arguments are self-referential type variables* and the
//! generic-array-typed method references they exercise.
//!
//! A class `K, T extends Box<K,T>` may store and return its own
//! parameterized type. The field access is typed from the *receiver's*
//! type arguments ([§4.10.2] substitution), the method parameter from the
//! method's own `Resolver`, so the two `Box<K,T>` handles are interned
//! separately and their type variables carry different bound depths
//! (the recursion guard of [§4.4] bounds resolution truncates at different
//! points). Assignment and return ([§5.2], [§14.17]) must still succeed:
//! the pair is identical *by type-variable name*, even though the interned
//! handles differ.
//!
//! A second family exercises `java.util.Arrays.copyOf` with a fixed-length
//! primitive length argument and casts of a type variable `R` to array
//! types. In the loose invocation phase ([§15.12.2.3], [§18.5.2]) a
//! primitive argument must not be boxed against an array formal whose
//! element is still an inference variable, or `<T> copyOf(T[],int)` can
//! never be instantiated ([JLS §18.5.2]); and a cast of `R` to `Object[]` /
//! `double[]` is always a legal compile-time cast ([§5.5], [§5.5.1] — the
//! erasure check happens at runtime). Red cases confirm the checks still
//! reject genuinely wrong types.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: identical self-referential parameterized types --------------------
// `Box<K,T>` with `T extends Box<K,T>`; reading and writing the field and
// returning it must be assignable to the same-looking `Box<K,T>`.

snapshot!(
    self_referential_field_access,
    check_body_types(&[(
        "/src/com/example/Box.java",
        "\
package com.example;

class Box<K, T extends Box<K, T>> {
    private Box<K, T> inner;

    Box<K, T> get() {
        return this.inner;
    }

    void set(Box<K, T> value) {
        this.inner = value;
    }

    void combine(Box<K, T> value) {
        Box<K, T> local = this.get();
        this.set(local);
        this.set(value);
    }
}
",
    )])
);
// Green: the field read, the field write and the return each compare a
// `Box<K,T>` resolved through the receiver to one resolved through the
// method's own scope. The two handles carry differently-deep bounds on the
// self-referential `T extends Box<K,T>`, but the types are identical by
// name ([§5.2], [§4.10.2] same-erasure) — no `incompatible-types`.

snapshot!(
    self_referential_through_subclass,
    check_body_types(&[(
        "/src/com/example/Box.java",
        "\
package com.example;

class Box<K, T extends Box<K, T>> {
}

class Node extends Box<Node, Node> {
}

class Holder<K, T extends Box<K, T>> {
    Box<K, T> box;
    void copy(Holder<K, T> other) {
        this.box = other.box;
    }
}
",
    )])
);
// Green: `other.box` resolves through `other`'s own receiver args while
// `this.box` resolves through `this`'s — both `Box<K,T>` by name, both
// with the self-referential bound resolved to different depths.

// -- green: `copyOf` with an array arg, primitive length, and `(R)` casts ----
// The array-typed generic `copyOf(T[],int)` with an `Object[]` actual and an
// `int` length, and casts of a type variable `R` to array types.

snapshot!(
    array_copy_and_type_var_casts,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Snap.java",
        "\
package com.example;

import java.util.Arrays;

class Snap<T extends Value<R, ?>, R> {
    private T source;

    void set(Object object) {
        if (object instanceof Object[]) {
            Object[] copy = Arrays.copyOf((Object[]) object, ((Object[]) object).length);
        }
    }

    boolean isDefault() {
        R defaultValue = this.source.getDefaultValue();
        if (defaultValue instanceof Object[]) {
            return Arrays.equals((Object[]) defaultValue, (Object[]) this.source.currentValue());
        }
        if (defaultValue instanceof double[]) {
            return Arrays.equals((double[]) defaultValue, (double[]) this.source.currentValue());
        }
        return true;
    }
}

class Value<R, T> {
    R getDefaultValue() {
        return null;
    }
    R currentValue() {
        return null;
    }
}
",
    )])
);
// Green: `Arrays.copyOf(Object[], int)` resolves the generic
// `<T> copyOf(T[],int)` with `T := Object` (the length stays a primitive
// `int` in the loose phase — boxing it against an array formal would leave
// the element uninstantiated, [JLS §18.5.2]); the `(R)` casts to `Object[]`
// and `double[]` are legal casting conversions ([§5.5.1] — a type variable
// erases to its bound at runtime), so no `inconvertible-types` either.

snapshot!(
    generic_array_copy_element_inference,
    check_body_types(&[(
        "/src/com/example/Snap.java",
        "\
package com.example;

import java.util.Arrays;

class Snap {
    String[] copy(String[] src) {
        return Arrays.copyOf(src, src.length);
    }
}
",
    )])
);
// Green: `Arrays.copyOf(String[], int)` instantiates `T := String` from the
// array argument; the returned `String[]` converts to the declared return
// type ([§5.2], [§14.17]).

snapshot!(
    generic_array_copy_in_cast_context,
    check_body_types(&[(
        "/src/com/example/Snap.java",
        "\
package com.example;

import java.util.Arrays;

class Snap<T extends Value<R, ?>, R> {
    private Object value;

    void set(Object object) {
        if (object instanceof Object[]) {
            this.value = (R) Arrays.copyOf((Object[]) object, ((Object[]) object).length);
        }
    }
}

class Value<R, T> {
}
",
    )])
);
// Green: the nested generic `Arrays.copyOf(Object[], int)` is a *standalone*
// invocation inside the `(R)` cast ([§15.16] — a cast of a poly invocation
// does not propagate the target), so it instantiates `T := Object` from the
// array argument and resolves `copyOf(T[],int)`; the `(R)` cast is a legal
// casting conversion ([§5.5.1]). No `cant.apply`, no `inconvertible`.

// -- red: genuinely wrong conversions still report ----------------------------

snapshot!(
    array_cast_unrelated_class,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(Object o) {
        if (o instanceof Object) {
        }
        int[] bad = (int[]) \"lit\";
    }
}
",
    )])
);
// Red: `String` (a final class) is not one of the array supertypes
// `Object`, `Cloneable` or `Serializable` ([§5.5.1], [§4.10.3]), so the
// cast `(int[]) \"lit\"` is inconvertible — but `(R)` casts to array types
// stay legal because a type variable's erasure is checked at runtime
// (the green `array_copy_and_type_var_casts` above).

snapshot!(
    array_copy_wrong_length,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Arrays;

class Body {
    void m(String[] src) {
        String[] bad = Arrays.copyOf(src, \"nope\");
    }
}
",
    )])
);
// Red: `copyOf(String[], String)` has no applicable overload — the generic
// `<T> copyOf(T[],int)` needs an `int` length, and no overload takes a
// `String` second argument, so the invocation reports `cant.apply`
// ([§15.12.2]).
