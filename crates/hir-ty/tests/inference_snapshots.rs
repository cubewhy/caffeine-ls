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

use crate::common::{
    ClassSpec, TestDatabase, TyBuilder, check_body_types, check_source_methods_ctx,
};

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

// -- generic static factory overloads from a third-party jar ---------------------
// A guava-shaped `ImmutableMap` (generic class, static generic `of`
// overloads of arities 0..4 plus a varargs form): the 4-argument call must
// instantiate `K`/`V` from the arguments ([JLS §18.5.2] applicability
// inference), not reject against the declared type variables.

snapshot!(
    library_generic_static_overloads,
    crate::common::check_body_types_with_libs(
        &[ClassSpec {
            fqn: "com/google/common/collect/ImmutableMap",
            super_class: Some("java/lang/Object"),
            interfaces: &[],
            access: 0x0021,
            methods: &[
                (
                    "of",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Lcom/google/common/collect/ImmutableMap;"
                ),
                (
                    "of",
                    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Lcom/google/common/collect/ImmutableMap;"
                ),
                (
                    "of",
                    "([Ljava/util/Map$Entry;)Lcom/google/common/collect/ImmutableMap;"
                ),
            ],
            method_sigs: &[
                "<K:Ljava/lang/Object;V:Ljava/lang/Object;>(TK;TV;)Lcom/google/common/collect/ImmutableMap<TK;TV;>;",
                "<K:Ljava/lang/Object;V:Ljava/lang/Object;>(TK;TV;TK;TV;)Lcom/google/common/collect/ImmutableMap<TK;TV;>;",
                "<K:Ljava/lang/Object;V:Ljava/lang/Object;>([Lcom/google/common/collect/ImmutableMap$Entry<TK;TV;>;)Lcom/google/common/collect/ImmutableMap<TK;TV;>;",
            ],
            method_access: &[0x0009, 0x0009, 0x0009],
            sig: Some("<K:Ljava/lang/Object;V:Ljava/lang/Object;>java/lang/Object;"),
            fields: &[],
        }],
        &[(
            "/src/com/example/App.java",
            "\
import com.google.common.collect.ImmutableMap;

class App {
    void m(java.util.List<String> l) {
        ImmutableMap<String, java.util.List<String>> m =
            ImmutableMap.of(\"a\", l, \"b\", l);
    }
}
",
        )],
    ),
);

// -- java.util.Arrays overloads with nested invocation arguments -----------------
// Both arguments are poly invocations resolved jointly against each candidate:
// only `equals(int[], int[])` applies, so the nested `copyOf(int[], int)`
// invocations must constrain ⟨int[] → formal⟩ per candidate — and the generic
// `copyOf(T[], int)` must die when its resolved return cannot satisfy the
// target ([JLS §15.12.2.5] joint resolution through §18.5.4).

snapshot!(
    arrays_equals_nested_invocation_args,
    check_body_types(&[(
        "/src/com/example/Repro.java",
        "\
import java.util.Arrays;

class Repro {
    boolean m(int[] a, int[] b) {
        return Arrays.equals(Arrays.copyOf(a, 1), Arrays.copyOf(b, 1));
    }
}
",
    )])
);
