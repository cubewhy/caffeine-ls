//! JLS SE 26 scenario snapshots for the package-declaration vs filesystem
//! path consistency check ([JLS §7.2.1]).
//!
//! A compilation unit that declares a package must sit in a directory chain
//! — under its source root — that equals that package: that is what makes the
//! class resolve by its fully qualified name on a conventional classpath.
//! javac compiles a misplaced file fine (it is a javadoc/`-d` convention), so
//! the diagnostic carries a custom code (`package-path-mismatch`), surfaced as
//! an error by the IDE.
//!
//! The check anchors each file's package directory to the source root's
//! longest common directory prefix. A root whose files all share one
//! directory degenerates to the file's own parent, which is skipped — that
//! leaves the single-package test fixtures (and single-file roots) free of
//! false positives.
//!
//! [JLS §7.2.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.2.1

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- green: the file sits in the directory chain of its declared package -------

snapshot!(
    matching_path,
    check_class_diagnostics(&[
        (
            "/src/com/example/A.java",
            "\
package com.example;

public class A {}
",
        ),
        (
            "/src/com/example/sub/B.java",
            "\
package com.example.sub;

public class B {}
",
        ),
        (
            "/src/default/DefaultPkg.java",
            "\
public class DefaultPkg {}
",
        ),
    ])
);
// Each declared package matches the file's directory under the source root
// (`/src`): `com.example` ↔ `com/example`, `com.example.sub` ↔
// `com/example/sub`. A default-package file is exempt ([JLS §7.4.2]).

// -- red: the declared package does not match the file path --------------------

snapshot!(
    mismatched_declared_package,
    check_class_diagnostics(&[
        (
            "/src/com/example/A.java",
            "\
package com.example;

public class A {}
",
        ),
        (
            "/src/org/other/Misplaced.java",
            "\
package com.example;

public class Misplaced {}
",
        ),
    ])
);
// `Misplaced` lives under `org/other` but claims `com.example` ([JLS §7.2.1]):
// on a conventional classpath nothing looks for it there, so it is reported
// against the package declaration's name.

snapshot!(
    wrong_depth,
    check_class_diagnostics(&[
        (
            "/src/com/example/A.java",
            "\
package com.example;

public class A {}
",
        ),
        (
            "/src/com/example/sub/B.java",
            "\
package com.example.sub;

public class B {}
",
        ),
        (
            "/src/com/example/sub/Shim.java",
            "\
package com.example;

public class Shim {}
",
        ),
    ])
);
// `Shim` declares only `com.example` but sits one level deeper; the segment
// chains still differ, so the mismatch is reported.

// -- green: module-info is exempt ([JLS §7.7]) ----------------------------------

snapshot!(
    module_info_exempt,
    check_class_diagnostics(&[
        (
            "/src/com/example/A.java",
            "\
package com.example;

public class A {}
",
        ),
        (
            "/src/module-info.java",
            "\
module com.example.app {
    requires java.base;
}
",
        ),
    ])
);
// A module declaration names no package ([JLS §7.7]); its location follows the
// JPMS convention (`module-info.java` directly under the source root), not the
// package-directory rule, so it must not be reported.

// -- red: the message renders the root-relative package directory ----------------

snapshot!(
    root_relative_dotted_path,
    check_class_diagnostics(&[
        (
            "/proj/src/main/java/org/example/Widget.java",
            "\
package org.example;

public class Widget {}
",
        ),
        (
            "/proj/src/main/java/com/acme/Other.java",
            "\
package com.acme;

public class Other {}
",
        ),
        (
            "/proj/src/main/java/org/example/Unmatched.java",
            "\
package org.example.unmatched.pkg;

public class Unmatched {}
",
        ),
    ])
);
// The source root's base (the longest common directory prefix of its files)
// is `/proj/src/main/java`, so the misplaced file's package directory renders
// root-relative and dotted: `org.example`. The declared package `org.example
// .unmatched.pkg` cannot be a suffix of that directory, so the check fires
// with the IntelliJ-style message ([JLS §7.2.1]).
