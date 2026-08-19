//! Snapshots of method resolution
//! ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
//! [§15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2))
//! over the hand-encoded JDK hierarchy and source classes.

#[macro_use]
mod common;

use hir_ty::{Ty, member_set};
use syntax::stub::PrimitiveType;

use crate::common::{TestDatabase, check_methods, check_source_methods};

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
}

fn l(db: &TestDatabase, args: Vec<Ty>) -> Ty {
    Ty::reference(db, "java.util.List", args)
}

fn str(db: &TestDatabase) -> Ty {
    r(db, "java.lang.String")
}

fn int(db: &TestDatabase) -> Ty {
    Ty::primitive(db, PrimitiveType::Int)
}

fn integer(db: &TestDatabase) -> Ty {
    r(db, "java.lang.Integer")
}

fn object(db: &TestDatabase) -> Ty {
    r(db, "java.lang.Object")
}

snapshot! {
    member_set_library,
    {
        let fixture = common::jdk_fixture();
        let mut db = TestDatabase::new();
        common::register_jdk(&mut db, &fixture);
        let scope = hir::ResolutionScope::Classpath(vec![fixture.lib]);
        let mut out = Vec::new();
        for (label, receiver, name) in [
            ("List<String>", l(&db, vec![str(&db)]), "add"),
            ("List<String>", l(&db, vec![str(&db)]), "get"),
            ("List<String>", l(&db, vec![str(&db)]), "subList"),
            ("ArrayList<String>", Ty::reference(&db, "java.util.ArrayList", vec![str(&db)]), "add"),
            ("List<Integer>", l(&db, vec![integer(&db)]), "add"),
        ] {
            let mut lines = vec![format!("{label}.{name}")];
            let ctx = hir_ty::InvocationContext::external(&scope);
            for method in member_set(&db, &scope, &receiver, name, &ctx) {
                lines.push(format!("  {} -> {}", method.display(&db), method.ret.display(&db)));
            }
            out.push(lines.join("\n"));
        }
        out.join("\n")
    },
}

snapshot! {
    method_resolution_library,
    check_methods(&[
        ("List<String>.add(String)", |db| l(db, vec![str(db)]), "add", &[str as common::TyBuilder]),
        ("List<String>.add(Object)", |db| l(db, vec![str(db)]), "add", &[object as common::TyBuilder]),
        ("List<String>.add(Integer)", |db| l(db, vec![str(db)]), "add", &[integer as common::TyBuilder]),
        ("List<Integer>.add(Integer)", |db| l(db, vec![integer(db)]), "add", &[integer as common::TyBuilder]),
        ("List<String>.get(int)", |db| l(db, vec![str(db)]), "get", &[int as common::TyBuilder]),
        ("List<String>.size()", |db| l(db, vec![str(db)]), "size", &[]),
        ("List<String>.subList(int,int)", |db| l(db, vec![str(db)]), "subList", &[int as common::TyBuilder, int as common::TyBuilder]),
        ("List<String>.subList(String)", |db| l(db, vec![str(db)]), "subList", &[str as common::TyBuilder]),
        ("ArrayList<String>.add(String)", |db| Ty::reference(db, "java.util.ArrayList", vec![str(db)]), "add", &[str as common::TyBuilder]),
        ("ArrayList<String>.add(Integer)", |db| Ty::reference(db, "java.util.ArrayList", vec![str(db)]), "add", &[integer as common::TyBuilder]),
        ("String.toString()", |db| r(db, "java.lang.String"), "toString", &[]),
        ("int.parseInt()", |db| Ty::primitive(db, PrimitiveType::Int), "parseInt", &[]),
    ]),
}

snapshot! {
    method_resolution_source,
    check_source_methods(
        &[(
            "/src/com/example/Util.java",
            r#"package com.example;
class Util {
    static String pick(Object o) { return null; }
    static String pick(String s) { return null; }
    static int box(Integer i) { return 0; }
    static int box(int i) { return 0; }
    static int add(int a, int b) { return 0; }
    static int add(int... xs) { return 0; }
    static String spread(String... xs) { return null; }
    static <T> T identity(T value) { return value; }
    static String miss(int... xs) { return null; }
}
"#,
        )],
        &[
            ("Util.pick(String)", |db| r(db, "com.example.Util"), "pick", &[str as common::TyBuilder]),
            ("Util.pick(Integer)", |db| r(db, "com.example.Util"), "pick", &[integer as common::TyBuilder]),
            ("Util.pick(Object)", |db| r(db, "com.example.Util"), "pick", &[object as common::TyBuilder]),
            ("Util.box(int)", |db| r(db, "com.example.Util"), "box", &[int as common::TyBuilder]),
            ("Util.box(Integer)", |db| r(db, "com.example.Util"), "box", &[integer as common::TyBuilder]),
            ("Util.add(1,2)", |db| r(db, "com.example.Util"), "add", &[int as common::TyBuilder, int as common::TyBuilder]),
            ("Util.add(1,2,3)", |db| r(db, "com.example.Util"), "add", &[int as common::TyBuilder, int as common::TyBuilder, int as common::TyBuilder]),
            ("Util.spread()", |db| r(db, "com.example.Util"), "spread", &[]),
            ("Util.spread(a,b)", |db| r(db, "com.example.Util"), "spread", &[str as common::TyBuilder, str as common::TyBuilder]),
            ("Util.spread(a,b,c)", |db| r(db, "com.example.Util"), "spread", &[str as common::TyBuilder, str as common::TyBuilder, str as common::TyBuilder]),
            ("Util.identity(String)", |db| r(db, "com.example.Util"), "identity", &[str as common::TyBuilder]),
            ("Util.identity(int)", |db| r(db, "com.example.Util"), "identity", &[int as common::TyBuilder]),
            ("Util.miss()", |db| r(db, "com.example.Util"), "miss", &[]),
            ("Util.nope()", |db| r(db, "com.example.Util"), "nope", &[]),
        ],
    ),
}

snapshot! {
    method_resolution_source_generic,
    check_source_methods(
        &[(
            "/src/com/example/Box.java",
            r#"package com.example;
class Box<T> {
    void put(T value) {}
    T get() { return null; }
}
class Animal {
    String sound() { return null; }
}
class Dog extends Animal {}
"#,
        )],
        &[
            ("Box<String>.put(String)", |db| Ty::reference(db, "com.example.Box", vec![str(db)]), "put", &[str as common::TyBuilder]),
            ("Box<String>.put(Object)", |db| Ty::reference(db, "com.example.Box", vec![str(db)]), "put", &[object as common::TyBuilder]),
            ("Box<String>.put(Integer)", |db| Ty::reference(db, "com.example.Box", vec![str(db)]), "put", &[integer as common::TyBuilder]),
            ("Box<String>.get()", |db| Ty::reference(db, "com.example.Box", vec![str(db)]), "get", &[]),
            ("Dog.sound()", |db| r(db, "com.example.Dog"), "sound", &[]),
            ("Dog.nope()", |db| r(db, "com.example.Dog"), "nope", &[]),
        ],
    ),
}
