use crossbeam_channel::unbounded;
use dashmap::DashMap;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use std::io::Write;
use std::{
    collections::HashMap,
    io::{BufReader, BufWriter},
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tempfile::TempDir;
use tokio::{io::split, sync::mpsc};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::io::SyncIoBridge;
use tower_lsp::{Client, LanguageServer, LspService, Server, lsp_types::*};

pub mod fixture;
pub mod macros;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct LspHarness {
    server_handle: JoinHandle<()>,
    client_connection: Connection,
    next_id: AtomicI32,
    pub workspace_root: TempDir,
    config: serde_json::Value,
    client_capabilities: ClientCapabilities,
    notification_sender: mpsc::UnboundedSender<Notification>,
    pub notification_receiver: mpsc::UnboundedReceiver<Notification>,
    marks: std::sync::RwLock<HashMap<String, Position>>,
    pending_requests: Arc<DashMap<RequestId, oneshot::Sender<serde_json::Value>>>,
    document_versions: DashMap<tower_lsp::lsp_types::Url, i32>,
}

impl LspHarness {
    pub async fn start<F, S>(config: serde_json::Value, init_backend: F) -> Self
    where
        F: FnOnce(Client) -> S,
        S: LanguageServer + Send + Sync + 'static,
    {
        let workspace_root = tempfile::tempdir().expect("Failed to create temporary workspace");

        let (client_stream, server_stream) = tokio::io::duplex(65536);

        let (server_rx, server_tx) = split(server_stream);
        let (service, socket) = LspService::new(init_backend);

        let server_handle = tokio::spawn(async move {
            Server::new(server_rx, server_tx, socket)
                .serve(service)
                .await;
        });

        let (client_rx, client_tx) = split(client_stream);

        let mut reader = BufReader::new(SyncIoBridge::new(client_rx));
        let mut writer = BufWriter::new(SyncIoBridge::new(client_tx));

        let (tx_msg, client_receiver) = unbounded::<Message>();
        let (client_sender, rx_msg) = unbounded::<Message>();

        tokio::task::spawn_blocking(move || {
            while let Ok(Some(msg)) = Message::read(&mut reader) {
                if tx_msg.send(msg).is_err() {
                    break;
                }
            }
        });

        tokio::task::spawn_blocking(move || {
            while let Ok(msg) = rx_msg.recv() {
                if msg.write(&mut writer).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        let client_connection = Connection {
            sender: client_sender,
            receiver: client_receiver,
        };

        let client_capabilities = ClientCapabilities {
            ..Default::default()
        };

        let (notif_tx, notif_rx) = mpsc::unbounded_channel();

        let harness = Self {
            server_handle,
            client_connection,
            next_id: AtomicI32::new(1),
            workspace_root,
            config,
            client_capabilities,
            notification_receiver: notif_rx,
            notification_sender: notif_tx,
            marks: Default::default(),
            pending_requests: Default::default(),
            document_versions: Default::default(),
        };

        let pending_requests = harness.pending_requests.clone();
        let notification_sender = harness.notification_sender.clone();
        let client_receiver = harness.client_connection.receiver.clone();

        tokio::task::spawn_blocking(move || {
            while let Ok(msg) = client_receiver.recv() {
                match msg {
                    Message::Response(res) => {
                        if let Some((_, tx)) = pending_requests.remove(&res.id) {
                            let _ = tx.send(res.result.unwrap_or_default());
                        }
                    }
                    Message::Notification(notif) => {
                        if let Err(e) = notification_sender.send(notif) {
                            eprintln!("Failed to send notification: {}", e);
                        }
                    }
                    Message::Request(req) => {
                        eprintln!("Received request from server: {}", req.method);
                    }
                }
            }
        });

        harness.init().await;

        harness
    }

    async fn init(&self) {
        let root_uri = tower_lsp::lsp_types::Url::from_file_path(self.workspace_root.path())
            .expect("Failed to convert workspace path to URI");

        let init_params = InitializeParams {
            root_uri: Some(root_uri.clone()),
            initialization_options: Some(self.config.clone()),
            capabilities: self.client_capabilities.clone(),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: "test_workspace".to_string(),
            }]),
            client_info: Some(ClientInfo {
                name: "lsp-test".to_string(),
                version: Some(VERSION.to_string()),
            }),

            ..Default::default()
        };

        let init_params =
            serde_json::to_value(init_params).expect("Failed to serialize init params");

        self.request("initialize", init_params).await;
        self.notify("initialized", serde_json::json!({}));
    }

    pub async fn write_file(
        &self,
        relative_path: &str,
        content: &str,
    ) -> tower_lsp::lsp_types::Url {
        let relative_path = relative_path.trim_start_matches('/');
        let path = self.workspace_root.path().join(relative_path);

        if path == self.workspace_root.path() {
            panic!(
                "Attempted to write content to the workspace root directory instead of a file. Path: '{}'",
                relative_path
            );
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }

        tokio::fs::write(&path, content).await.unwrap();
        tower_lsp::lsp_types::Url::from_file_path(path).unwrap()
    }

    pub async fn write_fixture_file(
        &self,
        path_str: &str,
        content: &str,
    ) -> tower_lsp::lsp_types::Url {
        let normalized_path = path_str.trim_start_matches('/');
        let mut final_content = content.to_string();

        if let Some(offset) = content.find("<|>") {
            let before = &content[..offset];
            let line = before.lines().count() as u32 - 1;
            let character = before.lines().last().map(|l| l.len()).unwrap_or(0) as u32;

            self.marks
                .write()
                .unwrap()
                .insert(normalized_path.to_string(), Position { line, character });
            final_content = content.replace("<|>", "");
        }

        self.write_file(normalized_path, &final_content).await
    }

    pub fn pos(&self, path: &str) -> Position {
        let normalized_path = path.trim_start_matches('/');

        self.marks
            .read()
            .unwrap()
            .get(normalized_path)
            .cloned()
            .unwrap_or_else(|| {
                let available_keys: Vec<_> = self.marks.read().unwrap().keys().cloned().collect();
                panic!(
                    "No mark <|> found in path: '{}'. Available marks: {:?}",
                    normalized_path, available_keys
                )
            })
    }

    pub fn pop_pos(&self, path: &str) -> Position {
        let normalized_path = path.trim_start_matches('/');
        self.marks
            .write()
            .unwrap()
            .remove(normalized_path)
            .unwrap_or_else(|| panic!("No mark <|> found to pop in path: '{}'", normalized_path))
    }

    pub fn uri(&self, relative_path: &str) -> tower_lsp::lsp_types::Url {
        let path = self
            .workspace_root
            .path()
            .join(relative_path.trim_start_matches('/'));
        tower_lsp::lsp_types::Url::from_file_path(path).expect("Failed to convert path to URI")
    }

    pub fn notify(&self, method: &str, params: serde_json::Value) {
        let notif = Notification::new(method.to_string(), params);
        self.client_connection
            .sender
            .send(Message::Notification(notif))
            .unwrap();
    }

    pub async fn request(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = RequestId::from(self.next_id.fetch_add(1, Ordering::SeqCst));
        let req = Request::new(id.clone(), method.to_string(), params);

        let (tx, rx) = oneshot::channel();
        self.pending_requests.insert(id.clone(), tx);

        self.client_connection
            .sender
            .send(Message::Request(req))
            .unwrap();

        rx.await.expect("Server dropped the request")
    }

    pub async fn open_document(&self, relative_path: &str) -> tower_lsp::lsp_types::Url {
        let path = self
            .workspace_root
            .path()
            .join(relative_path.trim_start_matches('/'));

        let content = tokio::fs::read_to_string(&path)
            .await
            .unwrap_or_else(|_| panic!("Failed to read fixture file at: {:?}", path));

        let uri = self.uri(relative_path);

        let language_id = match path.extension().and_then(|ext| ext.to_str()) {
            Some("java") => "java",
            Some("kotlin") => "kotlin",
            _ => "plaintext",
        };

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.to_string(),
                version: 0,
                text: content,
            },
        };

        let json_params =
            serde_json::to_value(params).expect("Failed to serialize DidOpenTextDocumentParams");

        self.notify("textDocument/didOpen", json_params);

        uri
    }

    pub async fn close_document(&self, relative_path: &str) {
        let uri = self.uri(relative_path);

        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
        };

        let json_params =
            serde_json::to_value(params).expect("Failed to serialize DidCloseTextDocumentParams");

        self.notify("textDocument/didClose", json_params);
    }

    pub async fn change_document_incremental(&self, relative_path: &str, range: Range, text: &str) {
        let uri = self.uri(relative_path);

        let mut version_entry = self.document_versions.entry(uri.clone()).or_insert(0);
        *version_entry += 1;
        let version = *version_entry;

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: text.to_string(),
            }],
        };

        let json_params = serde_json::to_value(params).expect("Failed to serialize");
        self.notify("textDocument/didChange", json_params);
    }

    pub async fn change_at_mark(&self, relative_path: &str, text: &str) {
        let old_pos = self.pop_pos(relative_path);

        let mut final_text = text.to_string();
        let normalized_path = relative_path.trim_start_matches('/');

        if let Some(offset) = text.find("<|>") {
            let before = &text[..offset];
            let lines_in_added_text = before.lines().count() as u32 - 1;

            let new_line = old_pos.line + lines_in_added_text;

            let new_character = if lines_in_added_text == 0 {
                old_pos.character + before.len() as u32
            } else {
                before.lines().last().map(|l| l.len()).unwrap_or(0) as u32
            };

            self.marks.write().unwrap().insert(
                normalized_path.to_string(),
                Position {
                    line: new_line,
                    character: new_character,
                },
            );

            final_text = text.replace("<|>", "");
        }

        let range = Range {
            start: old_pos,
            end: old_pos,
        };
        self.change_document_incremental(relative_path, range, &final_text)
            .await;
    }

    pub async fn pull_document_diagnostics(
        &self,
        relative_path: &str,
    ) -> tower_lsp::lsp_types::DocumentDiagnosticReport {
        let uri = self.uri(relative_path);
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri },
            previous_result_id: None,
            identifier: None,
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };

        let response = self
            .request(
                "textDocument/diagnostic",
                serde_json::to_value(params)
                    .expect("failed to serialize document diagnostic params"),
            )
            .await;

        serde_json::from_value(response).expect("Failed to deserialize diagnostic report")
    }

    pub async fn shutdown(self) {
        let _ = self.request("shutdown", serde_json::json!({})).await;
        self.notify("exit", serde_json::json!({}));

        self.server_handle.abort();
        let _ = self.server_handle.await;
    }
}
