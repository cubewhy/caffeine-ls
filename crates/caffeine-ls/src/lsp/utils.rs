use lsp_types::{MessageType, ShowMessageParams, notification as notif};

use crate::GlobalState;

/// LSP Helpers
impl GlobalState {
    /// Helper to send window/showMessage notifications to the client
    pub fn show_message(&self, typ: MessageType, message: String) {
        let params = ShowMessageParams { typ, message };
        self.notify::<notif::ShowMessage>(params);
    }
}
