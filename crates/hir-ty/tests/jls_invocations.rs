//! JLS SE 26 scenario snapshots for the *friendly bad-arguments* range of an
//! inapplicable invocation ([JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2)):
//! a `wrong-argument-count` diagnostic underlines the offending *arguments* —
//! IntelliJ-style, pre-argument — instead of the method name. When the arities
//! match, exactly the incompatible arguments are highlighted and each carries
//! its own `reason: … cannot be converted to …` `related_information` entry
//! ([§5.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.3)
//! loose conversion). When the call hands the closest candidate *too many*
//! arguments, the surplus ones are highlighted; a call that is too short, has
//! no usable candidate, or has no specific argument at fault falls back to the
//! method name. The whole argument list is never underlined.
//!
//! The renderer ([`check_body_diagnostic_spans`]) prints each diagnostic's
//! full `start..end` source range with the covered text, so the exact spans
//! are asserted verbatim.

#[macro_use]
mod common;

use crate::common::check_body_diagnostic_spans;

// -- red: arity matches, some arguments are incompatible ----------------------

snapshot!(
    bad_arguments_exact_example,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String arg0, int arg1) {}

    void use() {
        int i = 0;
        func(i, \"string\");
    }
}
",
    )])
);
// `func(String, int)` is called with `(int, String)`: the arities match, so
// the diagnostic underlines the first incompatible argument — `i` — with the
// `required:`/`found:`/`reason:` block, and the other incompatible argument
// (`"string"`) is reported as its own diagnostic at its own range. Every bad
// argument draws its own error line; no merged whole-list span.

snapshot!(
    bad_arguments_only_one,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String arg0, int arg1) {}

    void use() {
        func(\"ok\", \"bad\");
    }
}
",
    )])
);
// `func("ok", "bad")`: only the second argument is incompatible, so only
// `"bad"` is underlined (the first argument stays clean, IntelliJ-style); the
// `reason:` entry points at `"bad"` only.

snapshot!(
    bad_arguments_second_of_three,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String a, String b, int c) {}

    void use() {
        func(\"x\", 42, 1);
    }
}
",
    )])
);
// A three-parameter call with a bad *middle* argument: `42` (an `int` against
// `String`) is the only bad one, so the primary range covers just `42` and
// the two good arguments are left alone.

snapshot!(
    bad_arguments_split_across_parameters,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void foo(boolean s, String i, long b) {}

    void use() {
        foo(\"\", \"\", \"\");
    }
}
",
    )])
);
// `foo(boolean, String, long)` is called with three `String`s: the first
// (`""` against `boolean`) and the third (`""` against `long`) are
// incompatible while the middle one (`String` against `String`) converts. Each
// bad argument is reported at its own range — the first as the summary
// diagnostic, the third as its own `incompatible-types` line — so both draw an
// error line and neither is swallowed into the whole argument list.

// -- red: arity mismatch highlights the surplus arguments ----------------------

snapshot!(
    bad_arguments_arity_mismatch,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String arg0, int arg1) {}

    void use() {
        func(\"only one\");
        func(\"a\", 1, 2);
    }
}
",
    )])
);
// `func("only one")` is one argument short of `func(String, int)`, so there is
// no offending argument to highlight and the diagnostic keeps its name range.
// `func("a", 1, 2)` hands the candidate one *extra* argument — `2` — which is
// the surplus one the diagnostic highlights (never the whole list); the
// reason is the argument-list-length text.

// -- red: no-arg invocation keeps pointing at the name ------------------------

snapshot!(
    bad_arguments_no_args,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void func(String arg0, int arg1) {}

    void use() {
        func();
    }
}
",
    )])
);
// `func()` has no arguments at all, so there is nothing to highlight: the
// diagnostic falls back to the method name `func`, like `new Foo()` against a
// parameterized constructor.

// -- green: a nested poly argument keeps its own bad-argument range -----------

snapshot!(
    bad_arguments_nested,
    check_body_diagnostic_spans(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

import java.util.List;

class Body {
    void take(List<String> xs) {}

    void pick(int i) {}

    void use() {
        take(pick(\"nope\"));
    }
}
",
    )])
);
// `pick("nope")` is a nested poly argument of `take(...)`: the nested
// invocation's own diagnostic underlines its argument `"nope"` (its arity
// matches `pick(int)` and the `String` does not convert). The enclosing
// `take(pick("nope"))` arities also match, but its only argument is the poly
// `pick(...)` call — no concrete argument is at fault, so `take` stays on its
// name rather than underlining the whole nested call.

// §15.12.2.4/[§15.13.2]: a method reference supplied to a variable-arity
// parameter is packed against the *element* type of the varargs array —
// `registerListener(EventListener, Predicate<IEvent>...)` with
// `Vape::lambda$0` (a `boolean(IEvent)` method reference) resolves against
// the `Predicate<IEvent>` element, not the array type.

snapshot!(
    varargs_method_ref_packed_to_element,
    check_body_diagnostic_spans(&[(
        "/src/com/example/C.java",
        "\
package com.example;

import java.util.function.Predicate;

class C {
    interface EventListener {
    }

    interface IEvent {
    }

    static class EventBus {
        static final EventBus INSTANCE = new EventBus();

        @SafeVarargs
        public final void registerListener(EventListener eventListener, Predicate<IEvent> ... filters) {
        }
    }

    static class Tracker implements EventListener {
        boolean isEnabled() {
            return true;
        }
    }

    private static boolean lambda$0(IEvent event) {
        return true;
    }

    static void registerEventListeners() {
        EventBus.INSTANCE.registerListener(new Tracker(), C::lambda$0);
        EventBus.INSTANCE.registerListener(new Tracker(), new Predicate[0]);
    }
}
",
    )])
);
// Green: the method reference `C::lambda$0` is a `Predicate<IEvent>` and the
// zero-length array is the empty varargs argument — both resolve.
