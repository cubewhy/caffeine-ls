#![allow(unused)]
use base_db::LanguageKind;

pub fn check_lower(src: &str) -> String {
    check_lower_language(LanguageKind::Java, src)
}

pub fn check_lower_language(language: LanguageKind, src: &str) -> String {
    let parse = syntax::SourceFile::parse(language, src);
    let errors: Vec<String> = parse
        .errors()
        .iter()
        .map(|error| format!("{} @{:?}", error.message, error.range))
        .collect();
    let errors = if errors.is_empty() {
        "<none>".to_owned()
    } else {
        errors.join("\n")
    };

    let lowered = hir_def::lower::lower_source(language, src);
    let rendered = hir_expand::pretty::pretty_print(&lowered.items);

    format!(
        "\
SOURCE:
{src}
PARSE_ERRORS:
{errors}
ITEM_TREE:
{rendered}"
    )
}

/// Like [`check_lower`], but also renders the lowered bodies of the file
/// (`ITEM_TREE` + `BODIES`).
pub fn check_lower_bodies(src: &str) -> String {
    let parse = syntax::SourceFile::parse(LanguageKind::Java, src);
    let errors: Vec<String> = parse
        .errors()
        .iter()
        .map(|error| format!("{} @{:?}", error.message, error.range))
        .collect();
    let errors = if errors.is_empty() {
        "<none>".to_owned()
    } else {
        errors.join("\n")
    };

    let lowered = hir_def::lower::lower_source(LanguageKind::Java, src);
    let rendered = hir_expand::pretty::pretty_print(&lowered.items);
    let bodies = hir_expand::pretty::pretty_body(&lowered.items, &lowered.bodies);

    format!(
        "\
SOURCE:
{src}
PARSE_ERRORS:
{errors}
ITEM_TREE:
{rendered}
BODIES:
{bodies}"
    )
}

macro_rules! lower_snapshot {
    ($name:ident, $src:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = crate::common::check_lower($src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

macro_rules! lower_snapshot_lang {
    ($name:ident, $language:expr, $src:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = crate::common::check_lower_language($language, $src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

macro_rules! body_snapshot {
    ($name:ident, $src:expr $(,)?) => {
        #[test]
        fn $name() {
            let out = crate::common::check_lower_bodies($src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

pub(crate) use body_snapshot;
pub(crate) use lower_snapshot;
pub(crate) use lower_snapshot_lang;
