//! The LSP mapping of the inlay-hint model ([`ide::inlay_hints`]) onto the
//! `textDocument/inlayHint` and `inlayHint/resolve` requests.
//!
//! The model's labels are already part lists, so the first answer sends them
//! as-is; a resolve fills in what a client asks for only when it navigates or
//! hovers a hint — the tooltip, the declaration behind each label part that
//! rendered a class name, and the edits accepting the hint applies.
//!
//! The hint is *recomputed* on a resolve, not carried: the request's `data` is
//! the file, the anchor offset and the kind the analysis is asked about again,
//! so the resolved label can never disagree with the one the client holds.

use ide::InlayHintKind;
use lsp_types::{
    InlayHint, InlayHintKind as LspInlayHintKind, InlayHintLabelPart, Label, Location, TextEdit,
    Tooltip,
};
use vfs::FileId;

use crate::global_state::GlobalStateSnapshot;
use crate::line_index::LineIndex;
use crate::lsp::to_proto;

/// What `inlayHint/resolve` needs to recompute a hint: the file, the offset it
/// is anchored at and its kind.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct InlayHintData {
    pub(crate) file_id: u32,
    pub(crate) offset: u32,
    pub(crate) kind: InlayHintKindData,
}

/// The wire form of [`ide::InlayHintKind`], so the serialized kind and the
/// analysis' kind cannot drift: the two `From` impls below are the only
/// crossing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum InlayHintKindData {
    Type,
    Parameter,
}

impl From<InlayHintKindData> for InlayHintKind {
    fn from(kind: InlayHintKindData) -> Self {
        match kind {
            InlayHintKindData::Type => InlayHintKind::Type,
            InlayHintKindData::Parameter => InlayHintKind::Parameter,
        }
    }
}

impl From<InlayHintKind> for InlayHintKindData {
    fn from(kind: InlayHintKind) -> Self {
        match kind {
            InlayHintKind::Type => InlayHintKindData::Type,
            InlayHintKind::Parameter => InlayHintKindData::Parameter,
        }
    }
}

/// The first answer's form of one hint: the label, position and padding the
/// client renders, plus the `data` a later resolve names it by. The tooltip,
/// the label parts' locations and the edits are deliberately left to
/// [`resolve_to_proto`].
pub(crate) fn to_proto(
    hint: &ide::InlayHint,
    file_id: FileId,
    line_index: &LineIndex,
) -> InlayHint {
    InlayHint {
        position: to_proto::position(line_index, hint.offset),
        label: Label::InlayHintLabelPartList(
            hint.label
                .iter()
                .map(|part| InlayHintLabelPart {
                    value: part.value.clone(),
                    tooltip: None,
                    location: None,
                    command: None,
                })
                .collect(),
        ),
        kind: Some(match hint.kind {
            InlayHintKind::Type => LspInlayHintKind::Type,
            InlayHintKind::Parameter => LspInlayHintKind::Parameter,
        }),
        text_edits: None,
        tooltip: None,
        padding_left: Some(hint.padding_left),
        padding_right: Some(hint.padding_right),
        data: Some(
            serde_json::to_value(InlayHintData {
                file_id: file_id.index(),
                offset: hint.offset.into(),
                kind: hint.kind.into(),
            })
            .expect("an inlay hint's data is three integers"),
        ),
    }
}

/// The resolve answer: the same position and label with each part that
/// rendered a class name given its declaration as a `location` (and the
/// canonical name as its tooltip), the hint's tooltip, and the edits accepting
/// the hint applies.
///
/// A part whose canonical name the file's scope resolves to no declaration — a
/// library class whose source is not loaded — keeps no location: a click simply
/// does not navigate, rather than deferring the request to a materialization
/// this path does not drive.
pub(crate) fn resolve_to_proto(
    state: &GlobalStateSnapshot,
    detail: &ide::InlayHintDetail,
    file_id: FileId,
    line_index: &LineIndex,
) -> anyhow::Result<InlayHint> {
    let mut hint = to_proto(&detail.hint, file_id, line_index);
    if let Label::InlayHintLabelPartList(parts) = &mut hint.label {
        for (part, model) in parts.iter_mut().zip(&detail.hint.label) {
            let Some(fqn) = &model.class else {
                continue;
            };
            let Some(target) = state.analysis.class_definition(file_id, fqn)? else {
                continue;
            };
            let uri = state.file_id_to_url(target.file)?;
            let target_line_index = state.file_line_index(target.file)?;
            part.location = Some(Location {
                uri,
                range: to_proto::range(&target_line_index, target.range),
            });
            part.tooltip = Some(Tooltip::String(fqn.to_string()));
        }
    }
    hint.tooltip = Some(Tooltip::String(detail.tooltip.clone()));
    if !detail.edits.is_empty() {
        hint.text_edits = Some(
            detail
                .edits
                .iter()
                .map(|edit| TextEdit {
                    range: to_proto::range(line_index, edit.range),
                    new_text: edit.new_text.clone(),
                })
                .collect(),
        );
    }
    Ok(hint)
}
