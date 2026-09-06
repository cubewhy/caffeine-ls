#![allow(unused)]
use base_db::LanguageKind;

pub fn check_lower(src: &str) -> String {
    check_lower_language(LanguageKind::Java, src)
}

pub fn check_lower_language(language: LanguageKind, src: &str) -> String {
    let parse = syntax::SourceFile::parse(language, src);
    let source = parse.syntax_node(language);
    let map = hir_expand::ast_id_map::AstIdMap::from_source_file(&source);
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

    let lowered = hir_def::lower_source(language, src, &map);
    let rendered = hir_def::java::pretty::pretty_print(&lowered.items, &map, &source);

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
    let source = parse.syntax_node(LanguageKind::Java);
    let map = hir_expand::ast_id_map::AstIdMap::from_source_file(&source);
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

    let lowered = hir_def::lower_source(LanguageKind::Java, src, &map);
    let rendered = hir_def::java::pretty::pretty_print(&lowered.items, &map, &source);
    let bodies = hir_def::java::pretty::pretty_body(&lowered.items, &lowered.bodies);

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
