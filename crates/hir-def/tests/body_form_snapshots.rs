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
