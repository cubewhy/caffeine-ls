//! Incremental behavior of the per-file type inference: a symbol-affecting
//! edit to one file must not re-infer the bodies of files in unrelated
//! packages.
//!
//! Previously every resolution consulted one per-source-set aggregate symbol
//! index (`source_set_symbol_index_query`), which salsa re-derived on *any*
//! symbol edit — so a single edit re-inferred the whole workspace. Now the
//! resolver probes only the FQN's prefix-package file buckets
//! ([`hir::source_set_package_files`]) and each candidate's per-file symbols
//! ([`hir::file_symbols`]), both tracked per file/package, so an edit in an
//! unrelated package leaves an untouched file's `body_types` result served
//! from the salsa memo.
//!
//! `Arc::ptr_eq` on consecutive [`hir_ty::body_types`] results for the same
//! method is the probe: a memo hit returns the same stored `Arc`, while any
//! re-execution of the inference allocates a fresh one.

#[macro_use]
mod common;

use std::sync::Arc;
use vfs::FileId;

use common::{TestDatabase, edit_file, find_method, jdk_fixture, register_source_set};

#[test]
fn unrelated_package_edit_short_circuits_other_inference() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(
        &mut db,
        &fixture,
        &[
            (
                "/src/com/a/A.java",
                "package com.a;\npublic class A { public void m() {} }\n",
            ),
            (
                "/src/org/b/B.java",
                "package org.b;\npublic class B { void f() { com.a.A a = new com.a.A(); a.m(); } }\n",
            ),
            ("/src/com/c/C.java", "package com.c;\npublic class C {}\n"),
        ],
    );

    let a = FileId::from_raw(1);
    let b = FileId::from_raw(2);
    let c = FileId::from_raw(3);

    let tree = hir::file_item_tree(&db, b);
    let method = find_method(&tree, "f").expect("B.f");

    // Warm B's inference; it is clean (everything resolves).
    let before = hir_ty::body_types(&db, b, method).expect("B.f body");
    let before_report = ide_diagnostics::file_report(&db, b, base_db::LanguageKind::Java);
    assert!(
        before_report.is_empty(),
        "B must be clean: {before_report:?}"
    );

    // A symbol-affecting edit to an unrelated package (C renamed): B resolves
    // nothing in `com.c`, so its body inference must be a memo hit.
    edit_file(&mut db, c, "package com.c;\npublic class C2 {}\n");
    let after_c = hir_ty::body_types(&db, b, method).expect("B.f body");
    assert!(
        Arc::ptr_eq(&before, &after_c),
        "editing an unrelated package must not re-infer B's method"
    );

    // A symbol-affecting edit to the package B actually resolves against
    // (A renamed): B's inference must be re-derived and surface the
    // unresolved reference.
    edit_file(
        &mut db,
        a,
        "package com.a;\npublic class A2 { public void m() {} }\n",
    );
    let after_a_rename = hir_ty::body_types(&db, b, method).expect("B.f body");
    assert!(
        !Arc::ptr_eq(&before, &after_a_rename),
        "renaming A must re-infer B's method"
    );
    let renamed_report = ide_diagnostics::file_report(&db, b, base_db::LanguageKind::Java);
    assert!(
        !renamed_report.is_empty(),
        "renaming A must surface an unresolved `com.a.A` error in B: {renamed_report:?}"
    );
}
