use std::{
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use camino::Utf8PathBuf;
use crossbeam_channel::Receiver;
use hir::{Classpath, ClasspathEntry as HirClasspathEntry, LibraryInfo, LibraryKind, SourceSetId};
use ide_db::base_db::{FileChange, SourceRoot, SourceRootId, salsa::Cancelled};
use lsp_server::{Connection, ErrorCode, Notification, Request};
use lsp_types::*;
use project_model::{ClasspathEntry, SyncError};
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId};

use crate::{
    GlobalState,
    config::Config,
    diagnostics as diagnostics_store,
    global_state::{BackgroundTaskEvent, OutgoingRequest, ProgressEvent, ProgressState},
    handlers::{
        self,
        dispatch::{NotificationDispatcher, RequestDispatcher},
    },
    line_index::LineEndings,
};

const OPEN_BUILD_TOOL_LOG_ACTION: &str = "Open Build Tool Log";

/// Minimum interval between consecutive progress bar flashes of build tool
/// output lines.
const BUILD_TOOL_FLASH_INTERVAL: Duration = Duration::from_millis(150);

/// Maximum length of a build tool line flashed in the progress bar.
const BUILD_TOOL_FLASH_MAX_LEN: usize = 200;

pub fn main_loop(config: Config, connection: Connection) -> anyhow::Result<()> {
    tracing::info!("initial config: {:#?}", config);

    GlobalState::new(connection.sender, config).run(connection.receiver)
}

impl GlobalState {
    pub fn run(mut self, receiver: Receiver<lsp_server::Message>) -> anyhow::Result<()> {
        handlers::on_initialized(&mut self, InitializedParams {})
            .inspect_err(|err| tracing::error!(?err, "Failed to init lsp"))?;

        loop {
            let refresh_rx = self.refresh_timer.as_ref().unwrap_or(&self.never_rx);
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
                recv(refresh_rx) -> _ => {
                    self.collect_refresh_timer();
                }
            }

            self.process_changes();

            // Async requests cancelled by the write just applied can now be
            // re-run on a fresh snapshot.
            self.run_pending_requests();

            if self.exit_requested {
                break Ok(());
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
            .on::<ShutdownRequest>(|s, _| {
                s.shutdown_requested = true;
                Ok(())
            })
            .on_async::<DocumentDiagnosticRequest>(handlers::on_diagnostic)
            .on_async::<WorkspaceDiagnosticRequest>(handlers::on_workspace_diagnostic)
            .on_async::<DocumentSymbolRequest>(handlers::on_document_symbol)
            .on_async::<WorkspaceSymbolRequest>(handlers::on_workspace_symbol)
            .on_async::<DefinitionRequest>(handlers::on_goto_definition)
            .on_async::<HoverRequest>(handlers::on_hover)
            // Add more requests here
            .finish();
    }

    fn handle_notification(&mut self, notif: Notification) {
        let mut dispatcher = NotificationDispatcher {
            notif: Some(notif),
            global_state: self,
        };

        dispatcher
            .on::<ExitNotification>(handlers::on_exit)
            .on::<CancelNotification>(handlers::on_cancel)
            .on::<DidOpenTextDocumentNotification>(handlers::on_did_open)
            .on::<DidChangeTextDocumentNotification>(handlers::on_did_change)
            .on::<DidSaveTextDocumentNotification>(handlers::on_did_save)
            .on::<DidCloseTextDocumentNotification>(handlers::on_did_close)
            .on::<DidChangeWatchedFilesNotification>(handlers::on_did_change_watched_files)
            .on::<DidChangeConfigurationNotification>(handlers::on_did_change_configuration)
            .finish();
    }

    pub(crate) fn handle_select_build_system_response(
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

    /// Handles the client's answer to the "Open Build Tool Log" action shown
    /// after a failed sync, opening the saved log file in the editor.
    pub(crate) fn handle_open_build_tool_log_response(
        &mut self,
        resp: lsp_server::Response,
        log_file: PathBuf,
    ) {
        let Some(result_json) = resp.result else {
            tracing::warn!("Build tool log dialog dismissed without choice.");
            return;
        };

        let selected_item: Option<MessageActionItem> =
            serde_json::from_value(result_json).unwrap_or_default();

        let Some(item) = selected_item else {
            tracing::warn!("User cancelled the build tool log prompt.");
            return;
        };

        if item.title != OPEN_BUILD_TOOL_LOG_ACTION {
            tracing::warn!(
                "Client returned an unrecognized action title: '{}'",
                item.title
            );
            return;
        }

        let Ok(uri) = Uri::from_file_path(&log_file) else {
            tracing::error!(?log_file, "Failed to build URI for build tool log file");
            return;
        };

        let show_document_supported = self
            .config
            .client_capabilities
            .window
            .as_ref()
            .and_then(|w| w.show_document.as_ref())
            .map(|caps| caps.support)
            .unwrap_or(false);

        if show_document_supported {
            self.send_request::<ShowDocumentRequest>(
                ShowDocumentParams {
                    uri,
                    external: None,
                    take_focus: Some(true),
                    selection: None,
                },
                OutgoingRequest::Generic(|_, _| {}),
            );
        } else {
            self.show_message(
                MessageType::Info,
                format!("Build tool log saved to: {}", log_file.display()),
            );
        }
    }

    /// Called when a build tool backed workspace sync failed: offers the user
    /// a button to open the streamed build tool log (only when the tool
    /// produced output).
    fn handle_sync_failed(&mut self, message: String, log_file: Option<PathBuf>) {
        let log_file = match log_file {
            Some(log_file) => {
                let has_output = std::fs::metadata(&log_file)
                    .map(|meta| meta.len() > 0)
                    .unwrap_or(false);

                if !has_output {
                    let _ = std::fs::remove_file(&log_file);
                    None
                } else {
                    Some(log_file)
                }
            }
            None => None,
        };

        let Some(log_file) = log_file else {
            self.show_message(MessageType::Error, message);
            return;
        };

        let actions = vec![MessageActionItem {
            title: OPEN_BUILD_TOOL_LOG_ACTION.to_string(),
            properties: std::collections::HashMap::new(),
        }];

        self.show_message_request(
            MessageType::Error,
            message,
            Some(actions),
            OutgoingRequest::OpenBuildToolLog { log_file },
        );
    }

    /// Path of the log file the build tool output is streamed into, living in
    /// the cache dir's `logs` folder.
    fn build_tool_log_path(
        &self,
        root: &AbsPathBuf,
        system: project_model::BuildSystemType,
    ) -> PathBuf {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        root.as_str().hash(&mut hasher);
        let hash = hasher.finish();

        self.config.get_cache_dir().join("logs").join(format!(
            "build-tool-{}-{hash:x}.log",
            system.name().to_lowercase()
        ))
    }

    fn handle_background_task(&mut self, event: BackgroundTaskEvent) {
        match event {
            BackgroundTaskEvent::ProbeWorkspace { root } => {
                // The probe only checks the existence of a handful of build files,
                // so it's cheap enough to run on the main thread.
                self.probe_workspace(root);
            }

            BackgroundTaskEvent::AmbiguousWorkspace { root, systems } => {
                let actions: Vec<MessageActionItem> = systems
                    .iter()
                    .map(|sys| MessageActionItem {
                        title: sys.name().to_string(),
                        properties: std::collections::HashMap::new(),
                    })
                    .collect();

                self.show_message_request(
                    MessageType::Warning,
                    format!(
                        "Multiple build systems detected at '{}'. Please select one:",
                        root.as_str()
                    ),
                    Some(actions),
                    OutgoingRequest::SelectBuildSystem { root, systems },
                );
            }

            BackgroundTaskEvent::LoadWorkspace { root, system } => {
                let progress_token = format!("sync-{}", root.as_str());
                self.report_progress(ProgressEvent {
                    token: progress_token.clone(),
                    title: format!("Syncing Project Layout ({:?})", system),
                    message: Some("Extracting build graph metadata...".to_string()),
                    percentage: None,
                    state: ProgressState::Begin,
                });

                let task_sender = self.task_sender.clone();
                let Some(java_home) = self.config.get_java_home() else {
                    self.report_progress(ProgressEvent {
                        token: progress_token,
                        title: String::new(),
                        message: None,
                        percentage: None,
                        state: ProgressState::End,
                    });
                    self.show_message(MessageType::Error, "No JDK found".to_string());
                    tracing::error!("No JDK found in JAVA_HOME");
                    return;
                };

                let log_file = system
                    .get_executor()
                    .support_logging()
                    .then(|| self.build_tool_log_path(&root, system));

                self.thread_pool.execute(move || {
                    let finish_progress = || {
                        task_sender
                            .send(BackgroundTaskEvent::Progress(ProgressEvent {
                                token: progress_token.clone(),
                                title: String::new(),
                                message: None,
                                percentage: None,
                                state: ProgressState::End,
                            }))
                            .ok();
                    };

                    let system_name = system.name();
                    let mut in_model = false;
                    let mut last_flash: Option<Instant> = None;
                    let mut pending_flash: Option<String> = None;

                    let mut on_output = |line: String| {
                        let is_marker = line.contains("WORKSPACE_MODEL_BEGIN")
                            || line.contains("WORKSPACE_MODEL_END");

                        if is_marker || in_model {
                            tracing::debug!("[{system_name}] {line}");
                        } else {
                            tracing::info!("[{system_name}] {line}");
                        }

                        if line.contains("WORKSPACE_MODEL_BEGIN") {
                            in_model = true;
                        }
                        if line.contains("WORKSPACE_MODEL_END") {
                            in_model = false;
                        }

                        // Flash non-noise lines in the progress bar, throttled
                        // to avoid flooding the client. The last pending line
                        // is flushed once the sync finishes.
                        let trimmed = line.trim();
                        if trimmed.is_empty() || is_marker || in_model {
                            return;
                        }

                        let message = if trimmed.chars().count() > BUILD_TOOL_FLASH_MAX_LEN {
                            let mut capped: String =
                                trimmed.chars().take(BUILD_TOOL_FLASH_MAX_LEN).collect();
                            capped.push('…');
                            capped
                        } else {
                            trimmed.to_string()
                        };
                        pending_flash = Some(message.clone());

                        let now = Instant::now();
                        if let Some(last) = last_flash
                            && now.duration_since(last) < BUILD_TOOL_FLASH_INTERVAL
                        {
                            return;
                        }
                        last_flash = Some(now);

                        task_sender
                            .send(BackgroundTaskEvent::Progress(ProgressEvent {
                                token: progress_token.clone(),
                                title: String::new(),
                                message: Some(message),
                                percentage: None,
                                state: ProgressState::Report,
                            }))
                            .ok();
                        pending_flash = None;
                    };

                    let sync_result = system.get_executor().sync(
                        root.as_ref(),
                        &java_home,
                        log_file.as_deref(),
                        &mut on_output,
                    );

                    let mut flush_pending_flash = || {
                        if let Some(message) = pending_flash.take() {
                            task_sender
                                .send(BackgroundTaskEvent::Progress(ProgressEvent {
                                    token: progress_token.clone(),
                                    title: String::new(),
                                    message: Some(message),
                                    percentage: None,
                                    state: ProgressState::Report,
                                }))
                                .ok();
                        }
                    };

                    match sync_result {
                        Ok(graph) => {
                            flush_pending_flash();
                            finish_progress();
                            if let Some(log_file) = &log_file {
                                let _ = std::fs::remove_file(log_file);
                            }
                            task_sender
                                .send(BackgroundTaskEvent::WorkspaceLoaded { graph, root })
                                .ok();
                        }
                        Err(err) => {
                            tracing::error!(?root, "Metadata compilation failure: {}", err);
                            flush_pending_flash();
                            finish_progress();

                            match err.downcast::<SyncError>() {
                                Ok(sync_err) => {
                                    let message = if sync_err.tail.trim().is_empty() {
                                        format!("Failed to receive project metadata: {sync_err}")
                                    } else {
                                        format!(
                                            "Failed to receive project metadata: {sync_err}\n\n{}",
                                            sync_err.tail
                                        )
                                    };
                                    task_sender
                                        .send(BackgroundTaskEvent::SyncFailed { message, log_file })
                                        .ok();
                                }
                                Err(err) => {
                                    if let Some(log_file) = &log_file {
                                        let _ = std::fs::remove_file(log_file);
                                    }
                                    task_sender
                                        .send(BackgroundTaskEvent::NotifyUser {
                                            typ: MessageType::Error,
                                            message: format!(
                                                "Failed to receive project metadata: {err}"
                                            ),
                                        })
                                        .ok();
                                }
                            }
                        }
                    }
                });
            }

            BackgroundTaskEvent::WorkspaceLoaded { graph, root } => {
                tracing::info!("Project configuration graph successfully loaded: {graph:#?}");

                self.apply_loaded_graph(graph, root);
            }

            BackgroundTaskEvent::SyncFailed { message, log_file } => {
                self.handle_sync_failed(message, log_file);
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
            BackgroundTaskEvent::AsyncRequestRetry { id, run } => {
                tracing::debug!(?id, "request cancelled by pending write; queuing for retry");
                self.pending_requests.push(run);
            }
            BackgroundTaskEvent::NotifyUser { typ, message } => self.show_message(typ, message),

            BackgroundTaskEvent::DiagnosticsReady { changed } => {
                self.handle_diagnostics_ready(changed);
            }
            BackgroundTaskEvent::DiagnosticsRetry { changed } => {
                tracing::debug!("diagnostics pass cancelled by pending write; re-queueing");
                self.pending_changes.extend(changed);
                self.refresh_deadline = Some(Instant::now() + self.config.cross_file_debounce());
                self.arm_refresh_timer();
            }
        }
    }

    /// Entry point to kick off initialization/probing workflows.
    /// Call this inside your `handlers::on_initialized` callback.
    pub fn trigger_workspace_probe(&mut self) {
        for root in self.config.workspace_folders.clone() {
            self.probe_workspace(root);
        }
    }

    /// Probes a workspace root for a supported build system and dispatches the
    /// follow-up work. Runs on the main thread: the probe is just a few cheap
    /// `exists()` checks, and the `None` fallback needs to apply its source
    /// roots synchronously so that requests racing with initialization see a
    /// fully populated database.
    fn probe_workspace(&mut self, root: AbsPathBuf) {
        match project_model::probe_workspace_layout(root.as_ref()) {
            project_model::ProbeResult::Single(system) => {
                self.task_sender
                    .send(BackgroundTaskEvent::LoadWorkspace { root, system })
                    .ok();
            }
            project_model::ProbeResult::Ambiguous(systems) => {
                tracing::warn!(
                    ?root,
                    ?systems,
                    "Ambiguous build configurations discovered."
                );
                self.task_sender
                    .send(BackgroundTaskEvent::AmbiguousWorkspace { root, systems })
                    .ok();
            }
            project_model::ProbeResult::None => {
                tracing::info!(
                    ?root,
                    "No build system detected, treating workspace root as a plain source root"
                );
                // The plain path has no build-system sync to resolve the JDK
                // through, so it uses the same env-fallback-aware getter the
                // build-system path does — otherwise a `JAVA_HOME` set only
                // in the environment registers no SDK and resolution of every
                // platform class degrades silently.
                let graph =
                    project_model::WorkspaceGraph::plain(root.clone(), self.config.get_java_home());
                self.apply_loaded_graph(graph, root);
            }
        }
    }

    /// Turns a loaded [`project_model::WorkspaceGraph`] into database source
    /// roots and configures the vfs loader with the source roots declared by
    /// the build system.
    fn apply_loaded_graph(&mut self, graph: project_model::WorkspaceGraph, root: AbsPathBuf) {
        tracing::info!(?root, "Applying workspace source roots and loader config");

        // Group source roots by source set, in a deterministic order (project
        // id, then source set kind). This order is shared by the vfs partition
        // (one `SourceRoot` per source set) and the `ProjectGraph` map, so the
        // `SourceRootId(i)` assigned by `FileChange::apply` (vector order)
        // lines up with `source_sets[i]`.
        let mut source_sets: Vec<(SourceSetId, Vec<AbsPathBuf>)> = Vec::new();
        // Generated source roots (`target/generated-sources`, ...) hold real
        // compile inputs even though they live under gitignored paths.
        let mut generated_roots: FxHashSet<AbsPathBuf> = FxHashSet::default();
        let mut seen: FxHashSet<SourceSetId> = FxHashSet::default();
        for project in graph.projects.values() {
            for (kind, source_set) in &project.source_sets {
                let id = SourceSetId {
                    project: project.id,
                    kind: kind.clone(),
                };
                if !seen.insert(id.clone()) {
                    continue;
                }
                let mut roots = Vec::new();
                roots.extend(source_set.source_roots.iter().cloned());
                for generated in &source_set.generated_source_roots {
                    generated_roots.insert(generated.clone());
                    roots.push(generated.clone());
                }
                source_sets.push((id, roots));
            }
        }
        source_sets.sort_by_key(|(id, _)| id.clone());
        let source_set_ids: Vec<SourceSetId> =
            source_sets.iter().map(|(id, _)| id.clone()).collect();

        let mut source_roots: Vec<AbsPathBuf> = Vec::new();
        for (_, roots) in &source_sets {
            source_roots.extend(roots.iter().cloned());
        }
        source_roots.sort();
        source_roots.dedup();

        // One FileSet per source set, so each source set becomes its own
        // `SourceRoot` and `file → SourceRootId → SourceSetId` is a pure salsa
        // lookup.
        let mut builder = vfs::file_set::FileSetConfig::builder();
        for (_, roots) in &source_sets {
            builder.add_file_set(roots.iter().cloned().map(vfs::VfsPath::from).collect());
        }
        let file_set_config = builder.build();

        // Load and watch the source roots declared by the build system,
        // skipping paths that gitignore rules exclude.
        self.vfs_config_version += 1;
        let mut matchers = Vec::new();
        let entries: Vec<vfs::loader::Entry> = source_roots
            .iter()
            .map(|source_root| {
                // A generated root lives under a gitignored directory
                // (`target/`) yet holds real compile inputs ([JLS-adjacent]:
                // annotation-processor and grammar-generator output is part
                // of the compilation), so ignore rules do not apply to it.
                let is_generated = generated_roots.contains(source_root);
                let mut builder = ignore::WalkBuilder::new(source_root);
                builder.standard_filters(!is_generated).require_git(false);
                if !is_generated && let Some(matcher) = builder.build_matchers().into_iter().next()
                {
                    matchers.push((source_root.clone(), matcher));
                }

                vfs::loader::Entry::Directories(vfs::loader::Directories {
                    extensions: vec!["java".into(), "kt".into(), "kts".into()],
                    include: vec![source_root.clone()],
                    exclude: if is_generated {
                        Vec::new()
                    } else {
                        collect_ignored_paths(source_root)
                    },
                })
            })
            .collect();
        let watch = (0..entries.len()).collect();
        self.loader.handle.set_config(vfs::loader::Config {
            version: self.vfs_config_version,
            load: entries,
            watch,
        });
        self.source_root_matchers = matchers;

        self.file_set_config = Some(file_set_config);

        let roots = self.partition_source_roots();
        // The diagnostics store covers the exactly same file set as the
        // database source roots.
        let source_files: Vec<FileId> = roots.iter().flat_map(SourceRoot::iter).collect();
        self.diagnostics.set_source_files(source_files);
        let mut project_graph = self.build_project_graph(&graph, &source_set_ids);
        for (idx, source_set) in source_sets.iter().enumerate() {
            project_graph
                .source_root_to_source_set
                .insert(SourceRootId(idx as u32), source_set.0.clone());
        }
        let db = self.analysis_host.raw_database_mut();
        hir::set_project_graph(db, project_graph);

        // Applying roots must happen after the ProjectGraph registration so
        // the per-source-set SourceRootIds match `source_root_to_source_set`.
        let mut change = FileChange::default();
        change.set_roots(roots);
        self.analysis_host.apply_change(change);

        self.warmup_libraries(&root);
        self.refresh_diagnostics();
    }

    /// Builds the classpath-aware project model from the workspace graph:
    /// every reachable library, the JDK built-ins, and each source set's
    /// ordered compile classpath.
    fn build_project_graph(
        &self,
        graph: &project_model::WorkspaceGraph,
        source_set_ids: &[SourceSetId],
    ) -> hir::ProjectGraphData {
        let mut data = hir::ProjectGraphData::default();

        // SDK → the concrete jimage/rt.jar library id.
        let mut sdk_library: FxHashMap<project_model::SdkId, project_model::LibraryId> =
            FxHashMap::default();

        // JDKs: prefer the modular layout (`lib/modules`), fall back to the
        // legacy `lib/rt.jar`.
        for sdk in graph.sdks.values() {
            let modules = sdk.home_path.join("lib").join("modules");
            let rt_jar = sdk.home_path.join("lib").join("rt.jar");
            let (path, kind) =
                if std::fs::metadata(std::path::Path::new(modules.as_path().as_str())).is_ok() {
                    (modules, LibraryKind::Jimage)
                } else {
                    (rt_jar, LibraryKind::Jar)
                };
            if std::fs::metadata(std::path::Path::new(path.as_path().as_str())).is_ok()
                && let Ok(id) = project_model::LibraryId::from_file_path(path.as_path().as_ref())
            {
                data.libraries
                    .entry(id)
                    .or_insert_with(|| LibraryInfo::new(kind, path.clone()));
                if !data.jdk_libraries.contains(&id) {
                    data.jdk_libraries.push(id);
                }
                sdk_library.insert(sdk.id, id);
            }
        }

        // Classpath jars referenced by any source set.
        for project in graph.projects.values() {
            for source_set in project.source_sets.values() {
                for entry in &source_set.compile_classpath {
                    if let ClasspathEntry::External(lib_id) = entry
                        && let Some(lib) = graph.library_paths.get(lib_id)
                    {
                        data.libraries.entry(*lib_id).or_insert_with(|| {
                            LibraryInfo::new(LibraryKind::Jar, lib.path.clone())
                        });
                    }
                }
            }
        }

        // Per-source-set ordered classpaths. The order is preserved verbatim
        // from the build tool so that FQN resolution honors shadowing.
        for source_set_id in source_set_ids {
            let Some(project) = graph.projects.get(&source_set_id.project) else {
                continue;
            };
            let Some(source_set) = project.source_sets.get(&source_set_id.kind) else {
                continue;
            };
            let mut entries = Vec::new();
            for entry in &source_set.compile_classpath {
                match entry {
                    ClasspathEntry::Internal {
                        project_id,
                        source_set: kind,
                    } => {
                        entries.push(HirClasspathEntry::SourceSet(SourceSetId {
                            project: *project_id,
                            kind: kind.clone(),
                        }));
                    }
                    ClasspathEntry::External(lib_id) => {
                        entries.push(HirClasspathEntry::Library(*lib_id));
                    }
                    ClasspathEntry::Sdk(sdk_id) => {
                        if let Some(&id) = sdk_library.get(sdk_id) {
                            entries.push(HirClasspathEntry::Library(id));
                        }
                    }
                }
            }

            // The platform modules are implicitly on every compile classpath
            // ([JLS §7.3]); a build tool reports them as an explicit SDK
            // entry, plain workspaces fall back to the configured JDK.
            for jdk in &data.jdk_libraries {
                let entry = HirClasspathEntry::Library(*jdk);
                if !entries.contains(&entry) {
                    entries.push(entry);
                }
            }

            data.source_sets.insert(
                source_set_id.clone(),
                std::sync::Arc::new(Classpath { entries }),
            );
        }

        data
    }

    /// Warms the stub indexes of every registered library up on a background
    /// thread so the first type query does not pay the full JDK parse cost.
    fn warmup_libraries(&mut self, root: &AbsPathBuf) {
        let db = self.analysis_host.raw_database();
        let ids: Vec<hir::LibraryId> = hir::registered_libraries(db);
        if ids.is_empty() {
            return;
        }

        let token = format!("index-{}", root.as_str());
        let total = ids.len();
        self.report_progress(ProgressEvent {
            token: token.clone(),
            title: "Indexing libraries".to_string(),
            message: Some(format!("Indexing {total} libraries...")),
            percentage: Some(0),
            state: ProgressState::Begin,
        });

        let task_sender = self.task_sender.clone();
        let snapshot = self.analysis_host.snapshot();
        let done_count = Arc::new(AtomicUsize::new(0));

        for &id in ids.iter() {
            let task_sender = task_sender.clone();
            let token = token.clone();
            let done_count = Arc::clone(&done_count);

            let snapshot = snapshot.clone();

            self.thread_pool.execute(move || {
                let db = snapshot.raw_database();

                let _ = Cancelled::catch(AssertUnwindSafe(|| {
                    hir::warmup_library(db, id);
                }));

                let done = done_count.fetch_add(1, Ordering::SeqCst) + 1;
                let percentage = (done as f64 / total as f64 * 100.0) as u32;

                task_sender
                    .send(BackgroundTaskEvent::Progress(ProgressEvent {
                        token: token.clone(),
                        title: String::new(),
                        message: Some(format!("Indexed {done}/{total} libraries")),
                        percentage: Some(percentage),
                        state: ProgressState::Report,
                    }))
                    .ok();

                if done == total {
                    // All libraries are indexed: now is a safe point to drop
                    // cache entries of libraries no project uses anymore.
                    hir::prune_stub_cache(db);

                    task_sender
                        .send(BackgroundTaskEvent::Progress(ProgressEvent {
                            token,
                            title: String::new(),
                            message: Some("Indexing complete".to_string()),
                            percentage: Some(100),
                            state: ProgressState::End,
                        }))
                        .ok();
                }
            });
        }
    }

    /// Rebuilds the database source roots by partitioning the current vfs with
    /// [`Self::file_set_config`].
    fn partition_source_roots(&self) -> Vec<SourceRoot> {
        let file_set_config = match &self.file_set_config {
            Some(config) => config,
            None => return Vec::new(),
        };

        let vfs = self.vfs.read();
        let mut file_sets = file_set_config.partition(&vfs.0);
        // The last set is the catch-all for files outside any source root.
        file_sets.pop();
        file_sets.into_iter().map(SourceRoot::new).collect()
    }

    fn handle_vfs_task(&mut self, task: vfs::loader::Message) {
        match task {
            vfs::loader::Message::Loaded { files } => {
                {
                    let mut vfs = self.vfs.write();
                    for (path, contents) in files {
                        // Open documents are maintained by the client via
                        // didChange, so the loader's on-disk copy is stale and
                        // must not overwrite the in-memory text.
                        if self.mem_docs.contains(&path.clone().into()) {
                            continue;
                        }
                        // Drop files that gitignore rules exclude; the loader's
                        // exclude list only covers directories.
                        let ignored = self
                            .source_root_matchers
                            .iter_mut()
                            .find_map(|(root, matcher)| {
                                let rel = path.as_path().strip_prefix(root.as_path())?;
                                Some(matcher.matched(rel, false).is_ignore())
                            })
                            .unwrap_or(false);
                        if ignored {
                            continue;
                        }
                        vfs.0.set_file_contents(path.into(), contents);
                    }
                }
                self.process_changes();
            }
            vfs::loader::Message::Changed { files } => {
                let mut recorded: Vec<FileId> = Vec::new();
                {
                    let mut vfs = self.vfs.write();
                    for (path, contents) in files {
                        // Open documents are maintained by the client via
                        // didChange, so the loader's on-disk copy is stale and
                        // must not overwrite the in-memory text.
                        if self.mem_docs.contains(&path.clone().into()) {
                            continue;
                        }
                        // Drop files that gitignore rules exclude; the loader's
                        // exclude list only covers directories.
                        let ignored = self
                            .source_root_matchers
                            .iter_mut()
                            .find_map(|(root, matcher)| {
                                let rel = path.as_path().strip_prefix(root.as_path())?;
                                Some(matcher.matched(rel, false).is_ignore())
                            })
                            .unwrap_or(false);
                        if ignored {
                            continue;
                        }
                        let file_id = vfs.0.file_id(&path.clone().into()).map(|(id, _)| id);
                        // `set_file_contents` reports whether the content truly
                        // changed; a re-read after `didClose` with an unchanged
                        // file yields `false` and must not be treated as an edit.
                        if vfs.0.set_file_contents(path.into(), contents)
                            && let Some(file_id) = file_id
                        {
                            recorded.push(file_id);
                        }
                    }
                }
                for file_id in recorded {
                    // An on-disk edit to a source file can move the diagnostics
                    // of its dependents.
                    self.record_source_edit(file_id);
                }
                self.process_changes();
            }
            vfs::loader::Message::Progress {
                n_total,
                n_done,
                config_version,
                ..
            } => {
                if config_version != self.vfs_config_version {
                    return;
                }

                let token = format!("scan-{config_version}");
                match n_done {
                    vfs::loader::LoadingProgress::Started => {
                        self.scan_config_version = Some(config_version);
                        self.report_progress(ProgressEvent {
                            token,
                            title: "Scanning workspace files".to_string(),
                            message: None,
                            percentage: Some(0),
                            state: ProgressState::Begin,
                        });
                    }
                    vfs::loader::LoadingProgress::Progress(done) => {
                        if self.scan_config_version != Some(config_version) {
                            return;
                        }
                        let percentage = if n_total == 0 {
                            0
                        } else {
                            (done as f64 / n_total as f64 * 100.0) as u32
                        };
                        self.report_progress(ProgressEvent {
                            token,
                            title: String::new(),
                            message: Some(format!("{done}/{n_total} directories")),
                            percentage: Some(percentage),
                            state: ProgressState::Report,
                        });
                    }
                    vfs::loader::LoadingProgress::Finished => {
                        if self.scan_config_version == Some(config_version) {
                            self.report_progress(ProgressEvent {
                                token,
                                title: String::new(),
                                message: Some("Scan complete".to_string()),
                                percentage: Some(100),
                                state: ProgressState::End,
                            });
                            self.scan_config_version = None;
                        }
                        self.task_sender.send(BackgroundTaskEvent::VfsLoaded).ok();
                    }
                }
            }
        }
    }

    fn process_changes(&mut self) {
        let mut change = FileChange::default();

        let vfs_changed = {
            let mut vfs = self.vfs.write();
            let (vfs, line_endings_map) = &mut *vfs;
            let vfs_changes = vfs.take_changes();

            if !vfs_changes.is_empty() {
                for (file_id, changed_file) in vfs_changes {
                    let new_text = match changed_file.change {
                        vfs::Change::Create(items, _) | vfs::Change::Modify(items, _) => {
                            String::from_utf8(items).ok().map(|text| {
                                let (normalized_text, line_endings) = LineEndings::normalize(text);
                                line_endings_map.insert(file_id, line_endings);
                                normalized_text
                            })
                        }
                        vfs::Change::Delete => {
                            line_endings_map.remove(&file_id);
                            None
                        }
                    };
                    change.change_file(file_id, new_text);
                }
                true
            } else {
                false
            }
        };

        // Files were added to or removed from the vfs, so the source roots need
        // to be rebuilt to keep `file_language_kind` working.
        if vfs_changed && self.file_set_config.is_some() {
            let roots = self.partition_source_roots();
            // The diagnostics store covers the same file set; refresh the files
            // it covers, and get back the watched files whose diagnostics may
            // have moved because of the repartition.
            let source_files: Vec<FileId> = roots.iter().flat_map(SourceRoot::iter).collect();
            let affected = self.diagnostics.set_source_files(source_files);
            change.set_roots(roots);

            if self.config.cross_file_enabled() && !affected.is_empty() {
                // A file add/delete can resolve (or conflict with) references
                // in watched files; re-verify them all.
                self.pending_changes.extend(affected);
                self.refresh_deadline = Some(Instant::now() + self.config.cross_file_debounce());
                self.arm_refresh_timer();
            }
        }

        self.analysis_host.apply_change(change);
    }

    /// Queues a *client-visible* text edit for the debounced cross-file
    /// refresh. The background pass redisplays only the files whose
    /// diagnostics genuinely moved, so steady-state keystrokes are cheap.
    pub(crate) fn record_source_edit(&mut self, file_id: FileId) {
        if !self.config.cross_file_enabled() {
            return;
        }
        self.pending_changes.insert(file_id);
        self.refresh_deadline = Some(Instant::now() + self.config.cross_file_debounce());
        self.arm_refresh_timer();
    }

    /// Fired when the debounce timer wakes (or when a previous one-shot was
    /// consumed): clears the armed timer and either starts the refresh pass or
    /// re-arms for a later deadline.
    fn collect_refresh_timer(&mut self) {
        self.refresh_timer = None;
        match self.refresh_deadline {
            Some(deadline) if Instant::now() >= deadline => {
                self.start_diagnostics_pass();
            }
            Some(_) => self.arm_refresh_timer(),
            None => {}
        }
    }

    /// Arms a one-shot receiver that wakes the main loop when the debounce
    /// deadline is reached (if one is pending and no timer is already armed).
    fn arm_refresh_timer(&mut self) {
        if self.refresh_timer.is_some() {
            return;
        }
        if let Some(deadline) = self.refresh_deadline {
            self.refresh_timer = Some(crossbeam_channel::at(deadline));
        }
    }

    /// Launches the debounced diagnostics refresh pass on the worker pool. The
    /// pass recomputes the edited files and their watched dependents, returning
    /// the files whose diagnostics actually moved. A concurrent write cancels
    /// the pass ([`Cancelled::PendingWrite`]) and the worker hands the changed
    /// set back for a re-run after the write.
    fn start_diagnostics_pass(&mut self) {
        self.refresh_deadline = None;
        let changed = std::mem::take(&mut self.pending_changes);
        if changed.is_empty() {
            return;
        }

        let snapshot = self.snapshot();
        let task_sender = self.task_sender.clone();
        self.thread_pool.execute(move || {
            let result = diagnostics_store::run_diagnostics_pass(&snapshot, &changed);
            match result {
                Ok(moved) => {
                    let _ = task_sender.send(BackgroundTaskEvent::DiagnosticsReady {
                        changed: moved.into_iter().collect(),
                    });
                }
                Err(Cancelled::PendingWrite) => {
                    let _ = task_sender.send(BackgroundTaskEvent::DiagnosticsRetry { changed });
                }
                Err(cancelled) => {
                    tracing::debug!(?cancelled, "diagnostics pass aborted");
                }
            }
        });
    }

    /// Applies a completed diagnostics pass: asks the client to re-pull the
    /// documents whose diagnostics moved via `workspace/diagnosticRefresh` (the
    /// pull channel). Unopened files are never pushed.
    fn handle_diagnostics_ready(&mut self, changed: FxHashSet<FileId>) {
        if !self.config.cross_file_enabled() {
            return;
        }
        if changed.is_empty() {
            return;
        }
        tracing::debug!(changed = ?changed, "diagnostics moved; refreshing the client");
        self.refresh_diagnostics();
    }
}

/// Returns the paths under `root` that gitignore (and hidden-file) rules
/// exclude. The vfs loader uses these to skip them while walking.
fn collect_ignored_paths(root: &AbsPathBuf) -> Vec<AbsPathBuf> {
    let allowed: FxHashSet<PathBuf> = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .require_git(false)
        .build()
        .flatten()
        .map(|entry| entry.into_path())
        .collect();

    fn to_abs_path(path: PathBuf) -> Option<AbsPathBuf> {
        Utf8PathBuf::from_path_buf(path)
            .ok()
            .and_then(|path| AbsPathBuf::try_from(path).ok())
    }

    let mut ignored_dirs: Vec<AbsPathBuf> = Vec::new();
    let mut ignored_files: Vec<AbsPathBuf> = Vec::new();

    // Ignored directories are pruned from the walkdir iteration, so record
    // them in the filter itself.
    let ignored_dirs_cell = std::cell::RefCell::new(&mut ignored_dirs);
    let walk = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() > 0 && entry.file_type().is_dir() && !allowed.contains(entry.path()) {
                if let Some(abs_path) = to_abs_path(entry.path().to_path_buf()) {
                    ignored_dirs_cell.borrow_mut().push(abs_path);
                }
                return false;
            }
            true
        });

    for entry in walk.flatten() {
        if entry.depth() == 0 || allowed.contains(entry.path()) {
            continue;
        }
        if let Some(abs_path) = to_abs_path(entry.into_path()) {
            ignored_files.push(abs_path);
        }
    }

    ignored_dirs.extend(ignored_files);
    ignored_dirs
}
