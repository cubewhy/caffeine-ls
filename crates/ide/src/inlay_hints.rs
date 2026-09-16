//! Inlay hints — the IDE-side model behind the LSP `textDocument/inlayHint`
//! and `inlayHint/resolve` requests.
//!
//! An [`InlayHint`] is one piece of text a client renders *inside* a document
//! at an offset — the inferred type of a `var` local, the type of a lambda
//! parameter or of a method-chain call, the name of a method argument. None of
//! it is part of the source: the model carries the text, the offset it is
//! anchored at and the padding a client needs to lay it out.
//!
//! The four categories are ported from IntelliJ's Java inlay-hint providers and
//! selected by [`InlayHintsConfig`]; every one of them is on by default, as
//! IntelliJ ships them. Java is the implemented language ([`java`]); Kotlin has
//! no HIR yet and answers nothing ([`kotlin`]).
//!
//! The hints come from the inference the type layer already recorded for a body
//! ([`hir_ty::BodyTypes`]) plus the ranges of the body IR
//! ([`hir_expand::body::BodyTree`]), with the parser's CST consulted only for
//! the `var` keyword an edit replaces. A resolve ([`inlay_hint_resolve`]) adds
//! what a client asks for only on demand: the tooltip, the edits accepting the
//! hint applies, and — through every part's [`InlayHintLabelPart::class`] — the
//! declaration a click on a rendered class name navigates to.

use rowan::{TextRange, TextSize};
use smol_str::SmolStr;
use vfs::FileId;

use crate::RootDatabase;

pub mod java;
pub mod kotlin;

/// The inlay-hint categories the server renders. Every category is on by
/// default, as IntelliJ ships them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlayHintsConfig {
    /// The inferred type of a `var` local variable.
    pub var_types: bool,
    /// The inferred type of a lambda parameter written without one.
    pub lambda_parameter_types: bool,
    /// A method's parameter name at the argument it is passed.
    pub parameter_names: bool,
    /// The type of each intermediate call of a multi-line method chain.
    pub method_chains: bool,
}

impl Default for InlayHintsConfig {
    fn default() -> Self {
        Self {
            var_types: true,
            lambda_parameter_types: true,
            parameter_names: true,
            method_chains: true,
        }
    }
}

/// One hint: a label anchored at an offset of one file, with the padding a
/// client renders around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub offset: TextSize,
    pub label: Vec<InlayHintLabelPart>,
    pub kind: InlayHintKind,
    /// Render padding before the hint (a space the editor's background fills).
    pub padding_left: bool,
    /// Render padding after the hint.
    pub padding_right: bool,
}

/// One piece of a hint's label. Splitting a label into parts is what makes a
/// rendered class name clickable: a part that names one carries the canonical
/// name, and the resolve step turns that into its declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHintLabelPart {
    pub value: String,
    /// The canonical name of the class this part renders, when it renders one:
    /// the resolve step turns it into the declaration a click navigates to.
    pub class: Option<SmolStr>,
}

/// The kind of a hint — the LSP `InlayHintKind` of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlayHintKind {
    /// A type hint: an inferred type rendered where the source writes none.
    Type,
    /// A parameter hint: a parameter's name rendered at its argument.
    Parameter,
}

/// Everything an `inlayHint/resolve` answer adds: the tooltip and the edits a
/// client applies when the hint is accepted, over the hint as first sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHintDetail {
    pub hint: InlayHint,
    pub tooltip: String,
    pub edits: Vec<InlayHintEdit>,
}

/// An edit accepting a hint: the range of the source it replaces (empty for an
/// insertion) and the text to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHintEdit {
    pub range: TextRange,
    pub new_text: String,
}

/// The file's hints whose offset `range` contains, sorted by offset.
pub fn inlay_hints(
    db: &RootDatabase,
    file: FileId,
    range: TextRange,
    config: &InlayHintsConfig,
) -> Vec<InlayHint> {
    crate::lang::for_file(db, file).map_or_else(Default::default, |ide| {
        ide.inlay_hints(db, file, range, config)
    })
}

/// The library files the hints over `range` need loaded before their parameter
/// names can be rendered: the declaring sources of the library members their
/// invocations selected, where the source is in the library's archive but not
/// materialized. The LSP layer materializes them and re-runs the request — the
/// same deferral goto-definition and hover drive — so a library member's names
/// render on the first request rather than only once its source happens to be
/// open.
///
/// Empty for a Kotlin file ([`kotlin`]) and for a request the parameter-name
/// category is off for.
pub fn pending_library_files(
    db: &RootDatabase,
    file: FileId,
    range: TextRange,
    config: &InlayHintsConfig,
) -> Vec<crate::nav::LibraryFileRef> {
    crate::lang::for_file(db, file).map_or_else(Default::default, |ide| {
        ide.inlay_hint_pending_library_files(db, file, range, config)
    })
}

/// The one hint a resolve names, with its deferred detail. `None` when no hint
/// is anchored at `offset` with `kind` — the document changed under a hint the
/// client still holds.
pub fn inlay_hint_resolve(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
    kind: InlayHintKind,
    config: &InlayHintsConfig,
) -> Option<InlayHintDetail> {
    crate::lang::for_file(db, file)
        .and_then(|ide| ide.inlay_hint_resolve(db, file, offset, kind, config))
}
