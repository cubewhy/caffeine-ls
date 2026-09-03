//! JLS SE 26 scenario snapshots for the `@Target` applicability check of
//! annotations ([JLS §9.6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.1),
//! [§9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)):
//! an annotation type restricts the element types it may be applied to via
//! `@Target`, so an annotation used on a declaration whose element type is not
//! in that set is a compile-time error. A *type-use* annotation is applicable
//! iff its target contains `TYPE_USE` or the element type of the declaration
//! whose type it annotates.
//!
//! The renderer ([`check_class_diagnostics`]) prints one line per
//! `@line:col` diagnostic; the source annotations here all resolve in the
//! same compilation unit, so the `@Target` argument list is read from the
//! annotation type's own source declaration.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- red: a declaration annotation on the wrong element type ------------------

snapshot!(
    method_annotation_on_class,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target(ElementType.METHOD)
@interface Marker {}

@Marker
class Anns {}
",
    )])
);
// §9.6.4.1: `Marker` is targeted to `METHOD` only, so applying it to a class
// declaration (element type `TYPE`) is a compile-time error, reported at the
// annotation name.

snapshot!(
    field_annotation_on_method,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target({ ElementType.FIELD, ElementType.PARAMETER })
@interface F {}

class Anns {
    @F
    void run() {}
}
",
    )])
);
// `F` is targeted to `FIELD`/`PARAMETER`; a method declaration (element type
// `METHOD`) is not in the set, so `@F` on `run` is rejected.

snapshot!(
    array_target_multi,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target({ ElementType.FIELD, ElementType.TYPE })
@interface FF {}

@FF
class Anns {
    @FF
    int x;

    @FF
    void run() {}
}
",
    )])
);
// The array form `{ FIELD, TYPE }` ([§9.7.1]) accepts the class (TYPE) and the
// field (FIELD), and rejects the method (METHOD).

// -- green: correct targets stay clean ----------------------------------------

snapshot!(
    correct_targets,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target({ ElementType.TYPE, ElementType.METHOD, ElementType.FIELD })
@interface M {}

@M
class Anns {
    @M
    void run() {}

    @M
    int x;
}
",
    )])
);
// Every use site is in the target set; nothing is reported.

// -- red: annotation type itself needs ANNOTATION_TYPE (or TYPE) --------------

snapshot!(
    annotation_type_target,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target(ElementType.METHOD)
@interface Inner {}

@Target(ElementType.ANNOTATION_TYPE)
@interface Ok {}

@Inner
@Ok
@interface Anns {}
",
    )])
);
// §9.6.4.1: an annotation type declaration has element type `ANNOTATION_TYPE`,
// so `@Inner` (targeted to `METHOD`) is rejected there; `@Ok` (targeted to
// `ANNOTATION_TYPE`) is accepted.

// -- red: type-use annotations ([§9.7.4]) --------------------------------------

snapshot!(
    type_use_without_type_use,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target(ElementType.METHOD)
@interface M {}

@Target(ElementType.TYPE_USE)
@interface T {}

class Anns {
    @M String field;
    @T String ok;
}
",
    )])
);
// §9.6.4.1: `@M` on the field's *type* is a type-use annotation whose target
// is neither `TYPE_USE` nor the declaration's element type (`FIELD`), so it is
// rejected; `@T` (targeted to `TYPE_USE`) is accepted on a type position.

// -- green: type-use annotation on a matching declaration element type --------

snapshot!(
    type_use_with_declaration_element,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target(ElementType.FIELD)
@interface F {}

class Anns {
    @F String field;
}
",
    )])
);
// §9.6.4.1: a type-use annotation is applicable when its target contains the
// *declaration's* element type — `@F` (targeted to `FIELD`) is applicable to
// the type of a field declaration, even though it is not a `TYPE_USE`.

snapshot!(
    type_use_on_type_declaration,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target(ElementType.TYPE_USE)
@interface TU {}

@TU
class Anns {
    @TU
    interface I {}
}
",
    )])
);
// §9.7.4: a `TYPE_USE`-only annotation is legal on a *type declaration* — the
// declaration of a class or interface is a type context, so `@Unmodifiable
// class C` (an annotation type targeted solely to `TYPE_USE`) annotates the
// declared type and must not be rejected as "not applicable to TYPE".

// -- green: empty @Target and unresolvable annotation stay permissive ---------

snapshot!(
    no_target_and_unknown,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface NoTarget {}

class Anns {
    @NoTarget
    void run() {}
}
",
    )])
);
// §9.6.4.1: an annotation type with no `@Target` is applicable to every
// declaration (except type parameters and packages); nothing is reported.

// -- red: the fully qualified @Target name resolves too ------------------------

snapshot!(
    fully_qualified_target,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.annotation.ElementType;

@java.lang.annotation.Target(ElementType.METHOD)
@interface M {}

@M
class Anns {}
",
    )])
);
// §9.6.4.1/[§9.7]: the annotation's own `@Target` is resolved like any type
// name ([§6.5.5.2]), so the fully qualified form is recognized and `@M`
// (targeted to `METHOD`) is rejected on the class. The FQN form is the case a
// naive simple-name match would silently lose.

// -- green: a same-package `@interface Target` shadows, not annotates ----------

snapshot!(
    shadowed_target_not_the_jdk_annotation,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@interface Target {}

@Target
interface Anns {}
",
    )])
);
// §6.5.5.1/§7.5: a type of the same package shadows the implicitly imported
// `java.lang.annotation.Target`, so `@Target` here names the local
// `@interface Target` — which carries no `@Target` meta-annotation — and is
// applicable to the interface like any unconstrained annotation. Shadowing is
// exactly what a name-based `@Target` recognition gets wrong: the name is not
// the identity, the resolution is.
