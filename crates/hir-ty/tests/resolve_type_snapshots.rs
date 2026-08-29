//! Snapshots of the unknown-reference and import diagnostics
//! ([JLS §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1),
//! [§7.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.1),
//! [§7.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.2)):
//! a reference type name that resolves to nothing on the classpath is reported
//! in both declaration and body positions; a name shadowed by a broken
//! single-type import does not fall through; two on-demand imports that
//! supply the same simple name from different types are ambiguous; and the
//! single-type imports themselves are validated.

#[macro_use]
mod common;

use hir::{LibraryInfo, LibraryKind};
use vfs::AbsPathBuf;

use crate::common::{
    TestDatabase, check_body_types, check_class_diagnostics, class, register_source_set_classpath,
    temp_jar,
};

snapshot!(
    unknown_declaration_types,
    check_class_diagnostics(&[(
        "/src/com/example/App.java",
        "\
package com.example;

import java.util.List;

class App extends org.missing.Base implements org.missing.Marker {
    org.missing.List<String> list;
    org.missing.Missing nested;
    org.missing.Widget method(org.missing.Param p, java.util.List<org.missing.Elem> es)
        throws org.missing.Failure, org.missing.Outcome {
        return null;
    }
}
",
    )])
);
// A qualified name (`org.missing.{Base,Marker,List,Missing,Widget,Param,...
// Failure,Outcome}`) whose package exists nowhere on the classpath resolves to
// nothing ([§6.5.5.2]) — each occurrence is a compile-time error. The
// well-known names (`java.util.List`, `java.lang.String` inside the type
// argument) stay silent.

snapshot!(
    unknown_body_type_refs,
    check_body_types(&[(
        "/src/com/example/Body.java",
        "\
package com.example;

class Body {
    void m(Object o) {
        org.example.List<java.lang.String> a = new org.example.List<java.lang.String>();
        org.example.List<java.lang.String> b;
        Object c = (org.example.NoSuch) o;
        Class<?> k = org.example.MissingClass.class;
    }
}
",
    )])
);
// Body-owned type references: a local's declared type (§6.5.5.1), a
// class-instance creation / a class literal ([§15.8.2]) and a cast
// ([§15.16]) are reported at the reference name's span. `java.lang.String`
// resolves through the implicit `java.lang` import and stays silent.

snapshot!(
    unknown_broken_import_blocks_fallthrough,
    check_class_diagnostics(&[(
        "/src/com/example/App.java",
        "\
package com.example;

import org.missing.Thing;

class App {
    Thing field;
}
",
    )])
);
// §7.5.1: the single-type import names a non-existent class — an
// uncompilable import. §6.5.5.1: because the simple name is shadowed by the
// (broken) import, `Thing` does not fall through to a later candidate; the
// use is *unresolved*, not silently rebound.

// --- on-demand ambiguity & import conflicts (extra classpath library) ------

/// Renders the declaration diagnostics of `files` against a classpath of a
/// temporary library (`specs`) plus the JDK fixture.
fn check_with_libs(specs: &[common::ClassSpec<'static>], files: &[(&str, &str)]) -> String {
    let fixture = common::jdk_fixture();
    let extra = temp_jar("widgets", specs);
    let mut db = TestDatabase::new();
    let info = LibraryInfo::new(
        LibraryKind::Jar,
        AbsPathBuf::assert_utf8(extra.path.as_std_path().to_owned()),
    );
    let classpath = vec![
        hir::ClasspathEntry::Library(fixture.lib),
        hir::ClasspathEntry::Library(extra.lib),
    ];
    register_source_set_classpath(&mut db, &fixture, files, classpath, &[(extra.lib, info)]);

    let mut lines = files
        .iter()
        .map(|(path, text)| format!("FILE {path}:\n{text}"))
        .collect::<Vec<_>>();
    for (i, (_, text)) in files.iter().enumerate() {
        let file_id = vfs::FileId::from_raw((i + 1) as u32);
        let line_index = line_index::LineIndex::new(text);
        for diag in hir_ty::class_diagnostics(&db, file_id) {
            let at = diag
                .range()
                .map(|r| {
                    let lc = line_index.line_col(r.start());
                    format!("@{line}:{col}", line = lc.line, col = lc.col)
                })
                .unwrap_or_default();
            lines.push(format!(
                "method {}: {}: {}{}",
                diag.method_name(),
                diag.code(),
                at,
                diag.message(&db)
            ));
        }
    }
    lines.join("\n")
}

fn two_widget_specs() -> Vec<common::ClassSpec<'static>> {
    vec![
        class("com/a/Widget", Some("java/lang/Object"), &[]),
        class("com/b/Widget", Some("java/lang/Object"), &[]),
    ]
}

snapshot!(
    on_demand_import_ambiguity,
    check_with_libs(
        &two_widget_specs(),
        &[(
            "/src/com/example/App.java",
            "\
package com.example;

import com.a.*;
import com.b.*;

class App {
    Widget w;
}
"
        ),],
    )
);
// §6.5.5.1: `Widget` is reachable through two on-demand imports ([§7.5.2])
// that denote different types (`com.a.Widget`, `com.b.Widget`) — the use is
// ambiguous, a compile-time error.

snapshot!(
    single_type_import_conflicts,
    check_with_libs(
        &two_widget_specs(),
        &[(
            "/src/com/example/App.java",
            "\
package com.example;

import com.a.Widget;
import com.b.Widget;

class App {
    Widget w;
}
"
        ),],
    )
);
// §7.5.1: two single-type imports of the same simple name for different
// classes conflict — a compile-time error.
