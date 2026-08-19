//! Snapshots of method invocation type inference
//! ([JLS §18.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2))
//! in [`hir_ty::pick_method`]: generic methods resolve to their inferred
//! invocation type, with boxing in the loose phase, wildcard type argument
//! inference ([§18.5.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.2))
//! and variable arity ([§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4)).

#[macro_use]
mod common;

use hir_ty::Ty;
use syntax::stub::PrimitiveType;

use crate::common::{TestDatabase, TyBuilder, check_source_methods_ctx};

type Sample = (&'static str, TyBuilder, &'static str, &'static [TyBuilder]);

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
}

fn list_of(db: &TestDatabase, arg: Ty) -> Ty {
    Ty::reference(db, "java.util.List", vec![arg])
}

const GENERIC_SRC: &[(&str, &str)] = &[(
    "/src/com/example/Util.java",
    r#"package com.example;
class Util {
    static <T> T identity(T t) { return t; }
    static <T> T[] makeArray(T t) { return null; }
    static <A, B> Pair<A, B> pair(A a, B b) { return null; }
    static <T> T max(T a, T b) { return null; }
    static <T> T first(java.util.List<? extends T> l) { return null; }
    static <T> T pick(java.util.List<?> l) { return null; }
    static <T> void put(java.util.List<? super T> l, T t) {}
    static <T> T[] varargs(T... ts) { return null; }
    static <T> T take(T t) { return null; }
    static <T extends java.lang.String> T take(T t) { return null; }
    static <T extends Named> T asNamed(T t) { return null; }
}
class Pair<L, R> {
    L left;
    R right;
    Pair(L left, R right) { this.left = left; this.right = right; }
    static <L, R> Pair<L, R> of(L left, R right) { return null; }
}
interface Named {}
class Person implements Named {}
"#,
)];

const UTIL: &str = "com.example.Util";

fn inference_samples() -> &'static [Sample] {
    &[
        (
            "identity(String)",
            |db| r(db, UTIL),
            "identity",
            &[|db| r(db, "java.lang.String")],
        ),
        (
            "identity(int)",
            |db| r(db, UTIL),
            "identity",
            &[|db| Ty::primitive(db, PrimitiveType::Int)],
        ),
        (
            "makeArray(String)",
            |db| r(db, UTIL),
            "makeArray",
            &[|db| r(db, "java.lang.String")],
        ),
        (
            "pair(String, Integer)",
            |db| r(db, UTIL),
            "pair",
            &[
                |db| r(db, "java.lang.String"),
                |db| r(db, "java.lang.Integer"),
            ],
        ),
        (
            "max(String, String)",
            |db| r(db, UTIL),
            "max",
            &[
                |db| r(db, "java.lang.String"),
                |db| r(db, "java.lang.String"),
            ],
        ),
        (
            "max(String, Integer)",
            |db| r(db, UTIL),
            "max",
            &[
                |db| r(db, "java.lang.String"),
                |db| r(db, "java.lang.Integer"),
            ],
        ),
        (
            "first(List<String>)",
            |db| r(db, UTIL),
            "first",
            &[|db| list_of(db, r(db, "java.lang.String"))],
        ),
        (
            "pick(List<String>)",
            |db| r(db, UTIL),
            "pick",
            &[|db| list_of(db, r(db, "java.lang.String"))],
        ),
        (
            "put(List<String>, String)",
            |db| r(db, UTIL),
            "put",
            &[
                |db| list_of(db, r(db, "java.lang.String")),
                |db| r(db, "java.lang.String"),
            ],
        ),
        (
            "varargs(String)",
            |db| r(db, UTIL),
            "varargs",
            &[|db| r(db, "java.lang.String")],
        ),
        (
            "varargs(String, String)",
            |db| r(db, UTIL),
            "varargs",
            &[
                |db| r(db, "java.lang.String"),
                |db| r(db, "java.lang.String"),
            ],
        ),
        ("varargs()", |db| r(db, UTIL), "varargs", &[]),
        (
            "take(String)",
            |db| r(db, UTIL),
            "take",
            &[|db| r(db, "java.lang.String")],
        ),
        (
            "asNamed(Person)",
            |db| r(db, UTIL),
            "asNamed",
            &[|db| r(db, "com.example.Person")],
        ),
        (
            "asNamed(String)",
            |db| r(db, UTIL),
            "asNamed",
            &[|db| r(db, "java.lang.String")],
        ),
        (
            "Pair.of(String, Integer)",
            |db| r(db, "com.example.Pair"),
            "of",
            &[
                |db| r(db, "java.lang.String"),
                |db| r(db, "java.lang.Integer"),
            ],
        ),
    ]
}

snapshot! {
    generic_invocation,
    check_source_methods_ctx(GENERIC_SRC, inference_samples(), None),
}
