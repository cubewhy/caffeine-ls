//! JLS SE 26 scenario snapshots for *generics, erasure and reifiability*: a
//! type argument not within its type parameter's bounds
//! ([§4.5.1]) is `TypeArgumentOutOfBounds`; two methods of one class whose
//! erasures ([§4.6]) clash are `NameClashSameErasure` ([§8.4.2]); a wildcard
//! class instance creation (`new ArrayList<?>()`) is
//! `CannotInstantiateWildcard` ([§15.9]); a generic class may not extend
//! `Throwable` ([§8.1.2]) — `GenericCannotExtendThrowable`; a catch parameter
//! declared with a type variable is `CannotCatchTypeVariable` ([§14.20]); and
//! the reference type of an `instanceof` must be reifiable ([§4.7]) —
//! `IllegalGenericInstanceOf` ([§15.20.2]). Red cases render the diagnostics;
//! green cases confirm legal programs pass cleanly.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_class_diagnostics};

// -- §4.5.1: type argument not within bounds ----------------------------------

snapshot!(
    type_argument_out_of_bounds,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Bounds.java",
        "\
package com.example;

class M<T extends Number> {
    void m() {
        M<String> a = new M<String>();
        M<Integer> b = new M<Integer>();
    }
}
",
    )])
);
// Red: `M<String>` violates `T extends Number` at both the declaration and the
// `new`; `M<Integer>` is within bounds.

snapshot!(
    type_argument_within_bounds,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Bounds.java",
        "\
package com.example;

class M<T extends Number> {
    void m() {
        M<T> a = new M<T>();
    }
}
",
    )])
);
// Green: `T` (declared `extends Number`) and `Integer` both satisfy the bound.

snapshot!(
    type_argument_bound_decl,
    check_class_diagnostics(&[(
        "/src/com/example/Bounds.java",
        "\
package com.example;

class Box<T extends Number> {
    M<String> a;
    void m(M<String> p) {
    }
    M<Integer> n() {
        return null;
    }
}
class M<T extends Number> {
}
",
    )])
);
// Green: the declaration-level walk does not check type-argument bounds (the
// body layer reports them at local/`new`/cast/`instanceof` positions only), so
// this file carries no diagnostics.

// -- §8.4.2: name clash by same erasure ---------------------------------------

snapshot!(
    name_clash_same_erasure,
    check_class_diagnostics(&[(
        "/src/com/example/Clash.java",
        "\
package com.example;

import java.util.List;

class Clash {
    void m(List<String> l) {
    }
    void m(List<Integer> l) {
    }
}
",
    )])
);
// Red: the two `m(List<...>)` methods erase to the same signature, and neither
// overrides the other.

snapshot!(
    no_name_clash,
    check_class_diagnostics(&[(
        "/src/com/example/Clash.java",
        "\
package com.example;

import java.util.List;

class Ok {
    void m(List<String> l) {
    }
    void m(String s) {
    }
    <E> void n(E e) {
    }
    <F> void n(String s) {
    }
}
",
    )])
);
// Green: `m(List<String>)` and `m(String)` have different arities; `n(E)` and
// `n(String)` erase differently.

// -- §15.9: wildcard instantiation --------------------------------------------

snapshot!(
    cannot_instantiate_wildcard,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Wild.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.List;

class Wild {
    void m() {
        List<?> x = new ArrayList<?>();
    }
    void n() {
        List<?> y = new ArrayList<String>();
        List<?> z = new ArrayList<>();
    }
}
",
    )])
);
// Red: `new ArrayList<?>()` creates nothing — a wildcard is not a concrete
// type. `new ArrayList<String>()` and the diamond `new ArrayList<>()` are
// fine.

// -- §8.1.2: generic class may not extend Throwable ---------------------------

snapshot!(
    generic_cannot_extend_throwable,
    check_class_diagnostics(&[(
        "/src/com/example/Throwable.java",
        "\
package com.example;

class I<T> extends Exception {
}

class J<T> {
}
",
    )])
);
// Red: the generic `I<T>` subclasses `Throwable` (via `Exception`); the
// non-generic `J<T>` is fine.

// -- §14.20: catch parameter type variable ------------------------------------

snapshot!(
    cannot_catch_type_variable,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Catch.java",
        "\
package com.example;

class L<T> {
    void m() {
        try {
        } catch (T t) {
        }
    }
    void n() {
        try {
        } catch (RuntimeException e) {
        }
    }
}
",
    )])
);
// Red: `catch (T t)` — a type parameter is not a class; `RuntimeException` is
// fine.

// -- §4.7/[§15.20.2: non-reifiable instanceof ---------------------------------

snapshot!(
    illegal_generic_instanceof,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Inst.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.List;

class Inst {
    void m() {
        Object o = new Object();
        if (o instanceof List<? extends String>) {
        }
        if (o instanceof ArrayList<String>) {
        }
    }
    void n() {
        Object o = new Object();
        if (o instanceof List<?>) {
        }
        if (o instanceof List) {
        }
        if (o instanceof String) {
        }
    }
}
",
    )])
);
// Red: `List<? extends String>` and `ArrayList<String>` are not reifiable — a
// bounded wildcard and a concrete argument each lose the runtime type.
// Unbounded `List<?>`, the raw `List` and the non-generic `String` are.

// -- §4.7: instanceof against a type variable ---------------------------------

snapshot!(
    illegal_instanceof_type_variable,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Inst.java",
        "\
package com.example;

class P<T> {
    void m() {
        Object o = new Object();
        if (o instanceof T) {
        }
    }
}
",
    )])
);
// Red: a type variable is never reifiable — the check erases to `Object` and
// can never be safe.
