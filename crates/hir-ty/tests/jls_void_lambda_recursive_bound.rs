//! JLS SE 26 scenario snapshots for two inference gaps on registry-style
//! generic code:
//!
//! * a *void-compatible* lambda expression whose body is a value-returning
//!   generic invocation ([JLS §15.27.3]): the body is a statement expression
//!   whose value is discarded, so it must infer standalone ([§15.2]) instead
//!   of against the `void` SAM return — `Consumer<State>` with
//!   `s -> s.value(B(), true)` where `value<V>(Opt<V>, V)` returns `State`.
//!   Constraining the body against `void` would reject every applicable
//!   candidate.
//!
//! * a *recursive declared bound* of a type parameter being eliminated by an
//!   equality during invocation inference ([JLS §18.3.1]): `<E extends
//!   Enum<E>>` lowers to the dependency `α <: Enum<α>`, and when `α` is
//!   instantiated from an argument (`Class<K>`/`K` with the matching recursive
//!   `K extends Enum<K>`) the queued dependency must substitute `α` away
//!   before reduction, or the stale self-reference rejects the invocation.

#[macro_use]
mod common;

use crate::common::check_body_types;

snapshot!(
    void_lambda_expression_body_value_invocation,
    check_body_types(&[(
        "/src/com/example/Consume.java",
        "\
package com.example;

import java.util.function.Consumer;

class Consume {
    interface Opt<V> {}
    interface State {
        <V> State value(Opt<V> o, V v);
    }

    static Opt<Boolean> B() {
        return null;
    }

    static Consumer<State> use() {
        return s -> s.value(B(), true);
    }
}
",
    )])
);
// §15.27.3: the expression lambda targets the void SAM `accept(State)`. Its
// body `s.value(B(), true)` is a statement expression — the `State` result is
// discarded — so it infers standalone and `value<V>(Opt<V>, V)` unifies
// `V := Boolean` from `Opt<Boolean>`/`true`. No diagnostic.

snapshot!(
    void_lambda_chained_value_invocation,
    check_body_types(&[(
        "/src/com/example/Consume2.java",
        "\
package com.example;

import java.util.function.Consumer;

class Consume2 {
    interface Opt<V> {}
    interface State {
        <V> State value(Opt<V> o, V v);
        State build();
    }

    interface Ver {
        Ver version(int i, Consumer<State> c);
        State build();
    }

    static Opt<Boolean> B() {
        return null;
    }

    static Opt<Integer> I() {
        return null;
    }

    static State m(Ver v) {
        return v.version(0, s -> s.value(B(), true).value(I(), 42))
            .version(1, s -> s.value(B(), false))
            .build();
    }
}
",
    )])
);
// The chained `s.value(B(), true).value(I(), 42)` inside a void-compatible
// lambda body: each generic link infers standalone and resolves.

snapshot!(
    recursive_enum_bound_method_type_param,
    check_body_types(&[(
        "/src/com/example/Pick.java",
        "\
package com.example;

class Pick {
    interface Svc {
        <E extends Enum<E>> E pick(Class<E> c, E def);
    }

    <K extends Enum<K>> K m(Svc svc, Class<K> c, K def) {
        return svc.pick(c, def);
    }
}
",
    )])
);
// §18.3.1: `pick`'s `E` is instantiated to `K` from the `Class<K>`/`K`
// arguments. Eliminating `α` (whose declared bound is the self-referential
// `E extends Enum<E>`, lowered to `α <: Enum<α>`) must substitute the bound
// before pushing the dependency, or the stale `Enum<α>` rejects the call.

snapshot!(
    recursive_enum_bound_class_type_param,
    check_body_types(&[(
        "/src/com/example/Box.java",
        "\
package com.example;

class Box {
    interface Box2<E extends Enum<E>> {}

    interface Svc {
        <E extends Enum<E>> Box2<E> enumBox(String n, Class<E> c, E def);
    }

    <K extends Enum<K>> Box2<K> m(Svc svc, String n, Class<K> c, K def) {
        return svc.enumBox(n, c, def);
    }
}
",
    )])
);
// The same recursive-bound elimination with a parameterized return type
// (`Box2<E>`, not a bare `E`).
