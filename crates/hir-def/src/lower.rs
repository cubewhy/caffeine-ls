//! Entry point of lowering: turns the text of a file into its declaration
//! [`LoweredFile`], dispatching on the file's language.
//!
//! Lowering is a pure function of the file text, its parse and its
//! [`AstIdMap`], computed once per file by a salsa query
//! ([`crate::db::item_tree_query`] / [`crate::db::body_tree_query`]). Each
//! language lowers with its own context ([`crate::java::lower::LowerCtx`]) and
//! returns its own declaration model inside the neutral
//! [`FileItemTree`](crate::item_tree::FileItemTree) facade.

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::ast_id_map::AstIdMap;

use crate::item_tree::{FileItemTree, LoweredFile};

/// Lowers `text` for `language` into the file's item tree plus body IR,
/// anchoring every declaration to its syntax node through `map`.
///
/// A language with no lowering yet — and an unknown-language file — yields an
/// empty [`FileItemTree`](crate::item_tree::FileItemTree) that still carries
/// the file's language, so language-dispatched consumers route correctly.
pub fn lower_source(language: LanguageKind, text: &str, map: &AstIdMap) -> LoweredFile {
    match language {
        LanguageKind::Java => crate::java::lower::lower_java_source(text, map),
        LanguageKind::Kotlin => crate::kotlin::lower::lower_kotlin_source(text, map),
        // `.kts` scripts are not lowered yet: a script's top-level statements
        // declare no file item to hang off.
        LanguageKind::KotlinScript | LanguageKind::Unknown => LoweredFile {
            items: FileItemTree::Empty(language),
            bodies: Arc::default(),
        },
    }
}
