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

use triomphe::Arc;
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
    let before_report = ide_diagnostics::file_report(&db, b);
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
    let renamed_report = ide_diagnostics::file_report(&db, b);
    assert!(
        !renamed_report.is_empty(),
        "renaming A must surface an unresolved `com.a.A` error in B: {renamed_report:?}"
    );
}

/// A body-only edit must leave the file's item tree value equal, so salsa
/// backdates it and every declaration consumer (here `file_symbols`) stays a
/// memo hit — the `Arc::ptr_eq` probe, exactly like the inference test above.
///
/// The item tree stores no source offsets and no body content: its
/// [`hir_expand::ast_id_map::FileAstId`]s are a function of the declaration
/// skeleton only (the `AstIdMap` prunes method bodies, declarator
/// initializers, enum-constant arguments and annotation defaults — but *not*
/// the local class-like declarations of a block, [JLS §14.3], which are
/// declarations), so an edit *inside* a method body that declares no local
/// type changes the `BodyTree` but not the `ItemTree`.
#[test]
fn body_only_edit_backdates_file_item_tree_consumers() {
    let source = "\
package com.a;
class B {}
class A extends B {
    int f = 1;
    void m() {
        int local = 1;
    }
}
";
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(&mut db, &fixture, &[("/src/com/a/A.java", source)]);
    let a = FileId::from_raw(1);

    // Warm `file_symbols` (keyed on the item tree).
    let sym = hir::file_symbols(&db, a);

    // A *structural* edit inside the method body (a new typed local): new
    // CST nodes, but all of them under the pruned `BLOCK`, so the declaration
    // skeleton, the `AstIdMap` indexes and every `FileAstId` are untouched.
    // The item tree backdates; `file_symbols` is a memo hit.
    let body_edit = "\
package com.a;
class B {}
class A extends B {
    int f = 1;
    void m() {
        int local = 1;
        int extra = 7;
    }
}
";
    edit_file(&mut db, a, body_edit);
    let sym_after_body = hir::file_symbols(&db, a);
    assert!(
        Arc::ptr_eq(&sym, &sym_after_body),
        "a body-only edit must not re-execute file_symbols"
    );

    // Control 1: a declaration-skeleton edit (renaming the method) must
    // re-execute.
    let rename = "\
package com.a;
class B {}
class A extends B {
    int f = 1;
    void m2() {
        int local = 1;
        int extra = 7;
    }
}
";
    edit_file(&mut db, a, rename);
    let sym_after_rename = hir::file_symbols(&db, a);
    assert!(
        !Arc::ptr_eq(&sym, &sym_after_rename),
        "renaming a method must re-execute file_symbols"
    );

    // Control 2: an edit *adjacent to* a pruned declarator — the field's
    // type (a child of the `FIELD_DECL`, outside the pruned
    // `VARIABLE_DECLARATOR` subtree) — must re-execute. (A pure initializer
    // literal change is body-side and correctly backdates.)
    let field_ty = "\
package com.a;
class B {}
class A extends B {
    long f = 1;
    void m2() {
        int local = 1;
        int extra = 7;
    }
}
";
    edit_file(&mut db, a, field_ty);
    let sym_after_field_ty = hir::file_symbols(&db, a);
    assert!(
        !Arc::ptr_eq(&sym, &sym_after_field_ty),
        "changing a field's type must re-execute file_symbols"
    );

    // Control 3: a *local class* declared inside a method body is part of the
    // declaration skeleton ([JLS §14.3]) — it is an item of the file's item
    // tree, with its own modifiers, supertypes and members — so declaring one
    // re-executes `file_symbols` even though a local declaration has no
    // canonical name ([§6.7]) and is not itself indexed as a symbol.
    let local_class = "\
package com.a;
class B {}
class A extends B {
    long f = 1;
    void m2() {
        int local = 1;
        int extra = 7;
        class Local {
            int g;
            void n() {
                int k = 1;
            }
        }
    }
}
";
    edit_file(&mut db, a, local_class);
    let sym_after_local_class = hir::file_symbols(&db, a);
    assert!(
        !Arc::ptr_eq(&sym, &sym_after_local_class),
        "declaring a local class must re-execute file_symbols"
    );

    // A body edit *inside* the local class — its method's body — is body-side
    // like any other: the skeleton is unchanged, so `file_symbols` is a memo
    // hit.
    let local_class_body_edit = local_class.replace("int k = 1;", "int k = 2;");
    edit_file(&mut db, a, &local_class_body_edit);
    let sym_after_local_body = hir::file_symbols(&db, a);
    assert!(
        Arc::ptr_eq(&sym_after_local_class, &sym_after_local_body),
        "a body-only edit of a local class must not re-execute file_symbols"
    );
}
