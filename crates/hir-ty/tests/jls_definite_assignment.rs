//! JLS SE 26 scenario snapshots for definite assignment
//! ([JLS §16](https://docs.oracle.com/javase/specs/jls/se26/html/jls-16.html)):
//! blank finals assigned on every path ([§16.1.8]), loops whose bodies may
//! run zero times ([§16.1.10], [§16.1.14]) and compound assignments that read
//! before writing ([§15.26.2]). Red cases render the diagnostics the type
//! layer must report; green cases confirm legal forms pass without
//! diagnostics.

#[macro_use]
mod common;

use crate::common::check_body_types;

// -- green: a blank final assigned on both branches ([§16.1.8]) ------------------

snapshot!(
    blank_final_assigned_on_all_paths,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(boolean p) {
        final int f;
        if (p) {
            f = 1;
        } else {
            f = 2;
        }
        int use = f + 1;
    }
}
",
    )])
);

// -- green: a conditional return guards one path ----------------------------------
// When the then branch exits, only the fall-through path constrains the join.

snapshot!(
    return_guard,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    int m(boolean p) {
        int x;
        if (p) {
            return 0;
        }
        x = 1;
        return x + 1;
    }
}
",
    )])
);

// -- red: a value read before any assignment reaches it ---------------------------

snapshot!(
    use_before_assign,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m() {
        int x;
        int y = x + 1;
    }
}
",
    )])
);

// -- red: a loop body may run zero times ([§16.1.10]) ------------------------------

snapshot!(
    loop_may_not_run,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m(boolean p) {
        int x;
        while (p) {
            x = 1;
        }
        int y = x;
    }
}
",
    )])
);

// -- red: a compound assignment reads its left-hand side ([§15.26.2]) ---------------

snapshot!(
    compound_reads_first,
    check_body_types(&[(
        "/src/com/example/Da.java",
        "\
package com.example;

class Da {
    void m() {
        int x;
        x += 1;
    }
}
",
    )])
);
