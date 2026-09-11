//! Java platform-release snapshots (`javac --release N`,
//! [JEP 247](https://openjdk.org/jeps/247)): a platform API the runtime JDK
//! provides but the release's `ct.sym` view does not is reported as
//! `api-not-supported-in-release`, naming the release it first appeared in.
//!
//! Each case is a release boundary of the fake archive built by
//! [`common::release_fixture`] — whose directories `8`, `9A` and `BCDEFGHIJK`
//! cover releases 8, 9-10 and 11-20 — so a wrong release-set reading, a wrong
//! boundary or a missing report fails here. A source set with no release, and a
//! release outside the archive's bounds, must report nothing at all.

#[macro_use]
mod common;

use crate::common::{check_release_diagnostics, check_release_diagnostics_across_reloads};

/// A class the archive first lists in release 11 (`BCDEFGHIJK` starts at `B`).
const LATER: &[(&str, &str)] = &[(
    "/src/com/example/LaterUse.java",
    "\
package com.example;

class LaterUse {
    java.util.Later l;
}
",
)];

fn later_at(release: u8) -> String {
    check_release_diagnostics(Some(release), LATER)
}

// -- a class added later: 8 red, 10 red, 11 green ------------------------------

snapshot!(later_class_at_8_is_reported, later_at(8));
// Red: `class 'java.util.Later' is not supported in release 8 (added in
// release 11)`, at the reference name.

snapshot!(later_class_at_10_is_reported, later_at(10));
// Red: the boundary below — the directory `9A` covers 9 *and* 10, so 10 still
// lacks the class.

snapshot!(later_class_at_11_is_clean, later_at(11));
// Green: `B` is release 11, the class's first.

// -- the reference sites -------------------------------------------------------

snapshot!(
    later_class_import_at_8_is_reported,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/Imported.java",
            "\
package com.example;

import java.util.Later;

class Imported {}
",
        )],
    ),
);
// Red: javac rejects the import as well as every use, so the import name is
// reported on its own.

snapshot!(
    later_class_in_a_body_at_8_is_reported,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/Body.java",
            "\
package com.example;

class Body {
    void f() {
        java.util.Later l = null;
    }
}
",
        )],
    ),
);
// Red: a local's declared type is a *body* reference, so this exercises the
// inference-side mapping of the report, not the declaration pass.

snapshot!(
    nested_class_at_8_is_reported,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/Nested.java",
            "\
package com.example;

class Nested {
    java.util.Api.Nested n;
}
",
        )],
    ),
);
// Red: the nested class is `java.util.Api$Nested.sig` in the archive, so the
// binary-name (`$`) mapping is what makes the lookup hit.

// -- the check stays inert where it must ---------------------------------------

snapshot!(release_above_the_archive_reports_nothing, later_at(21),);
// Green: the archive's directories stop at release 20, so it cannot answer for
// 21 and reports nothing rather than everything.

snapshot!(
    no_release_reports_nothing,
    check_release_diagnostics(None, LATER),
);
// Green: without a `--release` there is no platform view to check against.

snapshot!(
    tracked_nowhere_reports_nothing,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/Untracked.java",
            "\
package com.example;

class Untracked {
    java.util.Absent a;
}
",
        )],
    ),
);
// Green: `java.util.Absent` is on the runtime classpath but in no directory of
// the archive — the shape of an internal `jdk.internal.*` class, which javac
// rejects through the module system, not through the release view.

// -- the report re-derives on a reload ----------------------------------------

snapshot!(
    release_across_reloads,
    check_release_diagnostics_across_reloads(8, 11, LATER),
);
// Red at 8, green at 11: the report is keyed on the source set's release, so a
// workspace reload at a different one re-derives it.

// -- members added later: 8 red, 11 green --------------------------------------

/// `Api`'s runtime shape: `old()` and both constructors, `newer()` and `FIELD`
/// added at release 11 (`BCDEFGHIJK`), exactly as the release-8 and release-11
/// `.sig` files of the fixture split them.
const API_USE: &[(&str, &str)] = &[(
    "/src/com/example/ApiUse.java",
    "\
package com.example;

class ApiUse {
    void f(java.util.Api a) {
        a.newer();
        String s = java.util.Api.FIELD;
        java.util.Api b = new java.util.Api(1);
    }
}
",
)];

fn api_use_at(release: u8) -> String {
    check_release_diagnostics(Some(release), API_USE)
}

snapshot!(members_added_later_at_8_are_reported, api_use_at(8));
// Red thrice: the invocation, the qualified field read and the constructor,
// each naming its own member kind.

snapshot!(members_at_11_are_clean, api_use_at(11));
// Green: release 11 is where the fixture's `.sig` first declares all three.

snapshot!(
    inherited_member_reports_its_declaring_class,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/SubUse.java",
            "\
package com.example;

class SubUse {
    void f(java.util.Sub s) {
        s.newer();
    }
}
",
        )],
    ),
);
// Red against `java.util.Api`, the *declaring* class (`Sub` itself declares
// only its constructor) — the identity a platform release is checked against.

snapshot!(
    member_at_the_release_is_clean,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/OldUse.java",
            "\
package com.example;

class OldUse {
    void f(java.util.Api a) {
        a.old();
        java.util.Api b = new java.util.Api();
    }
}
",
        )],
    ),
);
// Green: `old()` and the no-arg constructor are in the release-8 view.

snapshot!(
    method_reference_added_later_at_8_is_reported,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/RefUse.java",
            "\
package com.example;

class RefUse {
    java.util.function.Supplier<String> s;
    void f(java.util.Api a) {
        s = a::newer;
    }
}
",
        )],
    ),
);
// Red: a method reference resolves a member too, and reports at the reference.

snapshot!(
    static_import_of_a_later_member_at_8_is_reported,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/StaticImport.java",
            "\
package com.example;

import static java.util.Api.staticCall;

class StaticImport {
    void f() {
        staticCall();
    }
}
",
        )],
    ),
);
// Red at the import name — the source form names the member without its
// descriptor, so that report is the name-only `member` form — and again at the
// use, whose own reference resolves the member. javac reports both too
// (the import, then `cannot find symbol` at the use).

snapshot!(
    member_of_an_untracked_class_reports_nothing,
    check_release_diagnostics(
        Some(8),
        &[(
            "/src/com/example/AbsentUse.java",
            "\
package com.example;

class AbsentUse {
    void f(java.util.Absent a) {
        a.toString();
    }
}
",
        )],
    ),
);
// Green: `java.util.Absent` is in no directory of the archive, so none of its
// members is ever reported.
