//! Snapshots of throws inference — the throws clause of a method invocation
//! type ([JLS §18.5.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.3)):
//! a method type parameter that appears in a `throws` clause carries the
//! `throws` α bound ([§18.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.1.3)),
//! so resolution adapts the declaration to the call: the least upper bound of
//! lower bounds wins when the argument constrains it, and otherwise the
//! variable unifies with `RuntimeException` when every upper bound is
//! unchecked ([§18.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.4)).

#[macro_use]
mod common;

use hir_ty::Ty;

use crate::common::{TestDatabase, TyBuilder, jdk_fixture, register_source_set};

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
}

fn util(db: &TestDatabase) -> Ty {
    r(db, "com.example.Util")
}

const UTIL_SRC: &[(&str, &str)] = &[(
    "/src/com/example/Util.java",
    r#"package com.example;
import java.io.IOException;
class Util {
    static <T extends Exception> T throwChecked(boolean b, T t) throws T { return null; }
    static <T extends RuntimeException> void rethrow() throws T {}
    static void io() throws IOException {}
}
"#,
)];

fn throws_samples() -> Vec<(&'static str, TyBuilder, &'static str, Vec<TyBuilder>)> {
    vec![
        (
            "throwChecked(Exception)",
            util as TyBuilder,
            "throwChecked",
            vec![
                |db| Ty::primitive(db, syntax::stub::PrimitiveType::Boolean),
                |db| r(db, "java.lang.Exception"),
            ],
        ),
        (
            "throwChecked(IOException)",
            util as TyBuilder,
            "throwChecked",
            vec![
                |db| Ty::primitive(db, syntax::stub::PrimitiveType::Boolean),
                |db| r(db, "java.io.IOException"),
            ],
        ),
        (
            "throwChecked(RuntimeException)",
            util as TyBuilder,
            "throwChecked",
            vec![
                |db| Ty::primitive(db, syntax::stub::PrimitiveType::Boolean),
                |db| r(db, "java.lang.RuntimeException"),
            ],
        ),
        ("rethrow()", util as TyBuilder, "rethrow", vec![]),
        ("io()", util as TyBuilder, "io", vec![]),
    ]
}

fn check_throws() -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    let source_set = register_source_set(&mut db, &fixture, UTIL_SRC);
    let scope = hir::ResolutionScope::SourceSet(source_set);
    let ctx = hir_ty::InvocationContext::unconstrained();

    throws_samples()
        .iter()
        .map(|(label, build_receiver, name, arg_builders)| {
            let receiver = build_receiver(&db);
            let args: Vec<Ty> = arg_builders.iter().map(|build| build(&db)).collect();
            let arg_types: Vec<String> =
                args.iter().map(|ty| ty.display(&db).to_string()).collect();
            let picked = hir_ty::pick_method(&db, &scope, &receiver, name, &args, &ctx, None);
            let rendered = match picked {
                Some(method) => format!("{} -> {}", method.display(&db), method.ret.display(&db)),
                None => "<none>".to_owned(),
            };
            format!("{label}: {rendered} [args: {}]", arg_types.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

snapshot! {
    throws_inference,
    check_throws(),
}
