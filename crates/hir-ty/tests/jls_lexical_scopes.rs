//! Nested Java scopes follow JLS §6.4.1 and §15.12.1, not file order.
//! https://docs.oracle.com/javase/specs/jls/se25/html/jls-6.html#jls-6.4.1
//! https://docs.oracle.com/javase/specs/jls/se25/html/jls-15.html#jls-15.12.1
mod common;

use common::{TestDatabase, all_items, jdk_fixture, register_source_set};
use hir_def::java::item_tree::ItemData;
use hir_ty::java::diagnostics::TypeError;
use vfs::FileId;

#[test]
fn nearest_enclosing_types_methods_and_fields_shadow_outer_members() {
    // javac -Xlint:all -Werror accepts this. The two Builder declarations
    // have distinct identities; the fluent return must use the inner one.
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(
        &mut db,
        &fixture,
        &[(
            "/src/p/Scopes.java",
            r#"package p;
class Scopes {
    static class Builder {}
    static Scopes getDefaultInstance() { return null; }
    static String value;
    static class Playlist {
        static Playlist getDefaultInstance() { return null; }
        String getId() { return ""; }
        static int value;
        static class Builder {
            Builder self() { return this; }
            String id() { return getDefaultInstance().getId(); }
            int field() { return value; }
        }
    }
}"#,
        )],
    );
    let file = FileId::from_raw(1);
    let diagnostics = hir_ty::class_diagnostics(&db, file);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let tree = hir_def::java::plugin::tree(&db, file);
    for (item, _) in all_items(&tree) {
        if let Some(body) = hir_ty::body_types(&db, file, item) {
            assert!(body.diagnostics.is_empty(), "{:?}", body.diagnostics);
        }
    }
}

#[test]
fn shadowed_members_do_not_rescue_invalid_inner_uses() {
    // javac rejects each marked method: shadowing selects a declaration,
    // not whichever enclosing declaration would make the expression valid.
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(
        &mut db,
        &fixture,
        &[(
            "/src/p/Scopes.java",
            r#"package p;
class Scopes {
    static class Builder {}
    static Scopes getDefaultInstance() { return null; }
    String getOuterId() { return ""; }
    static String value;
    static class Playlist {
        static Playlist getDefaultInstance() { return null; }
        static int value;
        static class Builder {
            Builder wrongType() { return new Scopes.Builder(); }
            Scopes.Builder wrongThis() { return this; }
            String wrongMethod() { return getDefaultInstance().getOuterId(); }
            String wrongField() { return value; }
        }
    }
}"#,
        )],
    );
    let file = FileId::from_raw(1);
    let tree = hir_def::java::plugin::tree(&db, file);
    let mut rejected = Vec::new();
    for (item, data) in all_items(&tree) {
        let ItemData::Method(method) = data else {
            continue;
        };
        if !method.name.as_str().starts_with("wrong") {
            continue;
        }
        let body = hir_ty::body_types(&db, file, item).unwrap();
        match (method.name.as_str(), body.diagnostics.as_slice()) {
            ("wrongMethod", [TypeError::NoSuchMethod { name, .. }]) => {
                assert_eq!(name.as_str(), "getOuterId");
            }
            ("wrongType" | "wrongThis" | "wrongField", [TypeError::IncompatibleTypes { .. }]) => {}
            _ => panic!("{}: {:?}", method.name, body.diagnostics),
        }
        rejected.push(method.name.as_str().to_owned());
    }
    rejected.sort();
    assert_eq!(
        rejected,
        ["wrongField", "wrongMethod", "wrongThis", "wrongType"]
    );
}

#[test]
fn local_classes_keep_outermost_private_access_and_deprecation_exemption() {
    // JLS §6.6.1 / §9.6.4.6: both rules use the outermost class, even
    // when the immediate enclosing declaration is nested several levels.
    // javac -Xlint:all -Werror accepts the private read and deprecated calls.
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_source_set(
        &mut db,
        &fixture,
        &[(
            "/src/p/NestAccess.java",
            r#"package p;
class NestAccess {
    private static int secret;
    @Deprecated static void old() {}
    static class Middle {
        static class Inner {
            void use() { old(); }
            int local() {
                class Local { int read() { old(); return secret; } }
                return new Local().read();
            }
        }
    }
}"#,
        )],
    );
    let file = FileId::from_raw(1);
    let diagnostics = hir_ty::class_diagnostics(&db, file);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let tree = hir_def::java::plugin::tree(&db, file);
    for (item, _) in all_items(&tree) {
        if let Some(body) = hir_ty::body_types(&db, file, item) {
            assert!(body.diagnostics.is_empty(), "{:?}", body.diagnostics);
        }
    }
}
