//! JLS SE 26 scenario snapshots for *local* class-like declarations
//! ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)):
//! the modifiers they may carry (the access modifiers and `static` are
//! `modifier {m} not allowed here`, `sealed`/`non-sealed` are
//! `sealed or non-sealed local classes are not allowed`) and their direct
//! supertypes (a local class is never named in a `permits` clause, so a
//! `sealed` direct supertype is `local classes must not extend sealed
//! classes`). Red cases render the diagnostics; green cases confirm the
//! legal forms pass cleanly.

#[macro_use]
mod common;

use crate::common::check_class_diagnostics;

// -- §14.3: legal modifier prefixes -----------------------------------------

snapshot!(
    local_legal_modifiers,
    check_class_diagnostics(&[(
        "/src/com/example/Locals.java",
        "\
package com.example;

class Locals {
    void m() {
        final class Final {}
        abstract class Abstract {}
        strictfp class Strict {}
        @Deprecated class Annotated {}
    }
}
",
    )])
);
// Green: §14.3's grammar is `{ClassModifier} NormalClassDeclaration`, so
// `final`, `abstract`, `strictfp` and an annotation prefix parse and are legal.

// -- §14.3: `public`/`protected`/`private`/`static` -------------------------

snapshot!(
    local_access_and_static_modifiers,
    check_class_diagnostics(&[(
        "/src/com/example/Locals.java",
        "\
package com.example;

class Locals {
    void m() {
        public class Public {}
        protected class Protected {}
        private class Private {}
        static class Static {}
    }
}
",
    )])
);
// Red: §14.3 bans the access modifiers and `static` on a local declaration —
// reported once per offending modifier, at the modifier.

// -- §14.3: `sealed` and `non-sealed` ---------------------------------------

snapshot!(
    local_sealed_modifiers,
    check_class_diagnostics(&[(
        "/src/com/example/Locals.java",
        "\
package com.example;

class Locals {
    void m() {
        sealed class Sealed {}
        non-sealed class NonSealed {}
    }
}
",
    )])
);
// Red: §14.3 bans `sealed` and `non-sealed` on a local declaration — `non-sealed`
// lexes as the three tokens `non - sealed`, and the reported range covers all
// three.

// -- §14.3: a sealed direct supertype ---------------------------------------

snapshot!(
    local_class_extends_sealed,
    check_class_diagnostics(&[(
        "/src/com/example/Locals.java",
        "\
package com.example;

sealed class Shape permits Circle {}
final class Circle extends Shape {}

class Locals {
    void m() {
        class Local extends Shape {}
    }
}
",
    )])
);
// Red: §14.3 forbids a local class whose direct superclass is `sealed` — the
// local declaration can never be named in the sealed type's `permits` clause.

snapshot!(
    local_class_implements_sealed_interface,
    check_class_diagnostics(&[(
        "/src/com/example/Locals.java",
        "\
package com.example;

sealed interface Shape permits Circle {}
final class Circle implements Shape {}

class Locals {
    void m() {
        class Local implements Shape {}
    }
}
",
    )])
);
// Red: the same rule for a *direct superinterface*.

snapshot!(
    local_interface_extends_sealed,
    check_class_diagnostics(&[(
        "/src/com/example/Locals.java",
        "\
package com.example;

sealed interface Shape permits Circle {}
final class Circle implements Shape {}

class Locals {
    void m() {
        interface LocalI extends Shape {}
    }
}
",
    )])
);
// Red: a local *interface*'s direct superinterface may not be `sealed` either
// ([§14.3]).

snapshot!(
    local_class_extends_nonsealed,
    check_class_diagnostics(&[(
        "/src/com/example/Locals.java",
        "\
package com.example;

sealed class Shape permits Unsealed {}
non-sealed class Unsealed extends Shape {}
class Plain {}

class Locals {
    void m() {
        class Base {}
        class Local extends Unsealed {}
        class Other extends Plain {}
        class Sibling extends Base {}
    }
}
",
    )])
);
// Green: only a *sealed* supertype is forbidden — a `non-sealed` or an ordinary
// superclass is fine, and a local class may also extend a sibling local
// declaration.
