//! A synchronous in-process LSP client for end-to-end server tests.
//!
//! The test thread owns every receive (rust-analyzer's slow-test client works
//! the same way): a request blocks until the matching response arrives, every
//! other message is buffered for later inspection, and the server's
//! `window/workDoneProgress/create` requests are answered inline so progress
//! notifications keep flowing. Nothing can be lost to a reader thread, a server
//! error is reported as an error rather than an empty result, and a server that
//! never answers fails the test with the method it is stuck on.
//!
//! A request issued while the server is still loading its workspace waits for
//! the load to finish first: the load's salsa write blocks the server's main
//! loop, which is also what reads the request channel ([`LspHarness::request`]
//! → [`LspHarness::wait_for_in_flight_load`]).

use crossbeam_channel::RecvTimeoutError;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response, ResponseError};
use lsp_types::{
    ClientCapabilities, ClientInfo, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReport, FileChangeType, FileEvent, InitializeParams, PartialResultParams,
    Position, Range, TextDocumentContentChangePartial, TextDocumentIdentifier, TextDocumentItem,
    Uri, VersionedTextDocumentIdentifier, WindowClientCapabilities, WorkDoneProgressParams,
    WorkspaceFolder, WorkspaceFoldersInitializeParams,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fs,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::TempDir;

pub mod classfile;
pub mod fixture;
pub mod macros;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a request waits for its response. rust-analyzer's slow-test client
/// uses the same budget: a server that is loading a workspace blocks its main
/// loop (a salsa write waits for every database snapshot clone to drop), so a
/// request issued during the load is answered only after it finishes.
const REQUEST_TIMEOUT: Duration = if cfg!(target_os = "macos") {
    Duration::from_secs(300)
} else {
    Duration::from_secs(120)
};

/// Quiet period without `$/progress` traffic required before
/// [`LspHarness::wait_until_workspace_is_loaded`] declares the load over:
/// phase changes (a build sync ending, the workspace graph being applied) are
/// not announced by a token of their own, so the gate waits out the gap between
/// "all tokens ended" and "no further token begins".
const QUIESCE_WINDOW: Duration = Duration::from_millis(250);

/// Nothing arrived before the deadline (as opposed to a closed connection).
struct Timeout;

struct ProgressState {
    /// Whether any `$/progress` notification has been seen — distinguishes "the
    /// server has not started loading yet" from "everything finished".
    seen: bool,
    /// Tokens begun and not yet ended.
    active: HashSet<String>,
    /// When the last `$/progress` notification arrived.
    last_activity: Instant,
}

pub struct LspHarness {
    server_handle: Option<JoinHandle<()>>,
    client: Connection,
    next_id: Cell<i32>,
    pub workspace_root: TempDir,
    cache_dir: TempDir,
    /// Every message the client has received, oldest first.
    messages: RefCell<Vec<Message>>,
    marks: RefCell<HashMap<String, Position>>,
    document_versions: RefCell<HashMap<Uri, i32>>,
    progress: RefCell<ProgressState>,
    /// Methods of server→client requests this client deliberately does not answer.
    unanswered: RefCell<Vec<String>>,
    /// Set when a receive observes the server hanging up, so `Drop` can skip the
    /// shutdown handshake and let the join report the server's own panic.
    closed: Cell<bool>,
}

/// The capabilities every harness advertises. `workDoneProgress` is required
/// for `$/progress` notifications, which carry the workspace-load signal;
/// nothing else is advertised, because nothing else is observed by the tests.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl LspHarness {
    /// Starts `init_backend` on a background thread over an in-memory connection
    /// and completes the `initialize`/`initialized` handshake. `setup` runs on
    /// the workspace root first, so a test controls what the initial workspace
    /// probe finds.
    pub fn start_with_setup<F, S>(config: serde_json::Value, setup: S, init_backend: F) -> Self
    where
        F: FnOnce(Connection) + Send + 'static,
        S: FnOnce(&std::path::Path),
    {
        let workspace_root = tempfile::tempdir().expect("Failed to create temporary workspace");
        let cache_dir = tempfile::tempdir().expect("Failed to create temporary cache dir");

        setup(workspace_root.path());

        let (client, server) = Connection::memory();
        let server_handle = thread::spawn(move || init_backend(server));

        let harness = Self {
            server_handle: Some(server_handle),
            client,
            next_id: Cell::new(1),
            workspace_root,
            cache_dir,
            messages: RefCell::new(Vec::new()),
            marks: RefCell::new(HashMap::new()),
            document_versions: RefCell::new(HashMap::new()),
            progress: RefCell::new(ProgressState {
                seen: false,
                active: HashSet::new(),
                last_activity: Instant::now(),
            }),
            unanswered: RefCell::new(Vec::new()),
            closed: Cell::new(false),
        };

        harness.initialize(config);

        harness
    }

    fn initialize(&self, config: serde_json::Value) {
        let root_uri = Uri::from_file_path(self.workspace_root.path())
            .expect("Failed to convert workspace path to URI");

        let cache_dir = self
            .cache_dir
            .path()
            .to_str()
            .expect("cache dir path is not valid UTF-8")
            .to_owned();

        let mut config = config;
        let cache_dir_value = serde_json::Value::String(cache_dir);
        match &mut config {
            serde_json::Value::Object(obj) => {
                obj.insert("cache_dir".to_owned(), cache_dir_value);
            }
            _ => {
                let mut obj = serde_json::Map::new();
                obj.insert("cache_dir".to_owned(), cache_dir_value);
                config = serde_json::Value::Object(obj);
            }
        }

        #[allow(deprecated)]
        let init_params = InitializeParams {
            root_uri: Some(root_uri.clone()),
            initialization_options: Some(config),
            capabilities: client_capabilities(),
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

        // A real round trip: a server that never answers `initialize` fails here,
        // naming the method, instead of hanging on the first request after it.
        self.request("initialize", init_params);
        self.notify("initialized", serde_json::json!({}));
    }

    /// Receives one message, buffering it and answering the server requests the
    /// client implements. `Ok(None)` means the server closed the connection,
    /// `Err(Timeout)` that nothing arrived in time.
    fn recv(&self, timeout: Duration) -> Result<Option<Message>, Timeout> {
        let msg = match self.client.receiver.recv_timeout(timeout) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => return Err(Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                self.closed.set(true);
                return Ok(None);
            }
        };
        self.observe(&msg);
        Ok(Some(msg))
    }

    fn observe(&self, msg: &Message) {
        match msg {
            Message::Request(req) => {
                self.messages.borrow_mut().push(msg.clone());
                match req.method.as_str() {
                    // Answered inline: without the acknowledgement the server
                    // buffers every further `$/progress` event for the token, and
                    // the readiness gate would never see it end.
                    "window/workDoneProgress/create" | "workspace/diagnosticRefresh" => {
                        let response = Response::new_ok(req.id.clone(), serde_json::Value::Null);
                        let _ = self.client.sender.send(Message::Response(response));
                    }
                    // Deliberately left unanswered: the build-system selection
                    // dialog (`window/showMessageRequest`) is what keeps
                    // `test_syntax_diagnostics_before_workspace_load`'s workspace
                    // unloaded, which is the state under test.
                    other => {
                        tracing::debug!(method = other, "buffering server request");
                        self.unanswered.borrow_mut().push(other.to_string());
                    }
                }
            }
            Message::Notification(notif) => {
                self.messages.borrow_mut().push(msg.clone());
                if notif.method == "$/progress" {
                    self.track_progress(&notif.params);
                }
            }
            Message::Response(_) => {}
        }
    }

    /// Tracks `$/progress` token lifecycles; `token` is a string in this server
    /// (`scan-{version}`, `index-{root}`, `sync-{root}`), but integer tokens are
    /// handled too.
    fn track_progress(&self, params: &serde_json::Value) {
        let token = match params.get("token") {
            Some(serde_json::Value::String(token)) => token.clone(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            _ => return,
        };
        let kind = params
            .get("value")
            .and_then(|value| value.get("kind"))
            .and_then(|kind| kind.as_str());
        tracing::debug!(token, kind, "progress"); // the load timeline, for a hanging gate

        let mut progress = self.progress.borrow_mut();
        progress.seen = true;
        progress.last_activity = Instant::now();
        match kind {
            Some("begin") => {
                progress.active.insert(token);
            }
            Some("end") => {
                progress.active.remove(&token);
            }
            _ => {}
        }
    }

    /// Waits out a workspace load that is still in flight, so a request is sent
    /// to a server that can answer it. A no-op when the server announced no
    /// progress at all: only a workspace that is deliberately never loaded (the
    /// ambiguous build-system case) arrives here mid-load without a token.
    fn wait_for_in_flight_load(&self) {
        if self.progress.borrow().seen {
            self.wait_until_workspace_is_loaded();
        }
    }

    /// Blocks until the server has finished loading the workspace: every
    /// `$/progress` token it began has ended and no further token started within
    /// [`QUIESCE_WINDOW`].
    ///
    /// Panics when the server reports no progress at all: that means the
    /// workspace is deliberately never loaded (the ambiguous build-system case,
    /// whose test must not call this) or the server died.
    pub fn wait_until_workspace_is_loaded(&self) {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            {
                let progress = self.progress.borrow();
                if progress.seen
                    && progress.active.is_empty()
                    && progress.last_activity.elapsed() >= QUIESCE_WINDOW
                {
                    return;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "workspace never finished loading; {}",
                    self.describe_context()
                );
            }
            match self.recv(QUIESCE_WINDOW) {
                Ok(Some(_)) => continue,
                Err(Timeout) => continue, // quiet window elapsed; re-check readiness
                Ok(None) => panic!("server closed the connection while loading the workspace"),
            }
        }
    }

    /// All notifications of `method` received so far, waiting for more until
    /// `done` holds or `timeout` elapses (a panic with everything collected).
    pub fn wait_for_notifications(
        &self,
        method: &str,
        timeout: Duration,
        done: impl Fn(&[Notification]) -> bool,
    ) -> Vec<Notification> {
        let deadline = Instant::now() + timeout;
        loop {
            let collected: Vec<Notification> = self
                .messages
                .borrow()
                .iter()
                .filter_map(|msg| match msg {
                    Message::Notification(notif) if notif.method == method => Some(notif.clone()),
                    _ => None,
                })
                .collect();
            if done(&collected) {
                return collected;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for `{method}` notifications: {collected:#?}");
            }
            match self.recv(remaining) {
                Ok(Some(_)) => {}
                Ok(None) => panic!("server closed the connection while waiting for `{method}`"),
                Err(Timeout) => {
                    panic!("timed out waiting for `{method}` notifications: {collected:#?}")
                }
            }
        }
    }

    pub fn request(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.request_raw(method, params).unwrap_or_else(|err| {
            panic!(
                "server returned an error for {method}: {} (code {})",
                err.message, err.code
            )
        })
    }

    /// Like [`Self::request`], but hands the server's error response back instead
    /// of panicking. Callers use it where the error itself is what is under test
    /// (resolve without `data`, a pull of a deleted file).
    pub fn request_raw(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ResponseError> {
        // A request sent while the workspace load is in flight would only be read
        // after it finishes: the load's salsa write blocks the main loop, which
        // is also what reads this connection.
        self.wait_for_in_flight_load();

        let id = RequestId::from(self.next_id.get());
        self.next_id.set(self.next_id.get() + 1);
        tracing::info!(method, "send request");

        let request = Request::new(id.clone(), method.to_string(), params);
        self.client
            .sender
            .send(Message::Request(request))
            .unwrap_or_else(|_| {
                panic!(
                    "server closed the connection before {method}; {}",
                    self.describe_context()
                )
            });

        loop {
            match self.recv(REQUEST_TIMEOUT) {
                Ok(Some(Message::Response(response))) => {
                    assert_eq!(
                        response.id, id,
                        "response for a request this client did not send"
                    );
                    return match response.error {
                        Some(error) => Err(error),
                        None => Ok(response.result.unwrap_or(serde_json::Value::Null)),
                    };
                }
                Ok(Some(_)) => {} // buffered by `observe`
                Ok(None) => panic!(
                    "server closed the connection while answering {method}; {}",
                    self.describe_context()
                ),
                Err(Timeout) => panic!(
                    "no response for {method} within {REQUEST_TIMEOUT:?}; {}",
                    self.describe_context()
                ),
            }
        }
    }

    /// What the client saw, for failure messages: progress tokens still active
    /// and the server requests that were left unanswered (the latter is how a
    /// stuck build-system dialog shows up).
    fn describe_context(&self) -> String {
        let progress = self.progress.borrow();
        format!(
            "progress tokens active: {:?}, load progress seen: {}; unanswered server requests: {:?}",
            progress.active,
            progress.seen,
            self.unanswered.borrow(),
        )
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
                .borrow_mut()
                .insert(normalized_path.to_string(), Position { line, character });
            final_content = content.replace("<|>", "");
        }

        self.write_file(normalized_path, &final_content)
    }

    fn pop_pos(&self, path: &str) -> Position {
        let normalized_path = path.trim_start_matches('/');
        self.marks
            .borrow_mut()
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

    /// The server's cache directory (the `cache_dir` this client advertises),
    /// where materialized library sources land.
    pub fn cache_dir(&self) -> &std::path::Path {
        self.cache_dir.path()
    }

    fn notify(&self, method: &str, params: serde_json::Value) {
        tracing::info!(method, ?params, "send notification");
        let notif = Notification::new(method.to_string(), params);
        self.client
            .sender
            .send(Message::Notification(notif))
            .unwrap_or_else(|_| panic!("server closed the connection before {method}"));
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

        let version = {
            let mut versions = self.document_versions.borrow_mut();
            let version = versions.entry(uri.clone()).or_insert(0);
            *version += 1;
            *version
        };

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

            self.marks.borrow_mut().insert(
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
    /// A single request is enough: a request cancelled by a write landing
    /// mid-query is re-run on a fresh snapshot by the server itself, which still
    /// answers the original id.
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

        self.request("textDocument/diagnostic", json_params)
    }
}

impl Drop for LspHarness {
    fn drop(&mut self) {
        // A test that is already unwinding must not panic again while dropping.
        if std::thread::panicking() {
            return;
        }
        // `closed` is set by `recv` the moment it observes the server hanging up.
        if !self.closed.get() {
            // Best-effort handshake, without waiting for the response: a server
            // that died took its end of the connection with it, and its own panic
            // is the failure — the join below re-raises it, so a failed send here
            // must not be reported instead.
            let id = RequestId::from(self.next_id.get());
            let shutdown = Request::new(id, "shutdown".to_string(), serde_json::Value::Null);
            if self.client.sender.send(Message::Request(shutdown)).is_ok() {
                let exit = Notification::new("exit".to_string(), serde_json::Value::Null);
                let _ = self.client.sender.send(Message::Notification(exit));
            }
        }
        if let Some(handle) = self.server_handle.take()
            && let Err(payload) = handle.join()
        {
            // A panicking server thread is the failure, not the timeout it would
            // otherwise cause.
            std::panic::resume_unwind(payload);
        }
    }
}
