use crate::config::ConfigErrors;
use crate::handlers::dispatch::{NotificationDispatcher, RequestDispatcher};
use crate::handlers::{self, on_initialized};
use lsp_types::notification::Notification as _;
use project_model::WorkspaceGraph;
use std::{sync::Arc, time::Instant};
use syntax::LanguageId;
use vfs::loader::NotifyHandle;
use vfs::virtual_path::{JarHandler, JimageHandler};

use crossbeam_channel::{Receiver, Sender, unbounded};
use ide::{Analysis, AnalysisHost, ParsedFile};
use lsp_server::{ErrorCode, Notification, Request, Response};
use lsp_types::*;
use parking_lot::RwLock;

use vfs::{Vfs, VfsEvent};

use crate::config::Config;

pub enum BackgroundTaskEvent {
    WorkspaceLoaded(WorkspaceGraph),
    LoadWorkspace,
    Progress(ProgressEvent),
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

    pub(crate) shutdown_requested: bool,

    // Vfs
    pub(crate) loader: Handle<Box<dyn vfs::loader::Handle>, Receiver<vfs::loader::Message>>,
    pub(crate) vfs: Arc<RwLock<Vfs>>,
}

impl GlobalState {
    pub fn new(sender: Sender<lsp_server::Message>, config: Config) -> Self {
        let (task_sender, task_receiver) = unbounded();

        let thread_pool = threadpool::ThreadPool::new(num_cpus::get());

        let mut vfs = Vfs::new();
        vfs.register_handler(JarHandler::default());
        vfs.register_handler(JimageHandler::default());

        let loader = {
            let (sender, receiver) = unbounded();
            let handle: NotifyHandle = vfs::loader::Handle::spawn(sender);

            let handle = Box::new(handle) as Box<dyn vfs::loader::Handle>;

            Handle { handle, receiver }
        };

        let cache_dir = config.get_cache_dir();

        Self {
            sender,
            req_queue: ReqQueue::default(),

            task_sender,
            task_receiver,
            thread_pool,

            config: Arc::new(config),
            config_errors: None,

            analysis_host: AnalysisHost::new(&cache_dir),

            shutdown_requested: false,

            loader,
            vfs: Arc::new(RwLock::new(vfs)),
        }
    }

    pub fn run(mut self, receiver: Receiver<lsp_server::Message>) -> anyhow::Result<()> {
        on_initialized(&mut self, InitializedParams {})
            .inspect_err(|err| tracing::error!(?err, "Failed to init lsp"))?;

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

    fn handle_request(&mut self, req: Request) {
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

    fn handle_notification(&mut self, notif: Notification) {
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

    fn handle_background_task(&mut self, event: BackgroundTaskEvent) {
        match event {
            BackgroundTaskEvent::WorkspaceLoaded(graph) => {
                tracing::info!(?graph, "Workspace graph loaded successfully");
                self.analysis_host.add_workspace(graph);
            }
            BackgroundTaskEvent::LoadWorkspace => {
                self.thread_pool.execute(move || {
                    for root in self.config.workspace_folders.iter() {
                        let model = match project_model::load_workspace_from_disk(root) {
                            Ok(model) => model,
                            Err(err) => {
                                tracing::error!(root, "Failed to load workspace: {}", e);
                                self.show_message(
                                    MessageType::ERROR,
                                    format!("Build sync failed: {e}"),
                                );
                                return;
                            }
                        };
                        self.spawn_task(BackgroundTaskEvent::WorkspaceLoaded(model));
                    }
                });
            }
            BackgroundTaskEvent::Progress(progress) => {
                self.report_progress(progress);
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

    pub(crate) fn spawn_task(&self, task: BackgroundTaskEvent) {
        if let Err(e) = self.task_sender.send(task) {
            tracing::error!(
                ?e,
                "Failed to spawn task, this is an internal error, please report this to developers!"
            );
        }
    }

    /// Helper to send window/showMessage notifications to the client
    fn show_message(&self, typ: MessageType, message: String) {
        let params = ShowMessageParams { typ, message };
        self.notify::<notification::ShowMessage>(params);
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
                        vfs.set_file_contents(path, contents);
                    }
                }
                self.handle_vfs_change();
            }
            vfs::loader::Message::Progress { n_done, .. } => {
                if n_done == vfs::loader::LoadingProgress::Finished {
                    self.spawn_task(BackgroundTaskEvent::VfsLoaded);
                }
            }
        }
    }

    pub fn handle_vfs_change(&mut self) {
        let mut vfs = self.vfs.write();
        let events = vfs.take_events();

        if events.is_empty() {
            return;
        }

        let mut tasks_to_spawn = Vec::new();

        for event in events {
            match event {
                VfsEvent::Created { id, .. } | VfsEvent::Modified { id } => {
                    let new_rev = self.analysis_host.parse_cache.bump_revision(id);
                    tasks_to_spawn.push((id, new_rev));
                }
                VfsEvent::Deleted { id } => {
                    self.analysis_host.remove_file(id);
                }
            };
        }

        if !tasks_to_spawn.is_empty() {
            self.spawn_parsing_task(tasks_to_spawn);
        }
    }

    fn spawn_parsing_task(&self, tasks: Vec<(vfs::FileId, u64)>) {
        let vfs = Arc::clone(&self.vfs);
        let task_sender = self.task_sender.clone();
        let analysis = self.analysis_host.snapshot();

        self.thread_pool.execute(move || {
            let graph = analysis.workspace_graph;
            let parse_cache = analysis.parse_cache;
            for (file_id, task_revision) in tasks {
                if parse_cache.is_cancelled(file_id, task_revision) {
                    continue;
                }

                let (text, file_path) = {
                    let vfs_read = vfs.read();
                    let Some(file_path) = vfs_read.file_path(file_id).cloned() else {
                        tracing::error!("Failed to get vfs path for file {file_id:?}");
                        continue;
                    };
                    match vfs_read.fetch_content(file_id) {
                        Ok(bytes) => (String::from_utf8_lossy(&bytes).to_string(), file_path),
                        Err(_err) => continue,
                    }
                };

                let physic_path = match &file_path {
                    vfs::VfsPath::Physical(path) => path,
                    vfs::VfsPath::Virtual(url) => {
                        tracing::error!(?url, "Project dir cannot be virtual");
                        continue;
                    }
                };

                let Some(project) = graph.resolve_project_for_path(physic_path) else {
                    tracing::error!("Failed to resolve project for file {file_path:?}");
                    continue;
                };

                let Some(lang) = file_path.extension().and_then(LanguageId::from_ext) else {
                    continue;
                };

                let parse_result =
                    syntax::parse_file(lang, &text, analysis.symbol_index.get_interner());

                if parse_cache.is_cancelled(file_id, task_revision) {
                    continue;
                }

                parse_cache.update(
                    file_id,
                    ParsedFile::new(parse_result.tree, parse_result.errors),
                );
                analysis.symbol_index.update_workspace_file(
                    project.library_id,
                    file_id,
                    parse_result.stubs,
                );
            }

            let _ = task_sender.send(BackgroundTaskEvent::VfsLoaded);
        });
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
            analysis: self.analysis_host.snapshot(),
            vfs: Arc::clone(&self.vfs),
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
}
