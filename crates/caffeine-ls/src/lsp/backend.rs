use std::sync::Arc;

use ra_ap_vfs::VfsPath;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    self, DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    MessageType, ServerInfo,
};
use tower_lsp::{
    Client, LanguageServer,
    lsp_types::{InitializeParams, InitializeResult, InitializedParams},
};

use crate::config::Config;
use crate::global_state::GlobalState;
use crate::lsp::capabilities;

pub struct Backend {
    client: Client,
    state: GlobalState,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: GlobalState::default(),
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, initialize_params: InitializeParams) -> Result<InitializeResult> {
        let mut client_options = None;

        // deserialize client options (initialize params)
        if let Some(json) = initialize_params.initialization_options {
            match serde_json::from_value(json) {
                Ok(deserialized) => client_options = Some(deserialized),
                Err(err) => {
                    self.client
                        .show_message(
                            MessageType::ERROR,
                            format!("Failed to load user settings: {err:?}"),
                        )
                        .await;
                }
            }
        }

        let config = Config::new(
            initialize_params.capabilities,
            initialize_params.workspace_folders,
            initialize_params.client_info,
            client_options,
        );

        let capabilities = capabilities::server_capabilities(&config);

        self.state.config.swap(Some(Arc::new(Some(config))));

        Ok(InitializeResult {
            server_info: Some(server_info()),
            capabilities,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        tracing::info!("didOpen {}", params.text_document.uri);
        let text = params.text_document.text;
        let content = text.clone().into_bytes();

        if let Some(vfs_path) = to_vfs_path(&params.text_document.uri) {
            let mut vfs = self.state.vfs.write().await;
            vfs.set_file_contents(vfs_path, Some(content));
        } else {
            tracing::error!(
                "Failed to convert URI to file path: {}",
                params.text_document.uri
            );
        }
    }

    async fn did_change(&self, _params: DidChangeTextDocumentParams) {
        // TODO: update file content in salsa db
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        tracing::info!("didClose {}", params.text_document.uri);

        if let Some(vfs_path) = to_vfs_path(&params.text_document.uri) {
            let mut vfs = self.state.vfs.write().await;
            vfs.set_file_contents(vfs_path, None);
            drop(vfs);
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

fn server_info() -> ServerInfo {
    ServerInfo {
        name: crate::NAME.to_string(),
        version: Some(crate::VERSION.to_string()),
    }
}

fn to_vfs_path(uri: &lsp_types::Url) -> Option<VfsPath> {
    let path_buf = uri.to_file_path().ok()?;
    let normalized = path_buf.canonicalize().unwrap_or(path_buf);
    Some(VfsPath::new_real_path(
        normalized.to_string_lossy().to_string(),
    ))
}
