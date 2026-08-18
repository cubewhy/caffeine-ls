#[macro_use]
mod common;

use base_db::LanguageKind;

// -- top-level / package / imports -----------------------------------------

lower_snapshot! {
    empty_file,
    "",
}

lower_snapshot! {
    package_and_imports,
    r#"
package com.example;

import java.util.List;
import java.util.*;
import static java.lang.Math.max;
import static java.util.Collections.*;
"#,
}

lower_snapshot! {
    package_info,
    r#"
@Deprecated
package com.example;
"#,
}

// -- classes ----------------------------------------------------------------

lower_snapshot! {
    plain_class,
    r#"
class Foo {
}
"#,
}

lower_snapshot! {
    modifier_matrix,
    r#"
public abstract class AbstractThing {
    public static final int CONST = 1;
    private volatile boolean ready;
    protected transient Object cache;
    synchronized native void sync();
    @Deprecated public void legacy() {}
}
"#,
}

lower_snapshot! {
    strictfp_modifier,
    r#"
class Foo {
    strictfp void strict() {}
}
"#,
}

lower_snapshot! {
    generic_class,
    r#"
class Box<T> {
    T value;
}

class Foo<T extends Comparable<T> & Serializable, U> extends Bar<T> implements Runnable<T>, Serializable {
}
"#,
}

lower_snapshot! {
    nested_classes,
    r#"
class Outer {
    static class StaticNested {
        int x;
    }

    class Inner {
    }

    interface Iface {
    }
}
"#,
}

// -- methods and constructors ----------------------------------------------

lower_snapshot! {
    methods,
    r#"
class Foo {
    int x;
    void m() {}
    abstract void abstractMethod();
    static String s() { return ""; }
    private void privateMethod() {}
}
"#,
}

lower_snapshot! {
    constructors_varargs_throws,
    r#"
class Point {
    Point() {}
    Point(int x, int y) {}
    Point(int... coords) throws IllegalArgumentException {}
}
"#,
}

lower_snapshot! {
    c_style_arrays,
    r#"
class Foo {
    int a[];
    String args[];
    void m(int b[]) {}
}
"#,
}

lower_snapshot! {
    generics_usage,
    r#"
class Foo {
    <T> T map(T t, java.util.function.Function<T, T> f) { return t; }
    void m(Map<String, List<? extends Number>> x) {}
    String[][] matrix;
    Class<?> clazz;
}
"#,
}

// -- interfaces -------------------------------------------------------------

lower_snapshot! {
    interface_with_members,
    r#"
public interface Shape {
    int SIDES = 4;
    void draw();
    default void outline() {}
    static Shape empty() { return null; }
    private void helper() {}
}
"#,
}

// -- enums ------------------------------------------------------------------

lower_snapshot! {
    enum_with_members,
    r#"
public enum Color {
    RED,
    GREEN(255, 0, 0),
    BLUE {
        void extra() {}
    };

    private final int r;
    private final int g;

    Color() {}
    Color(int r, int g) {}

    int mix() { return 0; }
}
"#,
}

lower_snapshot! {
    enum_implements,
    r#"
enum Op implements Runnable {
    ADD,
    SUB {
        public void run() {}
    };
    public void run() {}
}
"#,
}

// -- records ----------------------------------------------------------------

lower_snapshot! {
    record,
    r#"
public record Point(int x, int y) implements Comparable<Point> {
    public Point {
    }

    int sum() { return x + y; }
}
"#,
}

lower_snapshot! {
    record_generic,
    r#"
record Pair<T extends Comparable<T>>(T first, T second, String... rest) {
}
"#,
}

// -- annotations ------------------------------------------------------------

lower_snapshot! {
    annotation_type,
    r#"
@Target(ElementType.TYPE)
public @interface Tag {
    String name() default "default";
    int[] sizes() default {1, 2, 3};
    Class<?> type();
}
"#,
}

// -- module-info ------------------------------------------------------------

lower_snapshot! {
    module_info,
    r#"
module com.example.foo {
    requires java.base;
    requires transitive java.sql;
    requires static lombok;
    exports com.example.foo.api;
    exports com.example.foo.internal to com.example.bar, com.example.baz;
    opens com.example.foo.spi;
    opens com.example.foo.util to com.example.qux;
    uses com.example.foo.spi.Plugin;
    provides com.example.foo.spi.Plugin with com.example.foo.impl.DefaultPlugin, com.example.foo.impl.OtherPlugin;
}
"#,
}

lower_snapshot! {
    open_module,
    r#"
open module com.example.reflect {
    exports com.example.api;
}
"#,
}

// -- initializers and fields ------------------------------------------------

lower_snapshot! {
    initializers,
    r#"
class Foo {
    static {
        System.out.println("static");
    }
    {
        System.out.println("instance");
    }
    int x = 42;
    int a = 1, b = 2, c;
    int y = a + b * 2;
}
"#,
}

// -- annotations on members -------------------------------------------------

lower_snapshot! {
    member_annotations,
    r#"
class Foo {
    @Deprecated @SuppressWarnings("all")
    void m(@NonNull String s) {}
}
"#,
}

// -- sealed -----------------------------------------------------------------

lower_snapshot! {
    sealed_class,
    r#"
sealed class Shape {
}
"#,
}

lower_snapshot! {
    sealed_permits_and_non_sealed,
    r#"
sealed class Shape permits Circle, Square {
}

non-sealed class Circle extends Shape {
}
"#,
}

// -- parse error recovery ---------------------------------------------------

lower_snapshot! {
    parse_errors,
    r#"
class Foo {
    void m( {
    }
}
"#,
}

lower_snapshot! {
    dangling_member,
    r#"
class Foo {
    void
}
"#,
}

// -- kotlin placeholder -----------------------------------------------------

lower_snapshot_lang! {
    kotlin_placeholder,
    LanguageKind::Kotlin,
    r#"
class Greeter {
    fun greet(name: String): String {
        return "hi"
    }
}
"#,
}
