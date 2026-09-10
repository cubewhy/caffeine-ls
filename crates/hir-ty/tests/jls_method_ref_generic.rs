//! JLS SE 26 scenario snapshots for *inexact method references to generic
//! methods* ([JLS §15.13.1], [§18.5.2.2]): a reference such as
//! `Optional::of` names `<T> Optional<T> of(T)`, whose type parameter is
//! only *potentially* applicable until the enclosing invocation's joint
//! inference instantiates it. When the reference is the argument of another
//! generic invocation — `opt.map(Optional::of)` over
//! `<U> Optional<U> map(Function<? super T, ? extends U>)` — the referenced
//! method's type parameter becomes a fresh inference variable of the shared
//! table, so the parameter constraint `⟨String → α⟩` and the return
//! constraint `Optional<α> <: Optional<U>` solve together from the expected
//! result type `Optional<Optional<String>>`.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: generic static factory reference as the argument of a generic
// -- invocation whose own type variable is target-driven.

snapshot!(
    generic_static_factory_ref_in_map,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Optional;

class Body {
    Optional<Optional<String>> nested() {
        Optional<String> id = Optional.of(\"x\");
        return id.map(Optional::of);
    }

    Optional<Optional<String>> direct() {
        Optional<String> id = Optional.of(\"x\");
        Optional<Optional<String>> r = id.map(Optional::of);
        return r;
    }

    static class RLoc {
        RLoc(String s) {}
    }

    Optional<RLoc> chained(Optional<String> id) {
        return id.map(Optional::of)
            .orElseGet(() -> Optional.of(\"item\"))
            .map(RLoc::new);
    }
}
",
    )])
);
// §15.13.1/§18.5.2.2: `Optional::of` is `<T> Optional<T> of(T)` — a generic
// method, so an inexact reference to it is only potentially applicable by
// arity. Its `T` becomes a fresh variable of `map`'s inference table:
// `id.map(Optional::of)` over an `Optional<String>` receiver contributes
// `⟨String → α⟩` (from the parameter) and `Optional<α> <: Optional<U>`
// (from the referenced return), so the target `Optional<Optional<String>>`
// resolves `U := Optional<String>` and `α := String`. The chained case
// resolves the receiver of `.map(RLoc::new)` — the `Optional<?>` returned
// by `orElseGet` — through the same joint table.

// -- red: the genuinely inapplicable uses still report -------------------------

snapshot!(
    generic_factory_ref_wrong_target,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Optional;
import java.util.function.Function;

class Body {
    Optional<String> wrong(Optional<String> id) {
        return id.map(Optional::of);
    }
}
",
    )])
);
// Red: `id.map(Optional::of)` over `Optional<String>` produces
// `Optional<Optional<String>>` — the enclosing method's `Optional<String>`
// return cannot accept it, so the invocation is reported against the
// expected type ([§18.5.2.4], [§14.17]).

// -- green: generic overloads are disambiguated by their fixed positions --------
// `MappedEntitySet` overloads `encode(MappedEntitySet<Z>, ClientVersion)` and
// `encode(PacketWrapper, MappedEntitySet<Z>)` — both generic, both two
// parameters. As a method reference targeting `NbtEncoder<T>` (SAM
// `NBT encode(PacketWrapper, T)`) only the second is potentially applicable:
// the first's leading `MappedEntitySet<Z>` can never accept the SAM's leading
// `PacketWrapper` under any instantiation of `Z`. The reference selection must
// reject it through the *erasure* of the type-variable-carrying parameter,
// or `set` is reported ambiguous.

snapshot!(
    generic_overload_fixed_position_disambiguation,
    check_body_types(&[(
        "/src/com/example/Codecs.java",
        "\
package com.example;

class Codecs {
    interface MappedEntity {}
    static class ItemType implements MappedEntity {}
    static class ClientVersion {}
    static class NBT {}

    static class MappedEntitySet<Z extends MappedEntity> {
        static <A extends MappedEntity> NBT encode(MappedEntitySet<A> value, ClientVersion version) {
            return null;
        }

        static <A extends MappedEntity> NBT encode(PacketWrapper writer, MappedEntitySet<A> value) {
            return null;
        }
    }

    interface NbtEncoder<T> {
        NBT encode(PacketWrapper writer, T value);
    }

    static class NBTCompound {
        <T> void set(String key, T value, NbtEncoder<T> encoder, PacketWrapper writer) {
        }
    }

    static class PacketWrapper {}

    static void store(NBTCompound nbt, MappedEntitySet<ItemType> supported) {
        nbt.set(\"supported_items\", supported, MappedEntitySet::encode, new PacketWrapper());
    }
}
",
    )])
);
// §15.13.1 inexact references: both overloads match the SAM's arity (two
// value parameters), but only `encode(PacketWrapper, MappedEntitySet<A>)` is
// congruent with `NbtEncoder<MappedEntitySet<ItemType>>` — the competing
// overload's first parameter erases to `MappedEntitySet`, which no
// `PacketWrapper` actual is assignable to. Selecting by the fixed (type-var
// independent) positions resolves the reference and keeps `set` applicable.

// -- green: unbound references with a still-variable receiver ----------------
// `toCompoundTag(Entry::getKey, Entry::getValue)` targets
// `Collector<Entry<String,?>,?,Compound>` — `T := Entry<String,?>` comes from
// the return (`Collector<T,...>` vs the target), not from the references. An
// unbound `Entry::getKey` as `Function<T,String>` with `T` still a variable
// must not constrain by its erasure (`Object → String`, which rejects); the
// target resolves `T` first and the final re-inference validates the reference
// against the concrete `Entry<String,?>` ([JLS §15.13.1], [§18.5.2.2]).
snapshot!(
    unbound_ref_var_receiver_defers_return,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Map;
import java.util.function.Function;
import java.util.stream.Collector;

class Body {
    interface Tag {
    }

    static Collector<Map.Entry<String, Tag>, ?, String> toTag() {
        return toTag(Map.Entry::getKey, Map.Entry::getValue);
    }

    static <T> Collector<T, ?, String> toTag(Function<T, String> k, Function<T, Tag> v) {
        return null;
    }
}
",
    )])
);

// -- §15.13.2/§18.5.2.2: a generic factory reference is checked instantiated --
// `MappedEntitySet::createEmpty` names `<Z extends MappedEntity>
// MappedEntitySet<Z> createEmpty()`. Compatibility with the target functional
// interface is decided with `Z` instantiated — here `Z := MappedEntity` from
// `MappedEntitySet<Z> <: MappedEntityRefSet<Z>` against the SAM return — so
// both the argument position and a direct `Supplier` target are legal.
// Comparing the declared, rigid `MappedEntitySet<Z>` leaves `Z` unsolved and
// the invariant-argument comparison fails.

snapshot!(
    generic_factory_ref_argument_position,
    check_body_types(&[(
        "/src/com/example/Factory.java",
        "\
package com.example;

import java.util.Optional;
import java.util.function.Supplier;

interface MappedEntity {}

interface MappedEntityRefSet<T extends MappedEntity> {}

class MappedEntitySet<T extends MappedEntity> implements MappedEntityRefSet<T> {
    public static <Z extends MappedEntity> MappedEntitySet<Z> createEmpty() {
        return null;
    }
}

class Factory {
    static <T> T orElseGet(Optional<T> o, Supplier<? extends T> s) {
        return null;
    }

    static MappedEntityRefSet<MappedEntity> pick(Optional<MappedEntityRefSet<MappedEntity>> o) {
        return orElseGet(o, MappedEntitySet::createEmpty);
    }
}
",
    )])
);

snapshot!(
    generic_factory_ref_supplier_target,
    check_body_types(&[(
        "/src/com/example/Factory2.java",
        "\
package com.example;

import java.util.function.Supplier;

interface MappedEntity2 {}

interface MappedEntityRefSet2<T extends MappedEntity2> {}

class MappedEntitySet2<T extends MappedEntity2> implements MappedEntityRefSet2<T> {
    public static <Z extends MappedEntity2> MappedEntitySet2<Z> createEmpty() {
        return null;
    }
}

class Factory2 {
    static Supplier<MappedEntityRefSet2<MappedEntity2>> supply() {
        return MappedEntitySet2::createEmpty;
    }
}
",
    )])
);

// -- §15.13.2/§18.5.2.2: the instantiation also reaches a receiver chain ------
// `Either::unwrap` names `<L, R> L unwrap(Either<L, R>)`; against
// `apply`'s `Function<Either<EasingType, Cubic>, U>` the reference's `L` and
// `R` come from the receiver's type argument, so `U := EasingType` and the
// enclosing `Codec<EasingType>` target is satisfied.

snapshot!(
    generic_method_ref_receiver_chain,
    check_body_types(&[(
        "/src/com/example/Easing.java",
        "\
package com.example;

import java.util.function.Function;

class Either<L, R> {
    static <L, R> Either<L, R> createLeft(L l) {
        return null;
    }

    static <L, R> Either<L, R> createRight(R r) {
        return null;
    }

    static <L, R> L unwrap(Either<L, R> e) {
        return null;
    }
}

class Codec<T> {
    <U> Codec<U> apply(Function<T, U> f, Function<U, T> g) {
        return null;
    }
}

class Cubic {}

interface EasingType {}

class Easing {
    static Codec<Either<EasingType, Cubic>> source() {
        return null;
    }

    static Codec<EasingType> make() {
        return source()
            .apply(Either::unwrap, e -> e instanceof Cubic ? Either.createRight((Cubic) e) : Either.createLeft(e));
    }
}
",
    )])
);

// -- §15.13.2/§18.5.2.2: a factory reference fixes the nested type argument --
// `readUTF("k", Function.identity())` against a `Function<String, R>` formal:
// `identity` names `<T> Function<T,T>`, so relating the reference's whole
// result to the formal gives `T = String` *and* `T = R`, fixing `R := String`
// from the argument alone. Only constraining `T <: R` left `R` free for the
// enclosing target to rebind, so both `openUrl(String)` and `openUrl(URL)`
// looked applicable and the call was reported inapplicable; javac infers
// `R := String` and selects `openUrl(String)`.

snapshot!(
    generic_factory_ref_fixes_nested_type_argument,
    check_body_types(&[(
        "/src/com/example/Open.java",
        "\
package com.example;

import java.net.URL;
import java.util.function.Function;

class ClickEvent {
    static ClickEvent openUrl(String url) {
        return null;
    }

    static ClickEvent openUrl(URL url) {
        return null;
    }
}

class Reader {
    public <R> R readUTF(String key, Function<String, R> function) {
        return null;
    }
}

class Open {
    static ClickEvent f(Reader reader, boolean modern) {
        return ClickEvent.openUrl(reader.readUTF(modern ? \"url\" : \"value\", Function.identity()));
    }
}
",
    )])
);
