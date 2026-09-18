//! The language registries must agree on what each kind is.
//!
//! Every layer that owns a registry answers per file kind, and each of them
//! covers a kind independently: the syntax layer parses a `.kts` script, the
//! declaration layer does not lower one, and an unknown-language file reaches
//! the IDE's features (which find nothing) but no parser. A kind added to one
//! registry and forgotten in another would answer *nothing* somewhere instead
//! of failing, so this test pins the coverage table the layers rely on and the
//! extension round trip `LanguageKind::from_path` performs.
//!
//! The table is behaviour, not bookkeeping: each row is what the file kind
//! means to that layer today (see the module docs of each `lang.rs`).

use ide_db::base_db::LanguageKind;

/// One kind's coverage: which registries answer for it, layer by layer.
struct Coverage {
    kind: LanguageKind,
    syntax: bool,
    lowering: bool,
    types: bool,
    index: bool,
    ide: bool,
    diagnostics: bool,
}

const COVERAGE: &[Coverage] = &[
    Coverage {
        kind: LanguageKind::Java,
        syntax: true,
        lowering: true,
        types: true,
        index: true,
        ide: true,
        diagnostics: true,
    },
    Coverage {
        kind: LanguageKind::Kotlin,
        syntax: true,
        lowering: true,
        types: true,
        index: true,
        ide: true,
        diagnostics: true,
    },
    Coverage {
        // A `.kts` script is Kotlin to every layer that reads *source* — it is
        // parsed with the `script` production, its type layer and IDE features
        // are Kotlin's — and its top-level statements lower as the body of the
        // implicit `main`
        // (<https://kotlinlang.org/docs/command-line.html#run-scripts>).
        kind: LanguageKind::KotlinScript,
        syntax: true,
        lowering: true,
        types: true,
        index: true,
        ide: true,
        diagnostics: true,
    },
    Coverage {
        // A file with no source root yet, or a non-JVM file: nothing parses it
        // and nothing lowers it; the IDE features and the diagnostics answer
        // through the Java paths (which find nothing in its empty model).
        kind: LanguageKind::Unknown,
        syntax: false,
        lowering: false,
        types: false,
        index: false,
        ide: true,
        diagnostics: true,
    },
];

#[test]
fn every_registry_covers_the_kinds_the_layers_rely_on() {
    for row in COVERAGE {
        let kind = row.kind;
        assert_eq!(
            syntax::lang::for_kind(kind).is_some(),
            row.syntax,
            "syntax: {kind:?}"
        );
        assert_eq!(
            hir::hir_def::lang::lowering(kind).is_some(),
            row.lowering,
            "hir-def lowering: {kind:?}"
        );
        assert_eq!(
            hir_ty::lang::types(kind).is_some(),
            row.types,
            "hir-ty types: {kind:?}"
        );
        assert_eq!(
            hir::lang::file_index(kind).is_some(),
            row.index,
            "hir file index: {kind:?}"
        );
        assert_eq!(ide::lang::ide(kind).is_some(), row.ide, "ide: {kind:?}");
    }

    let diagnostics = ide_diagnostics::registered_language_kinds();
    for row in COVERAGE {
        assert_eq!(
            diagnostics.contains(&row.kind),
            row.diagnostics,
            "ide-diagnostics: {:?}",
            row.kind
        );
    }
}

#[test]
fn the_registries_answer_the_same_kind_they_are_keyed_by() {
    for row in COVERAGE {
        let kind = row.kind;
        if let Some(syntax) = syntax::lang::for_kind(kind) {
            assert!(
                syntax.kinds().contains(&kind),
                "syntax answered {kind:?} without listing it"
            );
            // The parser reports the same kind back, so a layer that parses a
            // file can key its own lookup on the parse.
            assert_eq!(
                syntax::lang::language_id(kind),
                Some(syntax.name()),
                "the LSP languageId of {kind:?} is its name"
            );
        }
        if let Some(lowering) = hir::hir_def::lang::lowering(kind) {
            assert!(lowering.kinds().contains(&kind), "lowering: {kind:?}");
        }
        if let Some(types) = hir_ty::lang::types(kind) {
            assert!(types.kinds().contains(&kind), "types: {kind:?}");
        }
        if let Some(index) = hir::lang::file_index(kind) {
            assert!(index.kinds().contains(&kind), "file index: {kind:?}");
        }
        if let Some(ide) = ide::lang::ide(kind) {
            assert!(ide.kinds().contains(&kind), "ide: {kind:?}");
        }
    }
}

#[test]
fn a_owned_extension_round_trips_through_its_kind() {
    // Every extension the layers scan for resolves to the kind its language
    // claims it for: `.kts` is a script, `.kt` a Kotlin file, `.java` Java —
    // and each of those kinds is one a layer answers for, so a file found by
    // an extension allowlist is a file the layers can read.
    let mut extensions: Vec<&str> = syntax::lang::file_extensions().collect();
    extensions.sort_unstable();
    assert_eq!(extensions, ["java", "kt", "kts"]);

    for extension in extensions {
        let path = format!("Fixture.{extension}");
        let kind = LanguageKind::from_path(&path);
        assert_ne!(kind, LanguageKind::Unknown, "{path}");
        assert!(
            syntax::lang::for_kind(kind).is_some(),
            "no language parses {path}"
        );
        assert!(
            hir_ty::lang::types(kind).is_some(),
            "no type layer reads {path}"
        );
        assert!(
            ide::lang::ide(kind).is_some(),
            "no IDE features read {path}"
        );
    }

    // A path no language owns stays unknown, and no registry answers it.
    let kind = LanguageKind::from_path("Fixture.scala");
    assert_eq!(kind, LanguageKind::Unknown);
    assert!(syntax::lang::for_kind(kind).is_none());
}
