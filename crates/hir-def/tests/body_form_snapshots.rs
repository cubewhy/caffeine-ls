//! Snapshots of the lowered *body* IR
//! ([`hir_expand::body::BodyTree`]): every statement
//! ([JLS §14](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html))
//! and expression form
//! ([JLS §15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html))
//! of method bodies, initializers, field initializers, annotation element
//! defaults and enum constant arguments.

#[macro_use]
mod common;

// -- statements --------------------------------------------------------------

body_snapshot! {
    statements,
    r#"
class Foo {
    void m() {
        ; // empty
        int a = 1;
        long b = 2L, c = 3L;
        String s = "x";
        foo();
        a++;
        ++a;
        --a;
        a--;
        a += 2;
        a -= 1;
        if (a > 0) { a = 0; } else { a = 1; }
        while (a < 10) { a++; }
        do { a--; } while (a > 0);
        for (int i = 0; i < 3; i++) { a += i; }
        for (int x : new int[] { 1, 2 }) { a += x; }
        switch (a) {
            case 0: a = 1; break;
            case 1: a = 2; break;
            default: a = 0;
        }
        switch (a) {
            case 0 -> a = 1;
            case 1 -> { a = 2; }
            default -> throw new RuntimeException();
        }
        return;
    }
    int v(int x) {
        if (x) throw new RuntimeException();
        return x + 1;
    }
}
"#,
}

body_snapshot! {
    control_flow,
    r#"
class Foo {
    void m(int a) {
        lbl: for (int i = 0; ; i++) {
            if (i == 3) break lbl;
            if (i % 2 == 0) continue;
            assert a > 0 : "msg";
            synchronized (this) { a++; }
            try {
                risky();
            } catch (Exception e) {
                a = 1;
            } finally {
                a = 2;
            }
        }
    }
    void n() throws Exception { }
}
"#,
}

body_snapshot! {
    expressions,
    r#"
class Foo {
    int m() {
        int a = 1 + 2 * 3;
        long b = 1L << 2;
        boolean c = a > 0 && b >= 2 || !c;
        int d = (int) b;
        int e = c ? 1 : 2;
        int[] arr = new int[3];
        int f = arr[1];
        int g = (a).getClass().hashCode();
        String h = "a" + "b";
        Object i = "x" instanceof String;
        Runnable r = (int x, int y) -> x + y;
        Object j = Runnable::run;
        Foo k = new Foo();
        int l = this.m();
        return a;
    }
}
"#,
}

body_snapshot! {
    initializers,
    r#"
class Foo {
    int a = 1;
    int[] b = { 1, 2, 3 };
    int[] c = new int[] { 1, 2, 3 };
    int[] d = new int[3];
    String s = "x" + "y";
    static { int x = 1; }
    { int y = 2; }
    enum E { A(1), B(2) }
    @interface Anno { int value() default 1; }
}
"#,
}

// -- quick wins (JLS SE 26) --------------------------------------------------

// Try-with-resources ([JLS §14.20.3]) lowers each resource to a real local
// with its initializer; a bare `VARIABLE_ACCESS` resource (no initializer) is
// lowered to the local alone.

body_snapshot! {
    try_with_resources,
    r#"
class Foo {
    void m() throws Exception {
        try (FileReader r = new FileReader("x"); var w = new FileWriter("y")) {
            r.read();
        } catch (Exception e) {
            e.printStackTrace();
        }
    }
}
"#,
}

// `instanceof` patterns ([JLS §14.30.2]) and switch pattern labels
// ([§14.11.1]) lower to `PatternData::Type`/`Record`/`MatchAll`.

body_snapshot! {
    patterns,
    r#"
class Foo {
    void m(Object o) {
        if (o instanceof String s) {
            System.out.println(s);
        }
        if (o instanceof Point(int x, int y)) { }
        switch (o) {
            case Point p -> p.area();
            case String _ -> { }
            case null -> { }
            default -> { }
        }
    }
}

record Point(int x, int y) {
    int area() { return x * y; }
}
"#,
}

// String templates ([JLS §15.8.6, preview in JLS 22, removed in JLS 23]) lower
// `STR."\{x}"` (a `FIELD_ACCESS` wrapping a `TEMPLATE_EXPR`) to a template
// expression whose arguments are inferred.

body_snapshot! {
    string_templates,
    r#"
class Foo {
    void m(int x) {
        STR."value: \{x}";
    }
}
"#,
}

// Diamond instantiation ([JLS §15.9.1]) — `new Foo<>()` — is marked on the
// `New` expression so inference can substitute the target's type arguments.

body_snapshot! {
    diamond_new,
    r#"
class Foo<T> {
    void m() {
        new Foo<>();
    }
}
"#,
}

// Local class and record declarations
// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
// are carried as named statements, and the member methods of an anonymous
// class body ([JLS §15.9.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.5))
// are carried by name and arity on the `new` expression — neither form is
// dropped from the lowered body.

body_snapshot! {
    local_and_anonymous_types,
    r#"
class Foo {
    void m() {
        class Local {}
        record Pair(int a, int b) {}
        new Runnable() {
            public void run() {}
        };
    }
}
"#,
}
