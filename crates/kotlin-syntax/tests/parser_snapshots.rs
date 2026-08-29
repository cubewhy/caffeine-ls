mod common;

use common::parser_snapshot;
use indoc::indoc;

parser_snapshot!(parse_empty_file, "");

parser_snapshot!(
    parse_semicolons_only,
    indoc! {r#"
        ;;;
    "#}
);

parser_snapshot!(
    parse_shebang,
    indoc! {r#"
        #!/usr/bin/env kotlin
        fun main() {}
    "#}
);

parser_snapshot!(
    parse_package_header,
    indoc! {r#"
        package com.example.app
    "#}
);

parser_snapshot!(
    parse_package_header_with_semicolon,
    indoc! {r#"
        package com.example.app;
    "#}
);

parser_snapshot!(
    parse_import_single,
    indoc! {r#"
        import com.example.Foo
    "#}
);

parser_snapshot!(
    parse_import_star,
    indoc! {r#"
        import com.example.*
    "#}
);

parser_snapshot!(
    parse_import_alias,
    indoc! {r#"
        import com.example.Foo as Bar
    "#}
);

parser_snapshot!(
    parse_file_annotation,
    indoc! {r#"
        @file:JvmName("FooKt")
    "#}
);

parser_snapshot!(
    parse_multiple_file_annotations,
    indoc! {r#"
        @file:Suppress("UNUSED")
        @file:[JvmMultifileClass Foo Bar]
        package com.example
    "#}
);
