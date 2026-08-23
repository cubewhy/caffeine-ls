use lsp_types::*;

use crate::config::Config;

pub fn server_capabilities(_config: &Config) -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::Incremental),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                save: Some(true.into()),
            }
            .into(),
        ),
        diagnostic_provider: Some(
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
            }
            .into(),
        ),
        document_symbol_provider: Some(true.into()),
        workspace_symbol_provider: Some(true.into()),
        definition_provider: Some(true.into()),
        hover_provider: Some(true.into()),
        ..Default::default()
    }
}
