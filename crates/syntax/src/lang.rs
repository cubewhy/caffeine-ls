//! The language registry of the syntax layer: one registration per language for
//! the things this layer owns — how a file parses, which files it owns, how its
//! syntax errors are coded (IntelliJ: `Language` + `LanguageFileType` +
//! `ParserDefinition`).
//!
//! A lookup key is the *kind of the file*, and an implementation lists every
//! kind it answers for, so [`LanguageKind::KotlinScript`] is answered by the
//! Kotlin implementation here while a layer that does not treat a script as its
//! language leaves it to nobody. Callers never name a language: they ask the
//! registry, which is what makes a third language a registration and not an
//! edit.

use rowan::GreenNode;

use crate::{LanguageKind, Parse, SourceFile};

mod java;
mod kotlin;

/// The syntax of one language: how it parses, what files it owns, how its
/// errors are coded (IntelliJ: `Language` + `ParserDefinition` +
/// `LanguageFileType`).
pub trait LanguageSyntax: Sync {
    /// The kinds this implementation answers for.
    fn kinds(&self) -> &'static [LanguageKind];
    /// The file extensions it owns, most specific first, each with the kind a
    /// file with that extension is: `.kts` before `.kt`, so that a script is
    /// not read as a Kotlin file.
    fn file_extensions(&self) -> &'static [(&'static str, LanguageKind)];
    /// The LSP `languageId` of its documents.
    fn language_id(&self) -> &'static str;
    /// The lowercase name the snapshot renderers spell it with.
    fn name(&self) -> &'static str;
    /// Parses `text` (`kind` selects the file/script production).
    fn parse(&self, kind: LanguageKind, text: &str) -> Parse;
    /// Re-attaches a cached green tree to this language's syntax node.
    fn syntax_node(&self, kind: LanguageKind, green: GreenNode) -> SourceFile;
}

/// Every registered language, in lookup order.
static LANGUAGES: &[&dyn LanguageSyntax] = &[&java::JAVA, &kotlin::KOTLIN];

pub fn languages() -> &'static [&'static dyn LanguageSyntax] {
    LANGUAGES
}

/// The implementation answering for a file of `kind`.
pub fn for_kind(kind: LanguageKind) -> Option<&'static dyn LanguageSyntax> {
    LANGUAGES
        .iter()
        .copied()
        .find(|language| language.kinds().contains(&kind))
}

/// The kind of the file at `path`, by its extension, `None` when no registered
/// language owns one.
pub fn for_path(path: &str) -> Option<LanguageKind> {
    // The suffix after the last dot, exactly what `ends_with(".<ext>")` tests,
    // without allocating the dotted form.
    let extension = path.rsplit_once('.')?.1;
    LANGUAGES.iter().find_map(|language| {
        language
            .file_extensions()
            .iter()
            .find(|(owned, _)| *owned == extension)
            .map(|(_, kind)| *kind)
    })
}

/// Every owned file extension, for the allowlists that scan a directory tree.
pub fn file_extensions() -> impl Iterator<Item = &'static str> {
    LANGUAGES
        .iter()
        .flat_map(|language| language.file_extensions())
        .map(|(extension, _)| *extension)
}

/// The LSP `languageId` of a document of `kind`.
pub fn language_id(kind: LanguageKind) -> Option<&'static str> {
    for_kind(kind).map(|language| language.language_id())
}

/// The kind of the language named `name` — the spelling [`LanguageSyntax::name`]
/// answers with, which is what the CLI accepts. A language answering for several
/// kinds is named by its first, the file production rather than the script, so
/// this is the inverse of [`LanguageKind::name`] only for those.
pub fn for_name(name: &str) -> Option<LanguageKind> {
    LANGUAGES
        .iter()
        .find(|language| language.name() == name)
        .and_then(|language| language.kinds().first().copied())
}

/// The kind a parsed file reports: the registry key it was parsed under, with a
/// script reported as its base language (a `.kts` file is written in Kotlin).
/// The wrapper is the sum type the rowan `Lang` needs, so it is where the
/// language of a syntax tree is recorded.
pub(crate) fn kind_of(file: &SourceFile) -> LanguageKind {
    match file {
        SourceFile::Java(_) => LanguageKind::Java,
        SourceFile::Kotlin(_) => LanguageKind::Kotlin,
    }
}
