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
