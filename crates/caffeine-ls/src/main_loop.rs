use std::{collections::HashSet, sync::Arc, time::Instant};

use crossbeam_channel::Receiver;
use ide::delta::WorkspaceDelta;
use lsp_server::{Connection, ErrorCode, Notification, Request};
use lsp_types::{
    InitializedParams, MessageActionItem, MessageType, notification as notif, request,
};
use vfs::{AbsPathBuf, VfsEvent, VfsPath, loader::Directories};

use crate::{
    GlobalState,
    config::{Config, need_reload_workspace},
    global_state::{BackgroundTaskEvent, OutgoingRequest, ProgressEvent, ProgressState},
    handlers::{
        self,
        dispatch::{NotificationDispatcher, RequestDispatcher},
    },
};

pub fn main_loop(config: Config, connection: Connection) -> anyhow::Result<()> {
    tracing::info!("initial config: {:#?}", config);

    GlobalState::new(connection.sender, config).run(connection.receiver)
}

impl GlobalState {
    pub fn run(mut self, receiver: Receiver<lsp_server::Message>) -> anyhow::Result<()> {
        handlers::on_initialized(&mut self, InitializedParams {})
            .inspect_err(|err| tracing::error!(?err, "Failed to init lsp"))?;

        loop {
            crossbeam_channel::select! {
                recv(receiver) -> msg => {
                    match msg? {
                        lsp_server::Message::Request(req) => self.handle_request(req),
                        lsp_server::Message::Notification(notif) => self.handle_notification(notif),
                        lsp_server::Message::Response(resp) => self.complete_request(resp)
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
        let request_received = Instant::now();
        self.register_request(&req, request_received);

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
            .on::<notif::Exit>(handlers::on_exit)
            .on::<notif::Cancel>(handlers::on_cancel)
            .on::<notif::DidOpenTextDocument>(handlers::on_did_open)
            .on::<notif::DidChangeTextDocument>(handlers::on_did_change)
            .on::<notif::DidSaveTextDocument>(handlers::on_did_save)
            .on::<notif::DidCloseTextDocument>(handlers::on_did_close)
            .on::<notif::DidChangeWatchedFiles>(handlers::on_did_change_watched_files)
            .on::<notif::DidChangeConfiguration>(handlers::on_did_change_configuration)
            .finish();
    }

    pub(crate) fn handle_select_build_system_response(
        &mut self,
        resp: lsp_server::Response,
        root: AbsPathBuf,
        systems: Vec<project_model::BuildSystemType>,
        generation: u64,
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
                        generation,
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

    fn handle_background_task(&mut self, event: BackgroundTaskEvent) {
        match event {
            BackgroundTaskEvent::ProbeWorkspace { root, generation } => {
                let task_sender = self.task_sender.clone();

                // Perform fast, non-blocking file detection on the worker thread pool
                self.thread_pool.execute(move || {
                    match project_model::probe_workspace_layout(root.as_std_path()) {
                        project_model::ProbeResult::Single(system) => {
                            task_sender
                                .send(BackgroundTaskEvent::LoadWorkspace {
                                    root,
                                    system,
                                    generation,
                                })
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
                                .send(BackgroundTaskEvent::AmbiguousWorkspace {
                                    root,
                                    systems,
                                    generation,
                                })
                                .ok();
                        }
                        project_model::ProbeResult::None => {
                            tracing::error!(?root, "No supported Java project structures found.");
                        }
                    }
                });
            }

            BackgroundTaskEvent::AmbiguousWorkspace {
                root,
                systems,
                generation,
            } => {
                if self.workspace_generations.get(&root) != Some(&generation) {
                    return;
                }
                let actions: Vec<MessageActionItem> = systems
                    .iter()
                    .map(|sys| MessageActionItem {
                        title: sys.name().to_string(),
                        properties: std::collections::HashMap::new(),
                    })
                    .collect();

                self.show_message_request(
                    MessageType::WARNING,
                    format!(
                        "Multiple build systems detected at '{}'. Please select one:",
                        root.as_str()
                    ),
                    Some(actions),
                    OutgoingRequest::SelectBuildSystem {
                        root,
                        systems,
                        generation,
                    },
                );
            }

            BackgroundTaskEvent::LoadWorkspace {
                root,
                system,
                generation,
            } => {
                if self.workspace_generations.get(&root) != Some(&generation) {
                    return;
                }
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
                                .send(BackgroundTaskEvent::WorkspaceLoaded {
                                    graph,
                                    root,
                                    generation,
                                })
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

            BackgroundTaskEvent::WorkspaceLoaded {
                graph,
                root,
                generation,
            } => {
                if self.workspace_generations.get(&root) != Some(&generation) {
                    tracing::debug!(?root, generation, "Ignoring stale workspace sync");
                    return;
                }
                tracing::info!("Project configuration graph successfully loaded: {graph:#?}");

                let delta = self
                    .analysis_host
                    .apply_workspace_change(root.clone(), graph.clone());

                self.import_workspace(root.clone(), delta);

                self.report_progress(ProgressEvent {
                    token: format!("sync-{}", root.as_str()),
                    title: "Workspace indexing".to_string(),
                    message: Some(
                        "Project model loaded; indexing continues in background".to_string(),
                    ),
                    percentage: Some(100),
                    state: ProgressState::End,
                });
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

    fn import_workspace(&mut self, root: AbsPathBuf, delta: WorkspaceDelta) {
        if delta.is_empty() {
            tracing::info!("Skipping workspace import due empty workspace delta");
            return;
        }

        if let Some((_, index)) = self.analysis_host.index_for_path(&root) {
            for (library_id, _) in delta.libs.removed {
                index.symbols.detach_library(library_id);
            }
            for (library_id, library) in delta.libs.added {
                let index = Arc::clone(&index);
                let interner = self.analysis_host.interner();
                self.thread_pool.execute(move || {
                    index.symbols.attach_library(
                        &interner,
                        library_id,
                        library.path.as_std_path(),
                        |path| crate::indexing::parse_jar(path, &interner),
                    );
                });
            }
            for (_, sdk) in delta.sdks.removed {
                if let Some((_, library_id)) = index.sdk_libraries.remove(&sdk.id) {
                    index.symbols.detach_library(library_id);
                }
            }
            for (_, sdk) in delta.sdks.added {
                let artifact_path = sdk.home_path.join("lib/modules");
                let artifact_path = if artifact_path.exists() {
                    artifact_path
                } else {
                    sdk.home_path.join("lib/rt.jar")
                };
                let Ok(library_id) =
                    project_model::LibraryId::from_jar_path(artifact_path.as_std_path())
                else {
                    continue;
                };
                let index = Arc::clone(&index);
                index.sdk_libraries.insert(sdk.id, library_id);
                let interner = self.analysis_host.interner();
                let home = sdk.home_path.clone();
                self.thread_pool.execute(move || {
                    index.symbols.attach_library(
                        &interner,
                        library_id,
                        artifact_path.as_std_path(),
                        |_| crate::indexing::parse_jdk(home.as_std_path(), &interner),
                    );
                });
            }
        }

        // build vfs config
        let mut vfs_config = vfs::loader::Config {
            version: self.vfs_config_version,
            load: Vec::new(),
            watch: Vec::new(),
        };

        self.vfs_config_version += 1;

        for workspace in self.analysis_host.workspaces().values() {
            for project in workspace.projects.values() {
                for source_set in project.source_sets.values() {
                    let mut include = Vec::new();
                    include.extend(
                        source_set
                            .source_roots
                            .iter()
                            .cloned()
                            .map(vfs::VfsPath::Physical),
                    );
                    include.extend(
                        source_set
                            .generated_source_roots
                            .iter()
                            .cloned()
                            .map(vfs::VfsPath::Physical),
                    );
                    if include.is_empty() {
                        continue;
                    }
                    let entry_index = vfs_config.load.len();
                    vfs_config
                        .load
                        .push(vfs::loader::Entry::Directories(Directories {
                            extensions: vec!["java".into(), "kt".into(), "kts".into()],
                            include,
                            exclude: Vec::new(),
                        }));
                    vfs_config.watch.push(entry_index);
                }
            }
        }

        self.loader.handle.set_config(vfs_config);
    }

    /// Entry point to kick off initialization/probing workflows.
    /// Call this inside your `handlers::on_initialized` callback.
    pub fn trigger_workspace_probe(&mut self) {
        let roots = self.config.workspace_folders.clone();
        for root in roots {
            self.trigger_workspace_probe_for(root);
        }
    }

    pub fn trigger_workspace_probe_for(&mut self, root: AbsPathBuf) {
        let generation = self.workspace_generations.entry(root.clone()).or_insert(0);
        *generation += 1;
        self.task_sender
            .send(BackgroundTaskEvent::ProbeWorkspace {
                root,
                generation: *generation,
            })
            .ok();
    }

    fn handle_vfs_task(&mut self, task: vfs::loader::Message) {
        match task {
            vfs::loader::Message::Loaded {
                files,
                config_version,
            }
            | vfs::loader::Message::Changed {
                files,
                config_version,
            } => {
                if config_version + 1 != self.vfs_config_version {
                    tracing::debug!(config_version, "Ignoring stale VFS loader result");
                    return;
                }
                {
                    let mut vfs = self.vfs.write();
                    for (path, contents) in files {
                        vfs.set_disk_file_contents(path, contents);
                    }
                }
                self.handle_vfs_change();
            }
            vfs::loader::Message::Progress {
                n_done,
                config_version,
                ..
            } => {
                if config_version + 1 != self.vfs_config_version {
                    return;
                }
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

        for event in events {
            match event {
                VfsEvent::Created { id, path } => {
                    self.schedule_source_index(&vfs, id, path);
                }
                VfsEvent::Modified { id } => {
                    if let Some(path) = vfs.file_path(id).cloned() {
                        self.schedule_source_index(&vfs, id, path);
                    }
                }
                VfsEvent::Deleted { id, path } => {
                    let parse_cache = self.analysis_host.parse_cache();
                    parse_cache.remove(id);
                    if let VfsPath::Physical(path) = path
                        && let Some((root, index)) = self.analysis_host.index_for_path(&path)
                        && let Ok(relative) = path.strip_prefix(&root)
                    {
                        let key = relative.as_str();
                        index.lexical.remove_file(id);
                        index.symbols.remove_file(key);
                    }
                }
            };
        }
    }

    fn schedule_source_index(&self, vfs: &vfs::Vfs, id: vfs::FileId, path: VfsPath) {
        let VfsPath::Physical(path) = path else {
            return;
        };
        let Some(language) = path.extension().and_then(syntax::LanguageId::from_ext) else {
            return;
        };
        let Some((root, index)) = self.analysis_host.index_for_path(&path) else {
            return;
        };
        let Ok(relative) = path.strip_prefix(&root) else {
            return;
        };
        let file_key = relative.as_str().to_string();
        let Ok(content) = vfs.fetch_content(id) else {
            return;
        };
        let text = String::from_utf8_lossy(&content).to_string();
        let parse_cache = self.analysis_host.parse_cache();
        let revision = parse_cache.bump_revision(id);
        let interner = self.analysis_host.interner();

        self.thread_pool.execute(move || {
            let parsed = syntax::parse_file(language, &text, &interner);
            if parse_cache.is_cancelled(id, revision) {
                return;
            }

            let tokens = text
                .split(|character: char| !character.is_alphanumeric() && character != '_')
                .filter(|token| !token.is_empty())
                .map(ToOwned::to_owned)
                .collect::<HashSet<_>>();
            index.lexical.update_file_tokens(id, tokens);
            index
                .symbols
                .update_workspace_file(&interner, &file_key, parsed.stubs);
            parse_cache.update(
                id,
                ide::ParsedFile::new(revision, parsed.tree, parsed.errors),
            );
        });
    }
}
