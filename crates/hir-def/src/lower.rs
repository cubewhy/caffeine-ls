//! Entry point of lowering: turns the text of a file into its declaration
//! [`LoweredFile`], dispatching on the file's language through [`crate::lang`].
//!
//! Lowering is a pure function of the file text, its parse and its
//! [`AstIdMap`], computed once per file by a salsa query
//! ([`crate::db::item_tree_query`] / [`crate::db::body_tree_query`]). Each
//! language lowers with its own context ([`crate::java::lower::LowerCtx`]) and
//! registers itself with the language's [`crate::lang::LangLowering`] implementation;
//! nothing here names a language.

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::ast_id_map::AstIdMap;

use crate::item_tree::{FileItemTree, LoweredFile};

/// Lowers `text` for `language` into the file's item tree plus body IR,
/// anchoring every declaration to its syntax node through `map`.
///
/// A language with no lowering — and an unknown-language file — yields an
/// empty [`FileItemTree`] that still carries the file's language, so
/// language-dispatched consumers route correctly.
pub fn lower_source(language: LanguageKind, text: &str, map: &AstIdMap) -> LoweredFile {
    let Some(lowering) = crate::lang::lowering(language) else {
        // `.kts` scripts are not lowered yet: a script's top-level statements
        // declare no file item to hang off.
        return LoweredFile {
            items: FileItemTree::empty(language),
            bodies: Arc::default(),
        };
    };
    lowering.lower(text, map)
}
