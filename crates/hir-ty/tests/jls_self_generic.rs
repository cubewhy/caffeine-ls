//! JLS SE 26 scenario snapshots for the self-referential generic class
//! ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4),
//! [§4.10.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.2)):
//! the classic `abstract class Value<K, T extends Value<K, T>>` shape, where
//! every member is re-pointed at the receiver's own type argument.
//!
//! The generic base exposes a `final` field of the *same* declared type as
//! the parameterized self, so `this.field = this.selfTyped` must pass the
//! §5.2 assignment — and `K` must carry `Object` methods (`equals`,
//! `toString`) through its bound. When the subtype's own type argument is a
//! wildcard, `this.accessor` reads must resolve to the identical parameterized
//! field type on the receiver and on the wildcard-bound member, so the
//! assignment and the `new ArrayList<...>`-to-`List<...>` field initializer
//! both stay silent.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: self-referential type argument by simple name ---------------------

snapshot!(
    self_referential_bound_field_assign,
    check_body_types(&[(
        "/src/com/example/Store.java",
        "\
package com.example;

abstract class Value<K, T extends Value<K, T>> {
    private ValueAccessor<K, T> accessor;
    protected ValueAccessor<K, T> selfTyped;

    abstract class ValueAccessor<K, T> {}

    class DirectValueAccessor<K, T> extends ValueAccessor<K, T> {
        DirectValueAccessor(Value<K, T> value) {
            super();
        }
    }

    public void useDirectAccessor() {
        this.accessor = this.selfTyped;
    }

    public boolean isDefault() {
        K k = getValue();
        K k2 = getDefaultValue();
        return k2.equals(k);
    }

    abstract K getValue();
    abstract K getDefaultValue();

    public String getDisplayValue() {
        K k = getValue();
        return k.toString();
    }
}
",
    )])
);

// -- green: wildcard receiver field vs parameterized field initializer --------
// `accessor` is declared `ValueAccessor<K, T>`, `directAccessor`
// `DirectValueAccessor<K, T>`; the `Value` ctor assigns the latter to the
// former and initializes a `List<ValueChangeValidator<T, K>>` from a fresh
// `ArrayList`. Both assignments are §5.2 same-type writes that previously
// degraded to `prob.found.req`.

snapshot!(
    self_referential_ctor_field_init,
    check_body_types(&[(
        "/src/com/example/Store.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.List;

abstract class Value<K, T extends Value<K, T>> {
    private ValueAccessor<K, T> accessor;
    private final DirectValueAccessor<K, T> directAccessor;
    private final List<ValueChangeValidator<T, K>> validators;

    abstract class ValueAccessor<K, T> {}

    class DirectValueAccessor<K, T> extends ValueAccessor<K, T> {
        DirectValueAccessor(Value<K, T> value) {
            super();
        }
    }

    static class ValueChangeValidator<T, K> {}

    public Value(K defaultValue) {
        this.validators = new ArrayList<ValueChangeValidator<T, K>>();
        this.directAccessor = new DirectValueAccessor<K, T>(this);
        this.accessor = this.directAccessor;
    }

    public void useDirectAccessor() {
        this.accessor = this.directAccessor;
    }
}
",
    )])
);

// -- green: chaining on a self-returning type-variable receiver ----------------
// `interface Box<T extends Box<T>> { T self(); }` with a receiver `input: T`
// (`T extends Box<T>`, a method type parameter): `input.self()` returns the
// receiver's own type variable, so each further `.self()` in the same
// expression resolves against `T`'s bound `Box<T>`. The chained form used to
// degrade the intermediate result to a recursion-guarded bound-less `T` whose
// member set is empty, reporting `no-such-method` on the second call.

snapshot!(
    self_returning_type_var_chain,
    check_body_types(&[(
        "/src/com/example/G.java",
        "\
package com.example;

class G {
    interface Box<T extends Box<T>> {
        T self();

        T reset();

        int size();
    }

    static <T extends Box<T>> T rebuild(T input) {
        return input.self().reset().self();
    }

    static <T extends Box<T>> int depth(T input) {
        return input.self().self().size();
    }
}
",
    )])
);
