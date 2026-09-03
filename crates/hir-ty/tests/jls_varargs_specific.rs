//! JLS SE 26 scenario snapshots for *most-specific selection between two
//! variable-arity overloads* ([JLS §15.12.2.5]): a varargs overload with a
//! fixed prefix is more specific than a fully-varargs overload whose element
//! type is a supertype — `style(TextColor, Decoration...)` beats
//! `style(StyleBuilderApplicable...)`. The two candidates declare different
//! parameter-list lengths, so the specificity test must align them by
//! expanding the shorter varargs tail to the element type.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: fixed-prefix varargs beats fully-varargs overload -----------------

snapshot!(
    varargs_fixed_prefix_more_specific,
    check_body_types(&[(
        "/src/com/example/Style.java",
        "\
package com.example;

class Style {
    interface StyleBuilderApplicable {}
    interface TextColor extends StyleBuilderApplicable {}
    interface TextDecoration extends StyleBuilderApplicable {}

    enum NamedTextColor implements TextColor {
        GRAY
    }

    enum Decoration implements TextDecoration {
        ITALIC
    }

    static class Builder {
        static Builder style(TextColor color, Decoration... decorations) {
            return null;
        }

        static Builder style(StyleBuilderApplicable... applicables) {
            return null;
        }
    }

    static void use() {
        Builder one = Builder.style(NamedTextColor.GRAY);
        Builder two = Builder.style(NamedTextColor.GRAY, Decoration.ITALIC);
        Builder three = Builder.style(NamedTextColor.GRAY, NamedTextColor.GRAY);
    }
}
",
    )])
);
// §15.12.2.5 (variable arity): for `one`/`two` the fixed-prefix overload
// `style(TextColor, Decoration...)` is applicable (the trailing actuals pack
// into `Decoration...`) and is more specific than the fully-varargs
// `style(StyleBuilderApplicable...)`: `TextColor <: StyleBuilderApplicable`
// and `Decoration <: StyleBuilderApplicable`. For `three` only the
// fully-varargs overload applies. All three calls resolve.

// -- red: a genuinely ambiguous varargs pair still reports ----------------------

snapshot!(
    varargs_ambiguous_pair,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Style.java",
        "\
package com.example;

class Style {
    interface A {}
    interface B {}

    static void m(A... a) {}
    static void m(B... b) {}

    static void use(A a, B b) {
        m(a, b);
    }
}
",
    )])
);
// Red: `m(A...)` and `m(B...)` are both varargs-applicable to `(A, B)` and
// neither parameter is more specific than the other (`A` and `B` are
// unrelated), so §15.12.2.5 leaves the invocation ambiguous.
