//! JLS SE 26 scenario snapshots for the operator families
//! ([JLS §15.14](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.14)–
//! [§15.26](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.26),
//! [§5.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6)):
//! unary/postfix, binary numeric and string concatenation, shifts, relational,
//! equality, bitwise and conditional-and/or, the conditional operator and
//! casts. Every scenario is verified against `javac` before the snapshot is
//! accepted; the red ones render the diagnostics the type layer must report.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_body_types};

// -- green: operators type correctly ----------------------------------------

snapshot!(
    unary_postfix,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(byte b, short s, char c, java.lang.Integer box, int i) {
        int pos = +b;
        int neg = -i;
        int negB = -box;
        int not = ~b;
        int inc = ++i;
        int dec = b-- - ++c;
        int unboxed = -new java.lang.Integer(3);
        return pos + neg + not + inc + dec + unboxed + negB;
    }
}
",
    )])
);

snapshot!(
    binary_numeric,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    long m(byte a, short b, char c, int i, long lo, float f, double d, java.lang.Integer box) {
        long l1 = a + b;
        long l2 = i + lo;
        double d1 = i + d;
        float f1 = b + f;
        int i1 = a * b;
        int i2 = i / b;
        int rem = a % b;
        long b1 = lo - i;
        int bi = box + i;
        long bl = box + lo;
        return l1 + l2 + i1 + i2 + rem + (long) b1 + (long) bi + (long) bl;
    }
}
",
    )])
);

snapshot!(
    string_concat,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    String m(int i, boolean b, Object o) {
        String s1 = \"a\" + i;
        String s2 = b + \"b\";
        String s3 = o + \"c\";
        String s4 = \"\" + \"\";
        return s1 + s2 + s3 + s4;
    }
}
",
    )])
);

snapshot!(
    shifts,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(byte b, short s, char c, long lo, java.lang.Integer box) {
        int shl = b << 2;
        int shr = s >> 1;
        int ushr = c >>> 1;
        long lsh = lo << 3;
        int boxed = box << 2;
        return shl + shr + ushr + (int) lsh + boxed;
    }
}
",
    )])
);

snapshot!(
    relational_equality,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    boolean m(int i, long lo, double d, java.lang.Integer box, java.lang.String s, Object o, int[] arr) {
        boolean lt = i < lo;
        boolean gt = d > i;
        boolean le = i <= 1;
        boolean ge = box >= 1;
        boolean eq = i == 1L;
        boolean ne = box != lo;
        boolean ref = s == \"x\";
        boolean obj = o == s;
        boolean nullEq = s == null;
        boolean nulNe = null != arr;
        boolean unbox = box == i;
        return lt && gt && le && ge && eq && ne && ref && obj && nullEq && nulNe && unbox;
    }
}
",
    )])
);

snapshot!(
    bitwise_logical,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int m(int a, int b, boolean x, boolean y, java.lang.Boolean bx) {
        int band = a & b;
        int bor = a | b;
        int bxor = a ^ b;
        boolean land = x & y;
        boolean lor = x | y;
        boolean lxor = x ^ y;
        boolean and = x && bx;
        boolean or = !x || y;
        return band + bor + bxor + (land ? 1 : 0) + (lor ? 1 : 0) + (lxor ? 1 : 0);
    }
}
",
    )])
);

snapshot!(
    conditional_and_cast,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    Object m(boolean c, int i, long lo, String s, Object o, Number n, java.lang.Iterable<String> it) {
        int t = c ? 1 : 2;
        long mixed = c ? i : lo;
        java.lang.Integer boxed = c ? i : new java.lang.Integer(2);
        Object up = c ? i : o;
        String down = (String) o;
        Number num = (Number) n;
        Object widened = (Object) \"x\";
        long narrow = (long) i;
        byte b = (byte) lo;
        java.lang.Iterable<String> castIt = (java.lang.Iterable<String>) it;
        return up;
    }
}
",
    )])
);

snapshot!(
    compound_assignment,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    byte m(byte b, short s, int i, long lo) {
        b += 1;
        b -= 1;
        s *= 2;
        s /= 2;
        i %= 3;
        i <<= 2;
        i >>= 1;
        i >>>= 1;
        i &= 0x0F;
        i |= 0xF0;
        i ^= 0xFF;
        lo += i;
        return b;
    }
}
",
    )])
);

// -- red: operator misuse must produce diagnostics --------------------------

snapshot!(
    operator_errors,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    String s = \"x\";
    Object o = null;

    String unaryMinus() { return -s; }
    int stringMinus() { return \"a\" - \"b\"; }
    String bitNot() { return ~s; }
    int shiftBad() { return 1 << s; }
    boolean notBad() { return !1; }
    boolean eqBad() { return 1 == s; }
    boolean ltBad() { return 1 < s; }
    boolean boolAndBad() { return true && 1; }
    boolean boolOrBad() { return false || 1; }
    int castBad() { return (int) s; }
    int mulBad() { return 2 * s; }
}
",
    )])
);

snapshot!(
    nested_ternary_string,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    String marker(boolean a, boolean c) {
        return a ? \"+ \" : c ? \"~ \" : \"  \";
    }

    int nested(byte marker) {
        return marker == 3 ? 3 : marker == 2 ? 2 : marker == 1 ? 1 : 0;
    }
}
",
    )])
);
// §15.25: `ConditionalExpression ::= ConditionalOrExpression ? Expression :
// ConditionalExpression` is right-associative — `a ? b : c ? d : e` groups as
// `a ? b : (c ? d : e)`, so the nested marker/tooltip ternaries type as
// `String`/`int` and feed the enclosing return.

snapshot!(
    ternary_null_and_array,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    byte[] remap(int[] input, int factor) {
        int[] copy = new int[input.length];
        for (int i = 0; i < input.length; i++) {
            copy[i] = input[i] * factor;
        }
        byte[] out = new byte[copy.length];
        for (int i = 0; i < copy.length; i++) {
            out[i] = (byte) copy[i];
        }
        return out;
    }

    byte[] pick(boolean excluded, int[] input, int factor) {
        return excluded ? null : remap(input, factor);
    }
}
",
    )])
);
// §15.25: `cond ? null : T` has type `T`. An array is a reference type
// ([§4.3.1]), so a null/array-arm pair keeps the array type instead of taking
// a meaningless lub — `pick` returns `byte[]`.

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
// §15.25: a conditional with a primitive arm and `null` boxes the primitive —
// `b ? true : null` has type `Boolean`, `b ? null : 5` has type `Integer`.

// -- §15.20.2/[§4.7]: instanceof array targets follow reifiability -----------------
// An array type is reifiable exactly when its component type is ([§4.7]): the
// concrete `String[]`/`int[]`, the unbounded-wildcard `List<?>[]` and the
// raw `List[]` are legal `instanceof` targets, while `List<String>[]` carries
// a non-reifiable component and is rejected — `javac` 25 reports "Object cannot
// be safely cast to List<String>[]".

snapshot!(
    instanceof_array_reifiability,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void m(Object o, String[] s) {
        boolean a = o instanceof String[];
        boolean b = o instanceof int[];
        boolean c = o instanceof int[][];
        boolean d = s instanceof Object[];
        boolean e = o instanceof List<?>[];
        boolean f = o instanceof List[];
        boolean bad = o instanceof List<String>[];
    }
}
",
    )])
);
