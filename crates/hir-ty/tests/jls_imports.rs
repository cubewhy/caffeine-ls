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

use crate::common::check_class_diagnostics;

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
