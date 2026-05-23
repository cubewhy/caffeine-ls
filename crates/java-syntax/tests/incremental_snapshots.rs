use crate::common::incremental_snapshot;

mod common;

incremental_snapshot!(insert_field, "class A { § }", "int x;");

incremental_snapshot!(
    replace_method_name,
    "class A { void §oldName§() {} }",
    "newName"
);

incremental_snapshot!(delete_annotation, "§@Deprecated§ class A {}", "");

incremental_snapshot!(
    boundary_break_brace,
    "class A { void m() { §}§ }",
    " /* missing brace */ "
);

incremental_snapshot!(
    boundary_cross_nodes,
    "class A { void m1() {§}  void m2(){§} }",
    "} // gap // void m3() {"
);

incremental_snapshot!(
    boundary_touch_left,
    "class A {§void m(){}§}",
    " int x; void m(){} "
);

incremental_snapshot!(
    kind_block,
    "class A { void m() { §int a = 1;§ } }",
    "int a = 1; int b = 2;"
);

incremental_snapshot!(kind_class_body, "class A { §int f;§ }", "int f; void m(){}");

incremental_snapshot!(
    kind_interface_body,
    "interface I { §void m();§ }",
    "void m(); int CONST = 1;"
);

incremental_snapshot!(
    kind_switch_block,
    "class A { void m(int x) { switch(x) { §case 1: break;§ } } }",
    "case 1: break; default: return;"
);

incremental_snapshot!(
    kind_enum_body,
    "enum E { §A, B§ }",
    "A, B, C; void extra(){}"
);

incremental_snapshot!(
    kind_record_body,
    "record R(int i) { § § }",
    "public R { i = 0; }"
);

incremental_snapshot!(
    kind_annotation_body,
    "@interface MyAttr { §String value();§ }",
    "String value(); int id() default 0;"
);

incremental_snapshot!(
    kind_module_body,
    "module com.test { §requires java.base;§ }",
    "requires java.base; exports com.test.api;"
);

incremental_snapshot!(
    kind_array_initializer,
    "class A { int[] arr = { §1, 2§ }; }",
    "1, 2, 3, 4"
);

incremental_snapshot!(
    kind_root_fallback,
    "§package a.b.c;§ class A {}",
    "package x.y.z;"
);

incremental_snapshot!(test_utf8_offset, "class A { /* 好 § */ § }", "int x = 1;");
