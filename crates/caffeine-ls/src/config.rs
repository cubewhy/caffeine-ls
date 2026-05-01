use tower_lsp::lsp_types::{ClientCapabilities, ClientInfo, WorkspaceFolder};

#[derive(Debug, Clone)]
pub struct Config {
    pub client_capabilities: ClientCapabilities,
    pub workspace_folders: Option<Vec<WorkspaceFolder>>,
    pub client_info: Option<ClientInfo>,
    pub client_options: Option<ClientOptions>,
}

impl Config {
    pub fn new(
        client_capabilities: ClientCapabilities,
        workspace_folders: Option<Vec<WorkspaceFolder>>,
        client_info: Option<ClientInfo>,
        client_options: Option<ClientOptions>,
    ) -> Self {
        Self {
            client_capabilities,
            workspace_folders,
            client_info,
            client_options,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ClientOptions {}
