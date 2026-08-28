//! Tests for the cross-file dependency index (`hir-ty::dep_index`) and the
//! `FileId` provenance of resolved members (`MethodData::owner_file`,
//! `FieldData::owner_file`).
//!
//! These are the engine-level foundation of the LSP's exact reverse-dependency
//! pipeline: they pin down that a file's *resolved dependencies* are the files
//! its types actually resolve against (including members inherited from a
//! different file), that its *reference names* are the sound name-level
//! fallback, that resolved members carry their declaring file, and that a
//! file's diagnostics report stays stable for unrelated edits while a
//! dependency's edit changes it.

#[macro_use]
mod common;

use base_db::FileChange;
use hir_ty::db::{file_dependency_refs, file_resolved_deps};
use hir_ty::{pick_field, pick_method};
use vfs::FileId;

use crate::common::{TestDatabase, jdk_fixture, register_source_set, source_context};

/// The path of `file_id` within the `files` fixture (`FileId(i+1)` ↔ `files[i]`).
fn path_of(files: &[(&str, &str)], file_id: FileId) -> String {
    files[(file_id.index() - 1) as usize].0.to_owned()
}

/// Renders, for every fixture file, its resolved cross-file dependencies and
/// its resolution-relevant reference names, sorted for determinism.
fn check_dep_index(files: &[(&str, &str)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(&mut db, &fixture, files);

    let mut lines = Vec::new();
    for (i, _) in files.iter().enumerate() {
        let file_id = FileId::from_raw((i + 1) as u32);
        let deps = file_resolved_deps(&db, file_id);
        let mut deps: Vec<String> = deps.iter().map(|&dep| path_of(files, dep)).collect();
        deps.sort();
        let refs = file_dependency_refs(&db, file_id);
        let mut refs: Vec<String> = refs.iter().map(|name| name.as_str().to_owned()).collect();
        refs.sort();
        lines.push(format!(
            "FILE {}\n{}\nDEPS: {}\nREFNAMES: {}\n",
            path_of(files, file_id),
            files[i].1,
            deps.join(", "),
            refs.join(", ")
        ));
    }
    lines.join("\n")
}

snapshot!(
    deps_basic_cross_file_reference,
    check_dep_index(&[
        (
            "/src/p/A.java",
            "package p;\npublic class A {\n    public String name() { return \"a\"; }\n}\n",
        ),
        (
            "/src/p/B.java",
            "package p;\npublic class B {\n    void m(A a) {\n        a.name();\n        String s = a.name();\n    }\n}\n",
        ),
        (
            "/src/p/C.java",
            "package p;\npublic class C {\n    void m(String s) {}\n}\n",
        ),
    ])
);

snapshot!(
    deps_inherited_member_default_constructor,
    check_dep_index(&[
        (
            "/src/p/Base.java",
            "package p;\npublic class Base {\n    public void inherited() {}\n}\n",
        ),
        (
            "/src/p/Sub.java",
            "package p;\npublic class Sub extends Base {}\n",
        ),
        (
            "/src/p/Use.java",
            "package p;\npublic class Use {\n    void m(Sub s) {\n        s.inherited();\n    }\n}\n",
        ),
    ])
);

snapshot!(
    deps_static_import_fallback,
    check_dep_index(&[
        (
            "/src/p/StaticLib.java",
            "package p;\npublic class StaticLib {\n    public static void helper() {}\n}\n",
        ),
        (
            "/src/p/Caller.java",
            "package p;\nimport static p.StaticLib.helper;\npublic class Caller {\n    void m() {\n        helper();\n    }\n}\n",
        ),
    ])
);

snapshot!(
    refs_field_and_method_names,
    check_dep_index(&[
        (
            "/src/p/Holder.java",
            "package p;\npublic class Holder {\n    public String s;\n    public String get() { return s; }\n}\n",
        ),
        (
            "/src/p/Reader.java",
            "package p;\npublic class Reader {\n    String read(Holder h) {\n        return h.s + h.get();\n    }\n}\n",
        ),
    ])
);

#[test]
fn provenance_method_declared_in_superclass_file() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let files = [
        (
            "/src/p/Base.java",
            "package p; public class Base { public void inherited() {} }",
        ),
        (
            "/src/p/Sub.java",
            "package p; public class Sub extends Base {}",
        ),
    ];
    let source_set = register_source_set(&mut db, &fixture, &files);
    let scope = hir::ResolutionScope::SourceSet(source_set.clone());
    let context = source_context(&db, source_set);

    // The method is *inherited*: declared on `Base` (file 1), resolved on `Sub`.
    let receiver = hir_ty::Ty::reference(&db, "p.Sub", vec![]);
    let picked =
        pick_method(&db, &scope, &receiver, "inherited", &[], &context, None).expect("resolves");
    assert_eq!(picked.name, "inherited");
    assert_eq!(picked.owner, "p.Base");
    assert_eq!(picked.owner_file, Some(FileId::from_raw(1)));

    // A library method carries no source provenance.
    let receiver = hir_ty::Ty::reference(&db, "java.lang.String", vec![]);
    let picked = pick_method(&db, &scope, &receiver, "length", &[], &context, None)
        .expect("length resolves");
    assert_eq!(picked.owner, "java.lang.String");
    assert_eq!(picked.owner_file, None);
}

#[test]
fn provenance_field_declared_in_superclass_file() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let files = [
        (
            "/src/p/Base.java",
            "package p; public class Base { protected int count; }",
        ),
        (
            "/src/p/Sub.java",
            "package p; public class Sub extends Base {}",
        ),
    ];
    let source_set = register_source_set(&mut db, &fixture, &files);
    let scope = hir::ResolutionScope::SourceSet(source_set.clone());
    let context = source_context(&db, source_set);

    let receiver = hir_ty::Ty::reference(&db, "p.Sub", vec![]);
    let picked = pick_field(&db, &scope, &receiver, "count", &context).expect("count resolves");

    assert_eq!(picked.owner, "p.Base");
    assert_eq!(picked.owner_file, Some(FileId::from_raw(1)));
}

#[test]
fn file_resolved_deps_excludes_self_and_library_only() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let files = [
        (
            "/src/p/A.java",
            "package p; public class A { public void go() {} }",
        ),
        (
            "/src/p/Solo.java",
            "package p; public class Solo { void m() { String s = \"x\"; } }",
        ),
    ];
    register_source_set(&mut db, &fixture, &files);

    // A never references another source file: no cross-file deps.
    let deps = file_resolved_deps(&db, FileId::from_raw(1));
    assert!(deps.is_empty(), "unexpected deps: {deps:?}");
    // `Solo` only uses the JDK (`String`): no source-file deps either.
    let deps = file_resolved_deps(&db, FileId::from_raw(2));
    assert!(deps.is_empty(), "unexpected deps: {deps:?}");
}

#[test]
fn file_dependency_refs_captures_type_and_member_names() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let files = [
        (
            "/src/p/Holder.java",
            "package p; public class Holder { public int x; public int get() { return x; } }",
        ),
        (
            "/src/p/Use.java",
            "package p; public class Use { int read(Holder h) { return h.get() + h.x; } }",
        ),
    ];
    register_source_set(&mut db, &fixture, &files);

    let refs = file_dependency_refs(&db, FileId::from_raw(2));
    // Type-reference names (the declared type in the signature) …
    assert!(refs.iter().any(|n| n.as_str() == "Holder"));
    // … and member-access names (method + field), the name-level fallback.
    assert!(refs.iter().any(|n| n.as_str() == "get"));
    assert!(refs.iter().any(|n| n.as_str() == "x"));
}

#[test]
fn diagnostics_report_changes_when_dependency_edits() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let files = [
        (
            "/src/p/A.java",
            "package p; public class A { public void go() {} }",
        ),
        (
            "/src/p/B.java",
            "package p; public class B { void m(A a) { a.go(); } }",
        ),
    ];
    register_source_set(&mut db, &fixture, &files);

    let a = FileId::from_raw(1);
    let b = FileId::from_raw(2);
    let diagnostics_before = ide_diagnostics::file_diagnostics(&db, b);

    // Edit `A`: `go()` becomes one-argument, breaking `B`'s call.
    let mut change = FileChange::default();
    change.change_file(
        a,
        Some("package p; public class A { public void go(String x) {} }".to_owned()),
    );
    change.apply(&mut db);

    assert_ne!(
        ide_diagnostics::file_diagnostics(&db, b),
        diagnostics_before,
        "B's diagnostics must change when A (its dependency) edits"
    );
}

#[test]
fn diagnostics_report_stable_for_unrelated_edit() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let files = [
        (
            "/src/p/A.java",
            "package p; public class A { public void go() {} }",
        ),
        (
            "/src/p/B.java",
            "package p; public class B { void m(A a) { a.go(); } }",
        ),
        (
            "/src/p/C.java",
            "package p; public class C { void unrelated() {} }",
        ),
    ];
    register_source_set(&mut db, &fixture, &files);

    let b = FileId::from_raw(2);
    let c = FileId::from_raw(3);
    let b_before = ide_diagnostics::file_diagnostics(&db, b);
    let c_before = ide_diagnostics::file_diagnostics(&db, c);

    // Edit the unrelated `C` in a way that changes *its own* diagnostics (an
    // undefined-name report appears/disappears); `B`'s report must not move.
    let mut change = FileChange::default();
    change.change_file(
        c,
        Some("package p; public class C { void unrelated() { undefinedName; } }".to_owned()),
    );
    change.apply(&mut db);

    assert_eq!(
        ide_diagnostics::file_diagnostics(&db, b),
        b_before,
        "B must not be re-derived when an unrelated file edits"
    );
    assert_ne!(
        ide_diagnostics::file_diagnostics(&db, c),
        c_before,
        "C's own report moves with its own edit"
    );
}
