//! Invocation-type inference from *implicitly* typed lambda bodies
//! ([JLS §15.27.3], [§18.2.5], [§18.5.2.2]): an implicitly typed lambda is
//! not pertinent to applicability ([§15.12.2.2]) — its body never steers
//! overload resolution — but the *chosen* method's invocation-type pass
//! searches every poly argument's body ([§18.5.2.2]) and constrains the SAM
//! return type by the body result ([§18.2.5]). A lambda parameter typed by an
//! unresolved inference variable (`Encoder<T>.encode(T, int)` with `T := α`
//! under `define`) may leave the body result itself carrying α, which the
//! applicability probe must not mistake for a compatibility failure.
//! Every scenario is verified against `javac` before the snapshot is accepted.

#[macro_use]
mod common;

use crate::common::check_body_types;

// JLS §15.27.3/§18.2.5/§18.5.2.2: both implicitly typed lambdas unify
// `define`'s `<T>` — the `Decoder<T>` body (`var0 : String`) gives
// `⟨String → α⟩` and the `Encoder<T>` body (`var0 : α` — the SAM's first
// parameter is `T`) gives `⟨α → String⟩` — so the field target
// `T1<String>` closes `α := String`. During applicability the
// `Encoder<T>` lambda's body type is α itself: an inference-variable-typed
// body cannot be checked for compatibility against the `String` SAM return,
// and the candidate must stay applicable ([§15.12.2.2] — only arity).
snapshot!(
    define_implicit_lambdas_unify,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    interface T1<T> {
        interface Decoder<T> { T decode(String a, int b); }
        interface Encoder<T> { String encode(T a, int b); }
    }

    static <T> T1<T> define(String name, T1.Decoder<T> d, T1.Encoder<T> e) {
        return null;
    }

    static final T1<String> A = define(\"a\", (var0, var1) -> var0, (var0, var1) -> var0);
}
",
    )])
);

// JLS §15.27.3/[§18.5.2.2]: a *default* method whose body is an implicitly
// typed lambda — `upgrade()` returning `(nbt, wrapper, data) -> this.decode(
// nbt, wrapper.getServerVersion().toClientVersion(), data)` — contributes the
// lambda's body constraint to the invocation type of the delegation
// `this(baseRegistry, decoder.upgrade())`: the constructor's second
// overload pair picks the `NbtEntryDecoder<T>` candidate and `T` flows
// through the lambda that the default method's own body searches.
snapshot!(
    default_method_implicit_lambda_return,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    interface MappedEntity {}
    interface CopyableEntity<T extends MappedEntity> {}
    interface DeepComparableEntity {}
    interface IRegistry<T extends MappedEntity> {}
    interface TypesBuilderData {}
    static class NBT {}
    static class ClientVersion {
        ClientVersion toClientVersion() {
            return null;
        }
    }

    class PacketWrapper<K> {
        ClientVersion getServerVersion() {
            return null;
        }
    }

    interface LegacyNbtEntryDecoder<T> {
        T decode(NBT nbt, ClientVersion version, TypesBuilderData data);

        default NbtEntryDecoder<T> upgrade() {
            return (nbt, wrapper, data) ->
                    this.decode(nbt, wrapper.getServerVersion().toClientVersion(), data);
        }
    }

    interface NbtEntryDecoder<T> {
        T decode(NBT tag, PacketWrapper<?> wrapper, TypesBuilderData data);
    }

    static final class RegistryEntry<T extends MappedEntity & CopyableEntity<T> & DeepComparableEntity> {
        RegistryEntry(IRegistry<T> baseRegistry, LegacyNbtEntryDecoder<T> decoder) {
            this(baseRegistry, decoder.upgrade());
        }

        RegistryEntry(IRegistry<T> baseRegistry, NbtEntryDecoder<T> decoder) {}
    }
}
",
    )])
);
