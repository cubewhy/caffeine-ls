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
    ClassSpec, DeprecationSpec, TestDatabase, TyBuilder, check_body_types, check_source_methods_ctx,
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
            deprecation: DeprecationSpec::NONE,
            field_deprecations: &[],
            method_deprecations: &[],
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

// -- §18.2.2/[§18.2.3]: the reduction must not over-accept a wildcard array --
// A `Class<? extends T[]>` argument against a `Class<T[]>` formal is *not*
// convertible: the capture `CAP <: T[]` cannot become the invariant `T[]`
// required by `Class<T[]>`, so no instantiation of the formal's `T` exists.
// javac: `Class<CAP#1> cannot be converted to Class<T#1[]>`.

snapshot!(
    wildcard_array_token_not_assignable_to_invariant_formal,
    check_body_types(&[(
        "/src/com/example/QNeg.java",
        "\
package com.example;

class QNeg {
    static <T> T[] makePlain(Class<T[]> cls) {
        return null;
    }

    static <T> T[] mismatch(Class<? extends T[]> cls) {
        return makePlain(cls);
    }
}
",
    )])
);

// -- §18.3.1: a nested call's variable chained through equalities ------------
// `read`'s `R` is constrained only by an *equality chain* built from the nested
// `self()` call: `⟨?T = Boolean⟩` and `⟨?T = ?R⟩` imply `⟨Boolean = ?R⟩`. The
// enclosing `ofNullable` adds `⟨?R <: ?T_optional⟩` from its own formal, and
// the receiver chain then requires `Boolean`. Resolving `?T_optional` before
// flattening the chain leaves its bound with an inference variable, so the
// estimate pass resolves it to `Object` and the `boolean` return fails; javac
// accepts the call.

snapshot!(
    nested_call_result_solved_from_chained_equalities,
    check_body_types(&[(
        "/src/com/example/Chain.java",
        "\
package com.example;

import java.util.Optional;
import java.util.function.Function;

class Chain {
    static <T> Function<T, T> self() {
        return null;
    }

    static <R> R read(String key, Function<Boolean, R> f) {
        return null;
    }

    boolean flag() {
        return Optional.ofNullable(read(\"x\", self())).orElse(false);
    }
}
",
    )])
);

// -- §4.5.1/§18.2.2: an unbounded wildcard argument contains a variable ------
// `collectingAndThen(toCollection(ArrayList::new), …)` relates the nested
// call's `Collector<T, ?, C>` to the enclosing `<T2, A2, R2>` formals. The
// middle argument `?` meets a target argument that is itself a type variable
// (the enclosing target's own `?`, standing as its capture); §4.5.1 makes
// `? <= T` hold for every `T`, so the pair constrains nothing. Capturing the
// source first compared two distinct capture variables for equality and
// rejected the nested invocation. javac accepts the declaration.

snapshot!(
    unbounded_wildcard_argument_against_enclosing_variable,
    check_body_types(&[(
        "/src/com/example/Audiences.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.stream.Collector;
import java.util.stream.Collectors;

interface Audience {}

class ForwardingAudience implements Audience {}

class Audiences {
    static final Collector<? super Audience, ?, ForwardingAudience> COLLECTOR =
        Collectors.collectingAndThen(
            Collectors.toCollection(ArrayList::new),
            var0 -> new ForwardingAudience());
}
",
    )])
);

// -- §18.2.1: `⟨T = T⟩` is a tautology however the handles intern -------------
// A class type parameter reached through its own declared bound resolves at a
// different depth in a nested invocation's own bound than in the enclosing
// constructor's formals (§4.4's recursion guard truncates a self-referential
// bound at different points per resolver context), so the two handles name the
// same `T` without being the same interned value. Equality of the two must hold
// — it is the same declared variable on both sides — or the enclosing
// constructor is rejected. javac accepts the declaration.

snapshot!(
    same_type_variable_across_resolver_depths_is_equal,
    check_body_types(&[(
        "/src/com/example/Entry.java",
        "\
package com.example;

interface MappedEntity {}

interface CopyableEntity<T> {
    T copy(Object data);
}

interface DeepComparableEntity {}

interface IRegistry<T> {}

interface NbtDecoder<T> {
    T decode(Object tag, Object wrapper);
}

interface NbtEntryDecoder<T> extends NbtDecoder<T> {
    static <U extends MappedEntity & CopyableEntity<U>> NbtEntryDecoder<U> fromDecoder(
            NbtDecoder<U> decoder) {
        return null;
    }
}

class Entry<T extends MappedEntity & CopyableEntity<T> & DeepComparableEntity> {
    Entry(IRegistry<T> baseRegistry, NbtDecoder<T> decoder) {
        this(baseRegistry, NbtEntryDecoder.fromDecoder(decoder));
    }

    Entry(IRegistry<T> baseRegistry, NbtEntryDecoder<T> decoder) {}
}
",
    )])
);
