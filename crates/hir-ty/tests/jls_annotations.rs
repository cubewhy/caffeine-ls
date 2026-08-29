//! JLS SE 26 scenario snapshots for annotation resolution
//! ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7),
//! [§6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1)):
//! an annotation type is a reference type, so `@Name` on a declaration or a
//! type use resolves like any other type name — an unknown `@Name` is a
//! `cannot resolve type` error, reported at the annotation name.
//!
//! The annotation-as-modifier names are captured during lowering; the
//! type-use annotations ([§9.7.4], `int @Nullable []`, `List<@Nullable T>`)
//! ride the `SpannedTypeRef` reference list of their type.
//!
//! [JLS §9.7.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- green: annotations that resolve against the classpath ----------------------

snapshot!(
    valid_annotations,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.util.List;

@Deprecated
@SuppressWarnings(\"unchecked\")
interface AnnTarget {}

@FunctionalInterface
interface Workable {
    void work();
}

@interface Local {
    String value();
}

class Base {
    void run() {}
}

class Anns extends Base {
    @Override
    void run() {}

    @Deprecated
    int legacy;

    @Local
    void local() {}

    @java.lang.Deprecated
    void qualified() {}
}
",
    )])
);
// `@Override` (it overrides `Base.run`, [§9.6.4.4]),
// `@Deprecated`/`@SuppressWarnings`/`@FunctionalInterface` resolve against
// the JDK fixture; the same-package `@interface Local` is in scope by its own
// package; the qualified `@java.lang.Deprecated` resolves as a fully
// qualified name ([JLS §6.5.5.2]). Nothing is reported.

snapshot!(
    valid_record_component,
    check_class_diagnostics(&[(
        "/src/com/example/Rec.java",
        "\
package com.example;

import java.util.List;

@Deprecated
record Rec(@Deprecated @SuppressWarnings(\"x\") List<String> items) {}
",
    )])
);
// The annotations on the record declaration and its components resolve
// against the JDK fixture ([JLS §8.10.1], [§9.7]).

// -- red: an unknown annotation name on a declaration ----------------------------

snapshot!(
    unknown_annotation,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

@UnknownAnn
class Anns {
    @DoesNotExist
    int field;

    @NotARealAnnotation(\"arg\")
    void method() {}

    @bogus.Widget
    void qualified() {}
}
",
    )])
);
// Each unknown `@Name` reports the same `cannot resolve type` javac emits
// (`cannot find symbol: class Name`), at the annotation's own name — the
// qualified one at `bogus.Widget`. Note the `@Override` check is untouched:
// these are not `@Override`, so only the name resolution fires.

// -- red: an unknown annotation on a type use ([§9.7.4]) -------------------------

snapshot!(
    unknown_type_use_annotation,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.util.List;

class Anns {
    String @UnknownTypeUse [] annotatedArray;
    List<@UncheckedElement String> list;
    List<String @UnknownArgument []> argDim;
}
",
    )])
);
// `@UnknownTypeUse` on the array dimension, `@UncheckedElement` on the
// generic type argument and `@UnknownArgument` on the argument's own
// dimension are all type-use annotations ([JLS §9.7.4]); they resolve like
// any type name and each reports at its name.

// -- red: type-use annotation on a known type is fine ----------------------------

snapshot!(
    unknown_on_primitive_dimension,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        "\
package com.example;

import java.lang.Deprecated;

class Anns {
    int @Deprecated [] ok;
    int @BogusDim [] bad;
}
",
    )])
);
// `int @Deprecated []` — a type-use annotation on a *primitive* array
// dimension — resolves against the JDK fixture and is silent; `@BogusDim` is
// reported.
