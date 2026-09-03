//! JLS SE 26 scenario snapshots for *access control at the use site*
//! ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)):
//! a member (field or method) that exists on the receiver type but is not
//! accessible from the enclosing class — `private` outside its top-level class
//! ([§6.6.1]), `protected` outside its package and not through a subclass
//! ([§6.6.2]), package-private outside its package — is reported as an access
//! violation (javac `report.access`) rather than a missing member. Red cases
//! render the diagnostics body inference must report; green cases confirm
//! legal accesses pass without diagnostics.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- red: private members accessed from another top-level class --------------

snapshot!(
    private_field_method,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Guarded.java",
        "\
package com.example;

class Guarded {
    private int secret;
    private void run() {}
}

class Use {
    void f() {
        Guarded g = new Guarded();
        g.secret;
        g.run();
    }
}
",
    )])
);
// Red: `secret` and `run()` are `private` to `Guarded` and accessed from the
// unrelated top-level class `Use` — both reported as access violations, not as
// missing members (§6.6.1).

snapshot!(
    private_static_field,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Guarded.java",
        "\
package com.example;

class Guarded {
    private static int count;
}

class Use {
    void f() {
        int c = Guarded.count;
    }
}
",
    )])
);
// Red: a `private static` field accessed through its declaring class by name.

// -- green: private members within their own top-level class -----------------

snapshot!(
    same_top_level_private,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Guarded.java",
        "\
package com.example;

class Guarded {
    private int secret;
    private void run() {}

    void use() {
        int s = this.secret;
        run();
    }
}
",
    )])
);
// Green: §6.6.1 scopes private access to the whole top-level class, so the
// enclosing class's own private members are reachable.

// -- red/green: `protected` across packages ([§6.6.2]) -----------------------

snapshot!(
    protected_cross_package,
    check_body_diagnostic_spans(&[
        (
            "/src/holder/Base.java",
            "\
package holder;

public class Base {
    protected int prot;
    public int pub;
}
",
        ),
        (
            "/src/consumer/Use.java",
            "\
package consumer;

import holder.Base;

class Use {
    void f(Base b) {
        int x = b.prot;
        int y = b.pub;
    }
}
",
        ),
    ])
);
// Red/Green: `b.prot` is `protected` in an unrelated package — not accessible
// (§6.6.2) — while the `public` field is.

snapshot!(
    protected_subclass_cross_package,
    check_body_diagnostic_spans(&[
        (
            "/src/holder/Base.java",
            "\
package holder;

public class Base {
    protected int prot;
}
",
        ),
        (
            "/src/consumer/Sub.java",
            "\
package consumer;

import holder.Base;

class Sub extends Base {
    void f() {
        int x = this.prot;
    }
}
",
        ),
    ])
);
// Green: a subclass in a different package may access its superclass's
// `protected` members through a receiver of its own subtype (§6.6.2).

// -- green: an anonymous subclass may invoke a protected constructor ---------
// (§6.6.2 with §15.9.5): `new TypeToken<T>() {}` creates an *anonymous
// subclass* of TypeToken, whose body is responsible for the implementation
// of an object of the subclass — so the protected no-arg constructor is
// accessible even from another package (the Gson supertype-token idiom).

snapshot!(
    anonymous_subclass_protected_constructor,
    check_body_diagnostic_spans(&[
        (
            "/src/holder/TypeToken.java",
            "\
package holder;

public class TypeToken<T> {
    protected TypeToken() {
    }

    protected int prot;
}
",
        ),
        (
            "/src/consumer/Use.java",
            "\
package consumer;

import holder.TypeToken;
import java.util.List;

class Use {
    Object a = new TypeToken<List<String>>() {
    };
    Object b = new TypeToken<List<String>>() {
        int read() {
            return prot;
        }
    };
}
",
        ),
    ])
);
// Green: both the implicit invocation of the protected `TypeToken()`
// constructor for the anonymous subclass, and the access of the inherited
// protected `prot` field from the anonymous body, resolve across the package
// boundary — the anonymous class is a subclass of `TypeToken` (§6.6.2 with
// §15.9.5).

// -- green: a nested class of a subclass may access protected members ---------
// (§6.6.2): the access may appear in the body of a *nested* class of a
// subclass. `B.Inner2` inside `B extends holder.A` calls the inherited
// protected `value` through a receiver of the enclosing subclass's type —
// `B.this.value(...)` — which javac permits even though `Inner2` is not
// itself a subclass of `A`.

snapshot!(
    nested_class_of_subclass_protected_access,
    check_body_diagnostic_spans(&[
        (
            "/src/holder/A.java",
            "\
package holder;

public abstract class A<T> {
    protected T value(String key) {
        return null;
    }
}
",
        ),
        (
            "/src/app/B.java",
            "\
package app;

import holder.A;

public class B extends A<String> {
    class Inner2 {
        String get() {
            return B.this.value(\"y\");
        }
    }
}
",
        ),
    ])
);
// Green: `B.this.value` resolves the protected `A.value` across the package
// boundary — the access site's enclosing chain contains `B`, a subclass of
// `A`, and the receiver is of the subclass's own type (§6.6.2).

// -- red: package-private member across packages -----------------------------

snapshot!(
    package_private_cross_package,
    check_body_diagnostic_spans(&[
        (
            "/src/holder/Base.java",
            "\
package holder;

public class Base {
    int pkg() {
        return 1;
    }
    private void hidden() {}
}
",
        ),
        (
            "/src/consumer/Use.java",
            "\
package consumer;

import holder.Base;

class Use {
    void f(Base b) {
        b.pkg();
        b.hidden();
    }
}
",
        ),
    ])
);
// Red/Green: the package-private method is inaccessible from `consumer`
// (§6.6.1); the `private` method reports its own access violation.

snapshot!(
    same_package_package_private,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Same.java",
        "\
package com.example;

class A {
    void pkg() {}
    int field;
}

class B {
    void f() {
        A a = new A();
        a.pkg();
        int x = a.field;
    }
}
",
    )])
);
// Green: package-private members of a same-package top-level class are
// accessible from another top-level class in the same package.

// -- green: inaccessible members are not confused with missing ones ----------

snapshot!(
    missing_member_still_reported,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Guarded.java",
        "\
package com.example;

class Owner {
    private int secret;
}

class Use {
    void f() {
        Owner o = new Owner();
        o.missing;
        o.gone();
    }
}
",
    )])
);
// Green: a member that truly does not exist stays a no-such-member error — only
// members that exist (but are hidden by access control) become access
// violations.
