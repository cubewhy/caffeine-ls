use std::time::Duration;

use base_db::{LanguageId, SourceDatabase};
use ra_ap_line_index::{LineIndex, WideEncoding, WideLineCol};
use tokio::sync::mpsc;
use triomphe::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    self, DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    MessageType, ServerInfo,
};
use tower_lsp::{
    Client, LanguageServer,
    lsp_types::{InitializeParams, InitializeResult, InitializedParams},
};
use vfs::VfsPath;

use crate::config::Config;
use crate::global_state::GlobalState;
use crate::lsp::capabilities;
use crate::lsp::worker::{Action, Job, TaskKey};

pub struct Backend {
    client: Client,
    state: Arc<GlobalState>,

    worker_tx: mpsc::Sender<Job>,
}

impl Backend {
    pub fn new(client: Client, state: Arc<GlobalState>, worker_tx: mpsc::Sender<Job>) -> Self {
        Self {
            client,
            state,
            worker_tx,
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

        self.state
            .config
            .swap(Some(std::sync::Arc::new(Some(config))));

        // initialize worker

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
            vfs.set_file_contents(vfs_path.clone(), Some(content));

            if let Some(file_id) = vfs.file_id(&vfs_path).map(|(id, _)| id) {
                drop(vfs);
                self.sync_vfs_to_db().await;

                let _ = self
                    .worker_tx
                    .send(Job::file(
                        file_id,
                        Duration::ZERO,
                        Action::Diagnostics(file_id),
                    ))
                    .await;
            }
        } else {
            tracing::error!(
                "Failed to convert URI to file path: {}",
                params.text_document.uri
            );
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(vfs_path) = to_vfs_path(&params.text_document.uri) {
            let vfs = self.state.get_vfs().await;

            let Some(file_id) = vfs.file_id(&vfs_path).map(|(id, _excluded)| id) else {
                // LSP client issue
                tracing::error!("File not found in vfs: {}", params.text_document.uri);
                return;
            };

            drop(vfs);

            // get the file in salsa
            let db = self.state.db_snapshot().await;
            let file_text = db.file_text(file_id);
            let file_content = file_text.text(&db);

            let mut text = file_content.to_string();

            // apply edits
            for edit in params.content_changes {
                if let Some(range) = edit.range {
                    // incremental edit
                    let line_index = LineIndex::new(&text);

                    let start_wide = WideLineCol {
                        line: range.start.line,
                        col: range.start.character,
                    };
                    let start_line_col = line_index
                        .to_utf8(WideEncoding::Utf16, start_wide)
                        .expect("Invalid start position");
                    let start_offset = line_index
                        .offset(start_line_col)
                        .expect("Start offset out of bounds");
                    let start = u32::from(start_offset) as usize;

                    let end_wide = WideLineCol {
                        line: range.end.line,
                        col: range.end.character,
                    };
                    let end_line_col = line_index
                        .to_utf8(WideEncoding::Utf16, end_wide)
                        .expect("Invalid end position");
                    let end_offset = line_index
                        .offset(end_line_col)
                        .expect("End offset out of bounds");
                    let end = u32::from(end_offset) as usize;

                    text.replace_range(start..end, &edit.text);
                } else {
                    // full edit
                    text = edit.text;
                }
            }

            self.sync_vfs_to_db().await;

            let _ = self
                .worker_tx
                .send(Job::new(
                    TaskKey::File(file_id),
                    Duration::from_millis(300),
                    Action::Diagnostics(file_id),
                ))
                .await;
        } else {
            tracing::error!(
                "Failed to convert URI to file path: {}",
                params.text_document.uri
            );
        }
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

impl Backend {
    async fn sync_vfs_to_db(&self) {
        let mut vfs = self.state.vfs.write().await;
        let changes = vfs.take_changes();

        if changes.is_empty() {
            return;
        }

        let mut db = self.state.lock_db().await;

        for (file_id, changed_file) in changes {
            match changed_file.change {
                vfs::Change::Create(bytes, _) | vfs::Change::Modify(bytes, _) => {
                    let updated_text = String::from_utf8(bytes).unwrap_or_default();

                    let language_id = vfs
                        .file_path(file_id)
                        .name_and_extension()
                        .and_then(|(_, ext)| ext)
                        .map(LanguageId::from_extension)
                        .unwrap_or(LanguageId::Unknown);

                    db.set_file(file_id, &updated_text, language_id);
                }
                vfs::Change::Delete => {
                    db.set_file(file_id, "", LanguageId::Unknown);
                }
            }
        }
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
