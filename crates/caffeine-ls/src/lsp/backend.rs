use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{MessageType, ServerInfo};
use tower_lsp::{
    Client, LanguageServer,
    lsp_types::{InitializeParams, InitializeResult, InitializedParams},
};

use crate::config::Config;
use crate::lsp::capabilities;

pub struct Backend {
    client: Client,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self { client }
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

        Ok(InitializeResult {
            server_info: Some(server_info()),
            capabilities: capabilities::server_capabilities(&config),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
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
