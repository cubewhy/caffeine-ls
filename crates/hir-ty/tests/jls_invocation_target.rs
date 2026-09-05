//! JLS SE 26 scenario snapshots for the *expected type* of a method
//! invocation ([JLS §18.5.2.4]): the target type participates only at the
//! resolution of the *chosen* method, never in the §15.12.2.2/3/4
//! applicability phases or the §15.12.2.5 most-specific test. javac never
//! rejects an overload for an incompatible target while a weaker one remains
//! applicable — the mismatch is reported on the chosen method as an
//! incompatible-types problem, not as a cannot-apply.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: the target refines a generic method's invocation type ---------------

snapshot!(
    target_refines_generic_return,
    check_body_types(&[(
        "/src/com/example/Box.java",
        "\
package com.example;

class Box {
    static <T> T empty() {
        return null;
    }

    static String s() {
        String s = empty();
        return s;
    }
}
",
    )])
);
// javac: `String s = empty();` types the poly invocation `empty()` against
// the `String` target — `⟨R → T⟩` with `T := String` — so the call's type is
// `String`. Pre-fix the phase probe rejected the target-incompatible method
// and the call drew cannot-apply.

// -- red: an incompatible target is an incompatible-types problem, not a
// -- cannot-apply on the call ------------------------------------------------

snapshot!(
    target_mismatch_resolves_then_fails,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Box.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.List;

class Box {
    static <T> List<T> box() {
        return null;
    }

    static void bad() {
        int i = box();
    }
}
",
    )])
);
// javac reports `incompatible types` for `int i = box();` (the call resolves
// to `box()`; its `List<T>` result does not convert to `int`) — never
// `cannot find symbol` / cannot-apply on the call itself. The assertion is
// that no cannot-apply diagnostic appears on the `box()` call.
