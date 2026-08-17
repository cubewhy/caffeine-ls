use std::{mem, ops::Range};

use lsp_types::*;
use triomphe::Arc;

use crate::{
    GlobalState,
    global_state::{OutgoingRequest, ProgressEvent, ProgressState, ProgressTokenState},
    line_index::{LineEndings, LineIndex, PositionEncoding},
    lsp::from_proto,
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

    /// Helper to translate internal ProgressEvent into LSP $/progress notifications.
    ///
    /// Clients only deliver progress for tokens they were asked to create via
    /// `window/workDoneProgress/create`, so a `Begin` triggers that handshake
    /// and all events are buffered until the client acknowledges the token.
    pub(crate) fn report_progress(&mut self, event: ProgressEvent) {
        if !self
            .config
            .client_capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false)
        {
            return;
        }

        match event.state {
            ProgressState::Begin => {
                let token = event.token.clone();
                // A token still marked active means a previous cycle never
                // ended; close it before asking the client to create a fresh
                // one with the same id.
                if matches!(
                    self.progress_tokens.get(&token),
                    Some(ProgressTokenState::Active)
                ) {
                    self.report_progress_impl(ProgressEvent {
                        token: token.clone(),
                        title: String::new(),
                        message: None,
                        percentage: None,
                        state: ProgressState::End,
                    });
                    self.progress_tokens.remove(&token);
                }

                self.progress_tokens
                    .insert(token.clone(), ProgressTokenState::Creating(vec![event]));
                let params = WorkDoneProgressCreateParams {
                    token: ProgressToken::String(token.clone()),
                };
                self.send_request::<WorkDoneProgressCreateRequest>(
                    params,
                    OutgoingRequest::CreateProgress { token },
                );
            }
            ProgressState::Report | ProgressState::End => {
                let token = event.token.clone();
                let is_end = matches!(event.state, ProgressState::End);
                let buffered = match self.progress_tokens.get_mut(&token) {
                    Some(ProgressTokenState::Creating(buf)) => {
                        buf.push(event);
                        true
                    }
                    _ => {
                        self.report_progress_impl(event);
                        false
                    }
                };
                if is_end && !buffered {
                    self.progress_tokens.remove(&token);
                }
            }
        }
    }

    /// Flushes events buffered while a `window/workDoneProgress/create` request
    /// was in flight, once the client acknowledges the token.
    pub(crate) fn flush_progress(&mut self, token: &str) {
        if let Some(ProgressTokenState::Creating(events)) = self.progress_tokens.remove(token) {
            for event in events {
                self.report_progress_impl(event);
            }
            self.progress_tokens
                .insert(token.to_string(), ProgressTokenState::Active);
        }
    }

    /// Emits a single `$/progress` notification. Assumes the token has already
    /// been created on the client.
    fn report_progress_impl(&self, event: ProgressEvent) {
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

    pub(crate) fn refresh_diagnostics(&mut self) {
        if self
            .config
            .client_capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.diagnostics.as_ref())
            .and_then(|d| d.refresh_support)
            .unwrap_or(false)
        {
            self.send_request::<DiagnosticRefreshRequest>((), OutgoingRequest::Generic(|_, _| {}));
        }
    }
}
