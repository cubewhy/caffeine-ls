//! JLS SE 26 scenario snapshots for classes
//! ([JLS §8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html)):
//! covariant overriding ([§8.4.8.1], [§8.4.5]), field hiding and `super`
//! access ([§8.3.3.2]), constructor chaining via `this(...)` ([§8.8.7.1]) and
//! the illegal forward-reference restriction on field initializers
//! ([§8.3.3]). Red cases render the diagnostics the type layer must report;
//! green cases confirm the class forms type without errors, including
//! constructor delegation via `this(...)` ([§8.8.7.1]) and the
//! illegal-forward-reference restriction on field initializers ([§8.3.3]).
//! An incompatible overridden return type ([§8.4.8.3]) is a declaration-level
//! check rendered by `jls_decl_checks.rs`.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: covariant return in an override ([§8.4.5]) -------------------------

snapshot!(
    covariant_override,
    check_body_types(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

class Base {
    Base self() { return this; }
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

class Derived extends Base {
    Derived self() { return this; }
}
",
        ),
    ])
);

// -- green: field hiding resolved with super and a cast -------------------------

snapshot!(
    field_hiding,
    check_body_types(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

class Base {
    int x = 1;
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

class Derived extends Base {
    String x = \"d\";

    String pick() {
        if (((Base) this).x == 1 && super.x == 1) {
            return x;
        }
        return \"\";
    }
}
",
        ),
    ])
);

// -- green: constructor delegation via this(...) ([§8.8.7.1]) -------------------

snapshot!(
    constructor_this_chain,
    check_body_types(&[(
        "/src/com/example/CtorChain.java",
        "\
package com.example;

class CtorChain {
    int v;

    CtorChain() { this(1); }

    CtorChain(int v) { this.v = v; }
}
",
    )])
);

// -- red: illegal forward reference in a field initializer ([§8.3.3]) ------------
// A simple-name read of a same-class field declared textually later, of the
// same static/instance kind, is illegal; `javac` 25 reports "illegal forward
// reference". The qualified form `this.b` stays legal.

snapshot!(
    illegal_forward_reference,
    check_body_types(&[(
        "/src/com/example/InitOrder.java",
        "\
package com.example;

class InitOrder {
    int a = b;
    int b = 1;
}
",
    )])
);

// -- green: `$` in an identifier is an ordinary character ([§3.8]) --------------
// `A$B` is one identifier, so its constructor is looked up by that simple name
// ([§6.5], [§8.8]): the lambda argument is typed against the resolved formal
// parameter ([§15.9.2]) instead of staying an untyped poly expression.

snapshot!(
    dollar_identifier_class,
    check_body_types(&[(
        "/src/com/example/DollarUse.java",
        "\
package com.example;

class A$B {
    A$B(Runnable task) { task.run(); }
}

class DollarUse {
    void go() {
        new A$B(() -> {});
    }
}
",
    )])
);

// -- green: a nested class whose name contains `$` ([§3.8], [§6.7]) -------------
// Source nesting joins with dots ([§6.7]), so `Outer.Inner$Weird` declares the
// single identifier `Inner$Weird` inside `Outer`; its constructor keeps the
// whole identifier as its name.

snapshot!(
    nested_dollar_identifier,
    check_body_types(&[(
        "/src/com/example/NestedDollar.java",
        "\
package com.example;

class Outer {
    static class Inner$Weird {
        Inner$Weird(Runnable task) { task.run(); }
    }

    void build() {
        new Outer.Inner$Weird(() -> {});
    }
}
",
    )])
);

// -- red: instantiating an abstract class named with `$` ([§8.1.1.1]) -----------
// The diagnostic carries the declared identifier; splitting the name at `$`
// would report a non-existent class instead.

snapshot!(
    abstract_dollar_identifier,
    check_body_types(&[(
        "/src/com/example/AbstractDollar.java",
        "\
package com.example;

abstract class Abs$Tract {
    Abs$Tract() {}
}

class Instantiate {
    void make() {
        new Abs$Tract();
    }
}
",
    )])
);

// -- green: explicit superclass constructor invocation against a library
// superclass ([§8.8.7.1]) -------------------------------------------------------
// Library constructors are classfile `<init>` entries ([JVMS §4.6]); the
// lookup normalizes the target name so `super(args)` resolves over the same
// overload set as `new Super(args)`.

snapshot!(
    library_super_constructor_invocation,
    check_body_types(&[(
        "/src/com/example/Sub.java",
        "\
package com.example;

import java.io.PrintStream;

class Sub extends PrintStream {
    Sub() {
        super(new java.io.ByteArrayOutputStream());
    }
}
",
    )])
);

// -- green: the canonical constructor of a record ([§8.10.4]) -------------------
// A record body may declare an additional constructor delegating with
// `this(...)` to the canonical one, whose parameters mirror the component
// list with generic component types substituted.

snapshot!(
    record_canonical_constructor,
    check_body_types(&[(
        "/src/com/example/Pair.java",
        "\
package com.example;

import java.util.Collections;
import java.util.Iterator;

record Pair(Iterator<String> first, Iterator<String> second) {
    private Pair() {
        this(Collections.emptyIterator(), Collections.emptyIterator());
    }
}
",
    )])
);
