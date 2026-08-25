//! An embedded LSP client that drives the real server lifecycle in-process.
//!
//! The `diagnostics` subcommand starts the genuine server main loop over an
//! in-memory connection and speaks LSP to it, exactly like an editor would:
//! initialize → initialized (which triggers probe → build-system sync →
//! workspace load → library warmup server-side) → pull diagnostics per file
//! → shutdown. This guarantees the CLI observes the same analysis as the
//! editor with zero duplicated lifecycle logic.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, ClientInfo, GeneralClientCapabilities, InitializeParams, MessageType,
    PositionEncodingKind, ProgressParams, ShowMessageParams, Uri, WindowClientCapabilities,
    WorkDoneProgressCreateParams, WorkspaceFolder, WorkspaceFoldersInitializeParams,
};
use parking_lot::Mutex;
use vfs::AbsPathBuf;

use crate::{VERSION, cli::serve};

const STACK_SIZE: usize = 1024 * 1024 * 8;

/// Upper bound for the whole workspace load phase (build-system sync, VFS
/// scan, library indexing).
pub(crate) const WORKSPACE_LOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Upper bound for a single request/response round-trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Default)]
struct ClientState {
    /// Tokens whose `$/progress` reported an `end` event. The VFS scan token
    /// (`scan-…`) ending means every source file is loaded into the database.
    ended_tokens: Vec<String>,
    /// Messages the server sent via `window/showMessage` with severity
    /// `Error` (e.g. "No JDK found", failed build-system syncs). These mean
    /// the load pipeline broke and must fail the headless run.
    error_messages: Vec<String>,
}

pub(crate) struct HeadlessServer {
    sender: Sender<Message>,
    state: Arc<Mutex<ClientState>>,
    pending: Arc<Mutex<HashMap<RequestId, crossbeam_channel::Sender<Response>>>>,
    next_id: AtomicI32,
    server_handle: JoinHandle<anyhow::Result<()>>,
    reader_handle: JoinHandle<()>,
}

impl HeadlessServer {
    /// Spawns the real server over an in-memory connection and performs the
    /// initialize/initialized handshake against `workspace_root`.
    ///
    /// `select_build_system` is the action title to answer
    /// `window/showMessageRequest` with when the workspace layout turns out
    /// to be ambiguous; ambiguity is normally resolved before startup, so
    /// this is only a defensive fallback.
    pub fn start(
        workspace_root: &AbsPathBuf,
        select_build_system: Option<String>,
        java_home: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let root_uri: Uri = Uri::from_file_path(PathBuf::from(workspace_root.as_str()))
            .map_err(|_| anyhow::format_err!("failed to build URI for {workspace_root}"))?;

        let mut initialization_options = serde_json::json!({});
        if let Some(java_home) = java_home {
            // The key matches `ClientConfig`'s serde field name.
            initialization_options["java_home"] =
                serde_json::Value::String(java_home.to_string_lossy().into_owned());
        }

        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id() as i32),
            root_uri: Some(root_uri.clone()),
            capabilities: ClientCapabilities {
                window: Some(WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![PositionEncodingKind::UTF8]),
                    ..Default::default()
                }),
                // Refresh support stays off: the headless client pulls
                // diagnostics itself once the workspace is ready.
                workspace: None,
                ..Default::default()
            },
            initialization_options: Some(initialization_options),
            workspace_folders_initialize_params: WorkspaceFoldersInitializeParams::new(Some(
                vec![WorkspaceFolder {
                    uri: root_uri.clone(),
                    name: "workspace".to_string(),
                }]
                .into(),
            )),
            client_info: Some(ClientInfo {
                name: "caffeine-ls-cli".to_string(),
                version: Some(VERSION.to_string()),
            }),
            ..Default::default()
        };

        let (client_conn, server_conn) = Connection::memory();

        let server_handle = std::thread::Builder::new()
            .name("lsp-headless".to_string())
            .stack_size(STACK_SIZE)
            .spawn(move || serve::run(server_conn))?;

        let state = Arc::new(Mutex::new(ClientState::default()));
        let pending = Arc::new(Mutex::new(HashMap::new()));

        let reader_handle = std::thread::Builder::new()
            .name("lsp-headless-reader".to_string())
            .spawn({
                let sender = client_conn.sender.clone();
                let state = Arc::clone(&state);
                let pending = Arc::clone(&pending);
                move || {
                    read_loop(
                        client_conn.receiver,
                        sender,
                        state,
                        pending,
                        select_build_system,
                    )
                }
            })?;

        let this = Self {
            sender: client_conn.sender,
            state,
            pending,
            next_id: AtomicI32::new(1),
            server_handle,
            reader_handle,
        };

        let response = this.request("initialize", serde_json::to_value(params)?)?;
        if let Some(err) = response.error {
            anyhow::bail!("initialize failed: {}", err.message);
        }
        this.notify("initialized", serde_json::json!({}));

        Ok(this)
    }

    pub fn notify(&self, method: &str, params: serde_json::Value) {
        let notif = Notification::new(method.to_string(), params);
        self.sender.send(Message::Notification(notif)).ok();
    }

    /// Sends a request and waits for its response.
    pub fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<lsp_server::Response> {
        let id = RequestId::from(self.next_id.fetch_add(1, Ordering::SeqCst));
        let req = Request::new(id.clone(), method.to_string(), params);

        let (tx, rx) = crossbeam_channel::bounded(1);
        self.pending.lock().insert(id.clone(), tx);
        if self.sender.send(Message::Request(req)).is_err() {
            self.pending.lock().remove(&id);
            anyhow::bail!("server channel closed");
        }

        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(resp) => Ok(resp),
            Err(_) => {
                self.pending.lock().remove(&id);
                anyhow::bail!(
                    "no response to {method} within {}s",
                    REQUEST_TIMEOUT.as_secs()
                )
            }
        }
    }

    /// Waits until the workspace finished loading, signaled by the VFS scan
    /// progress token ending. By then every source file is loaded into the
    /// database and type queries see the full project graph. Fails early if
    /// the server reported an error message (e.g. no JDK found, failed
    /// build-system sync).
    pub fn wait_workspace_ready(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + WORKSPACE_LOAD_TIMEOUT;

        loop {
            {
                let state = self.state.lock();
                if let Some(message) = state.error_messages.first() {
                    anyhow::bail!("{message}");
                }
                if state.ended_tokens.iter().any(|t| t.starts_with("scan-")) {
                    return Ok(());
                }
            }

            if Instant::now() > deadline {
                anyhow::bail!(
                    "timed out after {}s waiting for the workspace to load \
                     (build system sync or file scan did not finish)",
                    WORKSPACE_LOAD_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Drains the error messages collected from `window/showMessage`.
    pub fn take_error_messages(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().error_messages)
    }

    /// Shuts the server down cleanly and waits for all threads to finish.
    pub fn shutdown(self) -> anyhow::Result<()> {
        let response = self.request("shutdown", serde_json::Value::Null)?;
        if let Some(err) = response.error {
            tracing::warn!("shutdown returned an error: {}", err.message);
        }
        // ExitNotification carries no params (`()`), which deserializes only
        // from JSON `null`.
        self.notify("exit", serde_json::Value::Null);

        drop(self.sender);

        let server_result = self
            .server_handle
            .join()
            .map_err(|_| anyhow::format_err!("server thread panicked"))?;
        let _ = self.reader_handle.join();
        server_result
    }
}

fn read_loop(
    receiver: Receiver<Message>,
    sender: Sender<Message>,
    state: Arc<Mutex<ClientState>>,
    pending: Arc<Mutex<HashMap<RequestId, crossbeam_channel::Sender<Response>>>>,
    select_build_system: Option<String>,
) {
    for msg in receiver {
        match msg {
            Message::Response(resp) => {
                let tx = pending.lock().remove(&resp.id);
                if let Some(tx) = tx {
                    tx.send(resp).ok();
                }
            }
            Message::Request(req) => {
                let response = answer_server_request(&req, select_build_system.as_deref());
                sender.send(Message::Response(response)).ok();
            }
            Message::Notification(notif) => handle_notification(&notif, &state),
        }
    }
}

/// Answers requests the server sends to the client:
///
/// - `window/workDoneProgress/create`: acknowledged so `$/progress`
///   notifications flow (the load-phase readiness signal depends on them).
/// - `window/showMessageRequest`: picks the pre-configured build system (or
///   cancels) so an unexpected selection dialog never stalls a headless run.
/// - anything else: rejected as unsupported.
fn answer_server_request(req: &Request, select_build_system: Option<&str>) -> Response {
    match req.method.as_str() {
        "window/workDoneProgress/create" => {
            if serde_json::from_value::<WorkDoneProgressCreateParams>(req.params.clone()).is_err() {
                return Response::new_err(
                    req.id.clone(),
                    ErrorCode::InvalidParams as i32,
                    "invalid workDoneProgress/create params".to_string(),
                );
            }
            Response::new_ok(req.id.clone(), serde_json::Value::Null)
        }
        "window/showMessageRequest" => {
            let actions =
                serde_json::from_value::<lsp_types::ShowMessageRequestParams>(req.params.clone())
                    .ok()
                    .and_then(|params| params.actions)
                    .unwrap_or_default();

            let chosen = match select_build_system {
                // The configured system is on offer: pick it.
                Some(title) if actions.iter().any(|item| item.title == title) => {
                    Some(title.to_string())
                }
                // Configured but not offered: cancel so the run fails with a
                // clear timeout instead of loading an unintended layout.
                Some(_) => None,
                // No configuration: fall back to the first option.
                None => actions.first().map(|item| item.title.clone()),
            };

            match chosen {
                Some(title) => Response::new_ok(
                    req.id.clone(),
                    serde_json::json!({"title": title, "properties": {}}),
                ),
                None => Response::new_ok(req.id.clone(), serde_json::Value::Null),
            }
        }
        other => Response::new_err(
            req.id.clone(),
            ErrorCode::MethodNotFound as i32,
            format!("headless client does not support {other}"),
        ),
    }
}

fn handle_notification(notif: &Notification, state: &Mutex<ClientState>) {
    match notif.method.as_str() {
        "$/progress" => {
            let Ok(params) = serde_json::from_value::<ProgressParams>(notif.params.clone()) else {
                return;
            };
            let token = match params.token {
                lsp_types::ProgressToken::Int(n) => n.to_string(),
                lsp_types::ProgressToken::String(s) => s,
            };
            let ended = params.value.get("kind").and_then(|kind| kind.as_str()) == Some("end");
            if ended {
                state.lock().ended_tokens.push(token);
            }
        }
        "window/showMessage" => {
            if let Ok(params) = serde_json::from_value::<ShowMessageParams>(notif.params.clone())
                && params.kind == MessageType::Error
            {
                state.lock().error_messages.push(params.message);
            }
        }
        _ => {}
    }
}
