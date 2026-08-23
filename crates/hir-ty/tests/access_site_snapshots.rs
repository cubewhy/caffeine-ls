//! Snapshots of access control ([JLS §6.6]) derived from the source call
//! site itself ([`hir_ty::access_context`]): the enclosing class and package
//! come from the file, so `private`, `protected`, package and `public`
//! members are filtered exactly as at their real invocation positions.

#[macro_use]
mod common;

use hir_ty::Ty;

use crate::common::{TestDatabase, TyBuilder, check_source_methods_site};

fn r(db: &TestDatabase, name: &str) -> Ty {
    Ty::reference(db, name, Vec::new())
}

const A: &str = "com.example.A";
const B: &str = "com.example.B";
const C: &str = "org.other.C";

const SITE_SRC: &[(&str, &str)] = &[
    (
        "/src/com/example/A.java",
        r#"package com.example;
class A {
    public void pub() {}
    protected void pro() {}
    void pkg() {}
    private void priv() {}
    public static void stat() {}
    void callPub() { pub(); }
    void callPriv() { priv(); }
    void callPkg() { pkg(); }
}
class B extends A {
    void callPro() { pro(); }
}
"#,
    ),
    (
        "/src/org/other/C.java",
        r#"package org.other;
class C extends com.example.A {
    void callPro() { pro(); }
}
class Other {
    void callPub(A a) { a.pub(); }
    void callPriv(A a) { a.priv(); }
    void callPkg(A a) { a.pkg(); }
}
"#,
    ),
];

type SiteSample = (
    &'static str,
    usize,
    &'static str,
    TyBuilder,
    &'static str,
    &'static [TyBuilder],
);

fn site_sample(
    label: &'static str,
    file: usize,
    method: &'static str,
    receiver: TyBuilder,
    name: &'static str,
) -> SiteSample {
    (label, file, method, receiver, name, &[])
}

fn site_samples() -> Vec<SiteSample> {
    vec![
        site_sample("A.callPub", 0, "callPub", |db| r(db, A), "pub"),
        site_sample("A.callPriv", 0, "callPriv", |db| r(db, A), "priv"),
        site_sample("A.callPkg", 0, "callPkg", |db| r(db, A), "pkg"),
        // B extends A in the same package: `pro` is protected and reachable
        // from the same package (§6.6.2).
        site_sample("B.callPro", 0, "callPro", |db| r(db, B), "pro"),
        // C extends A in another package: `pro` is protected and reachable
        // from a subclass (§6.6.2).
        site_sample("C.callPro", 1, "callPro", |db| r(db, C), "pro"),
        site_sample("Other.callPub", 1, "callPub", |db| r(db, A), "pub"),
        // Other does not extend A and is outside the package: private and
        // package members of A are inaccessible (§6.6.1).
        site_sample("Other.callPriv", 1, "callPriv", |db| r(db, A), "priv"),
        site_sample("Other.callPkg", 1, "callPkg", |db| r(db, A), "pkg"),
    ]
}

snapshot! {
    access_site_same_package,
    check_source_methods_site(SITE_SRC, &site_samples())
}

// -- `$` identifiers and same-top-level siblings ([§3.8], [§6.6.1]) --------------
// `A` and `A$B` are two distinct top-level classes: a private member of one is
// invisible in the other. Two classes nested in the *same* top-level class do
// share private members ([§6.6.1]), so `Sibling` reads `Nested.secret`.

const DOLLAR_SRC: &[(&str, &str)] = &[
    (
        "/src/com/example/Holder.java",
        r#"package com.example;
class Holder {
    static class Nested {
        private void secret() {}
        void selfCall() { secret(); }
    }
    static class Sibling {
        void call(Nested n) { n.secret(); }
    }
}
"#,
    ),
    (
        "/src/com/example/Dollar.java",
        r#"package com.example;
class A$B {
    private void priv() {}
    void callSelf() { priv(); }
}
class A {
    void callDollar(A$B x) { x.priv(); }
}
"#,
    ),
];

fn dollar_samples() -> Vec<SiteSample> {
    vec![
        site_sample(
            "Nested.self",
            0,
            "selfCall",
            |db| r(db, "com.example.Holder.Nested"),
            "secret",
        ),
        site_sample(
            "Sibling.call",
            0,
            "call",
            |db| r(db, "com.example.Holder.Nested"),
            "secret",
        ),
        site_sample(
            "Dollar.self",
            1,
            "callSelf",
            |db| r(db, "com.example.A$B"),
            "priv",
        ),
        site_sample(
            "A.callDollar",
            1,
            "callDollar",
            |db| r(db, "com.example.A$B"),
            "priv",
        ),
    ]
}

snapshot! {
    access_site_dollar_and_siblings,
    check_source_methods_site(DOLLAR_SRC, &dollar_samples())
}
