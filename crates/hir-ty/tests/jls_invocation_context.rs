//! Regression snapshots for invocation-context fixes:
//!
//! - a cast of a non-poly operand is a *standalone* expression ([JLS §15.16]),
//!   so the enclosing target must not reach a generic invocation inside the
//!   cast — `(R) Arrays.copyOf(...)` in `R x = ...` infers `copyOf` as
//!   `Object[]` and casts unchecked instead of constraining `copyOf`'s `T[]`
//!   against the type variable `R`;
//! - a captured `? super T` parameter is a subtype of its own *lower* bound's
//!   subtypes ([§4.10.2] with [§5.1.10]), so `consumer.accept(value)` with
//!   `consumer: Consumer<? super T>` and `value: T` resolves;
//! - a diamond `new Foo<>(...)` whose target fixes no type argument
//!   ([§15.9.2.2]) — including an all-wildcard target — infers the type
//!   arguments from the constructor arguments;
//! - a method reference's *parameters* constrain the target functional
//!   interface's type variables ([JLS §15.13.3], [§18.5.2.2]), so
//!   `Comparator.comparingInt(Friend::order)` infers `Comparator<Friend>`
//!   instead of `Comparator<Object>` and a chained `thenComparing(...)` stays
//!   applicable.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: a cast of a generic invocation is standalone (§15.16) -------------
// `(R) Arrays.copyOf(...)` with `R x = ...` must not let the `R` target
// reach `copyOf` — `T[]` can never convert to a type variable `R`, which
// would reject the applicable `copyOf(Object[], int)`.

snapshot!(
    cast_operand_is_standalone,
    check_body_types(&[(
        "/src/gg/vape/C.java",
        "\
package gg.vape;

import java.util.Arrays;

class C<T extends Value<R, ?>, R> {
    void m(Object object) {
        R x = (R) Arrays.copyOf((Object[]) object, ((Object[]) object).length);
    }
}

class Value<R, V> {
}
",
    )])
);

// -- green: a captured `? super T` admits the type variable itself ------------
// `consumer.accept(value)` with `consumer: Consumer<? super T>` and
// `value: T`: the wildcard capture `CAP` has the lower bound `T`
// ([§5.1.10]), so `T <: CAP` holds and the invocation resolves.

snapshot!(
    captured_super_admits_lower_bound,
    check_body_types(&[(
        "/src/gg/vape/N.java",
        "\
package gg.vape;

import java.util.function.Consumer;

class N {
    static <T> void apply(T value, Consumer<? super T> consumer) {
        consumer.accept(value);
    }
}
",
    )])
);

// -- green: an all-wildcard diamond target still infers from the arguments ----
// `ValueSnapshot<?, ?> vs = new ValueSnapshot<>(value)` fixes nothing with
// wildcards; `T := Value<?,?>` is inferred from the `Value<?,?>` argument
// ([JLS §15.9.2.2]).

snapshot!(
    diamond_all_wildcard_target,
    check_body_types(&[(
        "/src/gg/vape/ValueSnapshot.java",
        "\
package gg.vape;

class Value<R, V> {
}

class ValueSnapshot<T extends Value<R, ?>, R> {
    ValueSnapshot(T sourceValue) {
    }
}

class C {
    void m(Value<?, ?> value) {
        ValueSnapshot<?, ?> vs = new ValueSnapshot<>(value);
    }
}
",
    )])
);

// -- green: a method reference's parameters drive the comparator's type -------
// `Comparator.comparingInt(Friend::order)` infers `Comparator<Friend>` from
// the referenced method's `Friend` parameter ([JLS §15.13.3]), so the
// chained `thenComparing(...)` and the `List.sort(...)` invocation resolve.

snapshot!(
    method_ref_params_constrain_target,
    check_body_types(&[(
        "/src/gg/vape/C.java",
        "\
package gg.vape;

import java.util.ArrayList;
import java.util.Comparator;

class OnlineFriend {
    OnlineStatus getStatus() {
        return null;
    }

    String getDisplayName() {
        return null;
    }
}

enum OnlineStatus {
    OFFLINE,
    ONLINE
}

class C {
    private static int getOnlineStatusOrder(OnlineFriend f) {
        return 0;
    }

    private static String getOnlineFriendName(OnlineFriend f) {
        return \"\";
    }

    void m(ArrayList<OnlineFriend> list) {
        list.sort(Comparator.comparingInt(C::getOnlineStatusOrder).thenComparing(C::getOnlineFriendName, String.CASE_INSENSITIVE_ORDER));
    }
}
",
    )])
);

// -- green: an unbound method reference's receiver constrains the target ------
// `Comparator.comparing(Friend::getDisplayName)` — an *unbound* instance
// reference — takes the SAM's first parameter as the receiver
// ([§15.13.3]), so `Friend` constrains `T := Friend` and the comparator
// stays parameterized.

snapshot!(
    unbound_ref_receiver_constrains_target,
    check_body_types(&[(
        "/src/gg/vape/C.java",
        "\
package gg.vape;

import java.util.Comparator;

class OnlineFriend {
    String getDisplayName() {
        return null;
    }
}

class C {
    private static String getOnlineFriendName(OnlineFriend f) {
        return \"\";
    }

    Comparator<OnlineFriend> m() {
        return (Comparator<OnlineFriend>) Comparator.comparing(OnlineFriend::getDisplayName)
            .thenComparing(C::getOnlineFriendName, String.CASE_INSENSITIVE_ORDER);
    }
}
",
    )])
);
