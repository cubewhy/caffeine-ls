mod common;

use common::event_snapshot;
use indoc::indoc;

event_snapshot!(event_empty_class, r#"class A {}"#);
event_snapshot!(event_class_with_field, r#"class A { int x; }"#);
event_snapshot!(event_class_with_method, r#"class A { void f() {} }"#);
event_snapshot!(
    event_interface_with_default_method,
    r#"interface A { default void f() {} }"#
);
event_snapshot!(event_empty_record, r#"record R(int x) {}"#);
event_snapshot!(
    event_enum_with_members,
    r#"enum E { A, B; int x; void f() {} }"#
);
event_snapshot!(event_type_parameters_class, r#"class A<T, U> {}"#);
event_snapshot!(
    event_nested_wildcard_type_argument,
    r#"class A { Map<String, List<? extends Number>> xs; }"#
);
event_snapshot!(event_recovery_missing_member_name, r#"class A { int ; }"#);
event_snapshot!(
    event_multiline_class,
    indoc! {r#"
        class A {
            int x;
            void f() {}
        }
    "#}
);
// [JLS §14.3]: `@interface` is an annotation type declaration, which the
// grammar does not admit in a block, so it stays an error rather than being
// lowered as a local declaration.
event_snapshot!(
    event_local_annotation_type_is_an_error,
    indoc! {r#"
        class Test {
            void func() {
                @interface X {}
            }
        }
    "#}
);
