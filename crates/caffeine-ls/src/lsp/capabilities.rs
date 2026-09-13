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
                    // An edit in one file can change the diagnostics of another
                    // (the cross-file diagnostic pipeline), and the client is
                    // expected to re-pull open documents on refresh.
                    inter_file_dependencies: true,
                    workspace_diagnostics: true,
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
        workspace_symbol_provider: Some(
            WorkspaceSymbolOptions {
                resolve_provider: Some(true),
                ..Default::default()
            }
            .into(),
        ),
        definition_provider: Some(true.into()),
        references_provider: Some(true.into()),
        hover_provider: Some(true.into()),
        // The client renders the labels immediately and asks for the tooltips,
        // label-part locations and text edits per hint, on demand.
        inlay_hint_provider: Some(
            InlayHintOptions {
                resolve_provider: Some(true),
                ..Default::default()
            }
            .into(),
        ),
        semantic_tokens_provider: Some(
            SemanticTokensOptions {
                legend: crate::lsp::semantic_tokens::legend(),
                // A range request is answered from the file's full token
                // stream, filtered. `delta: true` lets the client send back the
                // `result_id` of the stream it holds and receive only the edit
                // that turns it into the current one.
                range: Some(true.into()),
                full: Some(SemanticTokensFullDelta { delta: Some(true) }.into()),
                work_done_progress_options: Default::default(),
            }
            .into(),
        ),
        ..Default::default()
    }
}
