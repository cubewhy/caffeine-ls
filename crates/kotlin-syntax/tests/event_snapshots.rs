mod common;

use common::event_snapshot;

event_snapshot!(event_empty_file, "");

event_snapshot!(event_semicolons_only, ";;;");

event_snapshot!(
    event_package_and_imports,
    "package a.b\nimport a.b.*\nimport a.b.C as D\n"
);

event_snapshot!(event_file_annotation, "@file:JvmName(\"FooKt\")\n");
