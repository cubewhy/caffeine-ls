//! Snapshots of source-class indexing and subtyping: classes declared in
//! workspace sources resolve as [`hir::Resolved::Source`] and participate in
//! the supertype/subtype walks of [JLS §4.10.2] exactly like library classes.
//! The source set's own classes shadow classpath libraries for name
//! resolution (§6.5.5).

#[macro_use]
mod common;

use hir_ty::{BoundKind, Ty, WildcardBound};

use crate::common::{Relation, TestDatabase, check_source_relations, check_source_supertypes};

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
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

fn list_of(db: &TestDatabase, ty: Ty) -> Ty {
    Ty::reference(db, "java.util.List", vec![ty])
}

snapshot! {
    source_supertypes,
    check_source_supertypes(
        &[
            ("/src/main/java/com/example/A.java",
             "package com.example;\nclass A {}\n"),
            ("/src/main/java/com/example/B.java",
             "package com.example;\nclass B extends A {}\n"),
            ("/src/main/java/com/example/I.java",
             "package com.example;\ninterface I {}\n"),
            ("/src/main/java/com/example/C.java",
             "package com.example;\nclass C extends A implements I, java.io.Serializable {}\n"),
            ("/src/main/java/com/example/E.java",
             "package com.example;\nenum E { X, Y }\n"),
            ("/src/main/java/com/example/R.java",
             "package com.example;\nrecord R(int x) {}\n"),
            ("/src/main/java/com/example/Anno.java",
             "package com.example;\n@interface Anno {}\n"),
            ("/src/main/java/com/example/Box.java",
             "package com.example;\nclass Box<T> implements java.util.List<T> {}\n"),
            ("/src/main/java/com/example/Nested.java",
             "package com.example;\nclass Nested {\n    static class Inner extends A {}\n}\n"),
        ],
        &[
            ("A", |db| r(db, "com.example.A")),
            ("B", |db| r(db, "com.example.B")),
            ("I", |db| r(db, "com.example.I")),
            ("C", |db| r(db, "com.example.C")),
            ("E", |db| r(db, "com.example.E")),
            ("R", |db| r(db, "com.example.R")),
            ("Anno", |db| r(db, "com.example.Anno")),
            ("Nested.Inner", |db| r(db, "com.example.Nested.Inner")),
            ("Box raw", |db| r(db, "com.example.Box")),
            ("Box<String>", |db| Ty::reference(db, "com.example.Box", vec![r(db, "java.lang.String")])),
            ("Box<List<String>>", |db| Ty::reference(db, "com.example.Box", vec![list_of(db, r(db, "java.lang.String"))])),
        ],
    ),
}

snapshot! {
    source_subtyping,
    check_source_relations(
        &[
            ("/src/main/java/com/example/A.java",
             "package com.example;\nclass A {}\n"),
            ("/src/main/java/com/example/B.java",
             "package com.example;\nclass B extends A {}\n"),
            ("/src/main/java/com/example/I.java",
             "package com.example;\ninterface I {}\n"),
            ("/src/main/java/com/example/C.java",
             "package com.example;\nclass C extends A implements I, java.io.Serializable {}\n"),
            ("/src/main/java/com/example/Box.java",
             "package com.example;\nclass Box<T> implements java.util.List<T> {}\n"),
            ("/src/main/java/com/example/E.java",
             "package com.example;\nenum E { X, Y }\n"),
        ],
        &[
            ("B <: A", |db| r(db, "com.example.B"), |db| r(db, "com.example.A"), Relation::Subtype),
            ("B <: Object", |db| r(db, "com.example.B"), |db| r(db, "java.lang.Object"), Relation::Subtype),
            ("A <: B", |db| r(db, "com.example.A"), |db| r(db, "com.example.B"), Relation::Subtype),
            ("C <: I", |db| r(db, "com.example.C"), |db| r(db, "com.example.I"), Relation::Subtype),
            ("C <: Serializable", |db| r(db, "com.example.C"), |db| r(db, "java.io.Serializable"), Relation::Subtype),
            ("C <: A[]", |db| r(db, "com.example.C"), |db| Ty::array(db, r(db, "com.example.A")), Relation::Subtype),
            ("Box<String> <: List<String>", |db| Ty::reference(db, "com.example.Box", vec![r(db, "java.lang.String")]), |db| list_of(db, r(db, "java.lang.String")), Relation::Subtype),
            ("Box<String> <: Collection<String>", |db| Ty::reference(db, "com.example.Box", vec![r(db, "java.lang.String")]), |db| Ty::reference(db, "java.util.Collection", vec![r(db, "java.lang.String")]), Relation::Subtype),
            ("Box<String> <: List<Integer>", |db| Ty::reference(db, "com.example.Box", vec![r(db, "java.lang.String")]), |db| list_of(db, r(db, "java.lang.Integer")), Relation::Subtype),
            ("Box<String> <: List<?>", |db| Ty::reference(db, "com.example.Box", vec![r(db, "java.lang.String")]), |db| list_of(db, extends(db, r(db, "java.lang.Object"))), Relation::Subtype),
            ("E <: Enum", |db| r(db, "com.example.E"), |db| r(db, "java.lang.Enum"), Relation::Subtype),
            ("B <: I", |db| r(db, "com.example.B"), |db| r(db, "com.example.I"), Relation::Subtype),
        ],
    ),
}

snapshot! {
    source_classpath_shadowing,
    check_source_relations(
        &[
            ("/src/main/java/com/example/App.java",
             "package com.example;\nclass App {\n    Box b;\n}\n"),
            ("/src/main/java/com/example/Box.java",
             "package com.example;\nclass Box implements java.io.Serializable {}\n"),
        ],
        &[
            ("App.field-is-Source-Box", |db| Ty::reference(db, "com.example.Box", Vec::new()), |db| r(db, "java.io.Serializable"), Relation::Subtype),
            ("Source-Box <: Serializable", |db| r(db, "com.example.Box"), |db| r(db, "java.io.Serializable"), Relation::Subtype),
        ],
    ),
}
