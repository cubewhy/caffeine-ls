//! Snapshots of the least upper bound
//! ([JLS §4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)).
//! The classes mirror the spec's running examples: `String` and `Integer`
//! share `Serializable` and `Comparable`, and `ArrayList<E>` extends
//! `List<E>`. Generic candidates recover their type arguments with the least
//! containing parameterization (`lcp`) / least containing type argument
//! (`lcta`).

#[macro_use]
mod common;

use hir::{LibraryId, LibraryInfo, LibraryKind};
use hir_ty::{Ty, least_upper_bound};
use tempfile::TempDir;
use vfs::AbsPathBuf;

use crate::common::{TestDatabase, TyBuilder, build_jar, class_sig, interface, interface_sig};

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
}

fn of(db: &TestDatabase, name: &str, arg: Ty) -> Ty {
    Ty::reference(db, name, vec![arg])
}

fn lub_classes() -> Vec<common::ClassSpec<'static>> {
    vec![
        common::class("java/lang/Object", None, &[]),
        interface("java/io/Serializable"),
        common::class("java/lang/Number", Some("java/lang/Object"), &[]),
        interface_sig(
            "java/lang/Comparable",
            &[],
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;"),
        ),
        class_sig(
            "java/lang/String",
            Some("java/lang/Object"),
            &["java/io/Serializable", "java/lang/Comparable"],
            Some(
                "Ljava/lang/Object;Ljava/io/Serializable;Ljava/lang/Comparable<Ljava/lang/String;>;",
            ),
        ),
        class_sig(
            "java/lang/Integer",
            Some("java/lang/Number"),
            &["java/io/Serializable", "java/lang/Comparable"],
            Some(
                "Ljava/lang/Number;Ljava/io/Serializable;Ljava/lang/Comparable<Ljava/lang/Integer;>;",
            ),
        ),
        interface_sig(
            "java/util/List",
            &[],
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;"),
        ),
        class_sig(
            "java/util/ArrayList",
            Some("java/lang/Object"),
            &["java/util/List"],
            Some("<E:Ljava/lang/Object;>Ljava/lang/Object;Ljava/util/List<TE;>;"),
        ),
    ]
}

fn check_lub(classes: Vec<common::ClassSpec<'static>>, samples: &[(&str, &[TyBuilder])]) -> String {
    let _dir = TempDir::new().unwrap();
    let base = camino::Utf8PathBuf::from_path_buf(_dir.path().join("lib")).unwrap();
    std::fs::create_dir_all(&base).unwrap();
    let jar = base.join("lib.jar");
    build_jar(&jar, &classes);
    let lib = LibraryId::from_file_path(jar.as_std_path()).unwrap();
    let mut db = TestDatabase::new();
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(
        lib,
        LibraryInfo::new(
            LibraryKind::Jar,
            AbsPathBuf::assert_utf8(jar.as_std_path().to_owned()),
        ),
    );
    data.jdk_libraries.push(lib);
    hir::set_project_graph(&mut db, data);
    let scope = hir::ResolutionScope::Classpath(vec![lib]);

    samples
        .iter()
        .map(|(label, builders)| {
            let tys: Vec<Ty> = builders.iter().map(|build| build(&db)).collect();
            let rendered: Vec<String> = tys.iter().map(|t| t.display(&db).to_string()).collect();
            let lub = least_upper_bound(&db, &scope, &tys);
            format!(
                "{label} lub({}) -> {}",
                rendered.join(", "),
                lub.display(&db)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

snapshot! {
    lub_intersection,
    check_lub(
        lub_classes(),
        &[
            ("singular", &[|db| r(db, "java.lang.String")]),
            (
                "string_integer",
                &[
                    |db| r(db, "java.lang.String"),
                    |db| r(db, "java.lang.Integer"),
                ],
            ),
            (
                "number_string",
                &[
                    |db| r(db, "java.lang.Number"),
                    |db| r(db, "java.lang.String"),
                ],
            ),
        ],
    ),
}

snapshot! {
    lub_parameterized,
    check_lub(
        lub_classes(),
        &[
            (
                "list_string_list_object",
                &[
                    |db| of(db, "java.util.List", r(db, "java.lang.String")),
                    |db| of(db, "java.util.List", r(db, "java.lang.Object")),
                ],
            ),
            (
                "arraylist_string_arraylist_integer",
                &[
                    |db| of(db, "java.util.ArrayList", r(db, "java.lang.String")),
                    |db| of(db, "java.util.ArrayList", r(db, "java.lang.Integer")),
                ],
            ),
            (
                "list_string_object",
                &[
                    |db| of(db, "java.util.List", r(db, "java.lang.String")),
                    |db| r(db, "java.lang.Object"),
                ],
            ),
            (
                // §4.10.4: when every operand shares the *same* parameterization
                // of the candidate, `lcp` is that parameterization exactly —
                // `lub(ArrayList<String>, ArrayList<String>)` is
                // `ArrayList<String>`, not `ArrayList<? extends Object>`.
                "arraylist_string_arraylist_string",
                &[
                    |db| of(db, "java.util.ArrayList", r(db, "java.lang.String")),
                    |db| of(db, "java.util.ArrayList", r(db, "java.lang.String")),
                ],
            ),
        ],
    ),
}
