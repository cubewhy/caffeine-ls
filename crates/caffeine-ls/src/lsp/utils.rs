use lsp_types::{
    MessageActionItem, MessageType, ProgressParams, ProgressToken, ShowMessageParams,
    ShowMessageRequestParams, WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressEnd,
    WorkDoneProgressReport,
    notification::{self as notif},
    request,
};

use crate::{
    GlobalState,
    global_state::{OutgoingRequest, ProgressEvent, ProgressState},
};

/// LSP Helpers
impl GlobalState {
    /// Helper to send window/showMessage notifications to the client
    pub fn show_message(&self, typ: MessageType, message: String) {
        let params = ShowMessageParams { typ, message };
        self.notify::<notif::ShowMessage>(params);
    }

    /// Helper to send window/showMessageRequest notifications to the client
    pub(crate) fn show_message_request(
        &mut self,
        typ: MessageType,
        message: String,
        actions: Option<Vec<MessageActionItem>>,
        state: OutgoingRequest,
    ) {
        let params = ShowMessageRequestParams {
            typ,
            message,
            actions,
        };

        self.send_request::<request::ShowMessageRequest>(params, state);
    }

    /// Helper to translate internal ProgressEvent into LSP $/progress notifications
    pub(crate) fn report_progress(&self, event: ProgressEvent) {
        let token = ProgressToken::String(event.token.clone());

        let work_done = match event.state {
            ProgressState::Begin => WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: event.title,
                message: event.message,
                percentage: event.percentage,
                cancellable: Some(false),
            }),
            ProgressState::Report => WorkDoneProgress::Report(WorkDoneProgressReport {
                message: event.message,
                percentage: event.percentage,
                cancellable: Some(false),
            }),
            ProgressState::End => WorkDoneProgress::End(WorkDoneProgressEnd {
                message: event.message,
            }),
        };

        let params = ProgressParams {
            token,
            value: lsp_types::ProgressParamsValue::WorkDone(work_done),
        };

        self.notify::<notif::Progress>(params);
    }
}
