use crossbeam_channel::{bounded, unbounded};
use dashmap::DashMap;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, ClientInfo, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReport, FileChangeType, FileEvent, InitializeParams, PartialResultParams,
    Position, Range, TextDocumentContentChangePartial, TextDocumentIdentifier, TextDocumentItem,
    Uri, VersionedTextDocumentIdentifier, WorkDoneProgressParams, WorkspaceFolder,
    WorkspaceFoldersInitializeParams,
};
use std::{
    collections::HashMap,
    fs,
    sync::{
        Arc, RwLock,
        atomic::{AtomicI32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tempfile::TempDir;

pub mod fixture;
pub mod macros;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct LspHarness {
    server_handle: Option<JoinHandle<()>>,
    client_connection: Connection,
    next_id: AtomicI32,
    pub workspace_root: TempDir,
    config: serde_json::Value,
    client_capabilities: ClientCapabilities,
    notification_sender: crossbeam_channel::Sender<Notification>,
    pub notification_receiver: crossbeam_channel::Receiver<Notification>,
    marks: RwLock<HashMap<String, Position>>,
    pending_requests: Arc<DashMap<RequestId, crossbeam_channel::Sender<serde_json::Value>>>,
    document_versions: DashMap<Uri, i32>,
}

impl LspHarness {
    /// Starts the LSP server in a background thread using an in-memory connection.
    /// `init_backend` is a closure that takes the server side of the `Connection`.
    pub fn start<F>(config: serde_json::Value, init_backend: F) -> Self
    where
        F: FnOnce(Connection) + Send + 'static,
    {
        Self::start_with_setup(config, |_| {}, init_backend)
    }

    /// Like [`Self::start`], but runs `setup` on the workspace root before the
    /// server is initialized, allowing tests to control what the initial
    /// workspace probe sees.
    pub fn start_with_setup<F, S>(config: serde_json::Value, setup: S, init_backend: F) -> Self
    where
        F: FnOnce(Connection) + Send + 'static,
        S: FnOnce(&std::path::Path),
    {
        let workspace_root = tempfile::tempdir().expect("Failed to create temporary workspace");

        setup(workspace_root.path());

        // Create an in-memory connection pair for the client (harness) and server
        let (client_connection, server_connection) = Connection::memory();

        // Spawn the language server on a background thread
        let server_handle = thread::spawn(move || {
            init_backend(server_connection);
        });

        let (notif_tx, notif_rx) = unbounded();

        let harness = Self {
            server_handle: Some(server_handle),
            client_connection,
            next_id: AtomicI32::new(1),
            workspace_root,
            config,
            client_capabilities: ClientCapabilities {
                ..Default::default()
            },
            notification_receiver: notif_rx,
            notification_sender: notif_tx,
            marks: Default::default(),
            pending_requests: Default::default(),
            document_versions: Default::default(),
        };

        let pending_requests = harness.pending_requests.clone();
        let notification_sender = harness.notification_sender.clone();
        let client_receiver = harness.client_connection.receiver.clone();
        let client_sender = harness.client_connection.sender.clone();

        // Spawn a background thread to listen to messages coming from the server
        thread::spawn(move || {
            for msg in client_receiver {
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
                        if req.method == "workspace/diagnosticRefresh" {
                            let _ = client_sender.send(Message::Response(
                                lsp_server::Response::new_ok(req.id, serde_json::Value::Null),
                            ));
                        } else {
                            eprintln!("Received request from server: {}", req.method);
                        }
                    }
                }
            }
        });

        harness.init();

        harness
    }

    /// Like [`Self::start_with_setup`], but advertises `workDoneProgress`
    /// support and answers `window/workDoneProgress/create` requests, so tests
    /// can observe `$/progress` notifications from the server.
    pub fn start_with_setup_progress<F, S>(
        config: serde_json::Value,
        setup: S,
        init_backend: F,
    ) -> Self
    where
        F: FnOnce(Connection) + Send + 'static,
        S: FnOnce(&std::path::Path),
    {
        let workspace_root = tempfile::tempdir().expect("Failed to create temporary workspace");

        setup(workspace_root.path());

        // Create an in-memory connection pair for the client (harness) and server
        let (client_connection, server_connection) = Connection::memory();

        // Spawn the language server on a background thread
        let server_handle = thread::spawn(move || {
            init_backend(server_connection);
        });

        let (notif_tx, notif_rx) = unbounded();

        let harness = Self {
            server_handle: Some(server_handle),
            client_connection,
            next_id: AtomicI32::new(1),
            workspace_root,
            config,
            client_capabilities: ClientCapabilities {
                window: Some(lsp_types::WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            notification_receiver: notif_rx,
            notification_sender: notif_tx,
            marks: Default::default(),
            pending_requests: Default::default(),
            document_versions: Default::default(),
        };

        let pending_requests = harness.pending_requests.clone();
        let notification_sender = harness.notification_sender.clone();
        let client_receiver = harness.client_connection.receiver.clone();
        let client_sender = harness.client_connection.sender.clone();

        // Spawn a background thread to listen to messages coming from the server
        thread::spawn(move || {
            for msg in client_receiver {
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
                        if req.method == "window/workDoneProgress/create"
                            || req.method == "workspace/diagnosticRefresh"
                        {
                            let _ = client_sender.send(Message::Response(
                                lsp_server::Response::new_ok(req.id, serde_json::Value::Null),
                            ));
                        } else {
                            eprintln!("Received request from server: {}", req.method);
                        }
                    }
                }
            }
        });

        harness.init();

        harness
    }

    fn init(&self) {
        let root_uri = Uri::from_file_path(self.workspace_root.path())
            .expect("Failed to convert workspace path to URI");

        #[allow(deprecated)]
        let init_params = InitializeParams {
            root_uri: Some(root_uri.clone()),
            initialization_options: Some(self.config.clone()),
            capabilities: self.client_capabilities.clone(),
            workspace_folders_initialize_params: WorkspaceFoldersInitializeParams::new(Some(
                vec![WorkspaceFolder {
                    uri: root_uri,
                    name: "test_workspace".to_string(),
                }]
                .into(),
            )),
            client_info: Some(ClientInfo {
                name: "lsp-test".to_string(),
                version: Some(VERSION.to_string()),
            }),
            ..Default::default()
        };

        let init_params =
            serde_json::to_value(init_params).expect("Failed to serialize init params");

        self.request("initialize", init_params);
        self.notify("initialized", serde_json::json!({}));
    }

    pub fn write_file(&self, relative_path: &str, content: &str) -> Uri {
        let relative_path = relative_path.trim_start_matches('/');
        let path = self.workspace_root.path().join(relative_path);

        if path == self.workspace_root.path() {
            panic!(
                "Attempted to write content to the workspace root directory instead of a file. Path: '{}'",
                relative_path
            );
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("Failed to create parent directories");
        }

        fs::write(&path, content).expect("Failed to write file");
        Uri::from_file_path(path).unwrap()
    }

    /// Deletes the fixture file at `relative_path` from the workspace root,
    /// mirroring what an editor does when a file is removed on disk.
    pub fn remove_file(&self, relative_path: &str) {
        let relative_path = relative_path.trim_start_matches('/');
        let path = self.workspace_root.path().join(relative_path);
        fs::remove_file(&path).expect("Failed to remove file");
    }

    pub fn write_fixture_file(&self, path_str: &str, content: &str) -> Uri {
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

        self.write_file(normalized_path, &final_content)
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

    pub fn uri(&self, relative_path: &str) -> Uri {
        let path = self
            .workspace_root
            .path()
            .join(relative_path.trim_start_matches('/'));
        Uri::from_file_path(path).expect("Failed to convert path to URI")
    }

    pub fn notify(&self, method: &str, params: serde_json::Value) {
        tracing::info!(method, ?params, "send notification");
        let notif = Notification::new(method.to_string(), params);
        self.client_connection
            .sender
            .send(Message::Notification(notif))
            .unwrap();
    }

    pub fn request(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = RequestId::from(self.next_id.fetch_add(1, Ordering::SeqCst));
        let req = Request::new(id.clone(), method.to_string(), params);

        let (tx, rx) = bounded(1);
        self.pending_requests.insert(id.clone(), tx);

        self.client_connection
            .sender
            .send(Message::Request(req))
            .unwrap();

        tracing::info!(method, "send request");

        // Fail loudly instead of hanging forever when the server never answers
        // (e.g. it is stuck or the connection was dropped): a diagnostic
        // deadlock must surface as a clear failure, not an infinite timeout.
        rx.recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("server never answered {method}: {e}"))
    }

    pub fn open_document(&self, relative_path: &str) -> Uri {
        let path = self
            .workspace_root
            .path()
            .join(relative_path.trim_start_matches('/'));

        let content = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("Failed to read fixture file at: {:?}", path));

        let uri = self.uri(relative_path);

        let language_id = match path.extension().and_then(|ext| ext.to_str()) {
            Some("java") => "java",
            Some("kotlin") | Some("kt") => "kotlin",
            _ => "plaintext",
        };

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.into(),
                version: 0,
                text: content,
            },
        };

        let json_params =
            serde_json::to_value(params).expect("Failed to serialize DidOpenTextDocumentParams");

        self.notify("textDocument/didOpen", json_params);

        uri
    }

    pub fn close_document(&self, relative_path: &str) {
        let uri = self.uri(relative_path);

        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
        };

        let json_params =
            serde_json::to_value(params).expect("Failed to serialize DidCloseTextDocumentParams");

        self.notify("textDocument/didClose", json_params);
    }

    /// Sends a `workspace/didChangeWatchedFiles` notification for `relative_path`.
    /// Mirrors what client-editor watchers send when a file is created, changed,
    /// or deleted on disk.
    pub fn did_change_watched_files(&self, relative_path: &str, kind: FileChangeType) {
        let params = DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: self.uri(relative_path),
                kind,
            }],
        };

        let json_params =
            serde_json::to_value(params).expect("Failed to serialize DidChangeWatchedFilesParams");
        self.notify("workspace/didChangeWatchedFiles", json_params);
    }

    pub fn change_document_incremental(&self, relative_path: &str, range: Range, text: &str) {
        let uri = self.uri(relative_path);

        let mut version_entry = self.document_versions.entry(uri.clone()).or_insert(0);
        *version_entry += 1;
        let version = *version_entry;

        #[allow(deprecated)]
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier::new(
                version,
                TextDocumentIdentifier::new(uri),
            ),
            content_changes: vec![
                TextDocumentContentChangePartial {
                    range,
                    range_length: None,
                    text: text.to_string(),
                }
                .into(),
            ],
        };

        let json_params = serde_json::to_value(params).expect("Failed to serialize");
        self.notify("textDocument/didChange", json_params);
    }

    pub fn change_at_mark(&self, relative_path: &str, text: &str) {
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
        self.change_document_incremental(relative_path, range, &final_text);
    }

    pub fn pull_document_diagnostics(&self, relative_path: &str) -> DocumentDiagnosticReport {
        let raw = self.pull_document_diagnostics_raw_with_previous(relative_path, None);
        serde_json::from_value(raw).expect("document diagnostic report")
    }

    /// Issues `textDocument/diagnostic` for `relative_path`, returning the raw
    /// JSON response so tests can assert on `resultId` and `relatedDocuments`.
    /// Retries while the server is cancelling (salsa raises `Cancelled` when a
    /// write lands mid-query; real clients do the same).
    pub fn pull_document_diagnostics_raw_with_previous(
        &self,
        relative_path: &str,
        previous_result_id: Option<String>,
    ) -> serde_json::Value {
        let uri = self.uri(relative_path);
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri },
            previous_result_id,
            identifier: None,
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };

        let json_params =
            serde_json::to_value(params).expect("failed to serialize document diagnostic params");

        for _ in 0..100 {
            let response = self.request("textDocument/diagnostic", json_params.clone());
            if response.is_null() {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            return response;
        }
        panic!("server never answered a diagnostic pull")
    }

    /// Drains `notification_receiver`, collecting at most `max` notifications
    /// whose method matches `method`, within `timeout`.
    pub fn wait_notifications(
        &self,
        method: &str,
        max: usize,
        timeout: Duration,
    ) -> Vec<Notification> {
        let deadline = std::time::Instant::now() + timeout;
        let mut out = Vec::new();
        while out.len() < max && std::time::Instant::now() < deadline {
            if let Ok(notif) = self
                .notification_receiver
                .recv_timeout(Duration::from_millis(25))
                && notif.method == method
            {
                out.push(notif);
            }
        }
        out
    }

    pub fn shutdown(mut self) {
        let _ = self.request("shutdown", serde_json::Value::Null);
        self.notify("exit", serde_json::Value::Null);

        // Close the connection channel gracefully to unblock loops depending on it.
        drop(self.client_connection.sender);

        if let Some(handle) = self.server_handle.take() {
            let _ = handle.join();
        }
    }
}
