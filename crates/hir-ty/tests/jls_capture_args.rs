//! JLS SE 26 scenario snapshots for *capture conversion of concrete
//! invocation arguments* ([JLS §5.1.10], [§15.12.2]): a value expression whose
//! static type is a wildcard-parameterized reference (`Box<?>`) is typed by
//! the capture of that wildcard at every use, so relating it to a generic
//! method's `Box<T>` formal constrains `T` by the capture's upper bound —
//! `<T extends PD> T get(Box<T>)` invoked with a `Box<?>` argument infers
//! `T := CAP#n` (`CAP#n <: PD`) and stays applicable. The source-class
//! type-parameter bound must survive capture (`class Box<T extends PD>`
//! captured from `Box<?>` yields `CAP extends PD`, not `CAP extends Object`).

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: wildcard actuals constrain generic method type variables ----------

snapshot!(
    captured_actual_to_generic_formal,
    check_body_types(&[(
        "/src/com/example/Box.java",
        "\
package com.example;

class Box {
    static class PD {}
    static class Particle<T extends PD> {
        Particle() {}
    }

    static <T extends PD> T get(Particle<T> p) {
        return null;
    }

    static <T extends PD> void accept(Particle<T> p) {}

    static void use(Particle<?> any) {
        PD value = get(any);
        accept(any);
    }
}
",
    )])
);
// §5.1.10/§15.12.2: `any` has the static type `Particle<?>`; as a value
// argument it is capture-converted to `Particle<CAP#n>` with `CAP#n <: PD`
// (the declared upper bound of `Particle`'s `T` — a source class, so the
// bound comes from the item tree). `get`'s inference variable instantiates to
// the capture and its result converts to `PD`; `accept` is applicable the
// same way. Without the capture the bare wildcard dead-ends against the
// invariant `Particle<T>` formal and both invocations report inapplicable.

snapshot!(
    captured_library_actual_to_generic_formal,
    check_body_types(&[(
        "/src/com/example/Opt.java",
        "\
package com.example;

import java.util.Optional;

class Opt {
    static <T> T unwrap(Optional<T> o) {
        return o.get();
    }

    Object read(Optional<?> any) {
        return unwrap(any);
    }
}
",
    )])
);
// The same capture applies to a library generic (`Optional`): the capture of
// `?` is an `Object`-bounded fresh variable, and `unwrap`'s `T` instantiates
// to it, so the call resolves where the bare wildcard argument would not.

// -- red: capture does not silently widen a concrete mismatch ------------------

snapshot!(
    captured_actual_not_assignable_to_concrete,
    check_body_types(&[(
        "/src/com/example/Box.java",
        "\
package com.example;

class Box {
    static class PD {}
    static class Particle<T extends PD> {}

    static void take(Particle<PD> p) {}

    static void use(Particle<?> any) {
        take(any);
    }
}
",
    )])
);
// Red: capture gives `any` the type `Particle<CAP#n>` with `CAP#n <: PD`, and
// `Particle<CAP#n>` is *not* a subtype of `Particle<PD>` (invariance,
// §4.10.2). javac reports the same incompatible-types error, so the capture
// fix must not make this call resolve.
