//! Snapshots of subtyping ([JLS §4.10]) and assignability ([JLS §5.2]) over
//! the hand-encoded JDK hierarchy.

#[macro_use]
mod common;

use hir_ty::{BoundKind, Ty, WildcardBound};
use syntax::stub::PrimitiveType;

use crate::common::{
    Relation, TestDatabase, check_relations, check_supertypes, check_supertypes_of,
};

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

fn bounded(db: &TestDatabase, name: &str, bounds: Vec<Ty>) -> Ty {
    Ty::type_var(db, name, bounds)
}

snapshot! {
    type_variable_bounds,
    check_relations(&[
        ("T<:Number <: Number", |db| bounded(db, "T", vec![r(db, "java.lang.Number")]), |db| r(db, "java.lang.Number"), Relation::Subtype),
        ("T<:Number <: Object", |db| bounded(db, "T", vec![r(db, "java.lang.Number")]), |db| r(db, "java.lang.Object"), Relation::Subtype),
        ("T<:ArrayList <: List", |db| bounded(db, "T", vec![r(db, "java.util.ArrayList")]), |db| r(db, "java.util.List"), Relation::Subtype),
        ("T<:none <: Object", |db| bounded(db, "T", vec![]), |db| r(db, "java.lang.Object"), Relation::Subtype),
        ("T<:Number <: String", |db| bounded(db, "T", vec![r(db, "java.lang.Number")]), |db| r(db, "java.lang.String"), Relation::Subtype),
        ("T<:U<:Number <: Number", |db| bounded(db, "T", vec![bounded(db, "U", vec![r(db, "java.lang.Number")])]), |db| r(db, "java.lang.Number"), Relation::Subtype),
        ("T<:Number <: U<:Number", |db| bounded(db, "T", vec![r(db, "java.lang.Number")]), |db| bounded(db, "U", vec![r(db, "java.lang.Number")]), Relation::Subtype),
        ("T<:Number <: Object[]", |db| bounded(db, "T", vec![r(db, "java.lang.Number")]), |db| Ty::array(db, r(db, "java.lang.Object")), Relation::Subtype),
    ]),
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
    generic_supertypes,
    check_supertypes_of(&[
        ("List<String>", |db| l(db, vec![r(db, "java.lang.String")])),
        ("List<?>", |db| l(db, vec![Ty::wildcard(db, None)])),
        (
            "List<? extends Number>",
            |db| l(db, vec![extends(db, r(db, "java.lang.Number"))]),
        ),
        ("ArrayList<Integer>", |db| Ty::reference(
            db,
            "java.util.ArrayList",
            vec![r(db, "java.lang.Integer")],
        )),
        ("Collection<Object>", |db| Ty::reference(
            db,
            "java.util.Collection",
            vec![r(db, "java.lang.Object")],
        )),
    ]),
}

snapshot! {
    generic_parameterized,
    check_relations(&[
        ("List<String> <: Collection<String>", |db| l(db, vec![r(db, "java.lang.String")]), |db| Ty::reference(db, "java.util.Collection", vec![r(db, "java.lang.String")]), Relation::Subtype),
        ("List<String> <: Collection<Integer>", |db| l(db, vec![r(db, "java.lang.String")]), |db| Ty::reference(db, "java.util.Collection", vec![r(db, "java.lang.Integer")]), Relation::Subtype),
        ("ArrayList<Integer> <: List<Integer>", |db| Ty::reference(db, "java.util.ArrayList", vec![r(db, "java.lang.Integer")]), |db| l(db, vec![r(db, "java.lang.Integer")]), Relation::Subtype),
        ("ArrayList<String> <: AbstractList<String>", |db| Ty::reference(db, "java.util.ArrayList", vec![r(db, "java.lang.String")]), |db| Ty::reference(db, "java.util.AbstractList", vec![r(db, "java.lang.String")]), Relation::Subtype),
        ("ArrayList<Integer> <: List<?>", |db| Ty::reference(db, "java.util.ArrayList", vec![r(db, "java.lang.Integer")]), |db| l(db, vec![Ty::wildcard(db, None)]), Relation::Subtype),
        ("List<String> <: Collection<? extends String>", |db| l(db, vec![r(db, "java.lang.String")]), |db| Ty::reference(db, "java.util.Collection", vec![extends(db, r(db, "java.lang.String"))]), Relation::Subtype),
        ("List<Integer> <: Collection<? extends Number>", |db| l(db, vec![r(db, "java.lang.Integer")]), |db| Ty::reference(db, "java.util.Collection", vec![extends(db, r(db, "java.lang.Number"))]), Relation::Subtype),
        ("List<String> <: Collection<? extends Number>", |db| l(db, vec![r(db, "java.lang.String")]), |db| Ty::reference(db, "java.util.Collection", vec![extends(db, r(db, "java.lang.Number"))]), Relation::Subtype),
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

snapshot! {
    boxing_unboxing,
    check_relations(&[
        ("int -> Integer", |db| Ty::primitive(db, PrimitiveType::Int), |db| r(db, "java.lang.Integer"), Relation::Assignable),
        ("int -> Number", |db| Ty::primitive(db, PrimitiveType::Int), |db| r(db, "java.lang.Number"), Relation::Assignable),
        ("int -> Long", |db| Ty::primitive(db, PrimitiveType::Int), |db| r(db, "java.lang.Long"), Relation::Assignable),
        ("long -> Integer", |db| Ty::primitive(db, PrimitiveType::Long), |db| r(db, "java.lang.Integer"), Relation::Assignable),
        ("char -> Number", |db| Ty::primitive(db, PrimitiveType::Char), |db| r(db, "java.lang.Number"), Relation::Assignable),
        ("Integer -> int", |db| r(db, "java.lang.Integer"), |db| Ty::primitive(db, PrimitiveType::Int), Relation::Assignable),
        ("Integer -> long", |db| r(db, "java.lang.Integer"), |db| Ty::primitive(db, PrimitiveType::Long), Relation::Assignable),
        ("Integer -> double", |db| r(db, "java.lang.Integer"), |db| Ty::primitive(db, PrimitiveType::Double), Relation::Assignable),
        ("Number -> int", |db| r(db, "java.lang.Number"), |db| Ty::primitive(db, PrimitiveType::Int), Relation::Assignable),
        ("String -> int", |db| r(db, "java.lang.String"), |db| Ty::primitive(db, PrimitiveType::Int), Relation::Assignable),
        ("int -> Object", |db| Ty::primitive(db, PrimitiveType::Int), |db| r(db, "java.lang.Object"), Relation::Assignable),
        ("String -> int", |db| r(db, "java.lang.String"), |db| Ty::primitive(db, PrimitiveType::Int), Relation::Assignable),
    ]),
}

snapshot! {
    wildcard_capture,
    check_relations(&[
        ("List<String> <: List<? extends Object>", |db| l(db, vec![r(db, "java.lang.String")]), |db| l(db, vec![extends(db, r(db, "java.lang.Object"))]), Relation::Subtype),
        ("List<String> <: List<? extends CharSequence>", |db| l(db, vec![r(db, "java.lang.String")]), |db| l(db, vec![extends(db, r(db, "java.lang.CharSequence"))]), Relation::Subtype),
        ("List<String> <: List<? extends Number>", |db| l(db, vec![r(db, "java.lang.String")]), |db| l(db, vec![extends(db, r(db, "java.lang.Number"))]), Relation::Subtype),
        ("List<String> <: List<? super String>", |db| l(db, vec![r(db, "java.lang.String")]), |db| l(db, vec![super_of(db, r(db, "java.lang.String"))]), Relation::Subtype),
        ("List<CharSequence> <: List<? super String>", |db| l(db, vec![r(db, "java.lang.CharSequence")]), |db| l(db, vec![super_of(db, r(db, "java.lang.String"))]), Relation::Subtype),
        ("List<String> <: List<? super Object>", |db| l(db, vec![r(db, "java.lang.String")]), |db| l(db, vec![super_of(db, r(db, "java.lang.Object"))]), Relation::Subtype),
        ("List<? extends String> <: List<? extends Object>", |db| l(db, vec![extends(db, r(db, "java.lang.String"))]), |db| l(db, vec![extends(db, r(db, "java.lang.Object"))]), Relation::Subtype),
        ("List<? extends Object> <: List<? extends String>", |db| l(db, vec![extends(db, r(db, "java.lang.Object"))]), |db| l(db, vec![extends(db, r(db, "java.lang.String"))]), Relation::Subtype),
        ("List<? extends String> <: List<? super Object>", |db| l(db, vec![extends(db, r(db, "java.lang.String"))]), |db| l(db, vec![super_of(db, r(db, "java.lang.Object"))]), Relation::Subtype),
        ("List<? super Object> <: List<? super String>", |db| l(db, vec![super_of(db, r(db, "java.lang.Object"))]), |db| l(db, vec![super_of(db, r(db, "java.lang.String"))]), Relation::Subtype),
        ("List<? super String> <: List<? super Object>", |db| l(db, vec![super_of(db, r(db, "java.lang.String"))]), |db| l(db, vec![super_of(db, r(db, "java.lang.Object"))]), Relation::Subtype),
        ("List<? super String> <: List<? extends Object>", |db| l(db, vec![super_of(db, r(db, "java.lang.String"))]), |db| l(db, vec![extends(db, r(db, "java.lang.Object"))]), Relation::Subtype),
        ("List<? super String> <: List<? extends CharSequence>", |db| l(db, vec![super_of(db, r(db, "java.lang.String"))]), |db| l(db, vec![extends(db, r(db, "java.lang.CharSequence"))]), Relation::Subtype),
        ("List<? extends String> <: List<String>", |db| l(db, vec![extends(db, r(db, "java.lang.String"))]), |db| l(db, vec![r(db, "java.lang.String")]), Relation::Subtype),
        ("List<? super String> <: List<String>", |db| l(db, vec![super_of(db, r(db, "java.lang.String"))]), |db| l(db, vec![r(db, "java.lang.String")]), Relation::Subtype),
        ("List<Integer> <: List<? super Number>", |db| l(db, vec![r(db, "java.lang.Integer")]), |db| l(db, vec![super_of(db, r(db, "java.lang.Number"))]), Relation::Subtype),
        ("List<String> <: List<? super CharSequence>", |db| l(db, vec![r(db, "java.lang.String")]), |db| l(db, vec![super_of(db, r(db, "java.lang.CharSequence"))]), Relation::Subtype),
    ]),
}
