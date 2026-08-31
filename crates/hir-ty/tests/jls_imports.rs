//! JLS SE 26 scenario snapshots for the import directives of a compilation
//! unit ([JLS §7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5)):
//! single-type imports ([§7.5.1]) and on-demand imports ([§7.5.2]).
//!
//! Single-type-import validation (the imported class must exist, and two
//! imports of the same simple name conflict) ships with the resolution
//! snapshots; this suite covers the on-demand-import checks that resolve
//! against packages rather than classes:
//!
//! - the *package* of `import pkg.*;` must exist ([§7.5.2]) — javac rejects
//!   `import java.*;` with `package java does not exist`;
//! - the *declaring type* of `import static pkg.Type.*;` must exist
//!   ([§7.5.4], [§7.5.2]).

#[macro_use]
mod common;

use crate::common::{
    ClassSpec, check_body_types_with_libs, check_class_diagnostics, class_with_methods_access,
};

// -- green: on-demand imports that resolve against the classpath ---------------

snapshot!(
    clean_on_demand,
    check_class_diagnostics(&[(
        "/src/com/example/Imports.java",
        "\
package com.example;

import com.example.*;
import java.util.*;
import java.io.*;
import static java.lang.Math.*;
import static java.util.Collections.*;

class Imports {}
",
    )])
);
// The package `com.example` is the file's own package (observable through its
// own sources); `java.util`/`java.io` are jars on the classpath. The static
// on-demand imports name `java.lang.Math` and `java.util.Collections`, which
// the JDK fixture provides ([JLS §7.5.4]).

// -- red: an on-demand import of an unknown package ([§7.5.2]) -----------------

snapshot!(
    unknown_package,
    check_class_diagnostics(&[(
        "/src/com/example/Imports.java",
        "\
package com.example;

import com.nonexistent.pkg.*;
import java.*;

class Imports {}
",
    ),])
);
// Neither `com.nonexistent.pkg` nor `java` carries a class on the classpath,
// so each on-demand import is a javac `package … does not exist` error.

snapshot!(
    unknown_package_multiple_files,
    check_class_diagnostics(&[
        (
            "/src/com/example/Widget.java",
            "\
package com.example;

public class Widget {}
",
        ),
        (
            "/src/com/example/Imports.java",
            "\
package com.example;

import com.missing.dep.*;

class Imports {
    void m() {
        com.missing.dep.Helper h = new com.missing.dep.Helper();
    }
}
",
        ),
    ])
);
// The on-demand import names a package with no classes anywhere on the
// classpath — the import itself is an error ([JLS §7.5.2]).

// -- red: a static on-demand import of an unknown type ([§7.5.4]) --------------

snapshot!(
    unknown_static_import_type,
    check_class_diagnostics(&[(
        "/src/com/example/Imports.java",
        "\
package com.example;

import static bogus.Type.*;
import static java.lang.MissingUtility.*;

class Imports {}
",
    )])
);
// The declaring types `bogus.Type` and `java.lang.MissingUtility` do not
// exist; javac reports `cannot find symbol: class`. `bogus` is additionally
// not even a package, but the static-import shape validates the *type* first.

snapshot!(
    static_import_type_source_resolves,
    check_class_diagnostics(&[
        (
            "/src/com/example/Widget.java",
            "\
package com.example;

public class Widget {
    public static final int answer = 42;
}
",
        ),
        (
            "/src/com/example/Imports.java",
            "\
package com.example;

import static com.example.Widget.*;

class Imports {
    int a = answer;
}
",
        ),
    ])
);
// The declaring type is a same-package source class, so the static on-demand
// import resolves and nothing is reported ([JLS §7.5.4]).

// -- §7.5.2: an on-demand import imports only *accessible* types ---------------
// A package-private class of another package is not a candidate, so a simple
// name that only the inaccessible class supplies is not ambiguous — it
// resolves to the accessible one.

snapshot!(
    on_demand_import_ignores_inaccessible,
    check_body_types_with_libs(
        &[
            class_with_methods_access(
                "org/objectweb/asm/tree/analysis/Frame",
                Some("java/lang/Object"),
                &[],
                &[("getStackSize", "()I")],
                &[""],
                &[0x0001], // ACC_PUBLIC
            ),
            class_with_methods_access(
                "org/objectweb/asm/tree/analysis/SourceValue",
                Some("java/lang/Object"),
                &[],
                &[],
                &[],
                &[],
            ),
            // A package-private class (`ACC_PUBLIC` unset) named `Frame` in
            // another package must not be a candidate of `import a.*`.
            ClassSpec {
                fqn: "org/objectweb/asm/Frame",
                super_class: Some("java/lang/Object"),
                interfaces: &[],
                access: 0x0020, // ACC_SUPER, package-private (no ACC_PUBLIC)
                fields: &[],
                methods: &[],
                method_sigs: &[],
                method_access: &[],
                sig: None,
            },
        ],
        &[(
            "/src/com/example/Body.java",
            "\
package com.example;

import org.objectweb.asm.tree.analysis.Frame;
import org.objectweb.asm.tree.analysis.SourceValue;

class Body {
    Frame<SourceValue>[] frames;
    int m() {
        return frames[0].getStackSize();
    }
}
",
        )],
    )
);
// Green: the only *accessible* `Frame` is the public `tree.analysis.Frame`; the
// package-private `org.objectweb.asm.Frame` (same simple name) is not a
// candidate, so the simple name resolves unambiguously ([§7.5.2]).
