//! Snapshots of subtyping ([JLS §4.10]) and assignability ([JLS §5.2]) over
//! the hand-encoded JDK hierarchy.

#[macro_use]
mod common;

use hir_ty::{BoundKind, Ty, WildcardBound};
use syntax::stub::PrimitiveType;

use crate::common::{Relation, TestDatabase, check_relations, check_supertypes};

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
}

fn l(db: &TestDatabase, args: Vec<Ty>) -> Ty {
    Ty::reference(db, "java.util.List", args)
}

fn extends(db: &TestDatabase, ty: Ty) -> Ty {
    Ty::wildcard(
        db,
        Some(Box::new(WildcardBound {
            kind: BoundKind::Upper,
            ty,
        })),
    )
}

fn super_of(db: &TestDatabase, ty: Ty) -> Ty {
    Ty::wildcard(
        db,
        Some(Box::new(WildcardBound {
            kind: BoundKind::Lower,
            ty,
        })),
    )
}

snapshot! {
    class_hierarchy,
    check_relations(&[
        ("String <: Object", |db| r(db, "java.lang.String"), |db| r(db, "java.lang.Object"), Relation::Subtype),
        ("String <: CharSequence", |db| r(db, "java.lang.String"), |db| r(db, "java.lang.CharSequence"), Relation::Subtype),
        ("Object <: String", |db| r(db, "java.lang.Object"), |db| r(db, "java.lang.String"), Relation::Subtype),
        ("ArrayList <: List", |db| r(db, "java.util.ArrayList"), |db| r(db, "java.util.List"), Relation::Subtype),
        ("ArrayList <: Collection", |db| r(db, "java.util.ArrayList"), |db| r(db, "java.util.Collection"), Relation::Subtype),
        ("ArrayList <: Object", |db| r(db, "java.util.ArrayList"), |db| r(db, "java.lang.Object"), Relation::Subtype),
        ("List <: ArrayList", |db| r(db, "java.util.List"), |db| r(db, "java.util.ArrayList"), Relation::Subtype),
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
        ("String[] <: Object[]", |db| Ty::array(db, r(db, "java.lang.String")), |db| Ty::array(db, r(db, "java.lang.Object")), Relation::Subtype),
        ("Object[] <: String[]", |db| Ty::array(db, r(db, "java.lang.Object")), |db| Ty::array(db, r(db, "java.lang.String")), Relation::Subtype),
        ("String[] <: Integer[]", |db| Ty::array(db, r(db, "java.lang.String")), |db| Ty::array(db, r(db, "java.lang.Integer")), Relation::Subtype),
        ("String[] <: Object", |db| Ty::array(db, r(db, "java.lang.String")), |db| r(db, "java.lang.Object"), Relation::Subtype),
        ("String[] <: Cloneable", |db| Ty::array(db, r(db, "java.lang.String")), |db| r(db, "java.lang.Cloneable"), Relation::Subtype),
        ("String[] <: Serializable", |db| Ty::array(db, r(db, "java.lang.String")), |db| r(db, "java.io.Serializable"), Relation::Subtype),
    ]),
}

snapshot! {
    primitives,
    check_relations(&[
        (
            "int <: int",
            |db| Ty::primitive(db, PrimitiveType::Int),
            |db| Ty::primitive(db, PrimitiveType::Int),
            Relation::Subtype,
        ),
        (
            "int <: long",
            |db| Ty::primitive(db, PrimitiveType::Int),
            |db| Ty::primitive(db, PrimitiveType::Long),
            Relation::Subtype,
        ),
    ]),
}

snapshot! {
    raw_and_parameterized,
    check_relations(&[
        ("List<String> <: List", |db| l(db, vec![r(db, "java.lang.String")]), |db| l(db, vec![]), Relation::Subtype),
        ("List <: List<String>", |db| l(db, vec![]), |db| l(db, vec![r(db, "java.lang.String")]), Relation::Subtype),
        ("List<String> <: List<String>", |db| l(db, vec![r(db, "java.lang.String")]), |db| l(db, vec![r(db, "java.lang.String")]), Relation::Subtype),
    ]),
}

snapshot! {
    wildcards,
    check_relations(&[
        ("List<Integer> <: List<?>", |db| l(db, vec![r(db, "java.lang.Integer")]), |db| l(db, vec![Ty::wildcard(db, None)]), Relation::Subtype),
        (
            "List<Integer> <: List<? extends Number>",
            |db| l(db, vec![r(db, "java.lang.Integer")]),
            |db| l(db, vec![extends(db, r(db, "java.lang.Number"))]),
            Relation::Subtype,
        ),
        (
            "List<Object> <: List<? extends Number>",
            |db| l(db, vec![r(db, "java.lang.Object")]),
            |db| l(db, vec![extends(db, r(db, "java.lang.Number"))]),
            Relation::Subtype,
        ),
        (
            "List<Integer> <: List<? super Integer>",
            |db| l(db, vec![r(db, "java.lang.Integer")]),
            |db| l(db, vec![super_of(db, r(db, "java.lang.Integer"))]),
            Relation::Subtype,
        ),
        (
            "List<Integer> <: List<? super Number>",
            |db| l(db, vec![r(db, "java.lang.Integer")]),
            |db| l(db, vec![super_of(db, r(db, "java.lang.Number"))]),
            Relation::Subtype,
        ),
    ]),
}

snapshot! {
    assignability,
    check_relations(&[
        (
            "int -> int",
            |db| Ty::primitive(db, PrimitiveType::Int),
            |db| Ty::primitive(db, PrimitiveType::Int),
            Relation::Assignable,
        ),
        (
            "int -> long",
            |db| Ty::primitive(db, PrimitiveType::Int),
            |db| Ty::primitive(db, PrimitiveType::Long),
            Relation::Assignable,
        ),
        (
            "byte -> int",
            |db| Ty::primitive(db, PrimitiveType::Byte),
            |db| Ty::primitive(db, PrimitiveType::Int),
            Relation::Assignable,
        ),
        (
            "char -> long",
            |db| Ty::primitive(db, PrimitiveType::Char),
            |db| Ty::primitive(db, PrimitiveType::Long),
            Relation::Assignable,
        ),
        (
            "long -> float",
            |db| Ty::primitive(db, PrimitiveType::Long),
            |db| Ty::primitive(db, PrimitiveType::Float),
            Relation::Assignable,
        ),
        (
            "float -> double",
            |db| Ty::primitive(db, PrimitiveType::Float),
            |db| Ty::primitive(db, PrimitiveType::Double),
            Relation::Assignable,
        ),
        (
            "long -> int",
            |db| Ty::primitive(db, PrimitiveType::Long),
            |db| Ty::primitive(db, PrimitiveType::Int),
            Relation::Assignable,
        ),
        (
            "double -> float",
            |db| Ty::primitive(db, PrimitiveType::Double),
            |db| Ty::primitive(db, PrimitiveType::Float),
            Relation::Assignable,
        ),
        ("String -> Object", |db| r(db, "java.lang.String"), |db| r(db, "java.lang.Object"), Relation::Assignable),
        ("ArrayList -> List", |db| r(db, "java.util.ArrayList"), |db| r(db, "java.util.List"), Relation::Assignable),
        ("Object -> String", |db| r(db, "java.lang.Object"), |db| r(db, "java.lang.String"), Relation::Assignable),
        ("List -> ArrayList", |db| r(db, "java.util.List"), |db| r(db, "java.util.ArrayList"), Relation::Assignable),
    ]),
}
