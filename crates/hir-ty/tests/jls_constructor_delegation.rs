//! JLS SE 26 scenario snapshots for the *constructor* rules
//! ([JLS §8.8.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7),
//! [§8.8.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7.1)):
//! the implicit `super()` of a class with no declared constructor must find an
//! accessible no-argument superclass constructor (`NoDefaultConstructor`,
//! §8.8.7); a `this(...)` delegation cycle never reaches the supertype
//! constructor (`RecursiveConstructorInvocation`, §8.8.7.1); an explicit
//! constructor invocation must be the first statement (`ConstructorCallNotFirst`,
//! §8.8.7.1); and no `this`/`super`/instance-member reference may precede the
//! supertype call (`CannotReferenceBeforeSuper`, §8.8.7.1). Red cases render
//! the diagnostics; green cases confirm legal constructors pass cleanly.

#[macro_use]
mod common;

use crate::common::{check_body_diagnostic_spans, check_class_diagnostics};

// -- §8.8.7: implicit super() needs an accessible no-arg superclass ctor ------

snapshot!(
    no_default_constructor,
    check_class_diagnostics(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class NeedsArg {
    NeedsArg(int x) {}
}

class Broken extends NeedsArg {}

class Fine extends NeedsArg {
    Fine() {
        super(1);
    }
}

class NoCtor {}
class Direct extends NoCtor {}
",
    )])
);
// Red/Green: `Broken` declares no constructor, so its implicit `super()` must
// call `NeedsArg()` — which does not exist. `Fine` delegates explicitly and
// `Direct`'s no-arg superclass is fine.

snapshot!(
    private_no_arg_superclass,
    check_class_diagnostics(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class PrivateBase {
    private PrivateBase() {}
}

class Sub extends PrivateBase {}
",
    )])
);
// Red: the superclass's only constructor is `private`, so the implicit
// `super()` cannot reach it.

// -- §8.8.7.1: recursive constructor invocation ------------------------------

snapshot!(
    self_recursive_ctor,
    check_class_diagnostics(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Self {
    Self() {
        this();
    }
}
",
    )])
);
// Red: a constructor delegating directly to itself with `this()` — javac:
// `recursive constructor invocation`.

snapshot!(
    mutually_recursive_ctors,
    check_class_diagnostics(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Pair {
    Pair() {
        this(1);
    }
    Pair(int x) {
        this();
    }
}
",
    )])
);
// Red: `Pair()` delegates to `Pair(int)` which delegates back — the cycle
// never reaches the supertype constructor.

snapshot!(
    non_recursive_delegation,
    check_class_diagnostics(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Chain {
    Chain() {
        this(1);
    }
    Chain(int x) {
        this(\"s\");
    }
    Chain(String s) {
        super();
    }
}
",
    )])
);
// Green: a *chain* of delegation that bottoms out in `super()` is legal.

// -- §8.8.7.1: the explicit constructor invocation must be first --------------

snapshot!(
    ctor_call_not_first,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Base {}

class Sub extends Base {
    Sub() {
        int x = 1;
        super();
    }
}
",
    )])
);
// Red: the explicit `super()` is not the first statement of the constructor
// body.

snapshot!(
    ctor_call_firsts,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Base {
    Base() {}
    Base(int x) {}
}

class Sub extends Base {
    Sub() {
        super();
    }
    Sub(int x) {
        super(x);
    }
    Sub(String s) {
    }
}
",
    )])
);
// Green: an explicit `super()`/`super(arg)` as the first statement, and a body
// with no explicit call (the implicit `super()`) are all fine.

// -- §8.8.7.1: no this / instance references before the supertype call --------

snapshot!(
    ref_this_before_super,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Base {}

class Sub extends Base {
    int f;
    int g() {
        return 1;
    }
    Sub() {
        this.f = 1;
        g();
        int k = f;
        super();
    }
}
",
    )])
);
// Red: `this.f`, the unqualified `g()` and the simple-name instance field read
// all precede the supertype constructor call.

snapshot!(
    ref_super_before_super,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Base {
    int b;
}

class Sub extends Base {
    Sub() {
        super.b = 2;
        super();
    }
}
",
    )])
);
// Red: `super.b` before the `super()` call.

snapshot!(
    ctor_arg_before_super,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Base {
    Base(int x) {}
}

class Sub extends Base {
    int f;
    Sub() {
        super(this.f);
    }
}
",
    )])
);
// Red: the `this` reference in the `super(...)` argument is itself evaluated
// before the supertype constructor runs.

snapshot!(
    legal_before_super_statements,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Ctors.java",
        "\
package com.example;

class Base {}

class Sub extends Base {
    Sub() {
        int x = 1;
        int[] arr = new int[1];
        String s = \"s\";
        super();
        int y = x + 1;
        thisField = y;
    }
    int thisField;
}
",
    )])
);
// Statements that only touch locals and `new` may precede `super()` — only
// `this`/`super`/instance-member references are rejected, and after the call
// the object is fully usable. (The explicit invocation itself is not the first
// statement, so the `constructor-call-not-first` rule of §8.8.7.1 still
// flags the call position.)
