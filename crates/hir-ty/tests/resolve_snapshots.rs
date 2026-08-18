//! Snapshots of source-side type name resolution ([JLS §6.5.5], [§7.5]):
//! every field and method signature in a sample file, resolved against the
//! JDK fixture. Type parameters in scope ([§6.3]) become `TyKind::TypeVar`.

#[macro_use]
mod common;

use crate::common::check_resolve_src;

snapshot! {
    imports_and_primitives,
    check_resolve_src(
        r#"
package com.example;
import java.util.List;
import java.util.*;
class Box {
    String name;
    List<String> items;
    ArrayList<Integer> nums;
    int count;
}
"#,
    ),
}

snapshot! {
    fully_qualified_reference,
    check_resolve_src(
        r#"
package com.example;
class Box {
    java.util.List<String> fq;
}
"#,
    ),
}

snapshot! {
    type_parameters,
    check_resolve_src(
        r#"
package com.example;
class Box<T> {
    T value;
    <U> U convert(U u) { return u; }
}
"#,
    ),
}

snapshot! {
    enclosing_type_parameters,
    check_resolve_src(
        r#"
package com.example;
class Outer<T> {
    class Inner {
        T innerValue;
    }
}
"#,
    ),
}

snapshot! {
    method_signatures,
    check_resolve_src(
        r#"
package com.example;
class Box {
    String concat(String a, int b) { return a; }
    void nothing() {}
}
"#,
    ),
}

snapshot! {
    unresolved_names,
    check_resolve_src(
        r#"
package com.example;
class Util {}
class Box {
    Util helper;
    MissingType missing;
}
"#,
    ),
}
