//! Snapshots of the [`Ty`] model: display ([JLS §4.1]–[§4.8]), erasure
//! ([§4.6]), classification flags and array element types.

#[macro_use]
mod common;

use hir_expand::name::Name;
use hir_ty::{BoundKind, Ty, WildcardBound, ty_from_source};
use syntax::stub::{PrimitiveType, TypeBound, TypeRef};

use crate::common::check_ty_model;

fn r(name: &str, args: Vec<Ty>) -> Ty {
    Ty::reference(name, args)
}

snapshot! {
    display_and_erasure,
    check_ty_model(&[
        ("int", Ty::primitive(PrimitiveType::Int)),
        ("void", Ty::void()),
        ("java.lang.String", r("java.lang.String", vec![])),
        (
            "java.util.List<java.lang.String>",
            r("java.util.List", vec![r("java.lang.String", vec![])]),
        ),
        (
            "java.util.Map<java.lang.String, java.lang.Integer>",
            r(
                "java.util.Map",
                vec![r("java.lang.String", vec![]), r("java.lang.Integer", vec![])],
            ),
        ),
        ("java.lang.String[]", Ty::array(r("java.lang.String", vec![]))),
        ("int[][]", Ty::array(Ty::array(Ty::primitive(PrimitiveType::Int)))),
        (
            "java.util.List<java.lang.String>[]",
            Ty::array(r("java.util.List", vec![r("java.lang.String", vec![])])),
        ),
        ("T", Ty::type_var("T")),
        ("java.lang.Object", r("java.lang.Object", vec![])),
        ("<error>", Ty::error()),
    ]),
}

snapshot! {
    wildcards,
    check_ty_model(&[
        ("?", Ty::wildcard(None)),
        (
            "? extends java.lang.Number",
            Ty::wildcard(Some(Box::new(WildcardBound {
                kind: BoundKind::Upper,
                ty: r("java.lang.Number", vec![]),
            }))),
        ),
        (
            "? super java.lang.Integer",
            Ty::wildcard(Some(Box::new(WildcardBound {
                kind: BoundKind::Lower,
                ty: r("java.lang.Integer", vec![]),
            }))),
        ),
    ]),
}

snapshot! {
    from_source_lowering,
    {
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
        check_ty_model(&[("Map<String, ? extends byte[]>", ty_from_source(&tyref))])
    },
}
