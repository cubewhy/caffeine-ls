//! Conformance snapshots for the raw-type warnings of *declaration*
//! positions — the parameter, field, return, supertype and record-component
//! types that
//! [JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)
//! and [§4.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.2)
//! make a raw use of a generic class — and their `@SuppressWarnings` scope
//! ([§9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5)).
//!
//! Every scenario is verified against `javac -Xlint:rawtypes` before the
//! snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// JLS §4.8/§4.12.2: a raw parameter type. javac:
// `warning: [rawtypes] found raw type: List`.
snapshot!(
    raw_parameter_type,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void m(List xs) {
    }
}
",
    )])
);

// JLS §4.8/§4.12.2: a raw field type, a raw return type and a raw record
// component are the same raw use — javac reports each.
snapshot!(
    raw_field_and_return_types,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;
import java.util.Map;

class Body {
    List field;

    Map m() {
        return null;
    }
}
",
    )])
);

// JLS §4.8: a raw use in an `extends`/`implements` clause. javac reports the
// raw supertype.
snapshot!(
    raw_supertype,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;

class Body extends ArrayList {
}
",
    )])
);

// JLS §4.8/§4.12.2: a parameterized use is *not* raw, and a non-generic class
// has no type arguments to omit — neither is reported.
snapshot!(
    parameterized_and_non_generic_are_not_raw,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void parameterized(List<String> xs) {
    }

    void nonGeneric(String s) {
    }
}
",
    )])
);

// JLS §9.6.4.5: `@SuppressWarnings("rawtypes")` on the declaring method covers
// its signature types — "the annotated declaration or any of its parts" — so
// the raw parameter is not reported. javac reports nothing.
snapshot!(
    rawtypes_scope_covers_signature,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings(\"rawtypes\")
    void m(List xs) {
    }
}
",
    )])
);

// JLS §9.6.4.5: the function-type name of a raw declaration type is keyed to
// the method, so a scope on a *sibling* leaves it in place. javac reports it.
snapshot!(
    rawtypes_scope_does_not_reach_sibling,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings(\"rawtypes\")
    void other(List xs) {
    }

    void m(List xs) {
    }
}
",
    )])
);

// JLS §9.6.4.5: the annotation that suppresses a warning is the *symbol*
// `java.lang.SuppressWarnings`, not a spelling of its name — the name is
// resolved like any other type name ([§6.5.5.1]), so a type of that name
// declared in the compilation unit, or imported from another package, is a
// different annotation that names nothing. javac still reports both raw
// parameter types:
// ```text
// Own.java:9: warning: [rawtypes] found raw type: List
// Imported.java:6: warning: [rawtypes] found raw type: List
// ```
snapshot!(
    same_named_annotation_does_not_suppress,
    check_class_diagnostics(&[
        (
            "/src/com/example/Own.java",
            "\
package com.example;

import java.util.List;

@interface SuppressWarnings {
    String[] value();
}

class Own {
    @SuppressWarnings(\"rawtypes\")
    void m(List xs) {
    }
}
",
        ),
        (
            "/src/com/other/SuppressWarnings.java",
            "\
package com.other;

public @interface SuppressWarnings {
    String[] value();
}
",
        ),
        (
            "/src/com/example/Imported.java",
            "\
package com.example;

import java.util.List;

import com.other.SuppressWarnings;

class Imported {
    @SuppressWarnings(\"rawtypes\")
    void m(List xs) {
    }
}
",
        ),
    ])
);

// JLS §9.6.4.5: the fully qualified spelling names the same annotation, so it
// suppresses exactly as the simple name does. javac reports nothing.
snapshot!(
    fully_qualified_annotation_suppresses,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @java.lang.SuppressWarnings(\"rawtypes\")
    void m(List xs) {
    }
}
",
    )])
);

// JLS §9.6.4.5/[§3.3]: the key a `@SuppressWarnings` names is the string
// literal's *value* — the lexer keeps a token's text as written, so a key
// spelled `"\u0072awtypes"` names `rawtypes` exactly as the plain spelling
// does. javac reports nothing.
snapshot!(
    rawtypes_key_spelled_with_unicode_escape,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    @SuppressWarnings(\"\\u0072awtypes\")
    void m(List xs) {
    }
}
",
    )])
);

// JLS §9.7.1/§15.29/§9.6.4.5: an element value is a *conditional expression*,
// so a key may be written as a constant variable rather than as a literal —
// `@SuppressWarnings(K)` names what `K` *denotes*, exactly as
// `@SuppressWarnings("rawtypes")` does. javac reports only the control:
// ```text
// Body.java:16: warning: [rawtypes] found raw type: List
//     void control(List xs) {
//                  ^
//   missing type arguments for generic class List<E>
// 1 warning
// ```
snapshot!(
    rawtypes_key_from_constant,
    check_class_diagnostics(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    static final String K = \"rawtypes\";

    @SuppressWarnings(K)
    void m(List xs) {
    }

    @SuppressWarnings(Body.K)
    void qualified(List xs) {
    }

    void control(List xs) {
    }
}
",
    )])
);

// JLS §9.6.4.5: a *field* declaration is a scope owner like any other, so the
// annotation on the field covers its declared type — "the annotated
// declaration or any of its parts" — while the sibling field's raw type
// stands. javac:
// ```text
// Field.java:9: warning: [rawtypes] found raw type: List
//     List control;
//     ^
//   missing type arguments for generic class List<E>
// 1 warning
// ```
snapshot!(
    field_scope_covers_its_own_type,
    check_class_diagnostics(&[(
        "/src/com/example/Field.java",
        "\
package com.example;

import java.util.List;

class Field {
    @SuppressWarnings(\"rawtypes\")
    List suppressed;

    List control;
}
",
    )])
);

// JLS §9.6.4.5: a *constructor* declaration, likewise. javac:
// ```text
// Ctor.java:10: warning: [rawtypes] found raw type: List
//     Ctor(List control, int other) {
//          ^
//   missing type arguments for generic class List<E>
// 1 warning
// ```
snapshot!(
    constructor_scope_covers_its_parameters,
    check_class_diagnostics(&[(
        "/src/com/example/Ctor.java",
        "\
package com.example;

import java.util.List;

class Ctor {
    @SuppressWarnings(\"rawtypes\")
    Ctor(List suppressed) {
    }

    Ctor(List control, int other) {
    }
}
",
    )])
);

// JLS §9.6.4.5: the annotation on a *parameter* scopes that parameter's own
// declared type — a narrower scope than the method's own annotation, which is
// why it suppresses this parameter and leaves its neighbour's alone. javac:
// ```text
// Param.java:9: warning: [rawtypes] found raw type: List
//     void n(List control) {
//            ^
//   missing type arguments for generic class List<E>
// 1 warning
// ```
snapshot!(
    parameter_scope_covers_its_own_type,
    check_class_diagnostics(&[(
        "/src/com/example/Param.java",
        "\
package com.example;

import java.util.List;

class Param {
    void m(@SuppressWarnings(\"rawtypes\") List suppressed) {
    }

    void n(List control) {
    }
}
",
    )])
);

// JLS §9.7.1/§15.18.1/§15.29/§9.6.4.5: an element value is an expression, and
// the key is the *whole* value it denotes — so a concatenation names one only
// when the concatenation itself spells it, and parentheses change nothing.
// javac:
// ```text
// Cat.java:13: warning: [rawtypes] found raw type: List
//     void tooLong(List control) {
//                  ^
//   missing type arguments for generic class List<E>
// Cat.java:17: warning: [rawtypes] found raw type: List
//     void suffixed(List control) {
//                   ^
//   missing type arguments for generic class List<E>
// Cat.java:24: warning: [rawtypes] found raw type: List
//     void plain(List control) {
//                ^
//   missing type arguments for generic class List<E>
// 3 warnings
// ```
snapshot!(
    concatenated_and_parenthesized_keys,
    check_class_diagnostics(&[(
        "/src/com/example/Cat.java",
        "\
package com.example;

import java.util.List;

class Cat {
    static final String K = \"types\";

    @SuppressWarnings(\"raw\" + K)
    void joined(List suppressed) {
    }

    @SuppressWarnings(\"raw\" + \"types\" + \"X\")
    void tooLong(List control) {
    }

    @SuppressWarnings(\"rawtypes\" + \"X\")
    void suffixed(List control) {
    }

    @SuppressWarnings((\"rawtypes\"))
    void parenthesized(List suppressed) {
    }

    void plain(List control) {
    }
}
",
    )])
);

// JLS §9.6.4.5/[§8.9.1]: an enum constant is a declaration, and §9.6.4.5
// scopes a suppression to "the annotated declaration or any of its parts" —
// the constant's class body is one of them. The grammar has no modifier list
// for an `ENUM_CONSTANT`, so its annotations are children of the constant
// itself; they nonetheless suppress within that constant and no sibling, and
// the members of the body are members of the anonymous class it denotes
// ([§15.9.1]), so their declared types are checked like any other field's.
// javac reports only the sibling:
// ```text
// Cases.java:11: warning: [rawtypes] found raw type: List
//         List raw;
//         ^
//   missing type arguments for generic class List<E>
// 1 warning
// ```
snapshot!(
    enum_constant_body_scope,
    check_class_diagnostics(&[(
        "/src/com/example/Cases.java",
        "\
package com.example;

import java.util.List;

enum Cases {
    @SuppressWarnings(\"rawtypes\")
    A {
        List raw;
    },
    B {
        List raw;
    };
}
",
    )])
);
