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

// -- §8.3.1.2/[§16]: a blank static final field is assigned once in a static
// initializer or a static field initializer — the legal initialization, not a
// cannot-assign error.

snapshot!(
    blank_static_final_in_static_initializer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
    }
}
",
    )])
);

snapshot!(
    blank_static_final_in_static_field_initializer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static int copy = sf = 1;
}
",
    )])
);

// -- §8.3.1.2/[§16]: a blank final instance field is assigned once in a
// constructor or an instance initializer (via a bare simple name or a bare
// `this.field`) — the legal initialization.

snapshot!(
    blank_instance_final_in_constructor,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        f = 1;
    }
    Final(int x) {
        this();
    }
}
",
    )])
);

snapshot!(
    blank_instance_final_in_instance_initializer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    {
        f = 1;
    }
    Final() {
    }
}
",
    )])
);

snapshot!(
    blank_instance_final_qualified_this_in_constructor,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        this.f = 1;
    }
}
",
    )])
);

// -- §8.3.1.2/[§16]: a *second* write to a blank final field is the
// already-assigned error — javac: `variable {f} might already have been
// assigned` — reported at the second write's target. Across sibling bodies
// that run before it (another static initializer, a later instance
// initializer, a constructor after the instance initializers) and within one
// body after a branch both of whose paths assigned the field.

snapshot!(
    blank_static_final_double_assignment_same_block,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
        sf = 2;
    }
}
",
    )])
);

snapshot!(
    blank_static_final_double_assignment_sibling_blocks,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
    }
    static {
        sf = 2;
    }
}
",
    )])
);

snapshot!(
    blank_static_final_double_assignment_field_init_then_block,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static int copy = sf = 1;
    static {
        sf = 2;
    }
}
",
    )])
);

snapshot!(
    blank_instance_final_ctor_after_instance_initializer,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    {
        f = 1;
    }
    Final() {
        f = 2;
    }
}
",
    )])
);

snapshot!(
    blank_instance_final_double_assignment_two_initializers,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    {
        f = 1;
    }
    {
        f = 2;
    }
    Final() {
    }
}
",
    )])
);

snapshot!(
    blank_final_after_both_branches_assigned,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        if (\"x\".length() > 0) {
            sf = 1;
        } else {
            sf = 2;
        }
        sf = 3;
    }
}
",
    )])
);

// -- §8.3.1.2/[§16]: the *legal* one-time assignments that must not be
// flagged: a blank final assigned on both branches of an `if`/`else`, and a
// write after an `if` whose then-arm exits.

snapshot!(
    blank_final_assigned_on_both_branches,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        if (\"x\".length() > 0) {
            sf = 1;
        } else {
            sf = 2;
        }
    }
}
",
    )])
);

snapshot!(
    blank_final_after_exiting_if_branch,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        if (\"x\".length() > 0) {
            sf = 1;
            throw new RuntimeException();
        }
        sf = 2;
    }
}
",
    )])
);

// -- §8.3.1.2/[§16]: a blank final field written from a *method*, an instance
// context writing a static final, a static context writing an instance final,
// a qualified `Type.field`/`Type.this.field` write, and a non-blank final
// reassignment are all errors — the cannot-assign or non-static errors.

snapshot!(
    blank_final_after_this_delegation,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final(int x) {
        f = 1;
    }
    Final() {
        this(1);
        f = 2;
    }
}
",
    )])
);

snapshot!(
    blank_final_this_delegation_no_write,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final(int x) {
        f = 1;
    }
    Final() {
        this(1);
    }
}
",
    )])
);

snapshot!(
    blank_static_final_written_from_static_method,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
    }
    static void m() {
        sf = 2;
    }
}
",
    )])
);

snapshot!(
    blank_instance_final_written_from_instance_method,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        f = 1;
    }
    void m() {
        f = 2;
    }
}
",
    )])
);

snapshot!(
    static_final_written_from_instance_context,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        sf = 1;
    }
    Final() {
        sf = 2;
    }
}
",
    )])
);

snapshot!(
    instance_final_written_from_static_context,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        f = 1;
    }
    static {
        f = 2;
    }
}
",
    )])
);

snapshot!(
    blank_final_qualified_type_write,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    static final int sf;
    static {
        Final.sf = 1;
    }
}
",
    )])
);

snapshot!(
    blank_final_qualified_this_write,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f;
    Final() {
        Final.this.f = 1;
    }
}
",
    )])
);

snapshot!(
    initialized_final_field_reassignment,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Final.java",
        "\
package com.example;

class Final {
    final int f = 1;
    static final int sf = 2;
    void m() {
        f = 3;
    }
    static void sm() {
        sf = 4;
    }
}
",
    )])
);

// -- §5.1.9: a *multi-dimensional* array of a raw type converts unchecked to
// an array of the parameterized element — `ArrayList[][] → List<String>[][]`
// is legal, exactly like the 1-D `Frame[] → Frame<String>[]` case.

snapshot!(
    multi_dim_raw_array_unchecked_conversion,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.List;

class Body {
    List<String>[][] foo() {
        ArrayList[][] arrayListArrayArray = new ArrayList[1][];
        return arrayListArrayArray;
    }
}
",
    )])
);
