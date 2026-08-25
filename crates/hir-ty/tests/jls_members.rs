//! Regression snapshots for real-world resolution and invocation behavior:
//! `super(...)` constructor delegation ([JLS §8.8.7.1]), member types of
//! enclosing declarations resolved by simple name ([§6.5.5.1], [§8.1]),
//! varargs parameters read as arrays in the body ([§8.4.1]), and the
//! most-specific method selection across inherited, covariantly-overridden
//! and primitive-widening candidates ([§15.12.2.5]).

#[macro_use]
mod common;

use crate::common::{
    check_body_types, check_class_diagnostics, check_methods, check_resolve_src,
    check_source_methods,
};
use syntax::stub::PrimitiveType;

// -- green: `super(args)` resolves against the direct superclass --------------

snapshot!(
    super_constructor_delegation,
    check_body_types(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

class Base {
    int tag;
    Base(int tag) { this.tag = tag; }
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

class Derived extends Base {
    Derived(int tag) {
        super(tag);
    }
}
",
        ),
    ])
);

// §8.8.7.1: an explicit `super(args)` invocation selects a constructor of
// the direct superclass — not of the enclosing class. Before the CtorCall
// target was lowered, `super(...)` degraded to a missing-name method call.

snapshot!(
    super_no_matching_constructor,
    check_body_types(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

class Base {
    Base(int tag) {}
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

class Derived extends Base {
    Derived() {
        super();
    }
}
",
        ),
    ])
);

// Red case: `Base` has only `Base(int)`; the no-arg `super()` finds no
// applicable constructor (`cant.resolve.location`, javac's
// `compiler.err.cant.resolve.location` on the superclass ctor lookup).

// -- green: member types of enclosing declarations by simple name -------------

snapshot!(
    enclosing_member_type,
    check_resolve_src(
        r#"
package com.example;

class Outer {
    Inner inner;
    Request make() { return new Request(); }

    class Inner {}

    interface Request {}
}
"#,
    ),
);

// §6.5.5.1: a simple type name may denote a member type of an enclosing
// declaration; such members shadow single-type imports and same-package
// types ([§6.4.1]). Before this step existed, `Inner`/`Request` degraded to
// `com.example.Inner`.

snapshot!(
    nested_member_type_chain,
    check_resolve_src(
        r#"
package com.example;

class A {
    class B {
        class C {}
    }

    B.C field;
}
"#,
    ),
);

// §6.7: the canonical name of a nested type joins every enclosing simple
// name with `.`; resolution probes `com.example.A.B.C`.

// -- green/red: varargs parameters are arrays in the body ([§8.4.1]) ----------

snapshot!(
    varargs_parameter_is_array,
    check_body_types(&[(
        "/src/com/example/Varargs.java",
        "\
package com.example;

class Varargs {
    int total(String... names) {
        return names.length;
    }
}
",
    ),])
);

// §8.4.1: `T... last` is exactly equivalent to `T[] last`; in the body the
// parameter's type is the array type, so `names.length` (§10.7) types as
// `int`. Before the wrap existed, `names` typed as `String` and `.length`
// reported `cant.resolve.location`.

// -- most-specific selection ([§15.12.2.5]) -----------------------------------

snapshot!(
    inherited_covariant_override_not_ambiguous,
    check_source_methods(
        &[
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
    void onlyDerived() {}
}
",
            ),
        ],
        &[
            (
                "receiver-derived-self",
                |db| hir_ty::Ty::reference(db, "com.example.Derived", Vec::new()),
                "self",
                &[],
            ),
            (
                "receiver-base-self",
                |db| hir_ty::Ty::reference(db, "com.example.Base", Vec::new()),
                "self",
                &[],
            ),
        ]
    )
);

// §15.12.2.5/§8.4.8.1: a covariant override leaves two applicable members
// with identical formals but different returns; they are one method seen
// through overriding paths, and the most-derived declaring type wins.
// Treating both as equally specific used to report an ambiguity.

snapshot!(
    primitive_widening_most_specific,
    check_methods(&[
        (
            "append-string",
            |db| hir_ty::Ty::reference(db, "java.lang.StringBuilder", Vec::new()),
            "append",
            &[|db| hir_ty::Ty::reference(db, "java.lang.String", Vec::new())],
        ),
        (
            "valueof-int",
            |db| hir_ty::Ty::reference(db, "java.lang.String", Vec::new()),
            "valueOf",
            &[|db| hir_ty::Ty::primitive(db, PrimitiveType::Int)],
        ),
        (
            "append-char",
            |db| hir_ty::Ty::reference(db, "java.lang.StringBuilder", Vec::new()),
            "append",
            &[|db| hir_ty::Ty::primitive(db, PrimitiveType::Char)],
        ),
    ])
);

// §15.12.2.5: among applicable overloads whose formals widen one another
// (`int` → `long` → …), the most specific wins by the primitive order of
// [§4.10.1]. Without it, `StringBuilder.append(char)` (overridden along
// `AbstractStringBuilder`) and `String.valueOf(int)` collapsed to ambiguity.

// -- declaration-level view of the same scenarios ------------------------------

snapshot!(
    ctor_delegation_class_diagnostics,
    check_class_diagnostics(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

class Base {
    Base(int tag) {}
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

class Derived extends Base {
    Derived() {
        super(1);
    }
}
",
        ),
    ])
);

// -- green: interface members are implicitly public ([§9.4]) -------------------

snapshot!(
    interface_members_implicitly_public,
    check_class_diagnostics(&[
        (
            "/src/com/example/pkg/Conn.java",
            "\
package com.example.pkg;

public interface Conn {
    Conn url(String u);
}
",
        ),
        (
            "/src/com/example/other/Impl.java",
            "\
package com.example.other;

import com.example.pkg.Conn;

public class Impl implements Conn {
    @Override
    public Conn url(String u) { return this; }
}
",
        ),
    ])
);

// §9.4: interface methods are implicitly `public` whether or not the modifier
// is spelled out; an implementation in another package overrides them, and
// `@Override` sees the super declaration.

// -- green/red: enum constants are public static fields ([§8.9.2]) --------------

snapshot!(
    enum_constant_static_field,
    check_body_types(&[
        (
            "/src/com/example/Syntax.java",
            "\
package com.example;

public enum Syntax {
    xml, html;
}
",
        ),
        (
            "/src/com/example/Use.java",
            "\
package com.example;

import static com.example.Syntax.xml;

class Use {
    boolean isXml(com.example.Syntax syntax) {
        return syntax == xml;
    }
}
",
        ),
    ])
);

// §8.9.2: each enum constant is an implicitly `public static final` field of
// the enum type — readable unqualified inside the enum, and through a static
// import anywhere ([§7.5.4]). Before constants surfaced as fields, both forms
// reported `cant.resolve.location`.

// -- green: anonymous class on an interface ([§15.9.5]) -------------------------

snapshot!(
    anonymous_interface_implementation,
    check_body_types(&[
        (
            "/src/com/example/Conn.java",
            "\
package com.example;

public interface Conn {
    Conn url(String u);
}
",
        ),
        (
            "/src/com/example/Anon.java",
            "\
package com.example;

class Anon {
    Conn make() {
        return new Conn() {
            public Conn url(String u) { return this; }
        };
    }
}
",
        ),
    ])
);

// §15.9.5: `new Interface() { body }` creates an anonymous class implementing
// the interface — legal despite the interface not being instantiable.

// -- green: nested invocation as a poly argument ([§18.5.2.4]) ------------------

snapshot!(
    nested_invocation_argument_overloads,
    check_body_types(&[
        (
            "/src/com/example/Assert.java",
            "\
package com.example;

class Assert {
    static void assertEquals(int expected, int actual) {}
    static void assertEquals(long expected, long actual) {}
    static void assertEquals(Object expected, Object actual) {}
}
",
        ),
        (
            "/src/com/example/Use.java",
            "\
package com.example;

import java.util.List;

import static com.example.Assert.assertEquals;

class Use {
    void t(List<String> list) {
        assertEquals(3, list.size());
    }
}
",
        ),
    ])
);

// §18.5.2.4: a method invocation argument is a poly expression resolved in
// the *same* phase as its enclosing invocation — `list.size()` (`int`)
// constrains ⟨int → int⟩ and picks `(int, int)`; the boxed-formal overload
// must not appear strictly applicable.

// -- green: `@Override` sees through nested if/else-if chains ([§16]) ----------

snapshot!(
    definite_assignment_else_if_chain,
    check_body_types(&[(
        "/src/com/example/Parse.java",
        "\
package com.example;

import java.io.IOException;

class Parse {
    int css(String arg) throws IOException {
        final int step, offset;
        if (arg.length() == 3) {
            step = 2;
            offset = 1;
        } else {
            if (arg.length() == 4) {
                step = 5;
                offset = 0;
            } else if (arg.length() > 2) {
                step = 0;
                offset = 1;
            } else {
                throw new IOException(\"bad\");
            }
        }
        return step + offset;
    }
}
",
    ),])
);

// §16.1.8/§14.21: every branch of the chain either assigns both locals or
// completes abruptly (`throw`), so both are definitely assigned at the read.

snapshot!(
    try_catch_throwing_catch_da,
    check_body_types(&[(
        "/src/com/example/Reader.java",
        "\
package com.example;

import java.io.IOException;

class Reader {
    String read() throws IOException { return \"x\"; }

    int len(java.io.Reader input) throws IOException {
        final String doc;
        try (java.io.Reader r = input) {
            doc = read();
        } catch (java.io.UncheckedIOException e) {
            throw e.getCause();
        }
        return doc.length();
    }
}
",
    ),])
);

// §16.2.15: a `catch` clause that itself completes abruptly contributes no
// path to the definite-assignment join — assignments in the `try` block
// reach the code after the statement.

// -- green: static fields of enclosing declarations by simple name --------------

snapshot!(
    enclosing_static_field_nested_class,
    check_resolve_src(
        r#"
package com.example;

class Outer {
    static final int LIMIT = 8;

    class Inner {
        int doubled() { return LIMIT * 2; }
    }
}
"#,
    ),
);

// §6.5.5.1/§8.3: a simple name may denote a static member of any enclosing
// declaration, not only the innermost one.

// -- green: instance fields of an enum body are visible to its members ----------
// §8.9.2: an enum body declares fields exactly like a class body; the
// constructor and instance methods read them by simple name and via `this`.

snapshot!(
    enum_instance_fields,
    check_body_types(&[(
        "/src/com/example/Sep.java",
        "\
package com.example;

enum Sep {
    A(\"#\"),
    B(System.lineSeparator());

    private final String sep;

    Sep(String s) {
        sep = s;
    }

    public int length() {
        return sep.length();
    }
}
",
    )])
);

// §8.9.3: every enum type has the implicit static members `values()` and
// `valueOf(String)` — usable both qualified (`Sep.values()`) and by simple
// name inside the enum itself.

snapshot!(
    enum_implicit_members,
    check_body_types(&[(
        "/src/com/example/Color.java",
        "\
package com.example;

enum Color {
    RED, GREEN;

    public static int total() {
        return values().length + valueOf(\"RED\").ordinal() + Color.GREEN.name().length();
    }
}
",
    )])
);

// -- green: record components synthesize fields and accessors -------------------
// §8.10.3: each component has a private field and a public accessor named
// after it — reads inside the record body and accessor calls on values
// resolve through the synthesis.

snapshot!(
    record_component_members,
    check_body_types(&[(
        "/src/com/example/Pt.java",
        "\
package com.example;

record Pt(int x, int y) {
    double dist() {
        return Math.sqrt(x * x + y * y);
    }
}

class UsePt {
    double use(Pt p) {
        return p.x() + p.y();
    }
}
",
    )])
);

// -- green: a member type inherited from a superclass is in scope ---------------
// §6.5.5.1: `Sub` may name `Super.Mode` by simple name, qualified through the
// subclass, or through a value of the subclass.

snapshot!(
    inherited_member_type,
    check_body_types(&[
        (
            "/src/com/example/Base.java",
            "\
package com.example;

public class Base {
    public enum Mode { ON, OFF }
}
",
        ),
        (
            "/src/com/example/Derived.java",
            "\
package com.example;

public class Derived extends Base {
    int use(Derived d) {
        Mode m = Mode.ON;
        if (d == null) {
            m = Base.Mode.OFF;
        }
        return m == Mode.ON ? 1 : 0;
    }
}
",
        ),
    ])
);
