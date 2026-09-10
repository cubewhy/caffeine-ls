//! Java source-level gating snapshots: every construct newer than the level
//! its source set is compiled at is reported
//! (`compiler.err.feature.not.supported.in.source`), and a preview feature used
//! without `--enable-preview` is reported with the preview wording
//! (`compiler.err.preview.feature.disabled`). Each case is a level/level+1
//! boundary or a distinct row of javac's `Source.Feature` table, so a wrong
//! threshold or display name fails here. A file whose source set declared no
//! level must report nothing at all.

#[macro_use]
mod common;

use crate::common::{
    check_level_diagnostics, check_level_diagnostics_across_first_load,
    check_level_diagnostics_across_reloads, check_level_diagnostics_unknown,
};
use hir::JavaLanguageLevel;

fn level(source: u8) -> JavaLanguageLevel {
    JavaLanguageLevel::new(source, false).expect("valid source level")
}

fn preview(source: u8) -> JavaLanguageLevel {
    JavaLanguageLevel::new(source, true).expect("valid source level")
}

const RECORD: &[(&str, &str)] = &[(
    "/src/com/example/Records.java",
    "\
package com.example;

record A(int x) {}
",
)];

// -- records: 15 red, 16 green ------------------------------------------------

snapshot!(
    records_at_15_is_reported,
    check_level_diagnostics(level(15), RECORD)
);
// Red: `records are not supported in source level 15 (use source level 16 or higher to
// enable records)`.

snapshot!(
    records_at_16_is_legal,
    check_level_diagnostics(level(16), RECORD)
);
// Green: 16 is the release records became standard.

// -- text blocks: 14 red, 15 green --------------------------------------------

const TEXT_BLOCK: &[(&str, &str)] = &[(
    "/src/com/example/TextBlocks.java",
    "\
package com.example;

class A {
    String s = \"\"\"
        block
        \"\"\";
}
",
)];

snapshot!(
    text_blocks_at_14_are_reported,
    check_level_diagnostics(level(14), TEXT_BLOCK)
);
// Red: `text blocks are not supported in source level 14` (plural wording).

snapshot!(
    text_blocks_at_15_are_legal,
    check_level_diagnostics(level(15), TEXT_BLOCK)
);

// -- switch expressions and rules: 13 red, 14 green ----------------------------

const SWITCH_RULE: &[(&str, &str)] = &[(
    "/src/com/example/SwitchRules.java",
    "\
package com.example;

class A {
    void m(int i) {
        switch (i) {
            case 1, 2 -> {}
            default -> {}
        }
    }
}
",
)];

snapshot!(
    switch_rules_at_13_are_reported,
    check_level_diagnostics(level(13), SWITCH_RULE)
);
// Red: `switch rules are not supported in source level 13`; `multiple case labels`
// is swallowed, because the rule containing it is reported first.

snapshot!(
    switch_rules_at_14_are_legal,
    check_level_diagnostics(level(14), SWITCH_RULE)
);

const SWITCH_EXPR: &[(&str, &str)] = &[(
    "/src/com/example/SwitchExpr.java",
    "\
package com.example;

class A {
    int m(int i) {
        return switch (i) {
            case 1 -> 1;
            default -> 2;
        };
    }
}
",
)];

snapshot!(
    switch_expressions_at_13_are_reported,
    check_level_diagnostics(level(13), SWITCH_EXPR)
);
// Red: `switch expressions are not supported in source level 13`.

snapshot!(
    switch_expressions_at_14_are_legal,
    check_level_diagnostics(level(14), SWITCH_EXPR)
);

const SWITCH_YIELD: &[(&str, &str)] = &[(
    "/src/com/example/SwitchYield.java",
    "\
package com.example;

class A {
    int m(int i) {
        return switch (i) {
            case 1:
                yield 1;
            default:
                yield 2;
        };
    }
}
",
)];

snapshot!(
    switch_yield_at_13_is_reported,
    check_level_diagnostics(level(13), SWITCH_YIELD)
);
// Red: `yield` is gated under `switch expressions`, so the enclosing switch
// expression is the reported construct.

snapshot!(
    switch_yield_at_14_is_legal,
    check_level_diagnostics(level(14), SWITCH_YIELD)
);

// -- private interface methods: 8 red, 9 green ---------------------------------

const PRIVATE_INTERFACE_METHOD: &[(&str, &str)] = &[(
    "/src/com/example/PrivateInterfaceMethod.java",
    "\
package com.example;

interface I {
    private int x() {
        return 1;
    }
}
",
)];

snapshot!(
    private_interface_method_at_8_is_reported,
    check_level_diagnostics(level(8), PRIVATE_INTERFACE_METHOD)
);
// Red: `private interface methods are not supported in source level 8`.

snapshot!(
    private_interface_method_at_9_is_legal,
    check_level_diagnostics(level(9), PRIVATE_INTERFACE_METHOD)
);

// A `private` method of a *class* body is not gated: the row is interfaces only.
snapshot!(
    private_class_method_is_never_reported,
    check_level_diagnostics(
        level(8),
        &[(
            "/src/com/example/PrivateClassMethod.java",
            "\
package com.example;

class A {
    private int x() {
        return 1;
    }
}
",
        )]
    )
);

// -- var: 9 red, 10 green ------------------------------------------------------

const VAR_LOCAL: &[(&str, &str)] = &[(
    "/src/com/example/VarLocal.java",
    "\
package com.example;

class A {
    void m() {
        var x = 1;
    }
}
",
)];

snapshot!(
    var_at_9_is_reported,
    check_level_diagnostics(level(9), VAR_LOCAL)
);
// Red: `local variable type inference is not supported in source level 9` (javac has
// no fragment for this row and reports a plain "cannot find symbol" instead).

snapshot!(
    var_at_10_is_legal,
    check_level_diagnostics(level(10), VAR_LOCAL)
);

// The same `var` written as an explicit `int` is legal at every level.
snapshot!(
    explicit_type_at_8_is_legal,
    check_level_diagnostics(
        level(8),
        &[(
            "/src/com/example/ExplicitType.java",
            "\
package com.example;

class A {
    void m() {
        int x = 1;
    }
}
",
        )]
    )
);

// -- var in an implicit lambda: 10 red, 11 green -------------------------------

const VAR_LAMBDA: &[(&str, &str)] = &[(
    "/src/com/example/VarLambda.java",
    "\
package com.example;

import java.util.function.Function;

class A {
    void m(Function<Integer, Integer> f) {
        f = (var x) -> x;
    }
}
",
)];

snapshot!(
    var_lambda_at_10_is_reported,
    check_level_diagnostics(level(10), VAR_LAMBDA)
);
// Red: `var syntax in implicit lambdas are not supported in source level 10` — the
// plural wording of javac's own `DiagKind`.

snapshot!(
    var_lambda_at_11_is_legal,
    check_level_diagnostics(level(11), VAR_LAMBDA)
);

// -- instanceof patterns: 15 red, 16 green -------------------------------------

const INSTANCEOF_PATTERN: &[(&str, &str)] = &[(
    "/src/com/example/InstanceofPattern.java",
    "\
package com.example;

class A {
    void m(Object o) {
        if (o instanceof String s) {
        }
    }
}
",
)];

snapshot!(
    instanceof_pattern_at_15_is_reported,
    check_level_diagnostics(level(15), INSTANCEOF_PATTERN)
);
// Red: `pattern matching in instanceof is not supported in source level 15` — the
// singular wording (javac's `DiagKind.NORMAL`).

snapshot!(
    instanceof_pattern_at_16_is_legal,
    check_level_diagnostics(level(16), INSTANCEOF_PATTERN)
);

// -- sealed classes: 16 red, 17 green ------------------------------------------

const SEALED: &[(&str, &str)] = &[(
    "/src/com/example/Sealed.java",
    "\
package com.example;

sealed interface Shape permits Circle {
}

final class Circle implements Shape {
}

non-sealed class Free implements Shape {
}
",
)];

snapshot!(
    sealed_at_16_is_reported,
    check_level_diagnostics(level(16), SEALED)
);
// Red: `sealed classes are not supported in source level 16` once for the `sealed`
// declaration — its `permits` clause is part of the same construct, not a
// second report — and once for the `non-sealed` modifier of `Free`.

snapshot!(
    sealed_at_17_is_legal,
    check_level_diagnostics(level(17), SEALED)
);

// -- null in switch cases: 20 red, 21 green ------------------------------------

const CASE_NULL: &[(&str, &str)] = &[(
    "/src/com/example/CaseNull.java",
    "\
package com.example;

class A {
    void m(Object o) {
        switch (o) {
            case null -> {}
            default -> {}
        }
    }
}
",
)];

snapshot!(
    case_null_at_20_is_reported,
    check_level_diagnostics(level(20), CASE_NULL)
);
// Red: `null in switch cases is not supported in source level 20`. javac names the
// pattern switch instead when the selector type is itself not a valid
// pre-21 switch type (`switch (o)` over `Object`), which is a *type* test a
// syntax walk cannot make; the label's own row is a real 21 error either way.

snapshot!(
    case_null_at_21_is_legal,
    check_level_diagnostics(level(21), CASE_NULL)
);

// A `case null, default` label at 21 is legal; below 21 the same two rows fire.
snapshot!(
    pattern_switch_at_20_is_reported,
    check_level_diagnostics(
        level(20),
        &[(
            "/src/com/example/PatternSwitch.java",
            "\
package com.example;

class A {
    void m(Object o) {
        switch (o) {
            case String s -> {}
            default -> {}
        }
    }
}
",
        )]
    )
);
// Red: `patterns in switch statements are not supported in source level 20`.

// -- deconstruction patterns: 20 red, 21 green ---------------------------------

const RECORD_PATTERN_INSTANCEOF: &[(&str, &str)] = &[(
    "/src/com/example/RecordPattern.java",
    "\
package com.example;

class A {
    record Point(int x, int y) {}

    void m(Object o) {
        if (o instanceof Point(int x, int y)) {
        }
    }
}
",
)];

snapshot!(
    record_pattern_in_instanceof_at_20_is_reported,
    check_level_diagnostics(level(20), RECORD_PATTERN_INSTANCEOF)
);
// Red: an `instanceof` deconstruction pattern needs only `deconstruction
// patterns` (21), not the pattern switch.

snapshot!(
    record_pattern_in_instanceof_at_21_is_legal,
    check_level_diagnostics(level(21), RECORD_PATTERN_INSTANCEOF)
);

snapshot!(
    record_pattern_in_switch_at_20_is_reported,
    check_level_diagnostics(
        level(20),
        &[(
            "/src/com/example/RecordPatternSwitch.java",
            "\
package com.example;

class A {
    record Point(int x, int y) {}

    void m(Object o) {
        switch (o) {
            case Point(int x, int y) -> {}
            default -> {}
        }
    }
}
",
        )]
    )
);
// Red: the pattern switch is reported once, swallowing its nested record
// pattern. A nested `int x` component is *not* a primitive pattern: javac only
// requires 23 for a component whose type differs from the expression's.

// -- unnamed variables: 21 red, 22 green ---------------------------------------

const UNNAMED_LOCAL: &[(&str, &str)] = &[(
    "/src/com/example/Unnamed.java",
    "\
package com.example;

class A {
    void m() {
        int _ = 1;
    }
}
",
)];

snapshot!(
    unnamed_local_at_21_is_reported,
    check_level_diagnostics(level(21), UNNAMED_LOCAL)
);
// Red: `unnamed variables are not supported in source level 21`.

snapshot!(
    unnamed_local_at_22_is_legal,
    check_level_diagnostics(level(22), UNNAMED_LOCAL)
);

const UNNAMED_LAMBDA: &[(&str, &str)] = &[(
    "/src/com/example/UnnamedLambda.java",
    "\
package com.example;

import java.util.function.BiFunction;

class A {
    void m(BiFunction<Integer, Integer, Integer> f) {
        f = (_, x) -> x;
    }
}
",
)];

snapshot!(
    unnamed_lambda_at_21_is_reported,
    check_level_diagnostics(level(21), UNNAMED_LAMBDA)
);

snapshot!(
    unnamed_lambda_at_22_is_legal,
    check_level_diagnostics(level(22), UNNAMED_LAMBDA)
);

// A `_` in an *expression* position is not a variable: the parser's "underscore
// not allowed here" is the only report, at any level.
snapshot!(
    underscore_expression_is_not_a_variable,
    check_level_diagnostics(
        level(22),
        &[(
            "/src/com/example/UnderscoreExpr.java",
            "\
package com.example;

class A {
    void m(Object o) {
        switch (o) {
            case _ -> {}
            default -> {}
        }
    }
}
",
        )]
    )
);
// Red: nothing from the level check — `case _` is not `UnnamedVariables`, which
// javac only accepts as a binding pattern. Green: the parser already reports it.

// -- primitive patterns: preview, so `--enable-preview` decides ---------------

const PRIMITIVE_PATTERN: &[(&str, &str)] = &[(
    "/src/com/example/PrimitivePattern.java",
    "\
package com.example;

class A {
    int m(Object o) {
        return switch (o) {
            case int i -> i;
            default -> 0;
        };
    }
}
",
)];

snapshot!(
    primitive_pattern_at_23_without_preview_is_reported,
    check_level_diagnostics(level(23), PRIMITIVE_PATTERN)
);
// Red: `primitive patterns are a preview feature and are disabled by default
// (use --enable-preview to enable primitive patterns)`.

snapshot!(
    primitive_pattern_at_23_with_preview_is_legal,
    check_level_diagnostics(preview(23), PRIMITIVE_PATTERN)
);

// `--enable-preview` does not lower the level: the pattern switch around the
// primitive pattern still needs 21 at a preview level of 20.
snapshot!(
    primitive_pattern_below_21_with_preview_is_reported,
    check_level_diagnostics(preview(20), PRIMITIVE_PATTERN)
);
// Red: `patterns in switch statements are not supported in source level 20` — the
// enclosing pattern switch, reported before the preview-disabled pattern inside.

// §4.3/[§14.30.1]: an array type is a *reference* type, so a type pattern
// declaring one — however primitive its element type — is an ordinary type
// pattern and never the preview primitive pattern. `byte[]`, `byte[][]` and
// `int[]` are all legal at a level without preview.

const PRIMITIVE_ELEMENT_ARRAY: &[(&str, &str)] = &[(
    "/src/com/example/PrimitiveArray.java",
    "\
package com.example;

class A {
    boolean m(Object o) {
        boolean a = o instanceof byte[] bytes && bytes.length > 0;
        boolean b = o instanceof byte[][] rows;
        boolean c = o instanceof int[] numbers;
        return a && b && c;
    }
}
",
)];

snapshot!(
    primitive_element_array_pattern_at_21_is_legal,
    check_level_diagnostics(level(21), PRIMITIVE_ELEMENT_ARRAY)
);

// -- unconditional patterns in instanceof: 20 red, 21 green -------------------

const UNCONDITIONAL_PATTERN: &[(&str, &str)] = &[(
    "/src/com/example/UnconditionalPattern.java",
    "\
package com.example;

class A {
    boolean m(String s) {
        return s instanceof Object o;
    }
}
",
)];

snapshot!(
    unconditional_pattern_at_20_is_reported,
    check_level_diagnostics(level(20), UNCONDITIONAL_PATTERN)
);
// Red: `unconditional patterns in instanceof are not supported in source level 20` —
// a `String` is always an `Object`, which is the only shape a syntax walk can
// prove unconditional.

snapshot!(
    unconditional_pattern_at_21_is_legal,
    check_level_diagnostics(level(21), UNCONDITIONAL_PATTERN)
);

// A conditional pattern whose type is `Object` is *not* unconditional and is
// legal at 16 (the pattern row), not 21.
snapshot!(
    conditional_object_pattern_is_a_plain_pattern,
    check_level_diagnostics(level(16), INSTANCEOF_PATTERN)
);

// -- one report per construct, but every distinct construct -------------------

// `case 1, 2 ->` violates two rows (multiple case labels inside a switch rule);
// the outer, more specific row is reported once.
snapshot!(
    one_report_for_a_multi_label_rule,
    check_level_diagnostics(
        level(13),
        &[(
            "/src/com/example/MultiLabel.java",
            "\
package com.example;

class A {
    void m(int i) {
        switch (i) {
            case 1, 2 -> {}
            default -> {}
        }
    }
}
",
        )]
    )
);

// A `record` nested in a `sealed` class is a *second* construct, so both are
// reported — javac reports both too.
snapshot!(
    nested_distinct_constructs_are_both_reported,
    check_level_diagnostics(
        level(15),
        &[(
            "/src/com/example/Nested.java",
            "\
package com.example;

sealed class A permits B {
    record B(int x) implements java.io.Serializable {}
}

final class C extends A {
}
",
        )]
    )
);

// -- a workspace reload at a new level re-derives the report -------------------

snapshot!(
    reloading_at_a_higher_level_clears_the_report,
    check_level_diagnostics_across_reloads(level(15), level(16), RECORD)
);
// Red then green: the level is read through the `ProjectGraph` salsa input, so
// loading the workspace again at 16 invalidates the memoized report. No extra
// invalidation work is needed for a level change.

snapshot!(
    reloading_at_a_lower_level_adds_the_report,
    check_level_diagnostics_across_reloads(level(16), level(15), RECORD)
);
// Green then red: the reverse direction, so the clear is not just a stale memo
// being dropped.

// -- modules: 8 red, 9 green ---------------------------------------------------

const MODULE: &[(&str, &str)] = &[(
    "/src/module-info.java",
    "\
module m {
    requires java.base;
}
",
)];

snapshot!(
    modules_at_8_are_reported,
    check_level_diagnostics(level(8), MODULE)
);
// Red: `modules are not supported in source level 8`.

snapshot!(
    modules_at_9_are_legal,
    check_level_diagnostics(level(9), MODULE)
);

// -- diamond with an anonymous class: 8 red, 9 green ---------------------------

const DIAMOND_ANONYMOUS: &[(&str, &str)] = &[(
    "/src/com/example/DiamondAnonymous.java",
    "\
package com.example;

class A {
    void m() {
        Runnable r = new Runnable<>() {
            public void run() {
            }
        };
    }
}
",
)];

snapshot!(
    diamond_anonymous_at_8_is_reported,
    check_level_diagnostics(level(8), DIAMOND_ANONYMOUS)
);
// Red: `'<>' with anonymous inner classes is not supported in source level 8` — the
// singular wording, and javac's quoting of the operator.

snapshot!(
    diamond_anonymous_at_9_is_legal,
    check_level_diagnostics(level(9), DIAMOND_ANONYMOUS)
);

// An empty diamond on a *named* class is not an anonymous class and is legal.
snapshot!(
    diamond_on_a_named_class_at_8_is_legal,
    check_level_diagnostics(
        level(8),
        &[(
            "/src/com/example/PlainDiamond.java",
            "\
package com.example;

class A {
    void m() {
        java.util.List<String> l = new java.util.ArrayList<>();
    }
}
",
        )]
    )
);

// -- effectively final variables in try-with-resources: 8 red, 9 green ---------

const TRY_RESOURCE_VARIABLE: &[(&str, &str)] = &[(
    "/src/com/example/TryResource.java",
    "\
package com.example;

class A {
    void m(java.io.Reader r) throws Exception {
        try (r) {
        }
    }
}
",
)];

snapshot!(
    try_resource_variable_at_8_is_reported,
    check_level_diagnostics(level(8), TRY_RESOURCE_VARIABLE)
);
// Red: `variables in try-with-resources are not supported in source level 8`.

snapshot!(
    try_resource_variable_at_9_is_legal,
    check_level_diagnostics(level(9), TRY_RESOURCE_VARIABLE)
);

// A resource *declared* in the `try` is the pre-9 form and is legal at 8.
snapshot!(
    declared_try_resource_at_8_is_legal,
    check_level_diagnostics(
        level(8),
        &[(
            "/src/com/example/DeclaredResource.java",
            "\
package com.example;

class A {
    void m(java.io.Reader r) throws Exception {
        try (java.io.Reader local = r) {
        }
    }
}
",
        )]
    )
);

// -- reifiable types in instanceof: 15 red, 16 green ---------------------------

const REIFIABLE_INSTANCEOF: &[(&str, &str)] = &[(
    "/src/com/example/ReifiableInstanceof.java",
    "\
package com.example;

class A {
    boolean m(Object o) {
        return o instanceof java.util.List<String>;
    }
}
",
)];

snapshot!(
    reifiable_instanceof_at_15_is_reported,
    check_level_diagnostics(level(15), REIFIABLE_INSTANCEOF)
);
// Red: `reifiable types in instanceof are not supported in source level 15`.

snapshot!(
    reifiable_instanceof_at_16_is_legal,
    check_level_diagnostics(level(16), REIFIABLE_INSTANCEOF)
);

// An *unbounded wildcard* argument is reifiable, so the same `instanceof` is
// legal at 8 — the rule must not fire merely because a `TYPE_ARGUMENTS` node
// exists.
snapshot!(
    unbounded_wildcard_instanceof_at_8_is_legal,
    check_level_diagnostics(
        level(8),
        &[(
            "/src/com/example/WildcardInstanceof.java",
            "\
package com.example;

class A {
    boolean m(Object o) {
        return o instanceof java.util.List<?>;
    }
}
",
        )]
    )
);

// -- a report computed before the workspace loaded re-derives after -----------

snapshot!(
    report_computed_before_the_workspace_load_re_derives,
    check_level_diagnostics_across_first_load(level(8), VAR_LOCAL)
);
// Red then green: an editor pulls diagnostics as soon as a document is opened,
// which happens before the build system reports the project. That early report
// must not be memoized as "no level" for the rest of the session — `var` at
// source level 8 is reported once the load lands.

// -- an unknown level disables the whole report --------------------------------

snapshot!(
    unknown_level_reports_nothing,
    check_level_diagnostics_unknown(RECORD)
);
// Green: with no level exported by the build system, no source-level check
// runs — a wrong level would report every file of the project.

snapshot!(
    unknown_level_reports_nothing_for_underscore,
    check_level_diagnostics_unknown(UNNAMED_LOCAL)
);
