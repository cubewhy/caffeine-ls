use crate::config::{ConfigErrors, need_reload_workspace};
use crate::handlers::dispatch::{NotificationDispatcher, RequestDispatcher};
use crate::handlers::{self, on_initialized};
use lsp_types::notification::Notification as _;
use project_model::WorkspaceGraph;
use std::{sync::Arc, time::Instant};
use vfs::loader::NotifyHandle;
use vfs::virtual_path::{JarHandler, JimageHandler};

use crossbeam_channel::{Receiver, Sender, unbounded};
use ide::{Analysis, AnalysisHost};
use lsp_server::{ErrorCode, Notification, Request, Response};
use lsp_types::*;
use parking_lot::RwLock;

use vfs::{AbsPathBuf, Vfs, VfsEvent, VfsPath};

use crate::config::Config;

pub enum BackgroundTaskEvent {
    ProbeWorkspace {
        root: AbsPathBuf,
    },
    AmbiguousWorkspace {
        root: AbsPathBuf,
        systems: Vec<project_model::BuildSystemType>,
    },
    LoadWorkspace {
        root: AbsPathBuf,
        system: project_model::BuildSystemType,
    },
    WorkspaceLoaded {
        root: AbsPathBuf,
        graph: WorkspaceGraph,
    },
    Progress(ProgressEvent),
    VfsLoaded,
    AsyncRequestCompleted {
        id: lsp_server::RequestId,
        result: Result<serde_json::Value, anyhow::Error>,
    },
    NotifyUser {
        typ: lsp_types::MessageType,
        message: String,
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
pub(crate) enum OutgoingRequest {
    Generic(ReqHandler),
    SelectBuildSystem {
        root: AbsPathBuf,
        systems: Vec<project_model::BuildSystemType>,
    },
    WorkspaceConfiguration,
}

type ReqQueue = lsp_server::ReqQueue<(String, Instant), OutgoingRequest>;

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
    pub(crate) vfs_config_version: u32,
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
            vfs_config_version: 0,
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
                            self.handle_response(resp);
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
            .on::<notification::DidChangeWatchedFiles>(handlers::on_did_change_watched_files)
            .on::<notification::DidChangeConfiguration>(handlers::on_did_change_configuration)
            .finish();
    }

    fn handle_response(&mut self, resp: Response) {
        let Some(outgoing_req) = self.req_queue.outgoing.complete(resp.id.clone()) else {
            tracing::warn!(?resp.id, "Received response for an unknown or untracked request");
            return;
        };

        if let Some(err) = &resp.error {
            tracing::error!(?resp.id, "Client returned error response: {:?}", err);
            return;
        }

        match outgoing_req {
            OutgoingRequest::SelectBuildSystem { root, systems } => {
                self.handle_select_build_system_response(resp, root, systems);
            }

            OutgoingRequest::WorkspaceConfiguration => {
                self.handle_config_response(resp);
            }

            OutgoingRequest::Generic(handler) => {
                handler(self, resp);
            }
        }
    }

    fn handle_select_build_system_response(
        &mut self,
        resp: lsp_server::Response,
        root: AbsPathBuf,
        systems: Vec<project_model::BuildSystemType>,
    ) {
        let Some(result_json) = resp.result else {
            tracing::warn!(
                ?root,
                "Build system selection dialog dismissed without choice."
            );
            return;
        };

        let selected_item: Option<MessageActionItem> =
            serde_json::from_value(result_json).unwrap_or_default();

        if let Some(item) = selected_item {
            let chosen_system = systems.iter().find(|sys| sys.name() == item.title);

            if let Some(system) = chosen_system {
                tracing::info!(?root, ?system, "User selected build system explicitly.");

                self.task_sender
                    .send(BackgroundTaskEvent::LoadWorkspace {
                        root,
                        system: *system,
                    })
                    .ok();
            } else {
                tracing::error!(
                    ?root,
                    "Client returned an unrecognized action title: '{}'",
                    item.title
                );
            }
        } else {
            tracing::warn!(?root, "User cancelled the build system selection prompt.");
        }
    }

    fn handle_config_response(&mut self, resp: lsp_server::Response) {
        // FIXME: the response structure may need manual confirm on real client behaviors.
        tracing::info!("Received configuration response from client");

        if let Some(err) = resp.error {
            tracing::error!("Client failed to return configuration: {:?}", err);
            return;
        }

        let Some(result) = resp.result else { return };

        let mut response_values: Vec<serde_json::Value> =
            serde_json::from_value(result).unwrap_or_default();
        if response_values.is_empty() {
            tracing::warn!("Empty configuration array received from client");
            return;
        }
        let raw_settings = response_values.remove(0);

        let mut change = crate::config::ConfigChange::default();
        change.change_client_config(raw_settings);

        let old_config = Arc::clone(&self.config);
        let current_config = (*old_config).clone();

        let (new_config, errors, config_changed) = current_config.apply_change(change);

        if !errors.is_empty() {
            tracing::warn!("{}", errors);
            self.show_message(lsp_types::MessageType::WARNING, errors.to_string());
            self.config_errors = Some(errors);
        } else {
            self.config_errors = None;
        }

        if config_changed {
            let need_reload = need_reload_workspace(&old_config, &new_config);
            self.config = Arc::new(new_config);
            tracing::info!("Global state configuration updated successfully.");

            if need_reload {
                tracing::info!("Reloading workspace due config change...");
                self.trigger_workspace_probe();
            }
        } else {
            tracing::info!("Configuration received but no effective changes detected.");
        }
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
        self.send(lsp_server::Message::Response(response));
    }

    /// Entry point to kick off initialization/probing workflows.
    /// Call this inside your `handlers::on_initialized` callback.
    pub fn trigger_workspace_probe(&self) {
        for root in self.config.workspace_folders.iter() {
            self.task_sender
                .send(BackgroundTaskEvent::ProbeWorkspace { root: root.clone() })
                .ok();
        }
    }

    fn handle_background_task(&mut self, event: BackgroundTaskEvent) {
        match event {
            BackgroundTaskEvent::ProbeWorkspace { root } => {
                let task_sender = self.task_sender.clone();

                // Perform fast, non-blocking file detection on the worker thread pool
                self.thread_pool.execute(move || {
                    match project_model::probe_workspace_layout(root.as_std_path()) {
                        project_model::ProbeResult::Single(system) => {
                            task_sender
                                .send(BackgroundTaskEvent::LoadWorkspace { root, system })
                                .ok();
                        }
                        project_model::ProbeResult::Ambiguous(systems) => {
                            // Convert the decoupled enum choices directly into an interactive
                            // LSP UI selection prompt on the main server actor loop
                            tracing::warn!(
                                ?root,
                                ?systems,
                                "Ambiguous build configurations discovered."
                            );
                            task_sender
                                .send(BackgroundTaskEvent::AmbiguousWorkspace { root, systems })
                                .ok();
                        }
                        project_model::ProbeResult::None => {
                            tracing::error!(?root, "No supported Java project structures found.");
                        }
                    }
                });
            }

            BackgroundTaskEvent::AmbiguousWorkspace { root, systems } => {
                let actions: Vec<MessageActionItem> = systems
                    .iter()
                    .map(|sys| MessageActionItem {
                        title: sys.name().to_string(),
                        properties: std::collections::HashMap::new(),
                    })
                    .collect();

                let params = ShowMessageRequestParams {
                    typ: MessageType::WARNING,
                    message: format!(
                        "Multiple build systems detected at '{}'. Please select one:",
                        root.as_str()
                    ),
                    actions: Some(actions),
                };

                self.send_request::<request::ShowMessageRequest>(
                    params,
                    OutgoingRequest::SelectBuildSystem { root, systems },
                );
            }

            BackgroundTaskEvent::LoadWorkspace { root, system } => {
                let progress_token = format!("sync-{}", root.as_str());
                self.report_progress(ProgressEvent {
                    token: progress_token.clone(),
                    title: format!("Syncing Project Layout ({:?})", system),
                    message: Some("Extracting build graph metadata...".to_string()),
                    percentage: Some(15),
                    state: ProgressState::Begin,
                });

                let task_sender = self.task_sender.clone();
                let Some(java_home) = self.config.get_java_home() else {
                    self.show_message(MessageType::ERROR, "No JDK found".to_string());
                    tracing::error!("No JDK found in JAVA_HOME");
                    return;
                };

                self.thread_pool.execute(move || {
                    match system.get_executor().sync(root.as_std_path(), &java_home) {
                        Ok(graph) => {
                            task_sender
                                .send(BackgroundTaskEvent::WorkspaceLoaded { graph, root })
                                .ok();
                        }
                        Err(err) => {
                            tracing::error!(?root, "Metadata compilation failure: {}", err);
                            task_sender
                                .send(BackgroundTaskEvent::NotifyUser {
                                    typ: MessageType::ERROR,
                                    message: format!("Failed to receive project metadata: {err}"),
                                })
                                .ok();
                        }
                    }
                });
            }

            BackgroundTaskEvent::WorkspaceLoaded { graph, root } => {
                tracing::info!("Project configuration graph successfully loaded: {graph:#?}");

                let delta = self
                    .analysis_host
                    .apply_workspace_change(root, graph.clone());

                tracing::info!("workspace delta: {:#?}", delta);
                // TODO: index with delta

                let mut load_entries = Vec::new();
                let mut watch_indices = Vec::new();

                for (_, project) in graph.projects.iter() {
                    let mut include_paths = Vec::new();
                    // Pre-emptively exclude build artifact targets to avoid directory crawling cycles
                    // NOTE: This .unwrap will always success since root_path is already absolute
                    let exclude_paths = vec![
                        VfsPath::Physical(project.root_path.join("build").try_into().unwrap()),
                        VfsPath::Physical(project.root_path.join("target").try_into().unwrap()),
                    ];
                    for (_, source_set) in project.source_sets.iter() {
                        for root in &source_set.source_roots {
                            include_paths.push(VfsPath::Physical(root.clone()));
                        }
                        for root in &source_set.generated_source_roots {
                            include_paths.push(VfsPath::Physical(root.clone()));
                        }
                    }

                    if !include_paths.is_empty() {
                        let directories = vfs::loader::Directories {
                            extensions: vec!["java".to_string()],
                            include: include_paths,
                            exclude: exclude_paths,
                        };

                        let current_idx = load_entries.len();
                        load_entries.push(vfs::loader::Entry::Directories(directories));

                        // Active source code directories must be tracked by file-system change watchers
                        watch_indices.push(current_idx);
                    }
                }

                // Package external compiled dependencies (.jar files) into a dedicated Entry block
                // let mut external_jars = Vec::new();
                // for (_, jar_path) in graph.library_paths.iter() {
                //     external_jars.push(VfsPath::Physical(jar_path.clone()));
                // }
                //
                // if !external_jars.is_empty() {
                //     // Append external libraries to the loading array
                //     load_entries.push(vfs::loader::Entry::Files(external_jars));
                //     // NOTE: We deliberately OMIT this index from the `watch_indices` array.
                //     // External dependency jars inside global .gradle or .m2 caches are immutable,
                //     // so watching them would waste valuable system kernel file handles.
                // }

                self.vfs_config_version += 1;
                let vfs_config = vfs::loader::Config {
                    version: self.vfs_config_version,
                    load: load_entries,
                    watch: watch_indices,
                };

                self.loader.handle.set_config(vfs_config);
                tracing::info!(
                    "VFS file system loader configured with structural directory roots."
                );
            }

            BackgroundTaskEvent::Progress(progress) => {
                self.report_progress(progress);
            }

            BackgroundTaskEvent::VfsLoaded => {
                tracing::info!("VFS file system synchronization completed.");
            }

            BackgroundTaskEvent::AsyncRequestCompleted { id, result } => match result {
                Ok(resp_json) => {
                    self.respond_ok(id, resp_json);
                }
                Err(err) => {
                    self.respond_err(id, ErrorCode::InternalError, err.to_string());
                }
            },
            BackgroundTaskEvent::NotifyUser { typ, message } => self.show_message(typ, message),
        }
    }

    #[track_caller]
    fn send(&self, msg: lsp_server::Message) {
        self.sender.send(msg).unwrap();
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

    pub(crate) fn send_request<R>(&mut self, params: R::Params, state: OutgoingRequest)
    where
        R: lsp_types::request::Request,
    {
        let req = self
            .req_queue
            .outgoing
            .register(R::METHOD.to_string(), params, state);
        self.send(req.into());
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
                    self.task_sender.send(BackgroundTaskEvent::VfsLoaded).ok();
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

        // let mut tasks_to_spawn = Vec::new();

        for event in events {
            match event {
                VfsEvent::Created { id, .. } | VfsEvent::Modified { id } => {
                    // let new_rev = self.analysis_host.parse_cache.bump_revision(id);
                    // tasks_to_spawn.push((id, new_rev));
                }
                VfsEvent::Deleted { id } => {
                    // self.analysis_host.remove_file(id);
                }
            };
        }

        // if !tasks_to_spawn.is_empty() {
        // self.spawn_parsing_task(tasks_to_spawn);
        // }
    }

    fn spawn_parsing_task(&self, tasks: Vec<(vfs::FileId, u64)>) {
        // let vfs = Arc::clone(&self.vfs);
        // let task_sender = self.task_sender.clone();
        // let analysis = self.analysis_host.snapshot();
        //
        // self.thread_pool.execute(move || {
        //     let graph = analysis.workspace_graph;
        //     let parse_cache = analysis.parse_cache;
        //     for (file_id, task_revision) in tasks {
        //         if parse_cache.is_cancelled(file_id, task_revision) {
        //             continue;
        //         }
        //
        //         let (text, file_path) = {
        //             let vfs_read = vfs.read();
        //             let Some(file_path) = vfs_read.file_path(file_id).cloned() else {
        //                 tracing::error!("Failed to get vfs path for file {file_id:?}");
        //                 continue;
        //             };
        //             match vfs_read.fetch_content(file_id) {
        //                 Ok(bytes) => (String::from_utf8_lossy(&bytes).to_string(), file_path),
        //                 Err(_err) => continue,
        //             }
        //         };
        //
        //         let physic_path = match &file_path {
        //             vfs::VfsPath::Physical(path) => path,
        //             vfs::VfsPath::Virtual(url) => {
        //                 tracing::error!(?url, "Project dir cannot be virtual");
        //                 continue;
        //             }
        //         };
        //
        //         let Some(project) = graph.resolve_project_for_path(physic_path) else {
        //             tracing::error!("Failed to resolve project for file {file_path:?}");
        //             continue;
        //         };
        //
        //         let Some(lang) = file_path.extension().and_then(LanguageId::from_ext) else {
        //             continue;
        //         };
        //
        //         let parse_result =
        //             syntax::parse_file(lang, &text, analysis.symbol_index.get_interner());
        //
        //         if parse_cache.is_cancelled(file_id, task_revision) {
        //             continue;
        //         }
        //
        //         parse_cache.update(
        //             file_id,
        //             ParsedFile::new(parse_result.tree, parse_result.errors),
        //         );
        //         analysis.symbol_index.update_workspace_file(
        //             project.id,
        //             file_id,
        //             parse_result.stubs,
        //         );
        //     }
        //
        //     let _ = task_sender.send(BackgroundTaskEvent::VfsLoaded);
        // });
    }

    pub fn reply_internal_error(&self, id: lsp_server::RequestId) {
        let response = Response::new_err(
            id,
            lsp_server::ErrorCode::InternalError as i32,
            "Internal Server Error".to_string(),
        );
        self.send(lsp_server::Message::Response(response))
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
