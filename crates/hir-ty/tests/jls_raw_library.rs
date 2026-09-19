//! Bounded classfile erasure and inherited abstract implementations:
//! [JLS §4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6),
//! [§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8), and
//! [§8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1).

mod common;

use common::{ClassSpec, TestDatabase, class_sig, class_with_methods_access_sig};
use hir::{LibraryInfo, LibraryKind};
use hir_def::java::item_tree::ItemData;
use hir_ty::{DeclDiagnostic, TypeError};
use vfs::{AbsPathBuf, FileId};

fn with_library(
    specs: &[ClassSpec<'static>],
    files: &[(&str, &str)],
    check: impl FnOnce(&TestDatabase),
) {
    let fixture = common::jdk_fixture();
    let extra = common::temp_jar("bounded", specs);
    let mut db = TestDatabase::new();
    let info = LibraryInfo::new(
        LibraryKind::Jar,
        AbsPathBuf::assert_utf8(extra.path.as_std_path().to_owned()),
    );
    let classpath = vec![
        hir::ClasspathEntry::Library(fixture.lib),
        hir::ClasspathEntry::Library(extra.lib),
    ];
    common::register_source_set_classpath(
        &mut db,
        &fixture,
        files,
        classpath,
        &[(extra.lib, info)],
    );
    check(&db);
}

fn builders() -> Vec<ClassSpec<'static>> {
    vec![
        class_sig(
            "lib/Message",
            Some("java/lang/Object"),
            &[],
            Some("<M:Llib/Message<TM;TB;>;B:Llib/MessageBuilder<TM;TB;>;>Ljava/lang/Object;"),
        ),
        ClassSpec {
            access: 0x0421, // ACC_PUBLIC | ACC_SUPER | ACC_ABSTRACT
            ..class_with_methods_access_sig(
                "lib/MessageBuilder",
                Some("java/lang/Object"),
                &[],
                &[
                    ("<init>", "()V"),
                    ("internalMergeFrom", "(Llib/Message;)Llib/MessageBuilder;"),
                ],
                &["", "(TM;)TB;"],
                &[0x0004, 0x0404], // protected constructor and abstract method
                Some("<M:Llib/Message<TM;TB;>;B:Llib/MessageBuilder<TM;TB;>;>Ljava/lang/Object;"),
            )
        },
        ClassSpec {
            access: 0x0421,
            ..class_with_methods_access_sig(
                "lib/FullBuilder",
                Some("lib/MessageBuilder"),
                &[],
                &[
                    ("<init>", "()V"),
                    ("internalMergeFrom", "(Llib/Message;)Llib/FullBuilder;"),
                    // A compiler bridge is not the source implementation.
                    ("internalMergeFrom", "(Llib/Message;)Llib/MessageBuilder;"),
                ],
                &["", "(Llib/Message;)TB;", ""],
                &[0x0004, 0x0004, 0x1044],
                Some("<B:Llib/FullBuilder<TB;>;>Llib/MessageBuilder;"),
            )
        },
        ClassSpec {
            access: 0x0421,
            ..class_with_methods_access_sig(
                "lib/ForwardBuilder",
                Some("lib/FullBuilder"),
                &[],
                &[("<init>", "()V")],
                &[""],
                &[0x0004],
                Some("<B:Llib/ForwardBuilder<TB;>;>Llib/FullBuilder<TB;>;"),
            )
        },
    ]
}

#[test]
fn bounded_raw_library_implementation_discharges_abstract_method() {
    with_library(
        &builders(),
        &[(
            "/src/use/Complete.java",
            "package use; class Complete extends lib.ForwardBuilder<Complete> {}",
        )],
        |db| {
            let diagnostics = hir_ty::class_diagnostics(db, FileId::from_raw(1));
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        },
    );
}

#[test]
fn bounded_raw_library_missing_or_wrong_implementation_is_rejected() {
    with_library(
        &builders(),
        &[
            (
                "/src/use/Missing.java",
                "package use; class Missing extends lib.MessageBuilder {}",
            ),
            (
                "/src/use/Wrong.java",
                "package use; class Wrong extends lib.MessageBuilder {
                    protected Wrong internalMergeFrom(Object value) { return this; }
                }",
            ),
        ],
        |db| {
            for (index, expected_class) in [(1, "Missing"), (2, "Wrong")] {
                let diagnostics = hir_ty::class_diagnostics(db, FileId::from_raw(index));
                let missing: Vec<_> = diagnostics
                    .iter()
                    .filter_map(|diagnostic| match diagnostic {
                        DeclDiagnostic::UnimplementedAbstractMethod {
                            class,
                            method,
                            owner,
                            ..
                        } => Some((class.as_str(), method.as_str(), owner.as_str())),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    missing,
                    [(expected_class, "internalMergeFrom", "lib.MessageBuilder")],
                    "{diagnostics:?}"
                );
            }
        },
    );
}

#[test]
fn raw_library_bound_chains_use_declaration_scope_and_erase_arrays() {
    // The method's X shadows the class's X, but T's bound still denotes the
    // class parameter. Hence raw choose() returns Number, never String/Object;
    // direct() instead uses the method's X and returns String. A static generic
    // method retains inference even when selected through the raw class name.
    let specs = [class_with_methods_access_sig(
        "lib/Chains",
        Some("java/lang/Object"),
        &[],
        &[
            ("choose", "()Ljava/lang/Number;"),
            ("direct", "()Ljava/lang/String;"),
            ("values", "()[Ljava/lang/Number;"),
            ("identity", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        ],
        &[
            "<X:Ljava/lang/String;U:TT;>()TU;",
            "<X:Ljava/lang/String;>()TX;",
            "()[TT;",
            "<U:Ljava/lang/Object;>(TU;)TU;",
        ],
        &[0x0001, 0x0001, 0x0001, 0x0009],
        Some("<X:Ljava/lang/Number;T:TX;>Ljava/lang/Object;"),
    )];
    with_library(
        &specs,
        &[(
            "/src/use/Uses.java",
            "package use; class Uses {
                Number good(lib.Chains raw) { return raw.choose(); }
                String direct(lib.Chains raw) { return raw.direct(); }
                Number[] arrays(lib.Chains raw) { return raw.values(); }
                String statics() { return lib.Chains.identity(\"ok\"); }
                String wrong(lib.Chains raw) { return raw.choose(); }
            }",
        )],
        |db| {
            let file = FileId::from_raw(1);
            let tree = hir_def::java::plugin::tree(db, file);
            for (item, data) in common::all_items(&tree) {
                let ItemData::Method(method) = data else {
                    continue;
                };
                let types = hir_ty::body_types(db, file, item).unwrap();
                let errors: Vec<_> = types
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| {
                        !matches!(diagnostic, TypeError::UncheckedInvocation { .. })
                    })
                    .collect();
                if method.name.as_str() == "wrong" {
                    assert_eq!(errors.len(), 1, "{errors:?}");
                    let TypeError::IncompatibleTypes {
                        found, expected, ..
                    } = errors[0]
                    else {
                        panic!("{errors:?}");
                    };
                    assert_eq!(found.display(db).to_string(), "java.lang.Number");
                    assert_eq!(expected.display(db).to_string(), "java.lang.String");
                } else {
                    assert!(errors.is_empty(), "{}: {errors:?}", method.name);
                }
            }
        },
    );
}
