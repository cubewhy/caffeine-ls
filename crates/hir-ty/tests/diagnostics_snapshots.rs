//! Snapshots of the type-layer diagnostics surfaced from method-body
//! inference ([`hir_ty::body_types`]). Each diagnostic carries a typed code
//! ([`JavaDiagnosticCode`]), a source range and a message rendered against
//! the body IR ([JLS §14.4], [§14.18], [§15.11], [§15.12]).

#[macro_use]
mod common;

use crate::common::check_body_types;

snapshot!(
    resolve_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(String s, Body b) {
        undefinedName;
        b.missing;
        b.missing();
        s.length(1, 2);
        throw new Object();
    }
}
",
    )])
);
// Each `§` section of the quick wins: a bare name with no local, field, static
// import or implicit-receiver field is a resolution error (§6.5); a field
// access and a method call through a receiver with no such member report
// §15.11/§15.12.1; a call whose members all have the wrong arity reports
// §15.12.2; and a `throw` of a non-`Throwable` reports §14.18.
