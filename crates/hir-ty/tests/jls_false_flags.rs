//! Regression snapshots for false positives resolved on the Java frontend:
//! legal programs that used to be misreported. Each section cites the JLS
//! chapter the fix follows.

#[macro_use]
mod common;

use crate::common::{
    ClassSpec, check_body_diagnostic_spans, check_body_types, check_body_types_with_libs,
    class_with_methods_access,
};

// -- §3.9: restricted identifiers are ordinary method/field names -------------
// `record`, `sealed` and `permits` cannot name a *type*, but a method or field
// of those names is perfectly legal ([JLS §3.9]).

snapshot!(
    restricted_identifier_method_names,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    private void record(int x, int y) {}
    private void sealed() {}
    private void permits() {}
    private int record;
    void m(int a) {
        record(a, a);
        sealed();
        permits();
    }
}
",
    )])
);

// -- §14.14.2: the enhanced-for loop variable is not implicitly final ---------
// A `for (String s : l)` variable may be reassigned in the body; only an
// explicit `final` modifier marks it.

snapshot!(
    enhanced_for_variable_mutable,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    static class Holder {
        int value;
    }
    void m(List<String> l, List<Holder> h, int[] arr) {
        for (String s : l) {
            s = \"x\";
        }
        for (int x : arr) {
            x = 5;
        }
        for (Holder holder : h) {
            holder.value = 1;
        }
    }
}
",
    )])
);

// -- §15.26/[§15.11.1]: a qualified field write does not assign the receiver --
// `final Holder h; h.value = 1` writes the *field*, not the local `h`; a
// `final` array reference and index are likewise only read by `a[i] = v`.

snapshot!(
    field_write_through_final_receiver,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static class Holder {
        int value;
        int[] data;
    }
    void m(final Holder h, final int[] arr, final int idx) {
        h.value = 5;
        h.data[0] = 6;
        arr[idx] = 7;
    }
}
",
    )])
);

// -- §15.25: a conditional with a primitive arm and `null` boxes the primitive
// `b ? true : null` has type `Boolean`, `b ? null : 5` has type `Integer`.

snapshot!(
    conditional_null_primitives_box,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static Boolean f(boolean b) {
        return b ? true : null;
    }
    static Integer g(boolean b) {
        return b ? null : 5;
    }
    static Long h(boolean b) {
        return b ? null : 5L;
    }
}
",
    )])
);

// -- §15.9.3/[§15.9.2.2]: a diamond creation is a poly expression in an
// invocation context — `synchronizedList(new ArrayList<>())` against a
// `List<String>` target infers `ArrayList<String>`.

snapshot!(
    diamond_in_invocation_context,
    check_body_types_with_libs(
        &[class_with_methods_access(
            "java/util/WrapUtil",
            None,
            &[],
            &[("synchronizedList", "(Ljava/util/List;)Ljava/util/List;")],
            &["<T:Ljava/lang/Object;>(Ljava/util/List<TT;>;)Ljava/util/List<TT;>;"],
            &[0x0009], // ACC_PUBLIC | ACC_STATIC
        )],
        &[(
            "/src/com/example/Body.java",
            "\
package com.example;

import java.util.ArrayList;
import java.util.List;
import java.util.WrapUtil;

class Body {
    List<String> m() {
        return WrapUtil.synchronizedList(new ArrayList<>());
    }
}
",
        )],
    )
);

// -- §5.1.9: an array of a raw type converts unchecked to a parameterized
// array — `Frame<BasicValue>[] f = new Frame[7]`.

snapshot!(
    raw_array_unchecked_conversion,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    static class Frame<T> {
    }
    Frame<String>[] m() {
        Frame<String>[] f = new Frame[7];
        return f;
    }
}
",
    )])
);

// -- §15.12.2.5: a non-generic overload is more specific than a generic one
// when the generic method can instantiate to the non-generic's signature;
// and a generic `that(T[])` beats `that(Object)` for an array argument.

snapshot!(
    generic_non_generic_most_specific,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Map;

class Body {
    private static void increment(Map<String, Integer> map, String key) {
    }
    private static <T> void increment(Map<T, Integer> map, T key) {
    }

    static class Subject {
        static class Builder {
            <T> ObjectArraySubject<T> that(T[] a) {
                return null;
            }

            Subject that(Object o) {
                return null;
            }
        }

        static class ObjectArraySubject<T> extends Subject {
            ObjectArraySubject<T> asList() {
                return null;
            }
        }
    }

    void m(Map<String, Integer> actual, String[] strings, Subject.Builder b) {
        increment(actual, \"x\");
        b.that(strings).asList();
    }
}
",
    )])
);

// -- §7.5.2: a type-import-on-demand imports only *accessible* types. A
// package-private class of another package is not a candidate, so a simple
// name that only the inaccessible class supplies is not ambiguous — it
// resolves to the accessible one.

snapshot!(
    on_demand_import_ignores_inaccessible,
    check_body_types_with_libs(
        &[
            class_with_methods_access(
                "org/objectweb/asm/tree/analysis/Frame",
                Some("java/lang/Object"),
                &[],
                &[("getStackSize", "()I")],
                &[""],
                &[0x0001], // ACC_PUBLIC
            ),
            class_with_methods_access(
                "org/objectweb/asm/tree/analysis/SourceValue",
                Some("java/lang/Object"),
                &[],
                &[],
                &[],
                &[],
            ),
            // A package-private class (`ACC_PUBLIC` unset) named `Frame` in
            // another package must not be a candidate of `import a.*`.
            ClassSpec {
                fqn: "org/objectweb/asm/Frame",
                super_class: Some("java/lang/Object"),
                interfaces: &[],
                access: 0x0020, // ACC_SUPER, package-private (no ACC_PUBLIC)
                fields: &[],
                methods: &[],
                method_sigs: &[],
                method_access: &[],
                sig: None,
            },
        ],
        &[(
            "/src/com/example/Body.java",
            "\
package com.example;

import org.objectweb.asm.tree.analysis.Frame;
import org.objectweb.asm.tree.analysis.SourceValue;

class Body {
    Frame<SourceValue>[] frames;
    int m() {
        return frames[0].getStackSize();
    }
}
",
        )],
    )
);
