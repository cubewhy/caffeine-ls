//! Snapshots of the [`Ty`] model: display ([JLS §4.1]–[§4.8]), erasure
//! ([§4.6]), classification flags and array element types.

#[macro_use]
mod common;

use hir_expand::name::Name;
use hir_ty::{BoundKind, Ty, WildcardBound, ty_from_source};
use syntax::stub::{PrimitiveType, TypeBound, TypeRef};

use crate::common::{TestDatabase, check_ty_model, check_ty_simple};

fn r(db: &TestDatabase, name: &str, args: Vec<Ty>) -> Ty {
    Ty::reference(db, name, args)
}

snapshot! {
    display_and_erasure,
    check_ty_model(&[
        ("int", |db| Ty::primitive(db, PrimitiveType::Int)),
        ("void", |db| Ty::void(db)),
        ("java.lang.String", |db| r(db, "java.lang.String", vec![])),
        (
            "java.util.List<java.lang.String>",
            |db| r(db, "java.util.List", vec![r(db, "java.lang.String", vec![])]),
        ),
        (
            "java.util.Map<java.lang.String, java.lang.Integer>",
            |db| r(
                db,
                "java.util.Map",
                vec![r(db, "java.lang.String", vec![]), r(db, "java.lang.Integer", vec![])],
            ),
        ),
        ("java.lang.String[]", |db| Ty::array(db, r(db, "java.lang.String", vec![]))),
        ("int[][]", |db| Ty::array(db, Ty::array(db, Ty::primitive(db, PrimitiveType::Int)))),
        (
            "java.util.List<java.lang.String>[]",
            |db| Ty::array(db, r(db, "java.util.List", vec![r(db, "java.lang.String", vec![])])),
        ),
        ("T", |db| Ty::type_var(db, "T", vec![])),
        (
            "T extends java.lang.Number",
            |db| Ty::type_var(db, "T", vec![r(db, "java.lang.Number", vec![])]),
        ),
        (
            "T extends java.lang.Number & java.io.Serializable",
            |db| Ty::type_var(
                db,
                "T",
                vec![
                    r(db, "java.lang.Number", vec![]),
                    r(db, "java.io.Serializable", vec![]),
                ],
            ),
        ),
        ("java.lang.Object", |db| r(db, "java.lang.Object", vec![])),
        ("<error>", |db| Ty::error(db)),
    ]),
}

snapshot! {
    wildcards,
    check_ty_model(&[
        ("?", |db| Ty::wildcard(db, None)),
        (
            "? extends java.lang.Number",
            |db| Ty::wildcard(db, Some(Box::new(WildcardBound {
                kind: BoundKind::Upper,
                ty: r(db, "java.lang.Number", vec![]),
            }))),
        ),
        (
            "? super java.lang.Integer",
            |db| Ty::wildcard(db, Some(Box::new(WildcardBound {
                kind: BoundKind::Lower,
                ty: r(db, "java.lang.Integer", vec![]),
            }))),
        ),
    ]),
}

snapshot! {
    from_source_lowering,
    check_ty_model(&[(
        "Map<String, ? extends byte[]>",
        |db| {
            let tyref = TypeRef::Reference {
                name: Name::new("java.util.Map"),
                generic_args: vec![
                    TypeRef::Reference {
                        name: Name::new("java.lang.String"),
                        generic_args: Vec::new(),
                    },
                    TypeRef::Wildcard {
                        bound: Some(Box::new(TypeBound::Upper(TypeRef::Array(Box::new(
                            TypeRef::Primitive(PrimitiveType::Byte),
                        ))))),
                    },
                ],
            };
            ty_from_source(db, &tyref)
        },
    )]),
}

snapshot! {
    simple_display,
    check_ty_simple(&[
        ("int", |db| Ty::primitive(db, PrimitiveType::Int)),
        ("void", |db| Ty::void(db)),
        ("java.lang.String", |db| r(db, "java.lang.String", vec![])),
        (
            "java.util.List<java.lang.String>",
            |db| r(db, "java.util.List", vec![r(db, "java.lang.String", vec![])]),
        ),
        (
            "java.util.Map<java.lang.String, java.lang.Integer>",
            |db| r(
                db,
                "java.util.Map",
                vec![r(db, "java.lang.String", vec![]), r(db, "java.lang.Integer", vec![])],
            ),
        ),
        ("java.lang.String[]", |db| Ty::array(db, r(db, "java.lang.String", vec![]))),
        (
            "java.util.List<java.lang.String>[]",
            |db| Ty::array(db, r(db, "java.util.List", vec![r(db, "java.lang.String", vec![])])),
        ),
        ("com.example.A$B", |db| r(db, "com.example.A$B", vec![])),
        ("com.example.Outer$Inner", |db| r(db, "com.example.Outer$Inner", vec![])),
        (
            "java.util.List<? extends java.lang.Number>",
            |db| r(
                db,
                "java.util.List",
                vec![Ty::wildcard(db, Some(Box::new(WildcardBound {
                    kind: BoundKind::Upper,
                    ty: r(db, "java.lang.Number", vec![]),
                })))],
            ),
        ),
        ("T", |db| Ty::type_var(db, "T", vec![])),
        ("NotFound", |db| r(db, "NotFound", vec![])),
        ("<error>", |db| Ty::error(db)),
    ]),
}
