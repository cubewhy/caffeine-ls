//! Snapshots of expression-level type inference
//! ([JLS §15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html),
//! [§14.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.4))
//! over the body IR lowered from source methods.

#[macro_use]
mod common;

use crate::common::check_body_types;

snapshot!(
    literals,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int literals() {
        int i = 42;
        long l = 42L;
        char c = 'x';
        float f = 1.5f;
        double d = 1.5;
        boolean b = true;
        String s = \"hi\";
        Object o = null;
        Class<?> clazz = String.class;
        return i;
    }
}
",
    )])
);

snapshot!(
    arithmetic,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int arithmetic(byte a, short b, char c, long lo) {
        int i = a + b;
        long j = a + lo;
        double d = 1 + 2.5;
        float f = a + 1.0f;
        int neg = -a;
        int shl = a << 2;
        boolean big = a > b;
        boolean eq = a == b;
        String s = \"x\" + i;
        return i;
    }
}
",
    )])
);

snapshot!(
    boxing_numeric_promotion,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int boxing(java.lang.Integer i, java.lang.Long l, java.lang.Character c) {
        int a = i + 1;
        int b = i - i;
        int d = -i;
        long e = i + l;
        int f = i + c;
        boolean big = i > l;
        int cond = true ? i : i;
        long mix = true ? i : l;
        return a + b + d + f;
    }
}
",
    )])
);

snapshot!(
    for_each_iterable,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    int forEach(List<String> xs, java.lang.Iterable<Integer> ys, String[] as, int[] ns) {
        int sum = 0;
        for (String x : xs) {
            sum += x.length();
        }
        for (Integer y : ys) {
            sum += y;
        }
        for (String a : as) {
            sum += a.length();
        }
        for (int n : ns) {
            sum += n;
        }
        return sum;
    }
}
",
    )])
);

snapshot!(
    new_array_initializer,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int[] newArray() {
        int[] a = new int[] { 1, 2, 3 };
        String[] b = new String[] { \"x\", \"y\" };
        int[][] c = new int[][] { { 1 }, { 2, 3 } };
        int[] d = new int[3];
        return a;
    }
}
",
    )])
);

snapshot!(
    locals_and_fields,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int count = 10;
    static Body singleton = new Body();

    int run(int[] values, int index) {
        int acc = 0;
        for (int i = 0; i < values.length; i++) {
            acc = acc + values[i];
        }
        return acc + this.count;
    }

    int arrays() {
        int[][] grid = new int[3][4];
        int[] row = grid[1];
        int[] init = { 1, 2, 3 };
        return init[0] + row[1];
    }

    int conditional(int x) {
        return x > 0 ? x : -x;
    }
}
",
    )])
);

snapshot!(
    calls,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int identity(int x) {
        return x;
    }

    String concat(String a, String b) {
        return a + b;
    }

    int call() {
        int x = identity(7);
        String s = concat(\"a\", \"b\");
        int len = s.length();
        Body other = new Body();
        int y = other.identity(x);
        return y;
    }
}
",
    )])
);

snapshot!(
    library_calls,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.List;

class Body {
    int listSize() {
        List<String> list = new ArrayList<String>();
        list.add(\"a\");
        int size = list.size();
        String first = list.get(0);
        return size;
    }
}
",
    )])
);

snapshot!(
    target_typing,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

class Body {
    List<String> empty() {
        return Collections.emptyList();
    }

    void use() {
        List<String> list = Collections.emptyList();
        Collections.sort(list);
        Collections.sort(new ArrayList<String>());
        Collections.emptyList();
    }
}
",
    )])
);

snapshot!(
    nested_invocation_argument,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}

    void call() {
        take(Collections.emptyList());
    }
}
",
    )])
);

snapshot!(
    nested_invocation_chain,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}

    <T> T id(T x) {
        return x;
    }

    void call() {
        take(Collections.emptyList());
        take(id(7));
    }
}
",
    )])
);

snapshot!(
    overload_by_target,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}
    void take(Object o) {}

    void call() {
        take(Collections.emptyList());
        take(new Object());
    }
}
",
    )])
);
// Each overload is probed with `emptyList()`'s inference shared against its
// formal parameter ([JLS §18.5.2.4]): `emptyList()` is `List<String>` against
// `take(List<String>)` and `List<Object>` against `take(Object)`. Both are
// applicable in the strict phase, and `take(List<String>)` is the most
// specific (§15.12.2.5), so it wins.

snapshot!(
    overload_retarget_choice,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}
    void take(List<Integer> ys) {}

    void call() {
        take(Collections.emptyList());
    }
}
",
    )])
);
// Both overloads are applicable — `emptyList()` is `List<String>` against
// `take(List<String>)` and `List<Integer>` against `take(List<Integer>)` —
// and neither is more specific than the other (§15.12.2.5), so the invocation
// is ambiguous; the nested `emptyList()` keeps its standalone `List<Object>`.

snapshot!(
    poly_conditional_invocation,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}

    void call(boolean flag) {
        take(flag ? Collections.emptyList() : Collections.emptyList());
    }
}
",
    )])
);
// The nested `emptyList()` is retargeted against `take`'s formal even when it
// is itself an argument to the generic `id`, whose own type parameter ranges
// over the formal ([JLS §18.5.2.2]): `id(emptyList())` is `List<String>`.

snapshot!(
    nested_invocation_deep,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}

    <T> T id(T x) {
        return x;
    }

    void call() {
        take(id(Collections.emptyList()));
    }
}
",
    )])
);

snapshot!(
    nested_generic_target,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    <T> List<T> wrap(List<T> xs) {
        return xs;
    }

    <T> T id(T x) {
        return x;
    }

    void call() {
        List<String> ys = wrap(id(Collections.emptyList()));
    }
}
",
    )])
);

snapshot!(
    poly_lambda_in_nested_call,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.function.Function;

class Body {
    void use(Function<String, Integer> f) {}

    <T> T id(T x) {
        return x;
    }

    void call() {
        use(id(s -> s.length()));
    }
}
",
    )])
);

snapshot!(
    nested_most_specific_overload,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    <T> T wrap(T t) {
        return t;
    }

    <T extends java.lang.Number> T transform(Object o) {
        return null;
    }

    String transform(String s) {
        return s;
    }

    void call() {
        String s = wrap(transform(\"\"));
    }
}
",
    )])
);
// The nested `transform(\"\")` is probed against `wrap`'s uninstantiated type
// parameter ([JLS §18.5.2.4]). Both overloads are applicable in the strict
// phase, but the non-generic `transform(String)` is more specific than the
// generic `transform(Object)` (§15.12.2.5), and only the winner's constraints
// are lifted into the enclosing table ([JLS §18.5.2.1/§18.5.2.2]). A greedy
// selection would commit `transform(Object)` first, whose `T extends Number`
// lower bound makes `wrap` inconsistent with the `String` target and the whole
// invocation an error.

snapshot!(
    nested_most_specific_overload_reversed,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    <T> T wrap(T t) {
        return t;
    }

    String transform(String s) {
        return s;
    }

    <T extends java.lang.Number> T transform(Object o) {
        return null;
    }

    void call() {
        String s = wrap(transform(\"\"));
    }
}
",
    )])
);
// The same source with the overloads declared in the other order resolves to
// the same `String` — the nested candidate selection is independent of
// declaration order ([§15.12.2.5]).

snapshot!(
    nested_overload_no_candidate,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}

    void pick(java.lang.Integer i) {}
    void pick(String s) {}

    void call() {
        take(pick(Collections.emptyList()));
    }
}
",
    )])
);
// Neither `pick` overload accepts a `List` argument, so the nested
// invocation has no applicable candidate against `take`'s formal — the
// enclosing call is an error, and the nested `pick` re-infers standalone as
// an error ([JLS §18.5.2.4]).

snapshot!(
    constructor_poly_argument,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.lang.Runnable;

class Job {
    Job(Runnable r) {}
}

class Body {
    void hire() {
        new Job(() -> {});
    }
}
",
    )])
);
// The invocation mode ([JLS §15.12.1]) is derived per call site: a bare type
// name receiver selects only static members (§15.12.3), an expression receiver
// (virtual invocation) may also select a class's static members.

snapshot!(
    mode_static_source,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static void s() {}
    void i() {}

    void use() {
        Body.s();
        Body b = new Body();
        b.s();
        b.i();
        Body.i();
    }
}
",
    )])
);
// A `super` invocation ([JLS §15.12.1]) selects only instance members of the
// direct superclass (§15.12.3); its receiver is the superclass type.

snapshot!(
    mode_super_source,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class A {
    void m() {}
    static void s() {}
}

class B extends A {
    void use() {
        super.m();
        super.s();
    }
}
",
    )])
);
// JLS §6.6.2: a `protected` instance member of `A` is accessible from the
// subclass `C` (in another package) only through a receiver that is `C` or a
// subtype of `C`, or through `super` — never through a plain `A` receiver.

snapshot!(
    protected_receiver_rule,
    check_body_types(&[
        (
            "/src/com/example/A.java",
            "\
package com.example;

public class A {
    protected void pro() {}
}
",
        ),
        (
            "/src/org/other/C.java",
            "\
package org.other;

class C extends com.example.A {
    A a = new A();
    C c = new C();

    void use() {
        a.pro();
        c.pro();
        super.pro();
        pro();
    }
}
",
        ),
    ])
);
// JLS §6.6.1: a `private` member is accessible throughout the body of its
// top-level class, including from nested classes.

snapshot!(
    private_nested_class,
    check_body_types(&[(
        "/src/com/example/Outer.java",
        "\
package com.example;

class Outer {
    private int secret;

    static class Inner {
        void use(Outer o) {
            o.secret = 1;
        }
    }
}

class Other {
    void use(Outer o) {
        o.secret = 2;
    }
}
",
    )])
);

snapshot!(
    boxed_binary_promotion,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int boxing(java.lang.Integer i, java.lang.Long l, java.lang.Character c) {
        int a = i + i;
        long b = i + l;
        double d = i + 1.5;
        int e = -i;
        long f = ~l;
        int g = i << 1;
        int h = c + c;
        boolean big = i > l;
        return a + e + g + h;
    }
}
",
    )])
);
// Each boxed operand is unboxed ([JLS §5.1.8]) and binary numeric promotion
// ([JLS §5.6.2]) applies: `Integer + Integer` → `int`, `Integer + Long` →
// `long`, `Integer + double` → `double`, `~Long` → `long`, `Integer << 1` →
// `int` (the shift operator promotes only the left operand), and
// `Character + Character` → `int`. Unary minus promotes a `Integer` to `int`.

snapshot!(
    for_each_source_iterable,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Iterator;

class Source implements Iterable<Integer> {
    public Iterator<Integer> iterator() {
        return null;
    }
}

class Item {
    int weight() {
        return 0;
    }
}

class Body {
    int forEach(Source src, Item[] items) {
        int sum = 0;
        for (Integer x : src) {
            sum += x;
        }
        for (Item it : items) {
            sum += it.weight();
        }
        return sum;
    }
}
",
    )])
);
// A for-each loop ([JLS §14.14.2]) over a source class implementing the
// generic `Iterable<T>` ([§14.14.2.1]) types the loop variable from `T`, and
// over an array types it from the element type. The `src.iterator()` and
// `iterator.next()` invocations are resolved against the source class.

snapshot!(
    array_initializer_empty,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int[] empty() {
        int[] a = new int[] {};
        String[][] b = new String[][] { {} };
        int[] c = { 1, 2 };
        int[] d = {};
        return a;
    }
}
",
    )])
);
// An array creation with an initializer ([JLS §15.10.1]) types the array from
// its declared element type even when the initializer is empty: `new int[] {}`
// is `int[]`. The standalone `{}` array-initializer expression infers its
// element type from its elements, so an empty one is an `error[]`.

snapshot!(
    package_private_access,
    check_body_types(&[
        (
            "/src/com/example/A.java",
            "\
package com.example;

public class A {
    void pkg() {}
}

class Body {
    void use(A a) {
        a.pkg();
    }
}
",
        ),
        (
            "/src/org/other/Use.java",
            "\
package org.other;

class Use {
    void use(com.example.A a) {
        a.pkg();
    }
}
",
        ),
    ])
);
// A package-private member ([JLS §6.6.1]) is accessible from any class of the
// same package — `Body` resolves `a.pkg()` — but not from another package —
// `Use` in `org.other` gets an error.

snapshot!(
    interface_static_mode,
    check_body_types(&[(
        "/src/com/example/Shape.java",
        "\
package com.example;

interface Shape {
    static void describe() {}
}

class Circle implements Shape {
}

class Body {
    void use() {
        Shape.describe();
        Circle c = new Circle();
        c.describe();
    }
}
",
    )])
);
// A static method of an interface ([JLS §9.4.3]) is invoked through the
// interface type ([§15.12.1] static invocation): `Shape.describe()` resolves.
// Invoked through an implementing-class expression receiver the invocation
// mode is virtual ([§15.12.3]), which must not select the interface's static
// member, so `c.describe()` is an error.

snapshot!(
    throw_statement,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    <T extends Exception> T makeEx() {
        return null;
    }

    String checked() {
        throw new RuntimeException();
        throw makeEx();
        throw new Object();
        return null;
    }
}
",
    )])
);
// §14.18: the operand of a `throw` is inferred standalone ([JLS §15.2]) and
// must be assignable to `Throwable` ([§5.2]): `new RuntimeException()` and the
// generic `makeEx()` — which resolves to its `Exception` bound rather than the
// method's `String` return type — are fine, while `new Object()` is not a
// `Throwable` and marks the operand as an error.

snapshot!(
    yield_target,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    <T> T id(T x) {
        return x;
    }

    int m(boolean b) {
        String s = switch (b) {
            case true -> \"a\";
            default -> { yield id(\"b\"); }
        };
        int n = switch (b) {
            case true -> 1;
            default -> { yield 2; }
        };
        return s.length() + n;
    }
}
",
    )])
);
// §14.21: a `yield` value has the enclosing switch expression's type as its
// target — `yield id(\"b\")` resolves `id` against `String`, not the method's
// `int` return type. §15.28: a block arm produces its value through its final
// `yield` statement, so the second switch expression types as `int`.

snapshot!(
    super_field_source,
    check_body_types(&[(
        "/src/com/example/A.java",
        "\
package com.example;

class A {
    int x = 1;
    protected int y = 2;
    static int z = 3;
}

class B extends A {
    int read() {
        return super.x + super.y;
    }

    int bad() {
        return super.z;
    }
}
",
    )])
);
// `super.field` ([§15.11.1]) is a field of the direct superclass, resolved in
// the super invocation mode ([§15.12.1]): `super.x` and `super.y` are `int`,
// a field that does not exist in the superclass is an error, and a static
// field accessed through `super` is illegal ([§15.11.1]).

snapshot!(
    var_local_decl,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m() {
        var s = \"x\";
        var n = 1 + 2;
        return s.length() + n;
    }
}
",
    )])
);
// §14.4.1: a `var` declaration infers the local's type from its initializer,
// inferred standalone (§15.2: a `var` initializer is never poly): `s` is
// `java.lang.String`, `n` is `int`, and `s.length()` resolves to `int`.

snapshot!(
    field_initializer_enum_args,
    check_body_types(&[(
        "/src/com/example/Shape.java",
        "\
package com.example;

enum Color {
    RED(1),
    GREEN(2);

    Color(int id) {}
}

@interface Tag {
    String value() default \"default\" + \"Tag\";
}

class Body {
    static final String NAME = \"x\" + \"y\";
    Color color = Color.RED;
}
",
    )])
);
// Field initializers ([§8.3.3]) are poly expressions whose target is the
// field's declared type; enum constant arguments ([§8.9.1]) are inferred
// standalone; an annotation element default ([§9.6.2]) is a poly expression
// whose target is the element's return type.

snapshot!(
    qualified_this,
    check_body_types(&[(
        "/src/com/example/Outer.java",
        "\
package com.example;

class Outer {
    int x = 1;

    class Inner {
        int read() {
            return Outer.this.x;
        }
    }
}
",
    )])
);
// §15.8.3: `Outer.this` — a qualified `this` — has the type of the named
// class `Outer`, not the innermost enclosing class.

snapshot!(
    static_import,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import static java.lang.Math.max;
import static java.lang.Math.*;

class Body {
    int use() {
        return max(1, 2) + min(3, 4);
    }
}
",
    )])
);
// §7.5.4: a static import makes an unqualified name a static member access
// through its declaring type — `max` resolves against `java.lang.Math`, and
// the on-demand form resolves `min` the same way.

snapshot!(
    var_without_initializer,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m() {
        var x;
        return x + 1;
    }
}
",
    )])
);
// §14.4.1: a `var` declaration must have an initializer. Without one the
// local has no type to infer, so `x` is a compile-time error: reported as a
// `var-without-initializer` diagnostic and degraded to `error` — later uses
// of `x` are `<error>` too instead of panicking.

snapshot!(
    var_multi_declarators,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m() {
        var a = 1, b = 2, c;
        return a + b + c;
    }
}
",
    )])
);
// The `var` type is shared across all declarators of one declaration
// ([§14.4.1]); each declarator still needs its own initializer, so `c` is
// reported while `a` and `b` infer as `int`.
