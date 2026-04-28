mod common;

use common::parser_snapshot;
use indoc::indoc;

parser_snapshot!(parse_package_decl, r#"package a.b;"#);
parser_snapshot!(parse_import_decl, r#"import java.util.List;"#);
parser_snapshot!(
    parse_import_static_star,
    r#"import static java.util.Collections.*;"#
);

parser_snapshot!(parse_empty_class, r#"class A {}"#);
parser_snapshot!(parse_empty_interface, r#"interface A {}"#);
parser_snapshot!(parse_empty_enum, r#"enum E {}"#);
parser_snapshot!(parse_empty_record, r#"record R(int x) {}"#);

parser_snapshot!(parse_class_with_field, r#"class A { int x; }"#);
parser_snapshot!(
    parse_class_with_initialized_field,
    r#"class A { int x = 1; }"#
);
parser_snapshot!(parse_class_with_multi_field, r#"class A { int a, b; }"#);
parser_snapshot!(
    parse_class_with_multi_initialized_field,
    r#"class A { int a = 1, b = 2; }"#
);
parser_snapshot!(parse_class_with_method, r#"class A { void f() {} }"#);
parser_snapshot!(parse_class_with_typed_method, r#"class A { int f() {} }"#);
parser_snapshot!(parse_class_with_constructor, r#"class A { A() {} }"#);
parser_snapshot!(
    parse_class_with_static_initializer,
    r#"class A { static {} }"#
);
parser_snapshot!(parse_class_with_instance_initializer, r#"class A { {} }"#);
parser_snapshot!(parse_class_with_empty_decls, r#"class A { ;;;;;; }"#);

parser_snapshot!(
    parse_interface_with_abstract_method,
    r#"interface A { void f(); }"#
);
parser_snapshot!(
    parse_interface_with_default_method,
    r#"interface A { default void f() {} }"#
);
parser_snapshot!(
    parse_interface_with_field_like_member,
    r#"interface A { int X = 1; }"#
);

parser_snapshot!(parse_annotation_type_decl, r#"@interface A {}"#);
parser_snapshot!(
    parse_annotation_type_member_like,
    r#"@interface A { int value(); }"#
);

parser_snapshot!(
    parse_record_with_compact_constructor,
    r#"record R(int x) { R {} }"#
);
parser_snapshot!(
    parse_record_with_normal_constructor_like,
    r#"record R(int x) { R() {} }"#
);
parser_snapshot!(
    parse_record_with_method_and_field,
    r#"record R(int x) { int y; void f() {} }"#
);

parser_snapshot!(parse_array_type_on_type, r#"class A { int[] a; }"#);
parser_snapshot!(parse_array_type_on_declarator, r#"class A { int a[]; }"#);
parser_snapshot!(parse_mixed_array_dimensions, r#"class A { int[] a, b[]; }"#);

parser_snapshot!(parse_type_parameters_class, r#"class A<T> {}"#);
parser_snapshot!(
    parse_type_parameters_method,
    r#"class A { <A> void func() {} }"#
);
parser_snapshot!(parse_type_parameters_multiple, r#"class A<T, U> {}"#);
parser_snapshot!(
    parse_type_parameters_with_bound,
    r#"class A<T extends Number> {}"#
);
parser_snapshot!(
    parse_type_parameters_with_intersection_bound,
    r#"interface A<T extends B & C> {}"#
);

parser_snapshot!(parse_enum_constants_simple, r#"enum E { A, B, C }"#);
parser_snapshot!(
    parse_enum_constants_with_trailing_comma,
    r#"enum E { A, B, C, }"#
);
parser_snapshot!(
    parse_enum_constants_and_members,
    r#"enum E { A, B; int x; void f() {} }"#
);

parser_snapshot!(
    parse_annotation_array_initializer,
    r#"@A({1, 2, 3}) class B {}"#
);
parser_snapshot!(
    parse_annotation_nested_array_initializer,
    r#"@A({{1}, {2}}) class B {}"#
);

parser_snapshot!(parse_recovery_missing_member_name, r#"class A { int ; }"#);
parser_snapshot!(
    parse_recovery_broken_params,
    r#"class A { void f( { int x; } }"#
);
parser_snapshot!(
    parse_recovery_constructor_in_interface,
    r#"interface A { A() {} }"#
);
parser_snapshot!(
    parse_recovery_default_in_class,
    r#"class A { default void f() {} }"#
);
parser_snapshot!(
    parse_recovery_compact_constructor_like_in_class,
    r#"class A { A {} }"#
);

parser_snapshot!(
    parse_class_with_type_parameters,
    r#"public class A<A, B> {}"#
);
parser_snapshot!(
    parse_interface_with_type_parameters,
    r#"public interface A<T, U> {}"#
);
parser_snapshot!(
    parse_record_with_type_parameters,
    r#"public record R<T, U>(T x, U y) {}"#
);

parser_snapshot!(
    parse_type_parameters_method_void,
    r#"class A { <A> void func() {} }"#
);
parser_snapshot!(
    parse_type_parameters_method_typed_return,
    r#"class A { <T> T func() {} }"#
);
parser_snapshot!(
    parse_type_parameters_method_interface,
    r#"interface A { <T> T func(); }"#
);
parser_snapshot!(
    parse_type_parameters_generic_constructor,
    r#"class A { <T> A() {} }"#
);

parser_snapshot!(
    parse_field_with_type_argument,
    r#"class A { List<String> xs; }"#
);
parser_snapshot!(
    parse_field_with_multiple_type_arguments,
    r#"class A { Map<K, V> map; }"#
);
parser_snapshot!(
    parse_qualified_type_with_arguments,
    r#"class A { java.util.List<String> xs; }"#
);
parser_snapshot!(
    parse_method_return_type_with_arguments,
    r#"class A { List<String> f() {} }"#
);

parser_snapshot!(
    parse_field_with_unbounded_wildcard,
    r#"class A { List<?> xs; }"#
);
parser_snapshot!(
    parse_field_with_extends_wildcard,
    r#"class A { List<? extends Number> xs; }"#
);
parser_snapshot!(
    parse_field_with_super_wildcard,
    r#"class A { List<? super Integer> xs; }"#
);
parser_snapshot!(
    parse_nested_wildcard_type_argument,
    r#"class A { Map<String, List<? extends Number>> xs; }"#
);

parser_snapshot!(
    parse_multiline_class,
    indoc! {r#"
        class A {
            int x;
            void f() {}
        }
    "#}
);

// annotation_type_decl (@interface)
parser_snapshot!(
    parse_annotation_simple_method,
    indoc! {r#"
        @interface MyAnno {
            String value();
            int id();
        }
    "#}
);

parser_snapshot!(
    parse_annotation_with_default,
    indoc! {r#"
        @interface Config {
            String name() default "unknown";
            int retryCount() default 3;
        }
    "#}
);

parser_snapshot!(
    parse_annotation_constant,
    indoc! {r#"
        @interface Limits {
            int MAX_SIZE = 100;
            String DEFAULT_TYPE = "JSON";
        }
    "#}
);

parser_snapshot!(
    parse_annotation_error_recovery,
    indoc! {r#"
        @interface ErrorProne {
            void invalid(int x);
            String valid();
        }
    "#}
);

parser_snapshot!(
    parse_annotation_mixed,
    indoc! {r#"
        @interface Complex {
            int count() default 0;
            String[] tags() default 1;
            double VERSION = 1.0;
        }
    "#}
);

parser_snapshot!(
    parse_varargs,
    indoc! {r#"
        class A {
            void func(String... args) {}
        }
    "#}
);

parser_snapshot!(
    parse_varargs_with_modifiers,
    indoc! {r#"
        class A {
            void func(@Annotation final String... args) {}
        }
    "#}
);

parser_snapshot!(
    parse_c_style_array,
    indoc! {r#"
        class A {
            void func(String args[]) {}
        }
    "#}
);

parser_snapshot!(
    parse_assert_statement,
    indoc! {r#"
        class A {
            void func() {
                assert true: "failed";
            }
        }
    "#}
);

parser_snapshot!(
    parse_assert_statement_missing_reason,
    indoc! {r#"
        class A {
            void func() {
                assert true:;
            }
        }
    "#}
);

parser_snapshot!(
    parse_while_stmt,
    indoc! {r#"
        class A {
            void func() {
                while (true) {
                    // do something
                }
            }
        }
    "#}
);

parser_snapshot!(
    parse_while_stmt_missing_condition,
    indoc! {r#"
        class A {
            void func() {
                while {
                    // do something
                }
            }
        }
    "#}
);

parser_snapshot!(
    parse_while_stmt_short,
    indoc! {r#"
        class A {
            void func() {
                while (true) break;
            }
        }
    "#}
);

parser_snapshot!(
    parse_synchronized_stmt,
    indoc! {r#"
        class Test {
            public static void main(String[] args) {
                Test t = new Test();
                synchronized(t) {
                    synchronized(t) {
                        System.out.println("made it!");
                    }
                }
            }
        }
    "#}
);

parser_snapshot!(
    parse_synchronized_stmt_missing_expr,
    indoc! {r#"
        class Test {
            public static void main(String[] args) {
                synchronized {
                    System.out.println("made it!");
                }
            }
        }
    "#}
);

parser_snapshot!(
    parse_do_while_stmt,
    indoc! {r#"
        class Test {
            void func() {
                do {

                } while (true);
            }
        }
    "#}
);

parser_snapshot!(
    parse_do_while_stmt_short,
    indoc! {r#"
        class Test {
            void func() {
                int i = 0;
                do i++; while (true);
            }
        }
    "#}
);

parser_snapshot!(
    parse_try_statement,
    indoc! {r#"
        class Test {
            void func() {
                try {}
                catch (final Exception e) {}
                finally {}
            }
        }
    "#}
);

parser_snapshot!(
    parse_try_statement_missing_catch_parameter,
    indoc! {r#"
        class Test {
            void func() {
                try {}
                catch {}
                finally {}
            }
        }
    "#}
);

parser_snapshot!(
    parse_for_stmt,
    indoc! {r#"
        class Test {
            void func() {
                for (int i = 0; i < 10; i++) {}
            }
        }
    "#}
);

parser_snapshot!(
    parse_enhanced_for_stmt,
    indoc! {r#"
        class Test {
            void func() {
                String[] strings;
                for (String s: strings) {}
            }
        }
    "#}
);

parser_snapshot!(
    parse_incomplete_for_stmt,
    indoc! {r#"
        class Test {
            void func() {
                String[] strings;
                for (String s: ) {}
            }
        }
    "#}
);

parser_snapshot!(
    parse_underscope_variable_id,
    indoc! {r#"
        class Test {
            void func() {
                String _;
            }
        }
    "#}
);

parser_snapshot!(
    parse_switch_stmt,
    indoc! {r#"
        class Test {
            void func() {
                switch (expr) {
                    case 1, 2, 3, 4:
                    case 5, 6, 7, 8:
                        break;
                    case String _:
                    case String s:
                        break;
                    case Point(int x, int _):
                        break;
                    default:
                        break;
                }
            }
        }
    "#}
);

parser_snapshot!(
    parse_switch_pattern_case,
    indoc! {r#"
        class Test {
            void func(Object obj) {
                switch (obj) {
                    case String s -> System.out.println(s);
                    case Integer _ -> System.out.println("Integer");
                    default -> {}
                }
            }
        }
    "#}
);

parser_snapshot!(
    parse_nested_record_pattern_in_switch,
    indoc! {r#"
        class Test {
            void func(Object obj) {
                switch (obj) {
                    case Box(Item(String name), _) -> System.out.println(name);
                    default -> {}
                }
            }
        }
    "#}
);

parser_snapshot!(
    parse_labeled_stmt,
    indoc! {r#"
        class Test {
            void func() {
                label: {
                    func();
                }

                labelNoShortIf: func();
            }
        }
    "#}
);

parser_snapshot!(
    parse_module_decl,
    indoc! {r#"
        module com.example.foo {
            requires com.example.foo.http;
            requires java.logging;

            requires transitive com.example.foo.network;

            exports com.example.foo.bar;
            exports com.example.foo.internal to com.example.foo.probe;

            opens com.example.foo.quux;
            opens com.example.foo.internal to com.example.foo.network,
                                              com.example.foo.probe;

            uses com.example.foo.spi.Intf;
            provides com.example.foo.spi.Intf with com.example.foo.Impl;
        }
    "#}
);

parser_snapshot!(
    parse_type_with_fqn,
    indoc! {r#"
        class Test {
            java.util.List<java.lang.String> list;
        }
    "#}
);

parser_snapshot!(
    parse_class_literal,
    indoc! {r#"
        class Test {
            void test() {
                Test.class;
            }
        }
    "#}
);

parser_snapshot!(
    parse_postfix_expr,
    indoc! {r#"
        class Test {
            void test() {
                i++;
                i--;
            }
        }
    "#}
);

parser_snapshot!(
    parse_prefix_expr,
    indoc! {r#"
        class Test {
            void test() {
                ++i;
                --i;
            }
        }
    "#}
);

parser_snapshot!(
    parse_type_cast_expr,
    indoc! {r#"
        class Test {
            void test() {
                Integer i = (Integer)1;
                boolean b = (boolean)1;
            }
        }
    "#}
);

parser_snapshot!(
    parse_type_cast_expr_with_bounds,
    indoc! {r#"
        class Test {
            void test() {
                Foo a = (Runnable & Serializable) o;
            }
        }
    "#}
);

parser_snapshot!(
    parse_type_cast_expr_with_method_call,
    indoc! {r#"
        class Test {
            void test() {
                Foo a = (Foo)Foo.getInstance();
            }
        }
    "#}
);

parser_snapshot!(
    parse_type_cast_expr_with_parentheses,
    indoc! {r#"
        class Test {
            void test() {
                Foo a = (Foo)(new Foo());
            }
        }
    "#}
);

parser_snapshot!(
    parse_switch_expr,
    indoc! {r#"
        class Test {
            void test() {
                var a = switch (expr) {
                    case 1 -> {}
                    case 2 -> {}
                };
            }
        }
    "#}
);

parser_snapshot!(
    // alias: ConditionalExpression, conditional_expression
    parse_binary_expr,
    indoc! {r#"
        class Test {
            void test() {
                if (a && b || c) {}
            }
        }
    "#}
);

parser_snapshot!(
    parse_conditional_expr,
    indoc! {r#"
        class Test {
            void test() {
                int max = num1 > num2? num1 : num2;
            }
        }
    "#}
);

parser_snapshot!(
    parse_assign_expr,
    indoc! {r#"
        class Test {
            void test() {
                a = 1;
                a += 1;
                a -= 1;
                a /= 1;
                a *= 1;
                a |= 1;
                a %= 1;
                a &= 1;
                a ^=1;
                a >>= 1;
                a <<= 1;
                a >>>= 1;
            }
        }
    "#}
);

parser_snapshot!(
    parse_var_name_is_contextual_kw,
    indoc! {r#"
        class Test {
            void test() {
                record record;
                record record() {};
            }
        }
    "#}
);

parser_snapshot!(
    parse_method_references,
    indoc! {r#"
        class Test {
            void test() {
                var alConstructor = ArrayList<String>::new;
                var stringValueOfMd = String::valueOf;
                var lengthOfAStringLiteral = "abc"::length;
                var arrayCreation = int[]::new;
                var innerClassConstructor = Outer.Inner::new;
                var genericClassGenericConstructor = Bar<String>::<Integer>new;
            }
        }
    "#}
);

parser_snapshot!(
    parse_this,
    indoc! {r#"
        class Test {
            String field;
            void test() {
                this.test(this.field);
            }

            void test(String s) {}
        }
    "#}
);

parser_snapshot!(
    parse_instanceof_expr,
    indoc! {r#"
        class Test {
            String field;
            void test() {
                if (a instanceof String) {}
            }

            void test(String s) {}
        }
    "#}
);

parser_snapshot!(
    parse_lambda,
    indoc! {r#"
        class Test {
            void test() {
                Consumer<String> concise = s -> System.out.println(s);
                Consumer<String> unnamed = _ -> System.out.println("Ignored input");
                BiFunction<Integer, Integer, Integer> inferredList = (a, b) -> a + b;
                BiConsumer<String, Integer> inferredWithUnderscore = (s, _) -> System.out.println(s);
                BinaryOperator<Integer> explicit = (Integer x, Integer y) -> x * y;
                BinaryOperator<Integer> varType = (var x, var y) -> x + y;
                BiFunction<String, String, String> withModifiers =
                    (@NotNull final var s1, final String s2) -> s1 + s2;
                VarargsHandler vHandler = (String... names) -> {
                    for (String n : names) System.out.println(n);
                };
                MultiParamHandler complex = (@NotNull final var s, int i, final double d) -> {
                    System.out.println(s + i + d);
                };
            }
        }
    "#}
);
