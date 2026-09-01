//! The JLS §15.12.2.6 `Object.getClass` special invocation type.
//!
//! [§15.12.2.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.6)
//! gives a non-generic `getClass()` invocation whose receiver type is `T`
//! the invocation type of the method with the return type
//! `Class<? extends |T|>` — the erasure `|T|` ([§4.6]) of the type searched
//! ([§15.12.1]). This is what makes the receiver's own type usable as a
//! `Class<? extends R>` argument and drives the type argument inference of a
//! generic method:
//!
//! - `<T extends Mod> T getMod(Class<T>)` invoked with `getMod(mod.getClass())`
//!   infers `T := Mod` (javac's `Mod`, not the declared `Class<?>`), so the
//!   result converts back to `Mod`.
//! - a generic `put(Class<? extends Mod>, Mod)` accepts `put(mod.getClass(),
//!   mod)` — the `Class<?>` declared return would not convert to the key type.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

snapshot!(
    get_class_infers_bound,
    check_body_diagnostic_spans(&[
        (
            "/src/gg/vape/Mod.java",
            "\
package gg.vape;

class Mod {
}
",
        ),
        (
            "/src/gg/vape/Registry.java",
            "\
package gg.vape;

class Registry {
    <T extends Mod> T getMod(Class<T> clazz) {
        return null;
    }

    Mod byClass(Mod probe) {
        return getMod(probe.getClass());
    }
}
",
        )
    ])
);

snapshot!(
    get_class_key_argument,
    check_body_diagnostic_spans(&[
        (
            "/src/gg/vape/Mod.java",
            "\
package gg.vape;

class Mod {
}
",
        ),
        (
            "/src/gg/vape/Registry.java",
            "\
package gg.vape;

class Registry {
    void register(Class<? extends Mod> key, Mod value) {
    }

    void doRegister(Mod module) {
        register(module.getClass(), module);
    }
}
",
        )
    ])
);
