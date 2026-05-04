use crate::handlers;
use crate::handlers::dispatch::{NotificationDispatcher, RequestDispatcher};
use crate::{config::ConfigErrors, project_model::ProjectWorkspace};
use lsp_types::notification::Notification as _;
use std::{sync::Arc, time::Instant};

use base_db::{LanguageId, SourceDatabase};
use crossbeam_channel::{Receiver, Sender, unbounded};
use ide::{Analysis, AnalysisHost};
use lsp_server::{ErrorCode, Notification, Request, Response};
use lsp_types::*;
use parking_lot::RwLock;

use vfs::{Vfs, VfsPath};

use crate::config::Config;

pub enum BackgroundTaskEvent {
    WorkspaceLoaded(anyhow::Result<ProjectWorkspace>),
    Progress(ProgressEvent),
    JdkIndexed,
    VfsLoaded,
    AsyncRequestCompleted {
        id: lsp_server::RequestId,
        result: Result<serde_json::Value, anyhow::Error>,
    },
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
        self.req_queue
            .incoming
            .register(req.id.clone(), (req.method.clone(), start_time));

        let mut dispatcher = RequestDispatcher {
            req: Some(req),
            global_state: self,
        };

        dispatcher
            .on::<request::Shutdown>(|s, _| {
                s.shutdown_requested = true;
                Ok(())
            })
            .on_async::<request::DocumentDiagnosticRequest>(handlers::on_diagnostic)
            // Add more requests here
            .finish();
    }

    pub(crate) fn handle_notification(&mut self, notif: Notification) {
        let mut dispatcher = NotificationDispatcher {
            notif: Some(notif),
            global_state: self,
        };

        dispatcher
            .on::<notification::Exit>(handlers::on_exit)
            .on::<notification::Cancel>(handlers::on_cancel)
            .on::<notification::DidOpenTextDocument>(handlers::on_did_open)
            .on::<notification::DidChangeTextDocument>(handlers::on_did_change)
            .on::<notification::DidSaveTextDocument>(handlers::on_did_save)
            .on::<notification::DidCloseTextDocument>(handlers::on_did_close)
            .finish();
    }

    // Helper to send response back to client
    pub(crate) fn handle_result<R>(
        &mut self,
        id: lsp_server::RequestId,
        result: anyhow::Result<R::Result>,
    ) where
        R: lsp_types::request::Request,
        R::Result: serde::Serialize,
    {
        match result {
            Ok(res) => self.respond_ok(id, res),
            Err(e) => self.respond_err(id, ErrorCode::InternalError, e.to_string()),
        }
    }

    /// Helper method to cleanly reject unhandled requests
    pub(crate) fn reply_not_implemented(&self, id: lsp_server::RequestId, method: String) {
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

                        // TODO: index jdk, index dependencies, index workspace
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
            BackgroundTaskEvent::AsyncRequestCompleted { id, result } => match result {
                Ok(resp_json) => {
                    self.respond_ok(id, resp_json);
                }
                Err(err) => {
                    self.respond_err(id, ErrorCode::InternalError, err.to_string());
                }
            },
        }
    }

    fn send(&self, msg: lsp_server::Message) {
        self.sender.send(msg).ok();
    }

    pub(crate) fn respond_ok<R>(&mut self, id: lsp_server::RequestId, result: R)
    where
        R: serde::Serialize,
    {
        if let Some((method, start)) = self.req_queue.incoming.complete(&id) {
            tracing::info!("handled {} in {:?}", method, start.elapsed());
        }
        let resp = lsp_server::Response::new_ok(id, result);
        self.send(resp.into());
    }

    pub(crate) fn respond_err(
        &mut self,
        id: lsp_server::RequestId,
        code: ErrorCode,
        message: String,
    ) {
        if let Some((method, _)) = self.req_queue.incoming.complete(&id) {
            tracing::error!("failed {}: {}", method, message);
        }
        let resp = lsp_server::Response::new_err(id, code as i32, message);
        self.send(resp.into());
    }

    pub(crate) fn notify<N>(&self, params: N::Params)
    where
        N: lsp_types::notification::Notification,
    {
        let notif = lsp_server::Notification::new(N::METHOD.to_string(), params);
        self.send(notif.into());
    }

    pub(crate) fn send_request<R>(&mut self, params: R::Params, handler: ReqHandler)
    where
        R: lsp_types::request::Request,
    {
        let req = self
            .req_queue
            .outgoing
            .register(R::METHOD.to_string(), params, handler);
        self.send(req.into());
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

    pub fn handle_vfs_change(&mut self) {
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

    pub fn reply_internal_error(&self, id: lsp_server::RequestId) {
        let response = Response::new_err(
            id,
            lsp_server::ErrorCode::InternalError as i32,
            "Internal Server Error".to_string(),
        );
        self.sender
            .send(lsp_server::Message::Response(response))
            .ok();
    }

    pub fn snapshot(&self) -> GlobalStateSnapshot {
        GlobalStateSnapshot {
            config: Arc::clone(&self.config),
            analysis: self.analysis_host.analysis(),
            vfs: Arc::clone(&self.vfs),
            workspaces: Arc::clone(&self.workspaces),
        }
    }

    pub(crate) fn cancel(&mut self, request_id: lsp_server::RequestId) {
        if let Some(response) = self.req_queue.incoming.cancel(request_id) {
            self.send(response.into());
        }
    }
}

pub struct GlobalStateSnapshot {
    pub(crate) config: Arc<Config>,
    pub(crate) analysis: Analysis,
    pub(crate) vfs: Arc<RwLock<Vfs>>,
    pub(crate) workspaces: Arc<Vec<ProjectWorkspace>>,
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
