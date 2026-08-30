//! JLS SE 26 scenario snapshots for `this`/`super` and unqualified instance
//! access in *static* contexts ([JLS §8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3),
//! [§15.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.3),
//! [§15.8.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.4),
//! [§15.11](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11)):
//! a static method body, a static field initializer, a static initializer and
//! an enum constant have no enclosing instance, so `this`, `super` and any
//! simple-name reference to an instance member of the implicit receiver are
//! compile-time errors (javac's `non-static … cannot be referenced from a
//! static context`). Instance methods and the implicit receiver's fields are
//! still reachable through an *explicit* value receiver or a static import.
//!
//! The renderer ([`check_body_diagnostic_spans`]) prints full source ranges
//! with the covered text, so the exact keyword/name being flagged is asserted.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- red: explicit `this` in a static context --------------------------------

snapshot!(
    this_in_static_method,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
class Main {
    int x;
    void m() {}

    static void test() {
        this.x = 1;
        this.m();
        int y = this.x;
    }
}
",
    )])
);
// §15.8.3/[§8.1.3]: `this` in a static method body is a compile-time error,
// whether the member reached through it is a field (`this.x`) or a method
// (`this.m()`); the keyword itself is flagged each time.

snapshot!(
    this_in_static_initializer,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
class Main {
    static int s = this.x;
    static int x = 0;

    static {
        int y = this.x;
    }
}
",
    )])
);
// §8.1.3: a static field initializer and a static initializer are static
// contexts; `this` is rejected in both.

snapshot!(
    qualified_this_in_static,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
class Main {
    static void test() {
        Main.this;
        new Main().test();
    }
}
",
    )])
);
// §15.8.3: the *qualified* form `TypeName.this` is equally a compile-time
// error from a static context; a fresh instance's method call stays legal.

// -- red: explicit `super` in a static context -------------------------------

snapshot!(
    super_in_static,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
class Base {
    int x;
    void m() {}
}

class Main extends Base {
    static void test() {
        super.m();
        int y = super.x;
    }
}
",
    )])
);
// §15.8.4/[§8.1.3]: `super` names the enclosing instance and is rejected
// from a static context — both `super.m()` (an invocation whose receiver
// bypasses expression inference) and `super.x` (a field access whose receiver
// does too).

snapshot!(
    qualified_super_in_static,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
interface I {
    default void d() {}
}

class Main implements I {
    static void test() {
        I.super.d();
    }
}
",
    )])
);
// §15.11.2: the qualified-super invocation `I.super.d()` selects the
// interface's default method through the enclosing instance, which a static
// context does not have — rejected like `this`.

// -- red: unqualified instance-field access in a static context --------------

snapshot!(
    instance_field_simple_name,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
class Main {
    int x;
    static int sx;
    static int helper = x;
    static int helper2 = sx;

    static void test() {
        int a = x;
        x = 1;
        int b = sx;
    }

    void use() {
        int c = x;
    }
}
",
    )])
);
// §15.11/[§8.1.3]: a simple-name read *and* write of an instance field of the
// implicit receiver is rejected from a static context (field initializer,
// initializer block and static method), while a static field by simple name
// stays legal everywhere and an instance method body may read it freely.

snapshot!(
    instance_field_through_value_receiver,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
class Main {
    int x;
    static int helper() {
        Main m = new Main();
        return m.x;
    }
}
",
    )])
);
// A *qualified* access through a value receiver is a virtual access ([§15.11])
// and stays legal from a static context: only the implicit-receiver form is
// rejected.

// -- green: static contexts stay clean ----------------------------------------

snapshot!(
    static_context_clean_this_and_fields,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
import static java.lang.Math.max;

class Main {
    static int x;
    static int y = x;

    static void test() {
        x = max(1, 2);
        Main m = new Main();
        m.x = 3;
        int z = x;
    }
}
",
    )])
);
// A static field by simple name, a statically imported static method
// ([§7.5.4]), and a qualified access through a fresh instance are all legal
// in a static context — no diagnostics.

// -- red: static context inside an enum constant ------------------------------

snapshot!(
    enum_constant_static_context,
    check_body_diagnostic_spans(&[(
        "/src/Main.java",
        "\
enum Color {
    RED(this.name()),
    GREEN;

    Color(String n) {}

    String name() { return null; }

    static void test() {
        int y = this.ordinal();
    }
}
",
    )])
);
// §8.1.3: an enum constant's argument list is a static context, so `this`
// there is rejected (javac rejects `this` in an enum constant argument);
// likewise in a static method of the enum.
