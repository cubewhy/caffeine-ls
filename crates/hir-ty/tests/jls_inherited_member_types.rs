//! JLS §6.5.5.2 / §8.5: inherited member types keep their declaring identity,
//! obey hiding/access, and do not become ambiguous merely through a diamond.
//! https://docs.oracle.com/javase/specs/jls/se25/html/jls-8.html#jls-8.5
mod common;

use common::{TestDatabase, all_items, jdk_fixture, register_source_set};
use hir_expand::name::Name;
use hir_ty::java::resolve::{NameResolution, Resolver, resolve_name_checked};
use vfs::FileId;

#[test]
fn inherited_parameter_types_match_overrides_and_constructor_arguments() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let files = [
        (
            "/src/api/Base.java",
            "package api; public class Base { protected interface Parent {} public static class Open {} protected Base(Parent p) {} protected void accept(Parent p) {} }",
        ),
        (
            "/src/api/Mid.java",
            "package api; public class Mid extends Base { protected Mid(Parent p) { super(p); } }",
        ),
        (
            "/src/use/Child.java",
            "package use; public class Child extends api.Mid { public Child(api.Mid.Parent p) { super(p); } @Override protected void accept(api.Mid.Parent p) {} public api.Mid.Open copy(api.Base.Open p) { return p; } }",
        ),
    ];
    register_source_set(&mut db, &fixture, &files);
    // javac accepts all three files. Test both consumers of canonical names:
    // declaration checking and the constructor/return conversion in bodies.
    for index in 1..=files.len() {
        let file = FileId::from_raw(index as u32);
        let diagnostics = hir_ty::class_diagnostics(&db, file);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let tree = hir_def::java::plugin::tree(&db, file);
        for (item, _) in all_items(&tree) {
            if let Some(body) = hir_ty::body_types(&db, file, item) {
                assert!(body.diagnostics.is_empty(), "{:?}", body.diagnostics);
            }
        }
    }
}

#[test]
fn inherited_types_obey_hiding_access_and_diamond_identity() {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let source_set = register_source_set(
        &mut db,
        &fixture,
        &[
            (
                "/src/p/Types.java",
                "package p; interface Root { class Shared {} } interface Left extends Root {} interface Right extends Root {} class Diamond implements Left, Right {} interface Other { class Shared {} } class Ambiguous implements Left, Other {} class Base { public static class Visible { public static class Deep {} } private static class Secret {} static class PackageOnly {} protected static class Protected {} } class Hidden extends Base { public static class Visible {} } class Same extends Base {} class CycleA extends CycleB {} class CycleB extends CycleA {}",
            ),
            (
                "/src/q/Cross.java",
                "package q; class Cross extends p.Base {} class Use {}",
            ),
            (
                "/src/p/Back.java",
                "package p; class Back extends q.Cross {}",
            ),
        ],
    );
    let file = FileId::from_raw(2);
    let tree = hir_def::java::plugin::tree(&db, file);
    let resolver = Resolver::for_file(&tree);
    let scope = hir::ResolutionScope::SourceSet(source_set);
    let resolve = |name: &str| resolve_name_checked(&db, &scope, &resolver, &Name::new(name));
    assert_eq!(
        resolve("p.Diamond.Shared"),
        NameResolution::Resolved(Name::new("p.Root.Shared"))
    );
    assert!(
        matches!(resolve("p.Ambiguous.Shared"), NameResolution::Ambiguous(names) if names.len() == 2)
    );
    assert_eq!(
        resolve("p.Hidden.Visible"),
        NameResolution::Resolved(Name::new("p.Hidden.Visible"))
    );
    assert_eq!(
        resolve("p.Same.Visible.Deep"),
        NameResolution::Resolved(Name::new("p.Base.Visible.Deep"))
    );
    for inaccessible in [
        "p.Same.Secret",
        "q.Cross.PackageOnly",
        "p.Back.PackageOnly",
        "p.Same.Protected",
        "p.CycleA.Missing",
    ] {
        assert_eq!(
            resolve(inaccessible),
            NameResolution::Unresolved,
            "{inaccessible}"
        );
    }
}

#[test]
fn binary_nested_visibility_is_read_from_inner_classes() {
    use common::{
        build_zip, class, class_bytes, class_with_methods_access, register_source_set_classpath,
        temp_jar,
    };
    use hir::{ClasspathEntry, LibraryInfo, LibraryKind};
    use rust_asm::{class_writer::ClassWriter, constants::*};
    use vfs::AbsPathBuf;

    let fixture = jdk_fixture();
    let base = class_with_methods_access(
        "lib/Base",
        Some("java/lang/Object"),
        &[],
        &[("accept", "(Llib/Base$Parent;)V")],
        &[""],
        &[ACC_PROTECTED],
    );
    let mid = class("lib/Mid", Some("lib/Base"), &[]);
    let library = temp_jar("member-types", &[]);
    let mut entries = vec![
        ("lib/Base.class".to_owned(), class_bytes(&base)),
        ("lib/Mid.class".to_owned(), class_bytes(&mid)),
    ];
    for (simple, visibility) in [("Parent", ACC_PROTECTED), ("Secret", ACC_PRIVATE)] {
        let binary = format!("lib/Base${simple}");
        let mut writer = ClassWriter::new(0);
        // javac's ClassFile header does not carry private/protected. The
        // InnerClasses entry must decide inheritance/access (JVMS §4.7.6).
        writer.visit(
            52,
            0,
            ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT,
            &binary,
            Some("java/lang/Object"),
            &[],
        );
        writer.visit_inner_class(
            &binary,
            Some("lib/Base"),
            Some(simple),
            visibility | ACC_STATIC | ACC_INTERFACE | ACC_ABSTRACT,
        );
        entries.push((format!("{binary}.class"), writer.to_bytes().unwrap()));
    }
    build_zip(&library.path, &entries);
    let mut db = TestDatabase::new();
    let set = register_source_set_classpath(
        &mut db,
        &fixture,
        &[(
            "/src/use/Child.java",
            "package use; class Child extends lib.Mid { @Override protected void accept(lib.Mid.Parent p) {} }",
        )],
        vec![
            ClasspathEntry::Library(fixture.lib),
            ClasspathEntry::Library(library.lib),
        ],
        &[(
            library.lib,
            LibraryInfo::new(
                LibraryKind::Jar,
                AbsPathBuf::assert_utf8(library.path.as_std_path().to_owned()),
            ),
        )],
    );
    let file = FileId::from_raw(1);
    let diagnostics = hir_ty::class_diagnostics(&db, file);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let tree = hir_def::java::plugin::tree(&db, file);
    let resolver = Resolver::for_file(&tree);
    let scope = hir::ResolutionScope::SourceSet(set);
    for denied in ["lib.Mid.Parent", "lib.Mid.Secret"] {
        assert_eq!(
            resolve_name_checked(&db, &scope, &resolver, &Name::new(denied)),
            NameResolution::Unresolved
        );
    }
}
