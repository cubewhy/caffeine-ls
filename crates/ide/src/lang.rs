//! The language registry of the IDE layer: one registration per language for
//! the features the IDE answers per file (IntelliJ: the per-feature extension
//! points of one language, gathered into one facade because this crate's
//! modules are already grouped per feature and per language).
//!
//! A language reaches another only through [`for_file`]: a cross-language
//! hover, or a definition whose target is written in another language, asks
//! the *target* file's language — keyed by that file, never by a language name.

use triomphe::Arc;

use ide_db::RootDatabase;
use ide_db::base_db::LanguageKind;
use rowan::{TextRange, TextSize};
use vfs::FileId;

use crate::{
    highlight, inlay_hints,
    inlay_hints::{InlayHintDetail, InlayHintKind, InlayHintsConfig},
    nav, symbols,
};

/// The IDE features of one language.
pub trait LanguageIde: Sync {
    /// The kinds this implementation answers for.
    fn kinds(&self) -> &'static [LanguageKind];

    /// The declarations the reference at `offset` resolves to.
    fn definition(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
    ) -> Vec<nav::NavigationTarget>;

    /// The reference sites of the declaration(s) the reference at `offset`
    /// names.
    fn references(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
        include_declaration: bool,
    ) -> Vec<nav::ReferenceTarget>;

    /// The library files the reference at `offset` resolves into but which are
    /// not loaded into the database yet.
    fn pending_library_files(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
    ) -> Vec<nav::LibraryFileRef>;

    /// The hover at `offset`.
    fn hover(&self, db: &RootDatabase, file: FileId, offset: TextSize) -> Option<nav::HoverInfo>;

    /// The documentation of declaration `item`, rendered as Markdown.
    fn hover_docs(&self, db: &RootDatabase, file: FileId, item: symbols::ItemId) -> Option<String>;

    /// The declaration of the class-like type `fqn` names in `file`'s scope.
    fn class_declaration(
        &self,
        db: &RootDatabase,
        file: FileId,
        fqn: &str,
    ) -> Option<nav::NavigationTarget>;

    /// The parameter names the callable `method` selected at a call site
    /// writes, for a caller that has none of its own.
    fn declared_parameter_names(
        &self,
        db: &RootDatabase,
        file: FileId,
        method: &hir_ty::MethodData,
        constructor: bool,
    ) -> Option<Vec<String>> {
        let _ = (db, file, method, constructor);
        None
    }

    /// The library file that has to be loaded before those names can be read.
    fn pending_parameter_names(
        &self,
        db: &RootDatabase,
        file: FileId,
        method: &hir_ty::MethodData,
        constructor: bool,
    ) -> Option<nav::LibraryFileRef> {
        let _ = (db, file, method, constructor);
        None
    }

    /// The semantic highlighting of the file, sorted by range start.
    fn highlight(&self, db: &RootDatabase, file: FileId) -> Vec<highlight::Highlight>;

    /// The file's inlay hints whose offset `range` contains, sorted by offset.
    fn inlay_hints(
        &self,
        db: &RootDatabase,
        file: FileId,
        range: TextRange,
        config: &InlayHintsConfig,
    ) -> Vec<inlay_hints::InlayHint>;

    /// The library files those hints need loaded before their parameter names
    /// can be rendered.
    fn inlay_hint_pending_library_files(
        &self,
        db: &RootDatabase,
        file: FileId,
        range: TextRange,
        config: &InlayHintsConfig,
    ) -> Vec<nav::LibraryFileRef> {
        let _ = (db, file, range, config);
        Vec::new()
    }

    /// The one hint a resolve names, with its deferred detail.
    fn inlay_hint_resolve(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
        kind: InlayHintKind,
        config: &InlayHintsConfig,
    ) -> Option<InlayHintDetail>;

    /// The file's document symbols: the declarations the outline shows.
    fn document_symbols(&self, db: &RootDatabase, file: FileId) -> Vec<symbols::DocumentSymbol>;

    /// The synthesized package symbol of the file, name and range.
    fn package_symbol(&self, db: &RootDatabase, file: FileId) -> symbols::DocumentSymbol;

    /// The range of the declaration a symbol names.
    fn source_symbol_range(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> Option<TextRange>;

    /// The range of the declaration's own *name* in `file` — the range a
    /// navigation target's selection points at.
    fn declaration_name_range(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> Option<TextRange>;

    /// The declared type of one declaration, rendered.
    fn item_ty(&self, db: &RootDatabase, file: FileId, item: symbols::ItemId) -> String;

    /// The parameter list of one callable, rendered.
    fn method_params(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> Arc<[String]>;
}

/// Every registered language, in lookup order.
static LANGUAGES: &[&dyn LanguageIde] =
    &[&crate::java::plugin::JAVA, &crate::kotlin::plugin::KOTLIN];

/// The IDE features of a file of `kind`.
pub fn ide(kind: LanguageKind) -> Option<&'static dyn LanguageIde> {
    LANGUAGES
        .iter()
        .copied()
        .find(|language| language.kinds().contains(&kind))
}

/// The IDE features of the language declaring `file`.
pub fn for_file(db: &RootDatabase, file: FileId) -> Option<&'static dyn LanguageIde> {
    // The file's *kind*, not its lowered model's: a `.kts` script declares
    // nothing and is still Kotlin to every feature above the declaration layer.
    let language = ide_db::base_db::file_language_kind(db, file)?;
    ide(language)
}
