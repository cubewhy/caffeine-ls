use crate::{config::ConfigErrors, project_model::ProjectWorkspace};
use line_index::{LineIndex, WideEncoding, WideLineCol};
use lsp_types::{notification::Notification as _, request::Request as _};
use std::panic::AssertUnwindSafe;
use std::{process, sync::Arc, time::Instant};

use base_db::{LanguageId, SourceDatabase};
use crossbeam_channel::{Receiver, Sender, unbounded};
use ide::AnalysisHost;
use lsp_server::{ErrorCode, Notification, Request, Response};
use lsp_types::*;
use parking_lot::RwLock;

use vfs::{Vfs, VfsPath};

use crate::config::Config;
use crate::lsp::diagnostics;

pub enum BackgroundTaskEvent {
    WorkspaceLoaded(anyhow::Result<ProjectWorkspace>),
    Progress(ProgressEvent),
    JdkIndexed,
    VfsLoaded,
}

pub struct ProgressEvent {
    pub token: String,
    pub title: String,
    pub message: Option<String>,
    pub percentage: Option<u32>,
    pub state: ProgressState,
}

pub enum ProgressState {
    Begin,
    Report,
    End,
}

pub(crate) struct Handle<H, C> {
    pub(crate) handle: H,
    pub(crate) receiver: C,
}

pub(crate) type ReqHandler = fn(&mut GlobalState, lsp_server::Response);
type ReqQueue = lsp_server::ReqQueue<(String, Instant), ReqHandler>;

pub struct GlobalState {
    sender: Sender<lsp_server::Message>,
    req_queue: ReqQueue,

    pub(crate) task_sender: Sender<BackgroundTaskEvent>,
    pub(crate) task_receiver: Receiver<BackgroundTaskEvent>,
    pub(crate) thread_pool: threadpool::ThreadPool,

    pub(crate) config: Arc<Config>,
    pub(crate) config_errors: Option<ConfigErrors>,
    pub(crate) analysis_host: AnalysisHost,
    pub(crate) workspaces: Arc<Vec<ProjectWorkspace>>,

    pub(crate) shutdown_requested: bool,

    // Vfs
    pub(crate) loader: Handle<Box<dyn vfs::loader::Handle>, Receiver<vfs::loader::Message>>,
    pub(crate) vfs: Arc<RwLock<Vfs>>,
}

impl GlobalState {
    pub fn new(sender: Sender<lsp_server::Message>, config: Config) -> Self {
        let loader = {
            let (sender, receiver) = unbounded::<vfs::loader::Message>();
            let handle: vfs_notify::NotifyHandle = vfs::loader::Handle::spawn(sender);
            let handle = Box::new(handle) as Box<dyn vfs::loader::Handle>;
            Handle { handle, receiver }
        };

        let (task_sender, task_receiver) = unbounded();

        let thread_pool = threadpool::ThreadPool::new(num_cpus::get());

        Self {
            sender,
            req_queue: ReqQueue::default(),

            task_sender,
            task_receiver,
            thread_pool,

            config: Arc::new(config),
            config_errors: None,

            analysis_host: AnalysisHost::default(),
            workspaces: Arc::new(Vec::new()),

            shutdown_requested: false,

            loader,
            vfs: Default::default(),
        }
    }

    pub fn run(mut self, receiver: Receiver<lsp_server::Message>) -> anyhow::Result<()> {
        loop {
            crossbeam_channel::select! {
                recv(receiver) -> msg => {
                    match msg? {
                        lsp_server::Message::Request(req) => self.handle_request(req),
                        lsp_server::Message::Notification(notif) => self.handle_notification(notif),
                        lsp_server::Message::Response(resp) => {
                            self.req_queue.outgoing.complete(resp.id);
                        }
                    }
                }
                recv(self.loader.receiver) -> task => {
                    self.handle_vfs_task(task?);
                }
                recv(self.task_receiver) -> task => {
                    self.handle_background_task(task?);
                }
            }
        }
    }

    pub(crate) fn handle_request(&mut self, req: Request) {
        let start_time = Instant::now();
        tracing::info!("handling request: {} ({})", req.method, req.id);

        self.req_queue
            .incoming
            .register(req.id.clone(), (req.method.clone(), start_time));

        // https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/
        match req.method.as_str() {
            request::Shutdown::METHOD => {
                self.shutdown_requested = true;

                // Shutdown needs special handling: return Ok(()) but don't exit the loop yet
                // (The exit happens when we receive the exit notification)
                let response = Response::new_ok(req.id, ());
                self.sender
                    .send(lsp_server::Message::Response(response))
                    .unwrap();
            }

            request::DocumentDiagnosticRequest::METHOD => {
                if let Ok(params) = serde_json::from_value(req.params.clone()) {
                    self.on_diagnostic(req.id, params);
                }
            }

            _ => {
                tracing::warn!("unhandled request: {}", req.method);
                self.reply_not_implemented(req.id, req.method);
            }
        }
    }

    pub(crate) fn handle_notification(&mut self, notif: Notification) {
        tracing::info!("handling notification: {}", notif.method);

        // https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/
        match notif.method.as_str() {
            notification::Cancel::METHOD => {
                if let Ok(params) = serde_json::from_value::<lsp_types::CancelParams>(notif.params)
                {
                    let id: lsp_server::RequestId = match params.id {
                        lsp_types::NumberOrString::Number(n) => n.into(),
                        lsp_types::NumberOrString::String(s) => s.into(),
                    };
                    // Mark the request as cancelled in our queue
                    self.req_queue.incoming.cancel(id);
                }
            }

            notification::Exit::METHOD => {
                // If shutdown was requested first, exit cleanly (0).
                // If the client abruptly sent exit, exit with an error code (1).
                if self.shutdown_requested {
                    process::exit(0);
                } else {
                    tracing::warn!("Exit notification received without prior shutdown request");
                    process::exit(1);
                }
            }

            notification::DidOpenTextDocument::METHOD => {
                if let Ok(params) = serde_json::from_value(notif.params) {
                    self.on_did_open(params);
                }
            }

            notification::DidChangeTextDocument::METHOD => {
                if let Ok(params) = serde_json::from_value(notif.params) {
                    self.on_did_change(params);
                }
            }

            notification::DidSaveTextDocument::METHOD => {
                if let Ok(params) = serde_json::from_value(notif.params) {
                    self.on_did_save(params);
                }
            }

            notification::DidCloseTextDocument::METHOD => {
                if let Ok(params) = serde_json::from_value(notif.params) {
                    self.on_did_close(params);
                }
            }

            _ => {
                tracing::warn!("unhandled notification: {}", notif.method);
            }
        }
    }

    /// Helper method to cleanly reject unhandled requests
    fn reply_not_implemented(&self, id: lsp_server::RequestId, method: String) {
        let response = Response::new_err(
            id,
            ErrorCode::MethodNotFound as i32,
            format!("Method not implemented: {}", method),
        );
        if let Err(err) = self.sender.send(lsp_server::Message::Response(response)) {
            tracing::error!("Failed to send MethodNotFound response: {}", err);
        }
    }

    pub(crate) fn handle_background_task(&mut self, event: BackgroundTaskEvent) {
        match event {
            BackgroundTaskEvent::WorkspaceLoaded(result) => {
                match result {
                    Ok(workspace) => {
                        tracing::info!("Workspace loaded successfully");

                        // Because self.workspaces is an Arc<Vec<_>>, we clone the inner
                        // vector, modify it, and wrap it in a new Arc.
                        let mut current_workspaces = self.workspaces.as_ref().to_vec();
                        current_workspaces.push(workspace);
                        self.workspaces = Arc::new(current_workspaces);

                        // TODO: Here you would typically trigger the VFS to start
                        // loading files based on the newly loaded workspace roots.
                    }
                    Err(err) => {
                        tracing::error!("Failed to load workspace: {:#}", err);
                        self.show_message(
                            MessageType::ERROR,
                            format!("Failed to load workspace: {}", err),
                        );
                    }
                }
            }

            BackgroundTaskEvent::Progress(progress) => {
                self.report_progress(progress);
            }

            BackgroundTaskEvent::JdkIndexed => {
                tracing::info!("JDK indexing completed");
                // TODO: preheat salsa
                // Trigger a re-analysis or update semantic tokens now that JDK types are available
            }

            BackgroundTaskEvent::VfsLoaded => {
                tracing::info!("VFS loading completed");
            }
        }
    }

    /// Helper to send window/showMessage notifications to the client
    fn show_message(&self, typ: MessageType, message: String) {
        let params = ShowMessageParams { typ, message };
        let notif = Notification::new(notification::ShowMessage::METHOD.to_string(), params);

        if let Err(e) = self.sender.send(lsp_server::Message::Notification(notif)) {
            tracing::error!("Failed to send ShowMessage notification: {}", e);
        }
    }

    /// Helper to translate internal ProgressEvent into LSP $/progress notifications
    fn report_progress(&self, event: ProgressEvent) {
        let token = ProgressToken::String(event.token.clone());

        let work_done = match event.state {
            ProgressState::Begin => WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: event.title,
                message: event.message,
                percentage: event.percentage,
                cancellable: Some(false),
            }),
            ProgressState::Report => WorkDoneProgress::Report(WorkDoneProgressReport {
                message: event.message,
                percentage: event.percentage,
                cancellable: Some(false),
            }),
            ProgressState::End => WorkDoneProgress::End(WorkDoneProgressEnd {
                message: event.message,
            }),
        };

        let params = ProgressParams {
            token,
            value: lsp_types::ProgressParamsValue::WorkDone(work_done),
        };

        let notif = Notification::new(notification::Progress::METHOD.to_string(), params);

        if let Err(e) = self.sender.send(lsp_server::Message::Notification(notif)) {
            tracing::error!("Failed to send Progress notification: {}", e);
        }
    }

    fn handle_vfs_task(&mut self, task: vfs::loader::Message) {
        match task {
            vfs::loader::Message::Loaded { files } | vfs::loader::Message::Changed { files } => {
                {
                    let mut vfs = self.vfs.write();
                    for (path, contents) in files {
                        let vfs_path: VfsPath = path.into();
                        vfs.set_file_contents(vfs_path, contents);
                    }
                }
                self.handle_vfs_change();
            }
            vfs::loader::Message::Progress { n_done, .. } => {
                if n_done == vfs::loader::LoadingProgress::Finished {
                    let _ = self.task_sender.send(BackgroundTaskEvent::VfsLoaded);
                }
            }
        }
    }

    fn handle_vfs_change(&mut self) {
        let mut vfs = self.vfs.write();

        let changes = vfs.take_changes();

        if changes.is_empty() {
            return;
        }

        let db = self.analysis_host.raw_database_mut();

        for (file_id, changed_file) in changes {
            let vfs_path = vfs.file_path(file_id);

            let language_id = vfs_path
                .name_and_extension()
                .and_then(|(_, ext)| ext)
                .map(LanguageId::from_extension)
                .unwrap_or(LanguageId::Unknown);

            if changed_file.is_created_or_deleted() || changed_file.is_modified() {
                let contents = match changed_file.change {
                    vfs::Change::Create(items, _) => Some(items),
                    vfs::Change::Modify(items, _) => Some(items),
                    vfs::Change::Delete => None,
                };
                if let Some(bytes) = contents {
                    let Ok(text) = String::from_utf8(bytes.to_vec()) else {
                        tracing::error!(?vfs_path, "failed to decode file content as utf8");
                        continue;
                    };
                    db.set_file(file_id, &text, language_id);
                } else {
                    db.remove_file(file_id);
                }
            }
        }
    }

    fn on_did_open(&mut self, params: DidOpenTextDocumentParams) {
        tracing::info!("didOpen {}", params.text_document.uri);
        let text = params.text_document.text;
        let content = text.clone().into_bytes();

        if let Some(vfs_path) = to_vfs_path(&params.text_document.uri) {
            self.vfs.write().set_file_contents(vfs_path, Some(content));
            self.handle_vfs_change();
        } else {
            tracing::error!("Failed to convert URI: {}", params.text_document.uri);
        }
    }

    fn on_did_change(&mut self, params: DidChangeTextDocumentParams) {
        tracing::debug!("didChange {}", params.text_document.uri);

        let Some(vfs_path) = to_vfs_path(&params.text_document.uri) else {
            return;
        };

        let file_id = {
            let vfs = self.vfs.read();
            let Some(file_id) = vfs.file_id(&vfs_path).map(|(id, _)| id) else {
                return;
            };
            file_id
        };

        // Fetch current text directly from Salsa DB
        let mut text = {
            let db = self.analysis_host.raw_database();
            let file_text = db.file_text(file_id);
            file_text.text(db).to_string()
        };

        // Apply edits
        for edit in params.content_changes {
            if let Some(range) = edit.range {
                let line_index = LineIndex::new(&text);

                let start_wide = WideLineCol {
                    line: range.start.line,
                    col: range.start.character,
                };
                let start_line_col = line_index.to_utf8(WideEncoding::Utf16, start_wide).unwrap();
                let start_offset = line_index.offset(start_line_col).unwrap();

                let end_wide = WideLineCol {
                    line: range.end.line,
                    col: range.end.character,
                };
                let end_line_col = line_index.to_utf8(WideEncoding::Utf16, end_wide).unwrap();
                let end_offset = line_index.offset(end_line_col).unwrap();

                let start = (u32::from(start_offset) as usize).min(text.len());
                let end = (u32::from(end_offset) as usize).max(start).min(text.len());

                text.replace_range(start..end, &edit.text);
            } else {
                text = edit.text; // Full edit
            }
        }

        self.vfs
            .write()
            .set_file_contents(vfs_path, Some(text.into_bytes()));

        self.handle_vfs_change();
    }

    fn on_did_save(&mut self, params: DidSaveTextDocumentParams) {
        tracing::info!("didSave {}", params.text_document.uri);

        if let Some(text) = params.text
            && let Some(vfs_path) = to_vfs_path(&params.text_document.uri)
        {
            self.vfs
                .write()
                .set_file_contents(vfs_path, Some(text.into_bytes()));
            self.handle_vfs_change();
        }
    }

    fn on_did_close(&mut self, params: DidCloseTextDocumentParams) {
        tracing::info!("didClose {}", params.text_document.uri);
        if let Some(vfs_path) = to_vfs_path(&params.text_document.uri) {
            self.vfs.write().set_file_contents(vfs_path, None);
            self.handle_vfs_change();
        }
    }

    fn on_diagnostic(&mut self, id: lsp_server::RequestId, params: DocumentDiagnosticParams) {
        tracing::info!(uri = ?params.text_document.uri, "request diagnostics");

        let Some(vfs_path) = to_vfs_path(&params.text_document.uri) else {
            self.reply_internal_error(id);
            return;
        };

        let file_id = {
            let vfs = self.vfs.read();
            let Some((id, _)) = vfs.file_id(&vfs_path) else {
                self.reply_internal_error(id);
                return;
            };
            id
        };

        // 1. Take a thread-safe snapshot of the current state
        // (Assuming AnalysisHost provides an immutable snapshot of the DB)
        let snapshot = self.analysis_host.analysis();
        let sender = self.sender.clone();

        // 2. Offload the heavy calculation to the thread pool
        self.thread_pool.execute(move || {
            // Run diagnostics on the background thread
            let unwind_safe_snapshot = AssertUnwindSafe(&snapshot);

            let diagnostics_result = std::panic::catch_unwind(move || {
                // Access the inner value of the wrapper
                let snapshot = *unwind_safe_snapshot;
                diagnostics::collect_diagnostics(snapshot.raw_database(), file_id)
            });

            match diagnostics_result {
                Ok(Ok(diagnostics)) => {
                    let result = DocumentDiagnosticReportResult::Report(
                        DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                            related_documents: None,
                            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                                result_id: None,
                                items: diagnostics,
                            },
                        }),
                    );

                    // 3. Send the response back through the LSP channel
                    let response = lsp_server::Response::new_ok(id, result);
                    sender.send(lsp_server::Message::Response(response)).ok();
                }
                _ => {
                    tracing::error!("Failed to collect diagnostics or task panicked");
                    let response = lsp_server::Response::new_err(
                        id,
                        lsp_server::ErrorCode::InternalError as i32,
                        "Internal error computing diagnostics".to_string(),
                    );
                    sender.send(lsp_server::Message::Response(response)).ok();
                }
            }
        });
    }

    fn reply_internal_error(&self, id: lsp_server::RequestId) {
        let response = Response::new_err(
            id,
            lsp_server::ErrorCode::InternalError as i32,
            "Internal Server Error".to_string(),
        );
        self.sender
            .send(lsp_server::Message::Response(response))
            .ok();
    }
}

/// Returns `None` if the file was excluded.
pub(crate) fn vfs_path_to_file_id(
    vfs: &vfs::Vfs,
    vfs_path: &VfsPath,
) -> anyhow::Result<Option<vfs::FileId>> {
    let (file_id, excluded) = vfs
        .file_id(vfs_path)
        .ok_or_else(|| anyhow::anyhow!("file not found: {vfs_path}"))?;
    match excluded {
        vfs::FileExcluded::Yes => Ok(None),
        vfs::FileExcluded::No => Ok(Some(file_id)),
    }
}

fn to_vfs_path(uri: &lsp_types::Url) -> Option<VfsPath> {
    let path_buf = uri.to_file_path().ok()?;
    // Avoid canonicalize() here if your VFS loader didn't also canonicalize
    Some(VfsPath::new_real_path(
        path_buf.to_string_lossy().to_string(),
    ))
}
