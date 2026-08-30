//! JLS SE 26 scenario snapshots for *JPMS module directives*
//! ([JLS §7.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7)):
//! a `requires` of a module that is not on the module path is
//! `ModuleNotFound` ([§7.7.1]); an `exports`/`opens` of a package with no
//! source files in the module is `PackageEmptyOrNotFound` ([§7.7.1]); and a
//! `provides` implementation that is not a subtype of its service interface
//! is `ServiceImplementationNotSubtype` ([§7.7.2]). Red cases render the
//! diagnostics; green cases confirm legal modules pass cleanly.

#[macro_use]
mod common;

use crate::common::check_module_diagnostics;

// -- §7.7.1: requires an unknown module ---------------------------------------

snapshot!(
    module_not_found,
    check_module_diagnostics(&[(
        "/src/module-info.java",
        "\
module m1 {
    requires missing.module;
}
",
    )])
);
// Red: `missing.module` is not on the module path — no source module and no
// classpath library declares it.

// -- §7.7.1: exports / opens an empty or unknown package ----------------------

snapshot!(
    package_empty_or_not_found,
    check_module_diagnostics(&[
        (
            "/src/module-info.java",
            "\
module m1 {
    exports com.nonexistent;
    opens com.example;
}
",
        ),
        (
            "/src/com/example/Service.java",
            "\
package com.example;

public interface Service {
}
",
        )
    ])
);
// Red: `com.nonexistent` has no source files in the module; the `opens` of the
// existing `com.example` is fine (an `open` package only restricts reflection).

// -- §7.7.2: provides implementation not a subtype ----------------------------

snapshot!(
    service_implementation_not_subtype,
    check_module_diagnostics(&[
        (
            "/src/module-info.java",
            "\
module m1 {
    provides com.example.Service with com.example.BadImpl;
}
",
        ),
        (
            "/src/com/example/Service.java",
            "\
package com.example;

public interface Service {
}
",
        ),
        (
            "/src/com/example/BadImpl.java",
            "\
package com.example;

public class BadImpl {
}
",
        )
    ])
);
// Red: `BadImpl` is a plain class, not a subtype of the `Service` interface.

// -- green: legal module directives -------------------------------------------

snapshot!(
    legal_module,
    check_module_diagnostics(&[
        (
            "/src/module-info.java",
            "\
module m1 {
    exports com.example;
    uses com.example.Service;
    provides com.example.Service with com.example.GoodImpl;
}
",
        ),
        (
            "/src/com/example/Service.java",
            "\
package com.example;

public interface Service {
}
",
        ),
        (
            "/src/com/example/GoodImpl.java",
            "\
package com.example;

public class GoodImpl implements Service {
}
",
        )
    ])
);
// Green: the exported package has a source file, and the `provides`
// implementation is a subtype of its service.
