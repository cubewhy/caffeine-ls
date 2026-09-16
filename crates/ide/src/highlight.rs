//! Semantic highlighting — the IDE-side model behind the LSP
//! `textDocument/semanticTokens` requests.
//!
//! A [`Highlight`] is one token of one file: a byte range, a [`HlTag`]
//! (the token type) and a set of [`HlMods`] (the token modifiers). The LSP
//! layer (`caffeine_ls::lsp::semantic_tokens`) encodes them into the flat,
//! delta-encoded `SemanticTokens` wire format.
//!
//! Java identifiers come from the semantic model — the item tree's
//! declarations and the resolution the type layer recorded for the bodies —
//! with the parser's CST used only for what the HIR cannot represent
//! (keywords, modifiers, literals, operators, comments) and as a
//! declaration gap-fill. Kotlin has no HIR yet, so all of its tokens come from
//! the CST ([`kotlin`]).
//!
//! # Invariants
//!
//! Every pass obeys these; the wire encoder depends on them.
//!
//! * A range covers a **single token** or a single non-interpolated literal
//!   piece — never a node range containing other tagged tokens.
//! * Ranges never overlap. A rule that would overlap tags the leaf tokens
//!   instead (which is why Kotlin strings are tagged per quote/content token
//!   rather than per `STRING_LITERAL` node).
//! * The result is sorted by range start, which the [`BTreeMap`] gives for
//!   free — keying by range also makes a later pass override an earlier one at
//!   the same range, which is how an annotation name re-tagged as a decorator
//!   wins over the type reference that found it first.

use std::collections::BTreeMap;

use bitflags::bitflags;
use rowan::{TextRange, TextSize};
use vfs::FileId;

use crate::RootDatabase;

pub mod java;
pub mod kotlin;

/// One semantic token: a byte range of one file, its kind and its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Highlight {
    pub range: TextRange,
    pub tag: HlTag,
    pub mods: HlMods,
}

/// The kind of a semantic token — the client-facing token *type*. The LSP
/// legend ([`crate::Analysis::highlight`]'s consumer) maps each variant to the
/// token type of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlTag {
    Namespace,
    Type,
    Class,
    Enum,
    Interface,
    Struct,
    TypeParameter,
    Parameter,
    Variable,
    Property,
    EnumMember,
    Function,
    Method,
    Keyword,
    Modifier,
    Comment,
    String,
    Number,
    Operator,
    Decorator,
}

bitflags! {
    /// Semantic token modifiers. **The flag order is the modifier order of the
    /// LSP legend** (`crates/caffeine-ls/src/lsp/semantic_tokens.rs`), so the
    /// wire bitset is exactly `mods.bits()`; the legend invariant is asserted
    /// by that module's tests.
    ///
    /// Style mirror of `JvmAccessFlags` (`lib/rust-asm/src/constants.rs`):
    /// `u16` storage, no `Default` derive — the empty set is [`HlMods::empty`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct HlMods: u16 {
        const DECLARATION = 1 << 0;
        const READONLY = 1 << 1;
        const STATIC = 1 << 2;
        const ABSTRACT = 1 << 3;
        const MODIFICATION = 1 << 4;
    }
}

/// The per-file accumulator: keyed by the token's *start* offset — injective
/// because the ranges never overlap — so a later pass overrides an earlier one
/// at the same range and iteration order is the wire order.
///
/// (`rowan::TextRange` is not `Ord`, and a start-keyed map holds the invariant
/// defensively: a pass that did produce a second range at one start would
/// replace the first token instead of emitting an overlapping pair.)
pub(crate) type Highlights = BTreeMap<TextSize, Highlight>;

/// Records one token, dropping empty ranges (a synthesized declaration has no
/// token to color).
pub(crate) fn insert(map: &mut Highlights, range: TextRange, tag: HlTag, mods: HlMods) {
    if range.is_empty() {
        return;
    }
    map.insert(range.start(), Highlight { range, tag, mods });
}

/// Whether a token over `range` is already recorded — the guard of a pass that
/// only fills what the passes before it did not classify.
pub(crate) fn contains(map: &Highlights, range: TextRange) -> bool {
    map.contains_key(&range.start())
}

/// The semantic highlighting of the file, sorted by range start. Empty for a
/// file whose language the server cannot tell (no source root yet) and for an
/// unknown language.
pub fn highlight(db: &RootDatabase, file_id: FileId) -> Vec<Highlight> {
    crate::lang::for_file(db, file_id)
        .map_or_else(Default::default, |ide| ide.highlight(db, file_id))
}
