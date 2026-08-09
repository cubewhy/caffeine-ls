use std::{mem, ops::Range};

use lsp_types::{
    MessageActionItem, MessageType, ProgressNotification, ProgressParams, ProgressToken,
    ShowMessageNotification, ShowMessageParams, ShowMessageRequest, ShowMessageRequestParams,
    WorkDoneProgressBegin, WorkDoneProgressEnd, WorkDoneProgressReport,
};
use triomphe::Arc;

use crate::{
    GlobalState, from_proto,
    global_state::{OutgoingRequest, ProgressEvent, ProgressState},
    line_index::{LineEndings, LineIndex, PositionEncoding},
};

pub(crate) fn apply_document_changes(
    encoding: PositionEncoding,
    file_contents: &str,
    mut content_changes: Vec<lsp_types::TextDocumentContentChangeEvent>,
) -> String {
    // If at least one of the changes is a full document change, use the last
    // of them as the starting point and ignore all previous changes.
    let (mut text, r_partial_changes);
    match content_changes
        .iter_mut()
        .rev()
        .try_fold(Vec::new(), |mut acc, change| match change {
            lsp_types::TextDocumentContentChangeEvent::TextDocumentContentChangePartial(
                partial,
            ) => {
                acc.push(partial);
                Ok(acc)
            }
            lsp_types::TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                whole,
            ) => Err((whole, acc)),
        }) {
        Err((whole_document, reversed_partial_changes)) => {
            text = mem::take(&mut whole_document.text);
            r_partial_changes = reversed_partial_changes;
        }
        Ok(partials) => {
            text = file_contents.to_owned();
            r_partial_changes = partials;
        }
    }
    if r_partial_changes.is_empty() {
        return text;
    }

    let mut line_index = LineIndex {
        // the index will be overwritten in the bottom loop's first iteration
        index: Arc::new(ide::LineIndex::new(&text)),
        // We don't care about line endings here.
        endings: LineEndings::Unix,
        encoding,
    };

    // The changes we got must be applied sequentially, but can cross lines so we
    // have to keep our line index updated.
    // Some clients (e.g. Code) sort the ranges in reverse. As an optimization, we
    // remember the last valid line in the index and only rebuild it if needed.
    // The VFS will normalize the end of lines to `\n`.
    let mut index_valid = !0u32;
    for change in r_partial_changes.iter().rev() {
        if index_valid <= change.range.end.line {
            *Arc::make_mut(&mut line_index.index) = ide::LineIndex::new(&text);
        }
        index_valid = change.range.start.line;
        if let Ok(range) = from_proto::text_range(&line_index, change.range) {
            text.replace_range(Range::<usize>::from(range), &change.text);
        }
    }
    text
}

/// LSP Helpers
impl GlobalState {
    /// Helper to send window/showMessage notifications to the client
    pub fn show_message(&self, kind: MessageType, message: String) {
        let params = ShowMessageParams { kind, message };
        self.notify::<ShowMessageNotification>(params);
    }

    /// Helper to send window/showMessageRequest notifications to the client
    pub(crate) fn show_message_request(
        &mut self,
        kind: MessageType,
        message: String,
        actions: Option<Vec<MessageActionItem>>,
        state: OutgoingRequest,
    ) {
        let params = ShowMessageRequestParams {
            kind,
            message,
            actions,
        };

        self.send_request::<ShowMessageRequest>(params, state);
    }

    /// Helper to translate internal ProgressEvent into LSP $/progress notifications
    pub(crate) fn report_progress(&self, event: ProgressEvent) {
        let token = ProgressToken::String(event.token.clone());

        let work_done = match event.state {
            ProgressState::Begin => serde_json::to_value(WorkDoneProgressBegin {
                title: event.title,
                message: event.message,
                percentage: event.percentage,
                cancellable: Some(false),
            })
            .unwrap(),
            ProgressState::Report => serde_json::to_value(WorkDoneProgressReport {
                message: event.message,
                percentage: event.percentage,
                cancellable: Some(false),
            })
            .unwrap(),
            ProgressState::End => serde_json::to_value(WorkDoneProgressEnd {
                message: event.message,
            })
            .unwrap(),
        };

        let params = ProgressParams {
            token,
            value: work_done,
        };

        self.notify::<ProgressNotification>(params);
    }
}
