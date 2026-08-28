//! Snapshots of capture conversion
//! ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
//! `? extends T` and `?` capture to fresh upper-bounded type variables, while
//! `? super T` captures to a fresh variable with the `Object` upper bound and
//! the `T` lower bound — which rounds out method resolution over
//! lower-bounded wildcard receivers.

#[macro_use]
mod common;

use std::collections::HashMap;

use hir_ty::{BoundKind, Ty, TyKind, WildcardBound, capture_conversion};

use crate::common::{TestDatabase, TyBuilder, jdk_fixture, register_jdk};

/// The capture-variable id counter ([`crate::capture_conversion`]) is a
/// process-global static, so absolute `CAP#<n>` ids depend on test
/// scheduling. Renumber them by first-seen order for deterministic snapshots.
fn normalize_caps(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut map: HashMap<String, usize> = HashMap::new();
    let mut i = 0;
    while i < input.len() {
        let bytes = input.as_bytes();
        if input[i..].starts_with("CAP#") {
            let mut j = i + 4;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let number = &input[i + 4..j];
            let next = map.len();
            let ordinal = *map.entry(number.to_owned()).or_insert(next);
            out.push_str(&format!("CAP#{ordinal}"));
            i = j;
        } else {
            let ch = input[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
}

fn wild(db: &TestDatabase, bound: Option<WildcardBound>) -> Ty {
    Ty::wildcard(db, bound.map(Box::new))
}

/// Renders a type recursively, so the bounds of nested capture type variables
/// show their `Object` upper bound and their lower bound.
fn render(db: &TestDatabase, ty: &Ty) -> String {
    let kind = ty.kind(db);
    match kind {
        TyKind::Reference { name, args } => {
            if args.is_empty() {
                name.as_str().to_owned()
            } else {
                format!(
                    "{}<{}>",
                    name,
                    args.iter()
                        .map(|a| render(db, a))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
        TyKind::Array(inner) => format!("{}[]", render(db, inner)),
        TyKind::Wildcard(bound) => match bound {
            None => "?".to_owned(),
            Some(bound) => match bound.kind {
                BoundKind::Upper => format!("? extends {}", render(db, &bound.ty)),
                BoundKind::Lower => format!("? super {}", render(db, &bound.ty)),
            },
        },
        TyKind::TypeVar { name, lower, .. } => {
            let upper: Vec<String> = ty
                .bounds(db)
                .iter()
                .map(|bound| bound.display(db).to_string())
                .collect();
            let upper = if upper.is_empty() {
                "<none>".to_owned()
            } else {
                upper.join(", ")
            };
            match lower {
                Some(lower) => format!("{} [upper: {upper}] [lower: {}]", name, render(db, lower)),
                None => format!("{} [upper: {upper}]", name),
            }
        }
        _ => ty.display(db).to_string(),
    }
}

fn check_capture(samples: &[(&str, TyBuilder)]) -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let scope = hir::ResolutionScope::JdkBuiltins;

    samples
        .iter()
        .map(|(label, build)| {
            let source = build(&db);
            let captured = capture_conversion(&db, &scope, source);
            format!(
                "{label}: {} => {}",
                source.display(&db),
                render(&db, &captured)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn list_of(db: &TestDatabase, arg: Ty) -> Ty {
    Ty::reference(db, "java.util.List", vec![arg])
}

fn capture_samples() -> Vec<(&'static str, TyBuilder)> {
    vec![
        ("unbounded", |db| -> Ty {
            let arg = wild(db, None);
            list_of(db, arg)
        }),
        ("upper", |db| -> Ty {
            let arg = wild(
                db,
                Some(WildcardBound {
                    kind: BoundKind::Upper,
                    ty: r(db, "java.lang.Number"),
                }),
            );
            list_of(db, arg)
        }),
        ("lower", |db| -> Ty {
            let arg = wild(
                db,
                Some(WildcardBound {
                    kind: BoundKind::Lower,
                    ty: r(db, "java.lang.Integer"),
                }),
            );
            list_of(db, arg)
        }),
    ]
}

snapshot! {
    capture_kinds,
    normalize_caps(&check_capture(&capture_samples())),
}

// Method resolution over a `List<? super Integer>` receiver: the captured
// `add(CAP)` accepts an `Integer` (via the capture variable's lower bound,
// §5.1.10) but not a `String` or a `Number`.
fn check_capture_method() -> String {
    let fixture = jdk_fixture();
    let mut db = TestDatabase::new();
    register_jdk(&mut db, &fixture);
    let scope = hir::ResolutionScope::JdkBuiltins;
    let receiver = list_of(
        &db,
        wild(
            &db,
            Some(WildcardBound {
                kind: BoundKind::Lower,
                ty: r(&db, "java.lang.Integer"),
            }),
        ),
    );
    let ctx = hir_ty::InvocationContext::external(&scope);

    let call = |arg_name: &str| {
        let arg = r(&db, arg_name);
        let picked = hir_ty::pick_method(
            &db,
            &scope,
            &receiver,
            "add",
            &[hir_ty::PolyArg::Concrete(arg)],
            &ctx,
            None,
        );
        match picked {
            Some(method) => {
                let params: Vec<String> = method.params.iter().map(|ty| render(&db, ty)).collect();
                format!("{} add({})", method.owner, params.join(", "))
            }
            None => "<none>".to_owned(),
        }
    };

    format!(
        "receiver: java.util.List<? super java.lang.Integer>\n\
         add(java.lang.Integer) -> {}\n\
         add(java.lang.Number) -> {}\n\
         add(java.lang.String) -> {}",
        call("java.lang.Integer"),
        call("java.lang.Number"),
        call("java.lang.String"),
    )
}

snapshot! {
    capture_method_resolution,
    normalize_caps(&check_capture_method()),
}
