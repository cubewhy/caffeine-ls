//! A declaration's documentation, rendered as Markdown.
//!
//! Two halves: the renderer ([`render_javadoc`]), a pure function of the
//! comment text — the module's testable seam — and the lookup
//! ([`hover_docs`]), which ties it to the per-file doc-comment index of the
//! HIR layer ([`hir::item_doc`]): the index owns the *ranges*, the file's
//! `FileText` owns the text, and this module owns the conversion to the
//! Markdown an editor renders.

mod javadoc;
mod kdoc;

use hir::hir_def::jvm::ids::ItemId;
use vfs::FileId;

use crate::RootDatabase;

/// Renders the text of a doc-comment token (`/** … */` or a `///` run,
/// delimiters included) as Markdown, per the JDK 25 Javadoc specification.
pub fn render_javadoc(raw: &str) -> String {
    javadoc::render(raw, None)
}

/// [`render_javadoc`] for the comment of the declaration named `owner`: the
/// simple name a bare `{@value}` displays. Without it, that tag renders as the
/// text it is written as — the constant's value itself is never computed here.
pub fn render_javadoc_of(raw: &str, owner: &str) -> String {
    javadoc::render(raw, Some(owner))
}

/// The documentation of declaration `item` in `file`, rendered as Markdown.
/// `None` when it has no doc comment, or when the language's arm has none.
pub(crate) fn hover_docs(db: &RootDatabase, file: FileId, item: ItemId) -> Option<String> {
    crate::lang::for_file(db, file).and_then(|ide| ide.hover_docs(db, file, item))
}

/// The Javadoc of a Java declaration, rendered as Markdown.
pub(crate) fn javadoc_of(db: &RootDatabase, file: FileId, item: ItemId) -> Option<String> {
    let tree = hir::hir_def::java::plugin::tree(db, file);
    let raw = hir::item_doc(db, file, item)?;
    let owner = tree.data(item).name().map(|name| name.simple_name());
    let rendered = javadoc::render(raw, owner);
    // A comment whose every tag is tooling-only (`@author`) documents
    // nothing an editor can show.
    (!rendered.trim().is_empty()).then_some(rendered)
}

/// The KDoc of a Kotlin declaration, rendered as Markdown.
///
/// KDoc is not Javadoc: the same comment structure, a Markdown body and its own
/// tag set, so it renders through its own renderer.
pub(crate) fn kdoc_of(db: &RootDatabase, file: FileId, item: ItemId) -> Option<String> {
    let raw = hir::item_doc(db, file, item)?;
    // KDoc has no tag that displays the owner's name; the owner is passed for
    // symmetry with the JavaDoc renderer.
    let rendered = kdoc::render(raw, None);
    (!rendered.trim().is_empty()).then_some(rendered)
}

/// The `@param <name>` description of `item`'s doc comment — the documentation
/// a record component inherits from its record's doc comment, because the
/// Javadoc specification recognises no doc-comment position before a record
/// component of its own.
pub(crate) fn doc_param(
    db: &RootDatabase,
    file: FileId,
    item: ItemId,
    name: &str,
) -> Option<String> {
    javadoc::param(hir::item_doc(db, file, item)?, name)
}
