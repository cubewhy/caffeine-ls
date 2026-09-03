//! JLS SE 26 scenario snapshots for two member-resolution gaps surfaced by
//! real classfiles:
//!
//! * *synthetic member hiding* ([JVMS §4.6]): javac keeps `ACC_BRIDGE` /
//!   `ACC_SYNTHETIC` methods of a classfile out of source member lookup. A
//!   covariant-return bridge (`Object[] arr()` next to the real
//!   `Collection<? extends String> coll()`-shaped overload pair) must not
//!   surface as a second candidate, or every call reports ambiguous / resolves
//!   the wrong overload.
//! * *captured wildcard SAM elements* ([§5.1.10], [§18.2.3]): a generic
//!   referenced method whose type parameter is bounded by a
//!   wildcard-parameterized SAM element — `<T extends CE<?>> void wf(…, CE<T>)`
//!   against a `CE<?>` element — instantiates `T` to the wildcard's capture.

#[macro_use]
mod common;

use crate::common::{check_body_types, check_body_types_with_libs, class_with_methods_access};
// -- green: a synthetic classfile member is invisible to source lookup ---------

snapshot!(
    synthetic_covariant_bridge_hidden,
    check_body_types_with_libs(
        &[class_with_methods_access(
            // The library classfile declares `lookup()` twice, exactly as a
            // covariant-return overload pair emitted into bytecode: the
            // `Object` form is the synthetic bridge ([JVMS §4.6]
            // `ACC_BRIDGE|ACC_SYNTHETIC`), the `String` form the real
            // declaration. javac ignores the synthetic member, so a call
            // resolves the `String` overload and `.length()` is reachable.
            "bukkit/Server",
            Some("java/lang/Object"),
            &[],
            &[
                ("lookup", "()Ljava/lang/Object;"),
                ("lookup", "()Ljava/lang/String;"),
            ],
            &["", ""],
            // First method: ACC_PUBLIC | ACC_STATIC | ACC_BRIDGE | ACC_SYNTHETIC.
            &[0x1009, 0x0009],
        )],
        &[(
            "/src/bukkit/Use.java",
            "\
package bukkit;

class Use {
    int len() {
        return Server.lookup().length();
    }
}
",
        )],
    )
);
// §8.4.8.3/[JVMS §4.6]: a covariant bridge is flagged synthetic; javac hides
// every `ACC_SYNTHETIC` member of a classfile from source member resolution, so
// `Server.lookup()` selects the `String` overload and `.length()` resolves.
// Without the filter the `Object` overload is also a candidate and the
// tie-break reports an ambiguity (or picks the wrong return).

// -- green: generic referenced method over a captured wildcard SAM element -----

snapshot!(
    generic_ref_captured_wildcard_sam_element,
    check_body_types(&[(
        "/src/com/example/Box.java",
        "\
package com.example;

import java.util.List;

class Box {
    abstract static class CE<T extends CE<?>> {}

    interface BC<A, B> {
        void accept(A a, B b);
    }

    static <T extends CE<?>> void wf(Box p, CE<T> e) {}

    static <K> void writeList(List<K> l, BC<Box, K> w) {}

    static void use(List<CE<?>> effects, Box p) {
        writeList(effects, Box::wf);
    }
}
",
    )])
);
// §5.1.10/§18.2.3: `writeList`'s `K` is fixed to `CE<?>` by the first actual;
// the method reference `Box::wf` targets `BC<Box, CE<?>>`, whose SAM element
// `CE<?>` is a wildcard-parameterized value type. Relating it to `wf`'s
// parameter `CE<T>` (a fresh inference variable for `<T extends CE<?>>`)
// captures the wildcard so `T := CAP#n`, exactly the instantiation javac
// derives for the reference — without the capture the bare wildcard degrades
// to `Object` and `writeList` is reported inapplicable.
