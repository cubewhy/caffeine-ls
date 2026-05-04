use lsp_types::*;

use crate::config::Config;

pub fn server_capabilities(_config: &Config) -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
            },
        )),
        diagnostic_provider: Some(DiagnosticServerCapabilities::RegistrationOptions(
            DiagnosticRegistrationOptions {
                diagnostic_options: DiagnosticOptions {
                    inter_file_dependencies: false,
                    workspace_diagnostics: false,
                    identifier: Some(crate::NAME.to_string()),
                    ..Default::default()
                },
                static_registration_options: StaticRegistrationOptions { id: None },
                text_document_registration_options: TextDocumentRegistrationOptions {
                    document_selector: None,
                },
            },
        )),
        ..Default::default()
    }
}
