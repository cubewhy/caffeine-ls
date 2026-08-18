//! Snapshots of subtyping ([JLS §4.10]) and assignability ([JLS §5.2]) over
//! the hand-encoded JDK hierarchy.

#[macro_use]
mod common;

use hir_ty::{BoundKind, Ty, WildcardBound};
use syntax::stub::PrimitiveType;

use crate::common::{Relation, check_relations, check_supertypes};

fn r(name: &str) -> Ty {
    Ty::reference(name, Vec::new())
}

fn l(args: Vec<Ty>) -> Ty {
    Ty::reference("java.util.List", args)
}

fn extends(ty: Ty) -> Ty {
    Ty::wildcard(Some(Box::new(WildcardBound {
        kind: BoundKind::Upper,
        ty,
    })))
}

fn super_of(ty: Ty) -> Ty {
    Ty::wildcard(Some(Box::new(WildcardBound {
        kind: BoundKind::Lower,
        ty,
    })))
}

snapshot! {
    class_hierarchy,
    check_relations(&[
        ("String <: Object", r("java.lang.String"), r("java.lang.Object"), Relation::Subtype),
        ("String <: CharSequence", r("java.lang.String"), r("java.lang.CharSequence"), Relation::Subtype),
        ("Object <: String", r("java.lang.Object"), r("java.lang.String"), Relation::Subtype),
        ("ArrayList <: List", r("java.util.ArrayList"), r("java.util.List"), Relation::Subtype),
        ("ArrayList <: Collection", r("java.util.ArrayList"), r("java.util.Collection"), Relation::Subtype),
        ("ArrayList <: Object", r("java.util.ArrayList"), r("java.lang.Object"), Relation::Subtype),
        ("List <: ArrayList", r("java.util.List"), r("java.util.ArrayList"), Relation::Subtype),
    ]),
}

snapshot! {
    supertypes,
    check_supertypes(&[
        "java.util.ArrayList",
        "java.util.List",
        "java.lang.String",
        "java.lang.Object",
    ]),
}

snapshot! {
    arrays,
    check_relations(&[
        ("String[] <: Object[]", Ty::array(r("java.lang.String")), Ty::array(r("java.lang.Object")), Relation::Subtype),
        ("Object[] <: String[]", Ty::array(r("java.lang.Object")), Ty::array(r("java.lang.String")), Relation::Subtype),
        ("String[] <: Integer[]", Ty::array(r("java.lang.String")), Ty::array(r("java.lang.Integer")), Relation::Subtype),
        ("String[] <: Object", Ty::array(r("java.lang.String")), r("java.lang.Object"), Relation::Subtype),
        ("String[] <: Cloneable", Ty::array(r("java.lang.String")), r("java.lang.Cloneable"), Relation::Subtype),
        ("String[] <: Serializable", Ty::array(r("java.lang.String")), r("java.io.Serializable"), Relation::Subtype),
    ]),
}

snapshot! {
    primitives,
    check_relations(&[
        (
            "int <: int",
            Ty::primitive(PrimitiveType::Int),
            Ty::primitive(PrimitiveType::Int),
            Relation::Subtype,
        ),
        (
            "int <: long",
            Ty::primitive(PrimitiveType::Int),
            Ty::primitive(PrimitiveType::Long),
            Relation::Subtype,
        ),
    ]),
}

snapshot! {
    raw_and_parameterized,
    check_relations(&[
        ("List<String> <: List", l(vec![r("java.lang.String")]), l(vec![]), Relation::Subtype),
        ("List <: List<String>", l(vec![]), l(vec![r("java.lang.String")]), Relation::Subtype),
        ("List<String> <: List<String>", l(vec![r("java.lang.String")]), l(vec![r("java.lang.String")]), Relation::Subtype),
    ]),
}

snapshot! {
    wildcards,
    check_relations(&[
        ("List<Integer> <: List<?>", l(vec![r("java.lang.Integer")]), l(vec![Ty::wildcard(None)]), Relation::Subtype),
        (
            "List<Integer> <: List<? extends Number>",
            l(vec![r("java.lang.Integer")]),
            l(vec![extends(r("java.lang.Number"))]),
            Relation::Subtype,
        ),
        (
            "List<Object> <: List<? extends Number>",
            l(vec![r("java.lang.Object")]),
            l(vec![extends(r("java.lang.Number"))]),
            Relation::Subtype,
        ),
        (
            "List<Integer> <: List<? super Integer>",
            l(vec![r("java.lang.Integer")]),
            l(vec![super_of(r("java.lang.Integer"))]),
            Relation::Subtype,
        ),
        (
            "List<Integer> <: List<? super Number>",
            l(vec![r("java.lang.Integer")]),
            l(vec![super_of(r("java.lang.Number"))]),
            Relation::Subtype,
        ),
    ]),
}

snapshot! {
    assignability,
    check_relations(&[
        (
            "int -> int",
            Ty::primitive(PrimitiveType::Int),
            Ty::primitive(PrimitiveType::Int),
            Relation::Assignable,
        ),
        (
            "int -> long",
            Ty::primitive(PrimitiveType::Int),
            Ty::primitive(PrimitiveType::Long),
            Relation::Assignable,
        ),
        (
            "byte -> int",
            Ty::primitive(PrimitiveType::Byte),
            Ty::primitive(PrimitiveType::Int),
            Relation::Assignable,
        ),
        (
            "char -> long",
            Ty::primitive(PrimitiveType::Char),
            Ty::primitive(PrimitiveType::Long),
            Relation::Assignable,
        ),
        (
            "long -> float",
            Ty::primitive(PrimitiveType::Long),
            Ty::primitive(PrimitiveType::Float),
            Relation::Assignable,
        ),
        (
            "float -> double",
            Ty::primitive(PrimitiveType::Float),
            Ty::primitive(PrimitiveType::Double),
            Relation::Assignable,
        ),
        (
            "long -> int",
            Ty::primitive(PrimitiveType::Long),
            Ty::primitive(PrimitiveType::Int),
            Relation::Assignable,
        ),
        (
            "double -> float",
            Ty::primitive(PrimitiveType::Double),
            Ty::primitive(PrimitiveType::Float),
            Relation::Assignable,
        ),
        ("String -> Object", r("java.lang.String"), r("java.lang.Object"), Relation::Assignable),
        ("ArrayList -> List", r("java.util.ArrayList"), r("java.util.List"), Relation::Assignable),
        ("Object -> String", r("java.lang.Object"), r("java.lang.String"), Relation::Assignable),
        ("List -> ArrayList", r("java.util.List"), r("java.util.ArrayList"), Relation::Assignable),
    ]),
}
