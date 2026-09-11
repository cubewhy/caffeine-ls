//! JLS SE 26 conformance snapshots for the annotations of *variable
//! declarations* —
//! [JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)
//! ("Where Annotations May Appear") and
//! [§9.6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.1)
//! (`@Target`).
//!
//! §9.7.4 resolves the ambiguity of an annotation written among the modifiers
//! of a variable declaration (`@Ann int x;`, `void m(@Ann int p)`) by the
//! applicability of the annotation interface:
//!
//! - if it is applicable in the declaration context corresponding to the
//!   declaration — `LOCAL_VARIABLE` for a local, enhanced-for, resource or
//!   pattern variable, `PARAMETER` for a formal, exception or lambda
//!   parameter ([§9.6.4.1] Table 9.7-1) — the annotation applies to the
//!   declaration;
//! - if it is applicable in type contexts (`TYPE_USE`) it applies to the type
//!   closest to it, which is the type written for the declared entity;
//! - it is a compile-time error if it is applicable in neither, and equally
//!   if it is applicable *only* in type contexts and the declaration writes
//!   no type at all — a `var` declaration ([§14.4], [§15.27.1]) — because
//!   there is then no closest type.
//!
//! A type *inside* a written type is a type context in its own right (a type
//! argument, an array dimension, the variable-arity modifier of `String @A
//! ... p`), so nothing but `TYPE_USE` is applicable there.
//!
//! The annotations themselves are type names, so each resolves like one
//! ([§6.5.5.1]): an annotation type that exists nowhere on the classpath is
//! reported as an unknown reference, exactly as it is for a declaration.
//!
//! Every scenario below is verified against `javac -XDrawDiagnostics`: the
//! reported positions and the `compiler.err.annotation.type.not.applicable` /
//! `...not.applicable.to.type` split match it. The one deliberate divergence
//! is which `var` declarations the rule is applied to. §9.7.4's `var` bullet
//! names the `var` form of §15.27.1 as well as §14.4, so *every* `var`
//! declaration is an error here, while javac 25/26 still accept three of the
//! forms (an enhanced-for variable, a lambda parameter and a record-pattern
//! component) that javac's own `VarVariables` regression test — JDK bug
//! 8371683, `test/langtools/tools/javac/annotations/typeAnnotations/
//! failures/target/VarVariables.java` — asserts are errors.

#[macro_use]
mod common;

use crate::common::{check_body_types, check_class_diagnostics};

/// The annotation types the scenarios below use: each restricts exactly one
/// element type, so what a diagnostic reports is the element type of the
/// declaration the annotation was written on.
const TARGETS: &str = "\
import java.lang.annotation.ElementType;
import java.lang.annotation.Target;

@Target(ElementType.TYPE_USE) @interface TYPE_USE {}
@Target(ElementType.LOCAL_VARIABLE) @interface LOCAL_VARIABLE {}
@Target(ElementType.PARAMETER) @interface PARAMETER {}
@Target(ElementType.FIELD) @interface FIELD {}
@Target(ElementType.METHOD) @interface METHOD {}
";

/// Wraps a scenario body into a compilation unit with [`TARGETS`].
fn unit(body: &str) -> String {
    format!("package com.example;\n\n{TARGETS}\nclass Anns {{\n{body}}}\n")
}

// -- locals ------------------------------------------------------------------

// §9.7.4: a local variable declaration accepts `LOCAL_VARIABLE` (the
// declaration) and `TYPE_USE` (its type); every other element type is a
// compile-time error — javac reports `annotation interface not applicable to
// this kind of declaration` at the annotation name.
snapshot!(
    local_variable_declaration,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void locals() {
        @LOCAL_VARIABLE int ok = 0;
        @TYPE_USE int typeUse = 0;
        @FIELD int field = 0;
        @PARAMETER int parameter = 0;
        @METHOD int method = 0;
        final @LOCAL_VARIABLE int finalOk = 0;
    }
"
        ),
    )])
);

// §9.7.4: a `var` declaration writes no type, so there is no closest type for
// a `TYPE_USE`-only annotation to apply to — a compile-time error (IntelliJ:
// `'var' type may not be annotated`). An annotation that is applicable to the
// declaration itself (`LOCAL_VARIABLE`) is unaffected, and javac agrees.
snapshot!(
    var_local_variable_declaration,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void locals() {
        @TYPE_USE var annotated = 0;
        @LOCAL_VARIABLE var ok = 0;
        var plain = 0;
        for (@TYPE_USE var init = 0; init < 1; init++) {
        }
    }
"
        ),
    )])
);

// §14.4/[§6.3]: the declarators of one declaration statement share its
// modifiers, so a written annotation is checked once — javac reports one
// diagnostic per written annotation, not one per declared variable — while
// each declarator's type is checked on its own.
snapshot!(
    multi_declarator_declaration,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void locals() {
        @FIELD int first = 0, second = 0;
        java.util.List<@FIELD String> a = null, b = null;
    }
"
        ),
    )])
);

// -- parameters --------------------------------------------------------------

// §9.6.4.1: a formal parameter's element type is `PARAMETER`; an annotation
// that is applicable in type contexts attaches to the parameter's type
// instead. A constructor's parameters are checked the same way, and so are
// the parameters of a body-less (abstract, here interface) method.
snapshot!(
    parameter_declaration,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    Anns(@PARAMETER int ok, @TYPE_USE int typeUse, @FIELD int field, @LOCAL_VARIABLE int local) {
    }

    void method(@PARAMETER int ok, @TYPE_USE int typeUse, @METHOD int method) {
    }
"
        ),
    )])
);

snapshot!(
    abstract_method_parameter,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &format!(
            "package com.example;\n\n{TARGETS}\ninterface Anns {{\n    void method(@FIELD int field, @PARAMETER int ok);\n}}\n"
        ),
    )])
);

// A `String @A ... p` variable-arity parameter writes its annotations between
// the type and the `...` ([§8.4.1]), which is a *type* position ([§9.7.4]):
// only `TYPE_USE` is applicable there. The same annotation before the type is
// the declaration's own, with element type `PARAMETER`.
snapshot!(
    varargs_parameter,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void varargs(@LOCAL_VARIABLE String ... locals) {}
    void varargsType(String @FIELD ... fields) {}
    void varargsTypeOk(String @TYPE_USE ... types) {}
    void varargsMixed(@PARAMETER String @TYPE_USE ... mixed) {}
"
        ),
    )])
);

// -- exception parameters and resources --------------------------------------

// §9.6.4.1: "Formal and exception parameter declarations" ([§8.4.1], [§9.4],
// [§14.20]) share the element type `PARAMETER` — so a `LOCAL_VARIABLE`
// annotation on a catch parameter is an error even though the variable is
// local to the clause.
snapshot!(
    catch_parameter,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void catcher() {
        try {
        } catch (@PARAMETER RuntimeException ok) {
        } catch (@FIELD RuntimeException field) {
        } catch (@TYPE_USE RuntimeException typeUse) {
        }
    }
"
        ),
    )])
);

// §9.6.4.1: a resource variable is a *local variable declaration*
// ([§14.20.3]), so `LOCAL_VARIABLE` and `TYPE_USE` apply; a `var` resource
// has no closest type, like any other `var` declaration.
snapshot!(
    resource_variable,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    static class Resource implements AutoCloseable {
        public void close() {
        }
    }

    void resources() throws Exception {
        try (@LOCAL_VARIABLE Resource ok = new Resource()) {
        }
        try (@TYPE_USE Resource typeUse = new Resource()) {
        }
        try (@PARAMETER Resource parameter = new Resource()) {
        }
        try (@TYPE_USE var annotated = new Resource()) {
        }
    }
"
        ),
    )])
);

// -- enhanced for ------------------------------------------------------------

// §9.6.4.1: an enhanced-for variable is a local variable declaration
// ([§14.14.2]) — `LOCAL_VARIABLE` and `TYPE_USE` apply, and a `var` loop
// variable has no closest type.
snapshot!(
    enhanced_for_variable,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void loops() {
        for (@LOCAL_VARIABLE int ok : new int[0]) {
        }
        for (@TYPE_USE int typeUse : new int[0]) {
        }
        for (@METHOD int method : new int[0]) {
        }
        for (@TYPE_USE var annotated : new int[0]) {
        }
        for (@TYPE_USE var initial = 0; initial < 1; initial++) {
        }
    }
"
        ),
    )])
);

// -- lambda parameters -------------------------------------------------------

// §9.7.4/[§15.27.1]: a lambda parameter is a formal parameter declaration
// (`PARAMETER`), whose written type accepts `TYPE_USE`. A *concise* parameter
// (`(v) -> ...`) admits no modifiers at all, and a `var` parameter writes no
// type — the `var` rule of §9.7.4 applies (see the module comment for javac's
// divergence on that row).
snapshot!(
    lambda_parameter,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    interface IntFunction {
        void apply(int value);
    }

    void lambdas() {
        IntFunction ok = (@PARAMETER int value) -> {};
        IntFunction typeUse = (@TYPE_USE int value) -> {};
        IntFunction field = (@FIELD int value) -> {};
        IntFunction annotatedVar = (@TYPE_USE var value) -> {};
        IntFunction parameterVar = (@PARAMETER var value) -> {};
        IntFunction concise = (value) -> {};
    }
"
        ),
    )])
);

// -- pattern variables -------------------------------------------------------

// §14.30.1/[§9.6.4.1]: a type pattern is a `LocalVariableDeclaration`, so the
// variable it binds has element type `LOCAL_VARIABLE` and its type accepts
// `TYPE_USE` — in an `instanceof` ([§15.20.2]) and in a `case` label
// ([§14.30.2]) alike.
snapshot!(
    pattern_variable,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void patterns(Object value) {
        if (value instanceof @LOCAL_VARIABLE String ok) {
        }
        if (value instanceof @TYPE_USE String typeUse) {
        }
        if (value instanceof @FIELD String field) {
        }
        if (value instanceof @PARAMETER String parameter) {
        }
    }

    void switchPatterns(Object value) {
        switch (value) {
            case @LOCAL_VARIABLE String ok -> {
            }
            case @FIELD String field -> {
            }
            default -> {
            }
        }
    }
"
        ),
    )])
);

// -- type positions inside a written type ------------------------------------

// §9.7.4/[§9.6.4.1]: a type argument and an array dimension are *type
// contexts*, so only an annotation applicable in type contexts may appear
// there — the element type of the declaration is not enough (javac:
// `annotation @X not applicable in this type context`).
snapshot!(
    nested_type_annotations,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    java.util.List<@FIELD String> field;
    java.util.List<@TYPE_USE String> typeUse;
    int @FIELD [] fieldArray;
    int @TYPE_USE [] typeUseArray;

    void locals() {
        java.util.List<@LOCAL_VARIABLE String> local = null;
        java.util.List<@TYPE_USE String> ok = null;
        int @METHOD [] method = null;
    }

    void parameters(java.util.List<@PARAMETER String> parameter) {}
"
        ),
    )])
);

// -- a type-use annotation on a written type is legal ------------------------

// §9.7.4: an annotation applicable in type contexts applies to the type
// closest to it, so a `TYPE_USE`-only annotation on a local, a parameter and
// a field with a written type is legal although it is not applicable to the
// declaration itself.
snapshot!(
    type_use_on_a_written_type,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    @TYPE_USE int field = 0;

    void declarations(@TYPE_USE int parameter) {
        @TYPE_USE int local = 0;
    }
"
        ),
    )])
);

// -- the annotation *name* resolves like a type name -------------------------

// §6.5.5.1: an annotation is written as a type name, so an unresolvable one
// is reported at the annotation name — for a formal parameter (the
// declaration pass) exactly as for a field.
snapshot!(
    unknown_annotation_on_a_parameter,
    check_class_diagnostics(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void method(@Missing int unknown, @FIELD int known) {
    }
"
        ),
    )])
);

// The same for the variables a *body* declares: a local, an enhanced-for
// variable, a resource, an exception parameter, a pattern binding and a
// lambda parameter. Each is reported at its own name ([§9.7.4]).
snapshot!(
    unknown_annotations_on_body_variables,
    check_body_types(&[(
        "/src/com/example/Anns.java",
        &unit(
            "\
    void locals() {
        @Missing int local = 0;
        for (@Missing int element : new int[0]) {
        }
        try (@MissingCloseable java.io.Closeable resource = null) {
        } catch (@Missing RuntimeException exception) {
        }
        Object value = null;
        if (value instanceof @Missing String pattern) {
        }
        java.util.function.Consumer<String> lambda = (@Missing String parameter) -> {
        };
    }
"
        ),
    )])
);
