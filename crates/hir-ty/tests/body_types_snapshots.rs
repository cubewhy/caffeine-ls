//! Snapshots of expression-level type inference
//! ([JLS §15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html),
//! [§14.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.4))
//! over the body IR lowered from source methods.

#[macro_use]
mod common;

use crate::common::check_body_types;

snapshot!(
    literals,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int literals() {
        int i = 42;
        long l = 42L;
        char c = 'x';
        float f = 1.5f;
        double d = 1.5;
        boolean b = true;
        String s = \"hi\";
        Object o = null;
        Class<?> clazz = String.class;
        return i;
    }
}
",
    )])
);

snapshot!(
    arithmetic,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int arithmetic(byte a, short b, char c, long lo) {
        int i = a + b;
        long j = a + lo;
        double d = 1 + 2.5;
        float f = a + 1.0f;
        int neg = -a;
        int shl = a << 2;
        boolean big = a > b;
        boolean eq = a == b;
        String s = \"x\" + i;
        return i;
    }
}
",
    )])
);

snapshot!(
    boxing_numeric_promotion,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int boxing(java.lang.Integer i, java.lang.Long l, java.lang.Character c) {
        int a = i + 1;
        int b = i - i;
        int d = -i;
        long e = i + l;
        int f = i + c;
        boolean big = i > l;
        int cond = true ? i : i;
        long mix = true ? i : l;
        return a + b + d + f;
    }
}
",
    )])
);

snapshot!(
    for_each_iterable,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    int forEach(List<String> xs, java.lang.Iterable<Integer> ys, String[] as, int[] ns) {
        int sum = 0;
        for (String x : xs) {
            sum += x.length();
        }
        for (Integer y : ys) {
            sum += y;
        }
        for (String a : as) {
            sum += a.length();
        }
        for (int n : ns) {
            sum += n;
        }
        return sum;
    }
}
",
    )])
);

snapshot!(
    new_array_initializer,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int[] newArray() {
        int[] a = new int[] { 1, 2, 3 };
        String[] b = new String[] { \"x\", \"y\" };
        int[][] c = new int[][] { { 1 }, { 2, 3 } };
        int[] d = new int[3];
        return a;
    }
}
",
    )])
);

snapshot!(
    locals_and_fields,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int count = 10;
    static Body singleton = new Body();

    int run(int[] values, int index) {
        int acc = 0;
        for (int i = 0; i < values.length; i++) {
            acc = acc + values[i];
        }
        return acc + this.count;
    }

    int arrays() {
        int[][] grid = new int[3][4];
        int[] row = grid[1];
        int[] init = { 1, 2, 3 };
        return init[0] + row[1];
    }

    int conditional(int x) {
        return x > 0 ? x : -x;
    }
}
",
    )])
);

snapshot!(
    calls,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    int identity(int x) {
        return x;
    }

    String concat(String a, String b) {
        return a + b;
    }

    int call() {
        int x = identity(7);
        String s = concat(\"a\", \"b\");
        int len = s.length();
        Body other = new Body();
        int y = other.identity(x);
        return y;
    }
}
",
    )])
);

snapshot!(
    library_calls,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.List;

class Body {
    int listSize() {
        List<String> list = new ArrayList<String>();
        list.add(\"a\");
        int size = list.size();
        String first = list.get(0);
        return size;
    }
}
",
    )])
);

snapshot!(
    target_typing,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

class Body {
    List<String> empty() {
        return Collections.emptyList();
    }

    void use() {
        List<String> list = Collections.emptyList();
        Collections.sort(list);
        Collections.sort(new ArrayList<String>());
        Collections.emptyList();
    }
}
",
    )])
);

snapshot!(
    nested_invocation_argument,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}

    void call() {
        take(Collections.emptyList());
    }
}
",
    )])
);

snapshot!(
    nested_invocation_chain,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}

    <T> T id(T x) {
        return x;
    }

    void call() {
        take(Collections.emptyList());
        take(id(7));
    }
}
",
    )])
);

snapshot!(
    overload_by_target,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}
    void take(Object o) {}

    void call() {
        take(Collections.emptyList());
        take(new Object());
    }
}
",
    )])
);
// The `take(Object)` overload is eagerly applicable to the standalone
// `List<Object>` of `emptyList()`, so the plain resolution already succeeds
// and the §18.5.2.2 retry (which only runs on failure) never engages: the
// nested call is not retargeted, and the call stays `take(Object)`. javac's
// per-candidate inference of §18.5.2.1 would find `take(List<String>)` more
// specific; the eager-wins behavior here is the documented degradation of the
// retry-on-failure heuristic.

snapshot!(
    overload_retarget_choice,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}
    void take(List<Integer> ys) {}

    void call() {
        take(Collections.emptyList());
    }
}
",
    )])
);
// Neither overload is eagerly applicable to the standalone `List<Object>`
// (invariant type arguments), so the retry runs and picks the first candidate
// whose retargeted arguments resolve — `take(List<String>)`, with the nested
// `emptyList()` typed `List<String>`. javac would report this as ambiguous;
// picking the first applicable candidate is a documented heuristic.

snapshot!(
    poly_conditional_invocation,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.Collections;
import java.util.List;

class Body {
    void take(List<String> xs) {}

    void call(boolean flag) {
        take(flag ? Collections.emptyList() : Collections.emptyList());
    }
}
",
    )])
);
