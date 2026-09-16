//! The language registry of the file-index layer: one registration per language
//! for the file-level indexes the workspace carries (IntelliJ: the declarations
//! a language's files contribute to the `PsiFacade`-served lookup, and the docs
//! and package a file declares).
//!
//! The indexes are what a file *contributes* to the workspace — its symbols, its
//! doc comments, its package and its synthesized facade class — so they are
//! per-language and a lookup is keyed by the file's own kind. Nothing here names
//! a language: a consumer asks [`for_file`], and a new language adds one
//! registration per registry.

use rowan::TextRange;

use base_db::LanguageKind;
use hir_def::lang::Declarations;
use hir_expand::{ids::ItemId, name::Name};
use vfs::FileId;

use crate::db::HirDatabase;
use crate::symbol_index::SourceSymbol;

/// The file-level indexes one language contributes.
pub trait LanguageFileIndex: Sync {
    /// The kinds this implementation answers for.
    fn kinds(&self) -> &'static [LanguageKind];

    /// The source-set symbols the file contributes to the workspace index: the
    /// declarations nameable from another file, in source order.
    fn file_symbols(&self, db: &dyn HirDatabase, file: FileId) -> Vec<SourceSymbol>;

    /// The doc comments the file declares, as the declaration's item and the
    /// source range of its doc comment, in source order. The range — not the
    /// text — is what the index keeps: the text is sliced out of the file's
    /// resident text when a consumer asks for it.
    fn file_docs(&self, db: &dyn HirDatabase, file: FileId) -> Vec<(ItemId, TextRange)>;

    /// The package the file declares, for the package-file index. A language
    /// without a package declaration of its own answers `None`.
    fn file_package(&self, db: &dyn HirDatabase, file: FileId) -> Option<Name> {
        let _ = (db, file);
        None
    }

    /// The JVM facade class a compiler synthesizes for the file's top-level
    /// declarations (`FooKt`), `None` when the language has none.
    fn file_facade_class(&self, db: &dyn HirDatabase, file: FileId) -> Option<Name> {
        let _ = (db, file);
        None
    }
}

/// Every registered language, in lookup order.
static LANGUAGES: &[&dyn LanguageFileIndex] =
    &[&crate::java::plugin::JAVA, &crate::kotlin::plugin::KOTLIN];

/// The file index answering for a file of `kind`.
pub fn file_index(kind: LanguageKind) -> Option<&'static dyn LanguageFileIndex> {
    LANGUAGES
        .iter()
        .copied()
        .find(|language| language.kinds().contains(&kind))
}

/// The file index of the language declaring `file`.
pub fn for_file(db: &dyn HirDatabase, file: FileId) -> Option<&'static dyn LanguageFileIndex> {
    file_index(declarations(db, file).language())
}

/// The declaration model of `file`, for the language-agnostic layer.
pub fn declarations(db: &dyn HirDatabase, file: FileId) -> std::sync::Arc<dyn Declarations> {
    hir_def::lang::declarations(db, file)
}
