//! Semantic tokens: the legend the server advertises in `initialize` and the
//! encoder that turns `ide`'s per-file [`Highlight`]s into the flat,
//! delta-encoded `SemanticTokens` wire format.

use lsp_types::{
    SemanticToken, SemanticTokenModifiers, SemanticTokenTypes, SemanticTokensEdit,
    SemanticTokensLegend,
};
use rowan::TextSize;
use rustc_hash::FxHashMap;
use vfs::FileId;

use crate::line_index::LineIndex;
use crate::lsp::to_proto;
use ide::{Highlight, HlTag};

/// The token types of the legend, **in wire-index order**: the position of an
/// entry is the `token_type` a client looks a token up by, so the order is part
/// of the protocol. [`token_type`] maps a tag to its index; the tests at the
/// bottom pin the correspondence.
const TOKEN_TYPES: &[SemanticTokenTypes] = &[
    SemanticTokenTypes::Namespace,
    SemanticTokenTypes::Type,
    SemanticTokenTypes::Class,
    SemanticTokenTypes::Enum,
    SemanticTokenTypes::Interface,
    SemanticTokenTypes::Struct,
    SemanticTokenTypes::TypeParameter,
    SemanticTokenTypes::Parameter,
    SemanticTokenTypes::Variable,
    SemanticTokenTypes::Property,
    SemanticTokenTypes::EnumMember,
    SemanticTokenTypes::Function,
    SemanticTokenTypes::Method,
    SemanticTokenTypes::Keyword,
    SemanticTokenTypes::Modifier,
    SemanticTokenTypes::Comment,
    SemanticTokenTypes::String,
    SemanticTokenTypes::Number,
    SemanticTokenTypes::Operator,
    SemanticTokenTypes::Decorator,
];

/// The token modifiers of the legend, **in bitset order**: bit `n` is the
/// modifier at index `n`, which is exactly [`HlMods`]' flag order — so the wire
/// bitset is `mods.bits()` and nothing has to be remapped. Asserted by the
/// tests at the bottom.
const TOKEN_MODIFIERS: &[SemanticTokenModifiers] = &[
    SemanticTokenModifiers::Declaration,
    SemanticTokenModifiers::Readonly,
    SemanticTokenModifiers::Static,
    SemanticTokenModifiers::Abstract,
    SemanticTokenModifiers::Modification,
];

/// The legend advertised in `initialize`.
pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.iter().cloned().map(String::from).collect(),
        token_modifiers: TOKEN_MODIFIERS.iter().cloned().map(String::from).collect(),
    }
}

/// The wire index of a tag — its position in [`TOKEN_TYPES`].
fn token_type(tag: HlTag) -> u32 {
    match tag {
        HlTag::Namespace => 0,
        HlTag::Type => 1,
        HlTag::Class => 2,
        HlTag::Enum => 3,
        HlTag::Interface => 4,
        HlTag::Struct => 5,
        HlTag::TypeParameter => 6,
        HlTag::Parameter => 7,
        HlTag::Variable => 8,
        HlTag::Property => 9,
        HlTag::EnumMember => 10,
        HlTag::Function => 11,
        HlTag::Method => 12,
        HlTag::Keyword => 13,
        HlTag::Modifier => 14,
        HlTag::Comment => 15,
        HlTag::String => 16,
        HlTag::Number => 17,
        HlTag::Operator => 18,
        HlTag::Decorator => 19,
    }
}

/// Encodes one file's highlights as LSP semantic tokens: delta-encoded against
/// the previous token, one token per line piece.
///
/// The wire format cannot express a token spanning a line break, so a
/// multi-line token (a block comment, a text block) becomes one token per line,
/// each ending where the line ends — the line's `\n` is never inside a token.
pub(crate) fn highlights_to_tokens(
    highlights: Vec<Highlight>,
    line_index: &LineIndex,
) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(highlights.len());
    // The position of the last emitted token, and the offset the last highlight
    // ended at. Overlapping highlights are dropped defensively — the passes are
    // designed not to produce any, but a client cannot represent them.
    let mut prev: Option<(u32, u32)> = None;
    let mut prev_end: Option<TextSize> = None;
    for highlight in highlights {
        let range = highlight.range;
        if prev_end.is_some_and(|end| range.start() < end) {
            continue;
        }
        prev_end = Some(range.end());
        for piece in line_index.index.lines(range) {
            let start = to_proto::position(line_index, piece.start());
            // `LineIndex::lines` cuts a range at *line starts*, so a piece only
            // ends where the next line begins when the line break that ends its
            // own line is inside the piece. The token then ends where that
            // break sits — the end of the piece's line — and never on the next
            // line, whose column 0 would be an empty token.
            let end = to_proto::position(line_index, piece.end());
            let end = match end.character {
                0 if piece.end() > piece.start() => {
                    to_proto::position(line_index, piece.end() - TextSize::from(1))
                }
                _ => end,
            };
            // An empty line inside the range has no character to colour.
            if end.character <= start.character {
                continue;
            }
            let (line, character) = (start.line, start.character);
            let (delta_line, delta_start) = match prev {
                Some((prev_line, _)) if prev_line != line => (line - prev_line, character),
                Some((_, prev_start)) => (0, character - prev_start),
                None => (line, character),
            };
            out.push(SemanticToken {
                delta_line,
                delta_start,
                length: end.character - character,
                token_type: token_type(highlight.tag),
                token_modifiers_bitset: u32::from(highlight.mods.bits()),
            });
            prev = Some((line, character));
        }
    }
    out
}

/// The token stream this server last sent for a document, keyed by the
/// `result_id` it labelled it with — the state a
/// `textDocument/semanticTokens/full/delta` request diffs against.
///
/// An id is a per-server counter, not a content fingerprint: the client echoes
/// the id of the last response it received for the document, so an id this
/// server never handed out — a restarted server, an answer that was dropped
/// mid-flight, a document never requested — matches nothing and that request is
/// answered in full. The ids of two documents are distinct, so a stream can
/// never be diffed against another document's.
#[derive(Default)]
pub(crate) struct DeltaCache {
    next_id: u64,
    documents: FxHashMap<FileId, DocumentTokens>,
}

struct DocumentTokens {
    result_id: String,
    tokens: Vec<SemanticToken>,
}

impl DeltaCache {
    /// Records `tokens` as the document's current stream and labels it with a
    /// fresh result id.
    pub(crate) fn store(&mut self, file_id: FileId, tokens: Vec<SemanticToken>) -> String {
        self.next_id += 1;
        let result_id = self.next_id.to_string();
        self.documents.insert(
            file_id,
            DocumentTokens {
                result_id: result_id.clone(),
                tokens,
            },
        );
        result_id
    }

    /// The stream the client's `previous_result_id` names for `file_id`, or
    /// `None` when this server did not send that id for this document (so no
    /// diff can be computed against it).
    pub(crate) fn previous(
        &self,
        file_id: FileId,
        previous_result_id: &str,
    ) -> Option<&[SemanticToken]> {
        let document = self.documents.get(&file_id)?;
        (document.result_id == previous_result_id).then(|| document.tokens.as_slice())
    }

    /// Forgets the document's stream: its client closed it, so the next request
    /// for it is a full one anyway.
    pub(crate) fn forget(&mut self, file_id: FileId) {
        self.documents.remove(&file_id);
    }
}

/// The one edit that turns `old` into `new`, or no edit at all when the two
/// streams are equal.
///
/// `start` and `delete_count` index the flat five-`u32`-per-token `data` array,
/// so both are multiples of five: the difference is taken over *whole tokens*
/// (a shared prefix and a shared suffix), which also keeps the tokens of the
/// untouched head and tail byte-identical — a later token's `delta_line` /
/// `delta_start` only encode correctly relative to its predecessor.
pub(crate) fn delta_edits(old: &[SemanticToken], new: &[SemanticToken]) -> Vec<SemanticTokensEdit> {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    if prefix + suffix == old.len() && prefix + suffix == new.len() {
        return Vec::new();
    }
    let mut data = Vec::with_capacity((new.len() - prefix - suffix) * 5);
    for token in &new[prefix..new.len() - suffix] {
        data.extend_from_slice(&[
            token.delta_line,
            token.delta_start,
            token.length,
            token.token_type,
            token.token_modifiers_bitset,
        ]);
    }
    vec![SemanticTokensEdit {
        start: (prefix * 5) as u32,
        delete_count: ((old.len() - prefix - suffix) * 5) as u32,
        data: (!data.is_empty()).then_some(data),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line_index::{LineEndings, PositionEncoding};
    use ide::HlMods;
    use rowan::TextRange;
    use triomphe::Arc;

    /// A line index over `text`, in the encoding the tests assert against.
    fn index(text: &str) -> LineIndex {
        LineIndex {
            index: Arc::new(ide::LineIndex::new(text)),
            endings: LineEndings::Unix,
            encoding: PositionEncoding::Utf8,
        }
    }

    fn highlight(text: &str, needle: &str, tag: HlTag) -> Highlight {
        let start = text.find(needle).expect("needle in text") as u32;
        Highlight {
            range: TextRange::new(
                TextSize::from(start),
                TextSize::from(start + needle.len() as u32),
            ),
            tag,
            mods: HlMods::empty(),
        }
    }

    /// The wire tokens decoded back into absolute rows — the client's own decode
    /// rule, so a wrong delta fails here.
    fn decode(tokens: &[SemanticToken]) -> Vec<(u32, u32, u32, u32, u32)> {
        let mut line = 0;
        let mut character = 0;
        tokens
            .iter()
            .map(|token| {
                if token.delta_line == 0 {
                    character += token.delta_start;
                } else {
                    line += token.delta_line;
                    character = token.delta_start;
                }
                (
                    line,
                    character,
                    token.length,
                    token.token_type,
                    token.token_modifiers_bitset,
                )
            })
            .collect()
    }

    /// The text a row covers — `(line, character)` and the length are byte
    /// offsets here because the fixtures are ASCII.
    fn covered<'a>(text: &'a str, row: (u32, u32, u32, u32, u32)) -> &'a str {
        let mut offset = 0;
        for _ in 0..row.0 {
            offset = text[offset..].find('\n').expect("line") + offset + 1;
        }
        let start = offset + row.1 as usize;
        &text[start..start + row.2 as usize]
    }

    #[test]
    fn same_line_deltas() {
        let text = "class A";
        let tokens = highlights_to_tokens(
            vec![
                highlight(text, "class", HlTag::Keyword),
                highlight(text, "A", HlTag::Class),
            ],
            &index(text),
        );
        assert_eq!(
            decode(&tokens),
            vec![
                (0, 0, 5, 13, 0), // `class`
                (0, 6, 1, 2, 0),  // `A`, six columns after the previous token's start
            ]
        );
    }

    #[test]
    fn later_line_deltas_are_absolute() {
        let text = "class A {\n    void m() {}\n}";
        let tokens = highlights_to_tokens(
            vec![
                highlight(text, "A", HlTag::Class),
                highlight(text, "void", HlTag::Keyword),
            ],
            &index(text),
        );
        assert_eq!(
            decode(&tokens),
            vec![
                (0, 6, 1, 2, 0),
                // A later line: `delta_line` steps, `delta_start` is absolute.
                (1, 4, 4, 13, 0),
            ]
        );
        assert_eq!(tokens[1].delta_line, 1);
        assert_eq!(tokens[1].delta_start, 4);
    }

    #[test]
    fn multi_line_token_is_split_per_line() {
        let text = "a\n/* x\n   y\n z */\nb";
        let tokens = highlights_to_tokens(
            vec![highlight(text, "/* x\n   y\n z */", HlTag::Comment)],
            &index(text),
        );
        let rows = decode(&tokens);
        assert_eq!(rows.len(), 3, "one token per line: {rows:?}");
        assert_eq!(
            rows.iter()
                .map(|row| covered(text, *row))
                .collect::<Vec<_>>(),
            vec!["/* x", "   y", " z */"]
        );
        assert_eq!(
            rows.iter().map(|row| row.2).collect::<Vec<_>>(),
            vec![4, 4, 5],
            "no line break is inside a token"
        );
    }

    #[test]
    fn a_token_ending_at_a_line_start_emits_no_empty_token() {
        let text = "a\n// x\nb";
        // The comment ends where the newline ends; the empty piece after it is
        // not a token.
        let tokens =
            highlights_to_tokens(vec![highlight(text, "// x", HlTag::Comment)], &index(text));
        assert_eq!(decode(&tokens), vec![(1, 0, 4, 15, 0)]);
    }

    #[test]
    fn overlapping_highlights_are_dropped() {
        let text = "class A";
        let mut class = highlight(text, "class", HlTag::Keyword);
        class.mods = HlMods::DECLARATION;
        // Same start, longer range: the passes never produce this, and the
        // encoder drops the second rather than emit an overlapping pair.
        let overlapping = Highlight {
            range: TextRange::new(TextSize::from(0), TextSize::from(7)),
            tag: HlTag::Class,
            mods: HlMods::empty(),
        };
        let tokens = highlights_to_tokens(vec![class, overlapping], &index(text));
        assert_eq!(decode(&tokens), vec![(0, 0, 5, 13, 1)]);
    }

    #[test]
    fn legend_covers_every_tag_and_modifier() {
        let expected = [
            (HlTag::Namespace, SemanticTokenTypes::Namespace),
            (HlTag::Type, SemanticTokenTypes::Type),
            (HlTag::Class, SemanticTokenTypes::Class),
            (HlTag::Enum, SemanticTokenTypes::Enum),
            (HlTag::Interface, SemanticTokenTypes::Interface),
            (HlTag::Struct, SemanticTokenTypes::Struct),
            (HlTag::TypeParameter, SemanticTokenTypes::TypeParameter),
            (HlTag::Parameter, SemanticTokenTypes::Parameter),
            (HlTag::Variable, SemanticTokenTypes::Variable),
            (HlTag::Property, SemanticTokenTypes::Property),
            (HlTag::EnumMember, SemanticTokenTypes::EnumMember),
            (HlTag::Function, SemanticTokenTypes::Function),
            (HlTag::Method, SemanticTokenTypes::Method),
            (HlTag::Keyword, SemanticTokenTypes::Keyword),
            (HlTag::Modifier, SemanticTokenTypes::Modifier),
            (HlTag::Comment, SemanticTokenTypes::Comment),
            (HlTag::String, SemanticTokenTypes::String),
            (HlTag::Number, SemanticTokenTypes::Number),
            (HlTag::Operator, SemanticTokenTypes::Operator),
            (HlTag::Decorator, SemanticTokenTypes::Decorator),
        ];
        for (tag, want) in expected {
            let index = token_type(tag) as usize;
            assert_eq!(TOKEN_TYPES[index], want, "legend drift for {tag:?}");
        }
        assert_eq!(
            TOKEN_MODIFIERS,
            [
                SemanticTokenModifiers::Declaration,
                SemanticTokenModifiers::Readonly,
                SemanticTokenModifiers::Static,
                SemanticTokenModifiers::Abstract,
                SemanticTokenModifiers::Modification,
            ]
        );
    }

    #[test]
    fn declared_readonly_is_the_first_two_bits() {
        let text = "final int x";
        let mut highlight = highlight(text, "x", HlTag::Property);
        highlight.mods = HlMods::DECLARATION | HlMods::READONLY;
        let tokens = highlights_to_tokens(vec![highlight], &index(text));
        assert_eq!(tokens[0].token_modifiers_bitset, 0b11);
    }

    #[test]
    fn a_multi_line_token_starting_after_a_line_start_keeps_its_column() {
        // The first piece keeps the piece's real column; only later pieces step
        // to their own line.
        let text = "class A {\n  /* x\n     y */\n}";
        let tokens = highlights_to_tokens(
            vec![highlight(text, "/* x\n     y */", HlTag::Comment)],
            &index(text),
        );
        let rows = decode(&tokens);
        assert_eq!(
            rows.iter()
                .map(|row| covered(text, *row))
                .collect::<Vec<_>>(),
            vec!["/* x", "     y */"]
        );
        assert_eq!(rows[0].1, 2, "the first piece keeps its column");
    }

    /// The flat `data` array of a stream — the client's own view of it.
    fn flatten(tokens: &[SemanticToken]) -> Vec<u32> {
        tokens
            .iter()
            .flat_map(|token| {
                [
                    token.delta_line,
                    token.delta_start,
                    token.length,
                    token.token_type,
                    token.token_modifiers_bitset,
                ]
            })
            .collect()
    }

    /// Applies `edits` the way a client does: at each `start`, delete
    /// `delete_count` elements and splice in `data`.
    fn apply(flat: &[u32], edits: &[SemanticTokensEdit]) -> Vec<u32> {
        let mut flat = flat.to_vec();
        for edit in edits {
            let start = edit.start as usize;
            flat.splice(
                start..start + edit.delete_count as usize,
                edit.data.clone().unwrap_or_default(),
            );
        }
        flat
    }

    /// Every edit list the encoder produces must turn the client's array into
    /// the new one — that is the whole contract of the delta.
    fn assert_round_trips(old: &[SemanticToken], new: &[SemanticToken]) {
        let edits = delta_edits(old, new);
        assert_eq!(
            apply(&flatten(old), &edits),
            flatten(new),
            "the delta does not reproduce the stream: {edits:?}"
        );
    }

    /// `(start, delete_count, inserted length)` of each edit — the shape of a
    /// delta, without restating the token values.
    fn shape(edits: &[SemanticTokensEdit]) -> Vec<(u32, u32, Option<usize>)> {
        edits
            .iter()
            .map(|edit| {
                (
                    edit.start,
                    edit.delete_count,
                    edit.data.as_ref().map(Vec::len),
                )
            })
            .collect()
    }

    /// The tokens of `text`'s `needles`, each as the given tag.
    fn tokens_of(text: &str, needles: &[(&str, HlTag)]) -> Vec<SemanticToken> {
        highlights_to_tokens(
            needles
                .iter()
                .map(|(needle, tag)| highlight(text, needle, *tag))
                .collect(),
            &index(text),
        )
    }

    #[test]
    fn an_unchanged_stream_needs_no_edit() {
        let text = "class A { int x; }";
        let tokens = tokens_of(text, &[("class", HlTag::Keyword), ("A", HlTag::Class)]);
        assert!(delta_edits(&tokens, &tokens).is_empty());
    }

    #[test]
    fn appending_a_token_needs_one_insertion() {
        let before = "class A {\n    int x;\n}";
        let after = "class A {\n    int x;\n    int y;\n}";
        let old = tokens_of(before, &[("class", HlTag::Keyword), ("x", HlTag::Property)]);
        let new = tokens_of(
            after,
            &[
                ("class", HlTag::Keyword),
                ("x", HlTag::Property),
                ("y", HlTag::Property),
            ],
        );
        assert_round_trips(&old, &new);
        // Token deltas are positions relative to their predecessor, so the
        // tokens before the appended line are byte-identical and the edit is a
        // pure insertion at its end.
        assert_eq!(shape(&delta_edits(&old, &new)), vec![(10, 0, Some(5))]);
    }

    #[test]
    fn a_retagged_token_is_replaced_and_the_tail_is_kept() {
        let text = "class A { int x; int y; }";
        let old = tokens_of(
            text,
            &[
                ("class", HlTag::Keyword),
                ("A", HlTag::Class),
                ("x", HlTag::Property),
                ("y", HlTag::Property),
            ],
        );
        let new = tokens_of(
            text,
            &[
                ("class", HlTag::Keyword),
                ("A", HlTag::Class),
                ("x", HlTag::Variable),
                ("y", HlTag::Property),
            ],
        );
        assert_round_trips(&old, &new);
        // `x` alone changes; `y`'s deltas are relative to `x` and survive, so
        // the shared suffix stays out of the edit.
        assert_eq!(shape(&delta_edits(&old, &new)), vec![(10, 5, Some(5))]);
    }

    #[test]
    fn emptying_and_filling_a_stream() {
        let text = "class A { int x; }";
        let tokens = tokens_of(text, &[("class", HlTag::Keyword), ("x", HlTag::Property)]);
        // Every token gone: one deletion, nothing inserted.
        assert_eq!(shape(&delta_edits(&tokens, &[])), vec![(0, 10, None)]);
        assert_round_trips(&tokens, &[]);
        // And the other way around: everything inserted at index 0.
        assert_eq!(shape(&delta_edits(&[], &tokens)), vec![(0, 0, Some(10))]);
        assert_round_trips(&[], &tokens);
    }
}
