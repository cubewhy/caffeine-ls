//! Snapshots of access control ([JLS §6.6]) and invocation mode
//! ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
//! [§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3))
//! in method resolution, over hand-encoded classes and source classes.

#[macro_use]
mod common;

use hir_ty::{InvocationContext, InvocationMode, Ty};

use crate::common::{
    ClassSpec, TestDatabase, TyBuilder, check_methods_lib_ctx, check_source_methods_ctx,
};

type Sample = (&'static str, TyBuilder, &'static str, &'static [TyBuilder]);

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
}

fn ctx(mode: InvocationMode, enclosing_class: &str, package: &str) -> InvocationContext {
    InvocationContext {
        mode,
        enclosing_class: Some(enclosing_class.to_owned()),
        package: Some(package.to_owned()),
    }
}

const A: &str = "com.example.A";
const B: &str = "com.example.B";
const C: &str = "org.other.C";

fn access_classes() -> Vec<ClassSpec<'static>> {
    vec![
        common::class("java/lang/Object", None, &[]),
        common::class_with_methods_access(
            "com/example/A",
            Some("java/lang/Object"),
            &[],
            &[
                ("pub", "()V"),
                ("pro", "()V"),
                ("pkg", "()V"),
                ("priv", "()V"),
                ("stat", "()V"),
            ],
            &["", "", "", "", ""],
            &[0x0001, 0x0004, 0x0000, 0x0002, 0x0001 | 0x0008],
        ),
        common::class("com/example/B", Some("com/example/A"), &[]),
        common::class("org/other/C", Some("com/example/A"), &[]),
        common::class("org/other/Other", Some("java/lang/Object"), &[]),
        ClassSpec {
            fqn: "com/example/I",
            super_class: None,
            interfaces: &[],
            access: 0x0601, // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
            fields: &[],
            methods: &[("inst", "()V"), ("istat", "()V")],
            method_sigs: &["", ""],
            method_access: &[0x0001, 0x0001 | 0x0008],
            sig: None,
        },
        common::class(
            "com/example/D",
            Some("java/lang/Object"),
            &["com/example/I"],
        ),
    ]
}

const SAMPLES: [Sample; 5] = [
    ("A.pub", |db| r(db, A), "pub", &[]),
    ("A.pro", |db| r(db, A), "pro", &[]),
    ("A.pkg", |db| r(db, A), "pkg", &[]),
    ("A.priv", |db| r(db, A), "priv", &[]),
    ("A.stat", |db| r(db, A), "stat", &[]),
];

fn library_access(ctx: InvocationContext) -> String {
    check_methods_lib_ctx(&access_classes(), &SAMPLES, &ctx)
}

snapshot! {
    access_same_package_subclass,
    library_access(ctx(InvocationMode::Virtual, B, "com.example")),
}

snapshot! {
    access_subclass_other_package,
    library_access(ctx(InvocationMode::Virtual, C, "org.other")),
}

snapshot! {
    access_unrelated_other_package,
    library_access(ctx(InvocationMode::Virtual, "org.other.Other", "org.other")),
}

fn library_modes(ctx: InvocationContext) -> String {
    check_methods_lib_ctx(
        &access_classes(),
        &[
            ("A.stat", |db| r(db, A), "stat", &[]),
            ("A.pub", |db| r(db, A), "pub", &[]),
            ("I.istat", |db| r(db, "com.example.I"), "istat", &[]),
            ("I.inst", |db| r(db, "com.example.I"), "inst", &[]),
            ("D.istat", |db| r(db, "com.example.D"), "istat", &[]),
            ("D.inst", |db| r(db, "com.example.D"), "inst", &[]),
        ],
        &ctx,
    )
}

snapshot! {
    mode_static,
    library_modes(ctx(InvocationMode::Static, B, "com.example")),
}

snapshot! {
    mode_super,
    library_modes(ctx(InvocationMode::Super, B, "com.example")),
}

snapshot! {
    mode_interface,
    library_modes(ctx(InvocationMode::Interface, B, "com.example")),
}

snapshot! {
    mode_virtual,
    library_modes(ctx(InvocationMode::Virtual, B, "com.example")),
}

const ACCESS_SRC: &[(&str, &str)] = &[
    (
        "/src/com/example/A.java",
        r#"package com.example;
class A {
    public void pub() {}
    protected void pro() {}
    void pkg() {}
    private void priv() {}
    public static void stat() {}
}
class B extends A {}
"#,
    ),
    (
        "/src/org/other/C.java",
        r#"package org.other;
class C extends com.example.A {}
"#,
    ),
];

fn source_access(ctx: InvocationContext) -> String {
    check_source_methods_ctx(ACCESS_SRC, &SAMPLES, &ctx)
}

snapshot! {
    source_access_same_package_subclass,
    source_access(ctx(InvocationMode::Virtual, B, "com.example")),
}

snapshot! {
    source_access_same_class_private,
    source_access(ctx(InvocationMode::Virtual, A, "com.example")),
}

snapshot! {
    source_access_subclass_other_package,
    source_access(ctx(InvocationMode::Virtual, C, "org.other")),
}

snapshot! {
    source_access_unrelated_other_package,
    source_access(ctx(InvocationMode::Virtual, "org.other.Other", "org.other")),
}
