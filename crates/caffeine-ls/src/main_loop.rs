use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use camino::{Utf8Path, Utf8PathBuf};
use crossbeam_channel::Receiver;
use ide::{
    Change, Classpath, ClasspathEntry as GraphClasspathEntry, LibraryId, LibraryInfo, LibraryKind,
    LibrarySources, ProjectGraphData, SourceSetId,
};
use ide_db::base_db::{SourceRoot, SourceRootId};
use lsp_server::{Connection, ErrorCode, Notification, Request};
use lsp_types::*;
use project_model::{ClasspathEntry, SyncError, SyncPhase, SyncProgress};
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;
use vfs::{AbsPathBuf, VfsPath};

use crate::{
    GlobalState,
    config::Config,
    decompiler,
    global_state::{
        BackgroundTaskEvent, OutgoingRequest, PendingRequest, ProgressEvent, ProgressState,
        SourceRootKind,
    },
    handlers::{
        self,
        dispatch::{NotificationDispatcher, RequestDispatcher},
    },
    library_sources,
    library_view::{self, LibraryView},
    line_index::LineEndings,
};

const OPEN_BUILD_TOOL_LOG_ACTION: &str = "Open Build Tool Log";

/// The largest classpath a decompiler is handed. CFR takes the whole list in
/// one `--extraclasspath` argument, and a command line that long is past what a
/// process can be started with on every platform; the *content* of the list is
/// only a readability hint for the decompiled output, so beyond the budget the
/// class's own archive alone is used.
const MAX_DECOMPILER_CLASSPATH: usize = 16_000;

/// The classpath a decompiler is given for a class declared in `archive`: that
/// archive first, then every other classpath jar, each once and in a
/// deterministic order. A JDK 9+ `lib/modules` is a jimage — neither backend
/// reads one — so only jars are passed.
fn decompiler_classpath(archive: &Path, jars: &[PathBuf]) -> Vec<PathBuf> {
    let mut externals = Vec::with_capacity(jars.len());
    externals.push(archive.to_path_buf());
    externals.extend(jars.iter().filter(|jar| jar.as_path() != archive).cloned());
    if externals.is_empty() {
        return externals;
    }
    // What `join_paths` would produce, without building it: the paths plus one
    // separator between each pair.
    let joined: usize = externals
        .iter()
        .map(|path| path.as_os_str().as_encoded_bytes().len())
        .sum::<usize>()
        + externals.len().saturating_sub(1);
    if joined > MAX_DECOMPILER_CLASSPATH {
        tracing::debug!(
            bytes = joined,
            libraries = externals.len(),
            "decompiler classpath exceeds the budget; falling back to the class's own archive"
        );
        externals.truncate(1);
    }
    externals
}

/// One registered source root, in `SourceRootId` order. Workspace roots come
/// first (sorted by path), library source roots follow (sorted by library id),
/// then decompiled library roots (also sorted by library id), so the id
/// `Change::apply` assigns to each root is this vector's index.
enum RootEntry {
    Workspace {
        path: AbsPathBuf,
        source_set: SourceSetId,
        generated: bool,
    },
    Library {
        path: AbsPathBuf,
        library: LibraryId,
    },
    DecompiledLibrary {
        path: AbsPathBuf,
        library: LibraryId,
    },
}

impl RootEntry {
    fn path(&self) -> &AbsPathBuf {
        match self {
            RootEntry::Workspace { path, .. }
            | RootEntry::Library { path, .. }
            | RootEntry::DecompiledLibrary { path, .. } => path,
        }
    }
}

/// The source set of the *detached* root: the files the vfs partitions into
/// the catch-all set, outside every configured source root. A synthetic
/// project id no build-system project can carry keeps the set apart from every
/// real one; a file that lands there resolves names against its own
/// declarations (and the platform), which is what makes a standalone or
/// scratch file navigate to itself.
fn detached_source_set() -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(u32::MAX),
        kind: project_model::SourceSetKind::Main,
    }
}

/// Percentage ranges assigned to each sync phase. Together they always span
/// 0..=99; the sync completes by reporting 100% explicitly once the workspace
/// model is parsed (right before the `WorkDoneProgressEnd`).
const PHASE_PERCENTAGE_RANGES: [(SyncPhase, (u32, u32)); 5] = [
    (SyncPhase::Resolving, (0, 15)),
    (SyncPhase::Downloading, (15, 60)),
    (SyncPhase::Configuring, (60, 85)),
    (SyncPhase::Compiling, (85, 99)),
    (SyncPhase::Exporting, (85, 99)),
];

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
            .on_async::<WorkspaceSymbolResolveRequest>(handlers::on_workspace_symbol_resolve)
            .on_async::<DefinitionRequest>(handlers::on_goto_definition)
            .on_async::<ReferencesRequest>(handlers::on_references)
            .on_async::<HoverRequest>(handlers::on_hover)
            .on_async::<InlayHintRequest>(handlers::on_inlay_hint)
            .on_async::<InlayHintResolveRequest>(handlers::on_inlay_hint_resolve)
            .on_async::<SemanticTokensRequest>(handlers::on_semantic_tokens)
            .on_async::<SemanticTokensDeltaRequest>(handlers::on_semantic_tokens_delta)
            .on_async::<SemanticTokensRangeRequest>(handlers::on_semantic_tokens_range)
            .on_async::<handlers::LibraryFileContent>(handlers::on_library_file_content)
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

                let options = project_model::SyncOptions {
                    download_sources: self.config.download_sources(),
                };

                let cache_dir = self.config.get_cache_dir();
                // Resolved before the spawn: the worker cannot borrow the
                // configuration, and the backend's id is all it needs.
                let decompiler_backend = self
                    .config
                    .decompiler()
                    .map(|(backend, _jar)| backend.id().to_owned());

                self.thread_pool.execute(move || {
                    let system_name = system.name();

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

                    // Aggregates structured SyncProgress events into a single
                    // phase-budgeted percentage, so the client sees a moving
                    // bar instead of a spinner plus raw line flashes. Both the
                    // output and progress callbacks run on this worker thread,
                    // so an Arc<Mutex> lets them share the aggregator without
                    // a second mutable borrow (the pool requires Send).
                    let aggregator = std::sync::Arc::new(parking_lot::Mutex::new(
                        PhaseProgressAggregator::new(),
                    ));

                    let report = |aggregator: &std::sync::Arc<
                        parking_lot::Mutex<PhaseProgressAggregator>,
                    >| {
                        let Some((message, percentage)) = aggregator.lock().current_status() else {
                            return;
                        };
                        task_sender
                            .send(BackgroundTaskEvent::Progress(ProgressEvent {
                                token: progress_token.clone(),
                                title: String::new(),
                                message: Some(message),
                                percentage: Some(percentage),
                                state: ProgressState::Report,
                            }))
                            .ok();
                    };

                    let mut on_output = {
                        let aggregator = std::sync::Arc::clone(&aggregator);
                        move |line: String| {
                            let is_marker = line.contains("WORKSPACE_MODEL_BEGIN")
                                || line.contains("WORKSPACE_MODEL_END");

                            if is_marker {
                                tracing::debug!("[{system_name}] {line}");
                            } else {
                                tracing::info!("[{system_name}] {line}");
                            }

                            if line.contains("WORKSPACE_MODEL_BEGIN") {
                                // The structural model is being printed: we are
                                // in the final phase and can already guarantee
                                // success once the closing marker arrives.
                                aggregator.lock().on_model_begin();
                            } else if line.contains("WORKSPACE_MODEL_END") {
                                aggregator.lock().on_model_end();
                            } else {
                                aggregator.lock().on_line(&line);
                            }
                            report(&aggregator);
                        }
                    };

                    let mut on_progress = {
                        let aggregator = std::sync::Arc::clone(&aggregator);
                        move |event: SyncProgress| {
                            aggregator.lock().on_event(event);
                            report(&aggregator);
                        }
                    };

                    let sync_result = system.get_executor().sync_with_progress(
                        root.as_ref(),
                        &java_home,
                        &options,
                        log_file.as_deref(),
                        &mut on_output,
                        &mut on_progress,
                    );

                    match sync_result {
                        Ok(graph) => {
                            aggregator.lock().on_sync_complete(true);
                            report(&aggregator);
                            finish_progress();
                            if let Some(log_file) = &log_file {
                                let _ = std::fs::remove_file(log_file);
                            }

                            // Locate each library's source archive and create
                            // its materialization root, and create the
                            // decompiled-output roots when a decompiler is
                            // configured. On the worker, so the main loop does
                            // no directory work.
                            let archives = library_sources::collect_archives(&graph);
                            let sources = library_sources::prepare_roots(&cache_dir, &archives);
                            let decompiled = match &decompiler_backend {
                                Some(backend) => decompiler::prepare_roots(
                                    &cache_dir,
                                    backend,
                                    &decompiler::decompilable_libraries(&graph),
                                ),
                                None => FxHashMap::default(),
                            };
                            report_prepared_sources(&task_sender, &progress_token, sources.len());

                            task_sender
                                .send(BackgroundTaskEvent::WorkspaceLoaded {
                                    graph,
                                    root,
                                    sources,
                                    decompiled,
                                })
                                .ok();
                        }
                        Err(err) => {
                            tracing::error!(?root, "Metadata compilation failure: {}", err);
                            aggregator.lock().on_sync_complete(false);
                            report(&aggregator);
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

            BackgroundTaskEvent::WorkspaceLoaded {
                graph,
                root,
                sources,
                decompiled,
            } => {
                tracing::info!("Project configuration graph successfully loaded: {graph:#?}");

                self.apply_loaded_graph(graph, root, sources, decompiled);
            }

            BackgroundTaskEvent::LoadLibraryFiles { files, retry } => {
                let cache_root = self.config.get_cache_dir();
                let mut pending_decompiles = Vec::new();
                for file in files {
                    let (library, archive, entry, path) = match file {
                        ide::LibraryFileRef::Source {
                            library,
                            archive,
                            entry,
                            path,
                        } => (library, archive, entry, path),
                        decompile => {
                            pending_decompiles.push(decompile);
                            continue;
                        }
                    };
                    let vfs_path = VfsPath::from(path.clone());
                    // Idempotent: a file an earlier round already loaded is
                    // left alone.
                    if self.vfs.read().0.file_id(&vfs_path).is_some() {
                        continue;
                    }
                    let bytes = match library_sources::read_entry(archive.as_ref(), &entry) {
                        Ok(bytes) => bytes,
                        Err(err) => {
                            tracing::warn!(
                                library = %library,
                                entry = %entry,
                                "failed to read library source: {err:#}"
                            );
                            continue;
                        }
                    };
                    let root = library_view::root_dir(&cache_root, &LibraryView::Source, library);
                    let target: &Utf8Path = path.as_ref();
                    let Some(relative) = target.strip_prefix(&root).ok() else {
                        tracing::warn!(
                            path = %path,
                            "materialized source lies outside its library cache root"
                        );
                        continue;
                    };
                    if let Err(err) = library_sources::materialize(&root, relative.as_str(), &bytes)
                    {
                        tracing::warn!(
                            path = %path,
                            "failed to materialize library source: {err:#}"
                        );
                        continue;
                    }
                    // The client owns `mem_docs`; a materialized library
                    // source is vfs-only.
                    self.vfs.write().0.set_file_contents(vfs_path, Some(bytes));
                }

                if pending_decompiles.is_empty() {
                    // The request's cancellation token stays registered: the
                    // retried run must still observe `$/cancelRequest`. The
                    // loop's `process_changes` then `run_pending_requests`
                    // sequence makes the writes visible to the retried
                    // snapshot.
                    let (id, run) = retry;
                    tracing::debug!(
                        ?id,
                        "library sources materialized; queuing deferred request"
                    );
                    self.pending_requests.push(run);
                } else {
                    self.decompile_library_files(pending_decompiles, retry);
                }
            }

            BackgroundTaskEvent::LibraryDecompiled {
                files,
                failed,
                retry,
            } => {
                if !failed.is_empty() {
                    self.report_decompiler_failures(&failed);
                }
                for (path, contents) in files {
                    // The client owns `mem_docs`; a decompiled library file is
                    // vfs-only, exactly like a materialized source.
                    self.vfs.write().0.set_file_contents(path, Some(contents));
                }

                // One push per event, after the writes: the decompiled files
                // lie in the source root now, so the retried request finds the
                // declarations it was looking for.
                let (id, run) = retry;
                tracing::debug!(?id, "library files decompiled; queuing deferred request");
                self.pending_requests.push(run);
            }

            BackgroundTaskEvent::SyncFailed { message, log_file } => {
                self.handle_sync_failed(message, log_file);
            }

            BackgroundTaskEvent::Progress(progress) => {
                self.report_progress(progress);
            }

            // The index stage is over, so a hint request now reads the caches
            // it just filled instead of building them itself.
            BackgroundTaskEvent::LibrariesIndexed => self.refresh_inlay_hints(),

            BackgroundTaskEvent::VfsLoaded => {
                tracing::info!("VFS file system synchronization completed.");
            }

            BackgroundTaskEvent::AsyncRequestCompleted { id, result } => {
                self.remove_async_cancellation(&id);
                match result {
                    Ok(resp_json) => {
                        self.respond_ok(id, resp_json);
                    }
                    Err(err) => {
                        self.respond_err(id, ErrorCode::InternalError, err.to_string());
                    }
                }
            }
            BackgroundTaskEvent::AsyncRequestRetry { id, run } => {
                tracing::debug!(?id, "request cancelled by pending write; queuing for retry");
                self.remove_async_cancellation(&id);
                self.pending_requests.push(run);
            }
            BackgroundTaskEvent::AsyncRequestAborted { id } => {
                tracing::debug!(?id, "request aborted by client cancellation");
                self.remove_async_cancellation(&id);
            }
            BackgroundTaskEvent::NotifyUser { typ, message } => self.show_message(typ, message),
        }
    }

    /// Decompiles the classes the deferred request asked for, on the pool: a
    /// JVM start costs 1-2 s and the main loop must keep answering while it
    /// runs. The produced text is materialized under the view root and handed
    /// back as [`BackgroundTaskEvent::LibraryDecompiled`], which pushes the
    /// retry once the files are in the vfs.
    fn decompile_library_files(
        &mut self,
        files: Vec<ide::LibraryFileRef>,
        retry: (lsp_server::RequestId, PendingRequest),
    ) {
        let Some((backend, jar)) = self.config.decompiler() else {
            // The configuration named a backend when the roots were created but
            // names none now. Nothing can be produced; the retried request
            // answers `null` rather than looping.
            tracing::warn!("a library decompile was requested with no decompiler configured");
            self.pending_requests.push(retry.1);
            return;
        };
        if self.library_archives.is_empty() {
            tracing::warn!("a library decompile was requested before a workspace was loaded");
            self.pending_requests.push(retry.1);
            return;
        }

        let cache_dir = self.config.get_cache_dir();
        let java = self.config.decompiler_java();
        // The classpath is one list for the whole batch: the class's own
        // archive plus every other jar, so a type the class references renders
        // under its real name instead of `<unknown>`.
        let jars: Vec<PathBuf> = {
            let mut jars: Vec<PathBuf> = self
                .library_archives
                .values()
                .filter(|library| library.kind == LibraryKind::Jar)
                .map(|library| {
                    let path: &Path = library.path.as_ref();
                    path.to_path_buf()
                })
                .collect();
            jars.sort();
            jars.dedup();
            jars
        };
        let archives = self.library_archives.clone();
        let task_sender = self.task_sender.clone();

        self.thread_pool.execute(move || {
            let view = LibraryView::Decompiled {
                backend: backend.id().to_owned(),
            };
            let mut produced = Vec::new();
            let mut failed = Vec::new();
            for file in files {
                let ide::LibraryFileRef::Decompile {
                    library,
                    class,
                    path,
                } = file
                else {
                    continue;
                };
                let Some(info) = archives.get(&library) else {
                    failed.push((
                        library,
                        class,
                        format!("library {library} is not registered"),
                    ));
                    continue;
                };
                let archive: &Path = info.path.as_ref();
                let target: &Utf8Path = path.as_ref();
                // A file an earlier session produced is reused as it is: the
                // root is keyed by backend and library id, so nothing under it
                // can be stale, and a JVM start is not worth repeating.
                if !target.is_file() {
                    let externals = decompiler_classpath(archive, &jars);
                    match decompiler::decompile(
                        backend, &java, &jar, archive, info.kind, &class, &externals,
                    ) {
                        Ok(text) => {
                            let root = library_view::root_dir(&cache_dir, &view, library);
                            let Some(relative) = target.strip_prefix(&root).ok() else {
                                failed.push((
                                    library,
                                    class,
                                    format!("{target} lies outside its view root"),
                                ));
                                continue;
                            };
                            if let Err(err) = library_sources::materialize(
                                &root,
                                relative.as_str(),
                                text.as_bytes(),
                            ) {
                                failed.push((
                                    library,
                                    class,
                                    format!("failed to write {relative}: {err:#}"),
                                ));
                                continue;
                            }
                        }
                        Err(err) => {
                            failed.push((library, class, format!("{err:#}")));
                            continue;
                        }
                    }
                }
                match std::fs::read(target) {
                    Ok(contents) => produced.push((VfsPath::from(path), contents)),
                    Err(err) => {
                        failed.push((library, class, format!("failed to read {target}: {err}")))
                    }
                }
            }
            task_sender
                .send(BackgroundTaskEvent::LibraryDecompiled {
                    files: produced,
                    failed,
                    retry,
                })
                .ok();
        });
    }

    /// Warns about the classes the decompiler could not produce. The first
    /// failure of the session also reaches the user: a broken JDK or jar fails
    /// every navigation, and one toast is a diagnosis where one per navigation
    /// is noise.
    fn report_decompiler_failures(&mut self, failed: &[(LibraryId, Arc<str>, String)]) {
        for (library, class, error) in failed {
            tracing::warn!(%library, class = %class, "decompiling the library class failed: {error}");
        }
        if self.decompiler_error_reported {
            return;
        }
        let Some((_, class, error)) = failed.first() else {
            return;
        };
        self.decompiler_error_reported = true;
        let backend = self
            .config
            .decompiler()
            .map_or("decompiler", |(backend, _)| backend.id());
        self.show_message(
            MessageType::Warning,
            format!("caffeine-ls: the {backend} decompiler failed for {class}: {error}"),
        );
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
                // platform class degrades silently. It is applied synchronously:
                // a request racing initialization must see a fully populated
                // database, and unlike the build-system path there is no sync
                // progress token to hold the client until the load lands.
                let graph =
                    project_model::WorkspaceGraph::plain(root.clone(), self.config.get_java_home());
                let cache_dir = self.config.get_cache_dir();
                let archives = library_sources::collect_archives(&graph);
                let sources = library_sources::prepare_roots(&cache_dir, &archives);
                // Decompilation is off — an empty map — unless a backend and
                // its jar are configured; that map is the whole feature switch
                // the analysis layer reads.
                let decompiled = match self.config.decompiler() {
                    Some((backend, _jar)) => decompiler::prepare_roots(
                        &cache_dir,
                        backend.id(),
                        &decompiler::decompilable_libraries(&graph),
                    ),
                    None => FxHashMap::default(),
                };
                self.apply_loaded_graph(graph, root, sources, decompiled);
            }
        }
    }

    /// Turns a loaded [`project_model::WorkspaceGraph`] into database source
    /// roots and configures the vfs loader with the source roots declared by
    /// the build system.
    ///
    /// `sources` holds the prepared roots of every library whose sources the
    /// driver located; they become read-only roots and are deliberately *not*
    /// loaded by the vfs loader, so their files come into the database one at a
    /// time, on the request that resolves into them.
    ///
    /// `decompiled` holds the decompiled-output root of every library the
    /// configured decompiler can be asked about, which become read-only roots
    /// of their own — the files are produced on demand exactly like library
    /// sources are read on demand. It is empty when no decompiler is
    /// configured.
    fn apply_loaded_graph(
        &mut self,
        graph: project_model::WorkspaceGraph,
        root: AbsPathBuf,
        sources: FxHashMap<LibraryId, LibrarySources>,
        decompiled: FxHashMap<LibraryId, AbsPathBuf>,
    ) {
        tracing::info!(?root, "Applying workspace source roots and loader config");

        // Collect every build-system source root with its owning source set,
        // in a deterministic order. Each root becomes its own `SourceRoot` and
        // one `FileSet` (ra-style: the source root *is* the base directory a
        // classpath looks packages up under), so `file → SourceRootId →
        // (SourceSetId, base dir)` is a pure salsa lookup and the package-path
        // diagnostic can anchor on the exact base the build tool resolved
        // ([JLS §7.2.1]). This order is shared by the vfs partition and the
        // `ProjectGraph` maps, so the `SourceRootId(i)` assigned by
        // `Change::apply` (vector order) lines up with `entries[i]`.
        let mut source_sets: Vec<SourceSetId> = Vec::new();
        let mut workspace_entries: Vec<(AbsPathBuf, SourceSetId, bool)> = Vec::new();
        let mut seen: FxHashSet<SourceSetId> = FxHashSet::default();
        let mut seen_roots: FxHashSet<AbsPathBuf> = FxHashSet::default();
        for project in graph.projects.values() {
            for (kind, source_set) in &project.source_sets {
                let id = SourceSetId {
                    project: project.id,
                    kind: kind.clone(),
                };
                if !seen.insert(id.clone()) {
                    continue;
                }
                source_sets.push(id.clone());
                // Generated source roots (`target/generated-sources`, ...)
                // hold real compile inputs even though they live under
                // gitignored paths; they are ordinary roots here, just loaded
                // without gitignore filtering (see below).
                for root in &source_set.source_roots {
                    if seen_roots.insert(root.clone()) {
                        workspace_entries.push((root.clone(), id.clone(), false));
                    }
                }
                for generated in &source_set.generated_source_roots {
                    if seen_roots.insert(generated.clone()) {
                        workspace_entries.push((generated.clone(), id.clone(), true));
                    }
                }
            }
        }
        source_sets.sort();
        source_sets.dedup();
        // Deterministic across reloads: order by source root path.
        workspace_entries.sort_by_key(|(root, _, _)| root.clone());

        // Library roots follow the workspace roots, sorted by library id, so
        // the mapping from `SourceRootId` to owner stays deterministic. Source
        // views come before decompiled ones for the same reason.
        let mut library_entries: Vec<(AbsPathBuf, LibraryId)> = sources
            .iter()
            .map(|(library, sources)| (sources.root.clone(), *library))
            .collect();
        library_entries.sort_by_key(|(_, library)| library.to_string());
        let mut decompiled_entries: Vec<(AbsPathBuf, LibraryId)> = decompiled
            .iter()
            .map(|(library, root)| (root.clone(), *library))
            .collect();
        decompiled_entries.sort_by_key(|(_, library)| library.to_string());

        let mut entries: Vec<RootEntry> = Vec::with_capacity(
            workspace_entries.len() + library_entries.len() + decompiled_entries.len(),
        );
        for (path, source_set, generated) in workspace_entries {
            entries.push(RootEntry::Workspace {
                path,
                source_set,
                generated,
            });
        }
        for (path, library) in library_entries {
            entries.push(RootEntry::Library { path, library });
        }
        for (path, library) in decompiled_entries {
            entries.push(RootEntry::DecompiledLibrary { path, library });
        }

        // One FileSet per source root, so each root becomes its own
        // `SourceRoot` and `file → SourceRootId → (SourceSetId, base dir)` is
        // a pure salsa lookup.
        let mut builder = vfs::file_set::FileSetConfig::builder();
        for entry in &entries {
            builder.add_file_set(vec![vfs::VfsPath::from(entry.path().clone())]);
        }
        let file_set_config = builder.build();

        // Load and watch the *workspace* source roots, skipping paths that
        // gitignore rules exclude. A library source root is deliberately absent
        // from `load` (and therefore from `watch`): that is what keeps the
        // sources out of memory until a request materializes one.
        self.vfs_config_version += 1;
        let mut matchers = Vec::new();
        let loader_entries: Vec<vfs::loader::Entry> = entries
            .iter()
            .filter_map(|entry| {
                let RootEntry::Workspace {
                    path, generated, ..
                } = entry
                else {
                    return None;
                };
                // A generated root lives under a gitignored directory
                // (`target/`) yet holds real compile inputs ([JLS-adjacent]:
                // annotation-processor and grammar-generator output is part
                // of the compilation), so ignore rules do not apply to it.
                let mut builder = ignore::WalkBuilder::new(path);
                builder.standard_filters(!generated).require_git(false);
                if !generated && let Some(matcher) = builder.build_matchers().into_iter().next() {
                    matchers.push((path.clone(), matcher));
                }

                Some(vfs::loader::Entry::Directories(vfs::loader::Directories {
                    extensions: syntax::lang::file_extensions()
                        .map(ToOwned::to_owned)
                        .collect(),
                    include: vec![path.clone()],
                    exclude: if *generated {
                        Vec::new()
                    } else {
                        collect_ignored_paths(path)
                    },
                }))
            })
            .collect();
        let watch = (0..loader_entries.len()).collect();
        self.loader.handle.set_config(vfs::loader::Config {
            version: self.vfs_config_version,
            load: loader_entries,
            watch,
        });
        self.source_root_matchers = matchers;

        self.file_set_config = Some(file_set_config);

        // The root kinds must be stored before `partition_source_roots`, which
        // tags every partitioned `FileSet` with its owner.
        self.source_root_kinds = entries
            .iter()
            .map(|entry| match entry {
                RootEntry::Workspace { .. } => SourceRootKind::SourceSet,
                RootEntry::Library { library, .. } => SourceRootKind::Library(*library),
                RootEntry::DecompiledLibrary { library, .. } => {
                    SourceRootKind::DecompiledLibrary(*library)
                }
            })
            .collect();

        let roots = self.partition_source_roots();

        let library_source_roots: FxHashMap<SourceRootId, LibraryId> = self
            .source_root_kinds
            .iter()
            .enumerate()
            .filter_map(|(idx, kind)| match kind {
                SourceRootKind::Library(library) => Some((SourceRootId(idx as u32), *library)),
                SourceRootKind::SourceSet | SourceRootKind::DecompiledLibrary(_) => None,
            })
            .collect();
        let library_decompiled_roots: FxHashMap<SourceRootId, LibraryId> = self
            .source_root_kinds
            .iter()
            .enumerate()
            .filter_map(|(idx, kind)| match kind {
                SourceRootKind::DecompiledLibrary(library) => {
                    Some((SourceRootId(idx as u32), *library))
                }
                SourceRootKind::SourceSet | SourceRootKind::Library(_) => None,
            })
            .collect();

        let mut project_graph = self.build_project_graph(
            &graph,
            &source_sets,
            &sources,
            &library_source_roots,
            &decompiled,
            &library_decompiled_roots,
        );
        // The classfile archives of the loaded workspace, which a decompile
        // reads a class's bytes out of and hands the backend as a classpath.
        self.library_archives = project_graph.libraries.clone();
        for (idx, entry) in entries.iter().enumerate() {
            if let RootEntry::Workspace {
                path, source_set, ..
            } = entry
            {
                project_graph
                    .source_root_to_source_set
                    .insert(SourceRootId(idx as u32), source_set.clone());
                project_graph
                    .source_root_dirs
                    .insert(SourceRootId(idx as u32), path.clone());
            }
        }
        // The detached root (`partition_source_roots`' catch-all) is the last
        // root, after every entry. It is a source set of its own so a file no
        // configured root covers still resolves names against its own
        // declarations; its classpath is the platform, so a standalone file
        // keeps resolving `java.lang` types and nothing else.
        let detached = detached_source_set();
        project_graph
            .source_root_to_source_set
            .insert(SourceRootId(entries.len() as u32), detached.clone());
        project_graph.source_sets.insert(
            detached,
            triomphe::Arc::new(Classpath {
                entries: project_graph
                    .jdk_libraries
                    .iter()
                    .map(|&library| GraphClasspathEntry::Library(library))
                    .collect(),
            }),
        );
        // The graph and the roots go in as one change: `Change::apply` writes
        // the graph first, so the `SourceRootId`s it maps are the ones this
        // same change assigns to `roots` (vector order).
        let mut change = Change::default();
        change.set_project_graph(project_graph);
        change.set_roots(roots);
        self.analysis_host.apply_change(change);

        self.warmup_libraries(&root);
        self.refresh_diagnostics();
        // The freshly applied graph can change a file's highlights with no
        // client-side edit, so a client holding tokens from before the load has
        // to be told to re-request them.
        self.refresh_semantic_tokens();
        // Inlay hints are refreshed from the *end of the index stage* instead
        // ([`BackgroundTaskEvent::LibrariesIndexed`]): the types a hint renders
        // resolve against the library archives that stage is indexing, and a
        // request sent before it finishes would pay for them itself.
    }

    /// Builds the classpath-aware project model from the workspace graph:
    /// every reachable library, the JDK built-ins, and each source set's
    /// ordered compile classpath.
    fn build_project_graph(
        &self,
        graph: &project_model::WorkspaceGraph,
        source_set_ids: &[SourceSetId],
        sources: &FxHashMap<LibraryId, LibrarySources>,
        library_source_roots: &FxHashMap<SourceRootId, LibraryId>,
        decompiled: &FxHashMap<LibraryId, AbsPathBuf>,
        library_decompiled_roots: &FxHashMap<SourceRootId, LibraryId>,
    ) -> ProjectGraphData {
        let mut data = ProjectGraphData::default();

        // SDK → the concrete jimage/rt.jar library id.
        let mut sdk_library: FxHashMap<project_model::SdkId, project_model::LibraryId> =
            FxHashMap::default();

        // JDKs: prefer the modular layout (`lib/modules`), then the legacy
        // `lib/rt.jar`, then the pre-JDK-9 layout (`jre/lib/rt.jar`), which is
        // where a JDK 8 install keeps its platform classes.
        for sdk in graph.sdks.values() {
            let Some((id, kind, path)) = library_sources::sdk_class_archive(sdk) else {
                continue;
            };
            data.libraries
                .entry(id)
                .or_insert_with(|| LibraryInfo::new(kind, path));
            if !data.jdk_libraries.contains(&id) {
                data.jdk_libraries.push(id);
            }
            sdk_library.insert(sdk.id, id);
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
            if let Some(level) = project.language_level {
                data.language_levels.insert(source_set_id.clone(), level);
            }
            if let Some(release) = project.release {
                data.releases.insert(source_set_id.clone(), release);
            }
            let mut entries = Vec::new();
            for entry in &source_set.compile_classpath {
                match entry {
                    ClasspathEntry::Internal {
                        project_id,
                        source_set: kind,
                    } => {
                        entries.push(GraphClasspathEntry::SourceSet(SourceSetId {
                            project: *project_id,
                            kind: kind.clone(),
                        }));
                    }
                    ClasspathEntry::External(lib_id) => {
                        entries.push(GraphClasspathEntry::Library(*lib_id));
                    }
                    ClasspathEntry::Sdk(sdk_id) => {
                        if let Some(&id) = sdk_library.get(sdk_id) {
                            entries.push(GraphClasspathEntry::Library(id));
                        }
                    }
                }
            }

            // The platform modules are implicitly on every compile classpath
            // ([JLS §7.3]); a build tool reports them as an explicit SDK
            // entry, plain workspaces fall back to the configured JDK.
            for jdk in &data.jdk_libraries {
                let entry = GraphClasspathEntry::Library(*jdk);
                if !entries.contains(&entry) {
                    entries.push(entry);
                }
            }

            data.source_sets.insert(
                source_set_id.clone(),
                triomphe::Arc::new(Classpath { entries }),
            );
        }

        // The library sources the driver prepared, and the source roots they
        // materialize into. Both maps are empty when no library has sources.
        data.library_sources = sources.clone();
        data.library_source_roots = library_source_roots.clone();
        // The decompiled-output roots, likewise empty when no decompiler is
        // configured — which is what turns the fallback in
        // `hir::library_source_decl` off.
        data.library_decompiled = decompiled.clone();
        data.library_decompiled_roots = library_decompiled_roots.clone();

        data
    }

    /// Warms every registered library's indexes up on a background thread so
    /// the first request does not pay them: the classfile stubs (the full JDK
    /// image parse), and the *source* layout of each library that ships sources
    /// (a JDK `src.zip` is ~15k central-directory entries) that the
    /// parameter-name hints read names through.
    ///
    /// The work deliberately runs through [`ide::LibraryWarmup`] instead of a
    /// database snapshot: a snapshot clone held for the length of an archive
    /// parse blocks the main loop's next write — and with it every request —
    /// until the parse finishes, which on a workspace reload means the JDK
    /// image (seconds) and the whole server frozen behind it.
    ///
    /// Both are reported under one progress token: a library's index is one
    /// step of the stage, and splitting the bar in two would make the (cheap)
    /// source scans wait for every (expensive) stub build.
    fn warmup_libraries(&mut self, root: &AbsPathBuf) {
        let ids: Vec<LibraryId> = self.analysis_host.registered_libraries();
        if ids.is_empty() {
            // Nothing to index: the stage is over, and the client can be told
            // its hints are worth re-requesting right away.
            self.task_sender
                .send(BackgroundTaskEvent::LibrariesIndexed)
                .ok();
            return;
        }

        let token = format!("index-{}", root.as_str());
        let total = ids.len();
        self.report_progress(ProgressEvent {
            token: token.clone(),
            title: "Indexing libraries and sources".to_string(),
            message: Some(format!("Indexing {total} libraries...")),
            percentage: Some(0),
            state: ProgressState::Begin,
        });

        let task_sender = self.task_sender.clone();
        let warmup = self.analysis_host.library_warmup();
        // The live set of the graph this pass warms; captured once, so the
        // per-task closures share it instead of cloning the set each.
        let live: Arc<FxHashSet<LibraryId>> = Arc::new(ids.iter().copied().collect());
        let done_count = Arc::new(AtomicUsize::new(0));

        for &id in ids.iter() {
            let task_sender = task_sender.clone();
            let token = token.clone();
            let done_count = Arc::clone(&done_count);
            let warmup = warmup.clone();
            let live = Arc::clone(&live);

            self.thread_pool.execute(move || {
                warmup.warm(id);

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
                    warmup.prune(&live);

                    task_sender
                        .send(BackgroundTaskEvent::Progress(ProgressEvent {
                            token,
                            title: String::new(),
                            message: Some("Indexing complete".to_string()),
                            percentage: Some(100),
                            state: ProgressState::End,
                        }))
                        .ok();
                    task_sender.send(BackgroundTaskEvent::LibrariesIndexed).ok();
                }
            });
        }
    }

    /// Rebuilds the database source roots by partitioning the current vfs with
    /// [`Self::file_set_config`]. Each partitioned `FileSet` is tagged with the
    /// kind recorded in [`GlobalState::source_root_kinds`], so a library's
    /// materialized sources become a read-only `SourceRoot`.
    fn partition_source_roots(&self) -> Vec<SourceRoot> {
        let file_set_config = match &self.file_set_config {
            Some(config) => config,
            None => return Vec::new(),
        };

        let vfs = self.vfs.read();
        let mut file_sets = file_set_config.partition(&vfs.0);
        // The last set is the catch-all for files outside every configured
        // source root: a document the client opened that no build system or
        // plain workspace root covers (a standalone file, a scratch file).
        // It becomes a source root of its own — the *detached* root, always
        // last, mapped to [`detached_source_set`] — so such a file is still
        // lowered and navigates to its own declarations instead of being
        // silently dropped from analysis.
        let detached = file_sets.pop();
        let mut roots: Vec<SourceRoot> = file_sets
            .into_iter()
            .zip(self.source_root_kinds.iter())
            .map(|(file_set, kind)| match kind {
                SourceRootKind::SourceSet => SourceRoot::new(file_set),
                SourceRootKind::Library(_) | SourceRootKind::DecompiledLibrary(_) => {
                    SourceRoot::library(file_set)
                }
            })
            .collect();
        if let Some(detached) = detached {
            roots.push(SourceRoot::new(detached));
        }
        roots
    }

    /// Whether the source root that *owns* `path` ignores it.
    ///
    /// The rules that apply are the ones of the most specific configured root
    /// containing the file — the root `vfs`'s file-set partition assigns it to
    /// — and not those of whichever root the loader happened to configure
    /// first. A root nested under another therefore keeps its own rules: the
    /// enclosing root's `.gitignore` decides only the files the nested root
    /// does not contain, which is what keeps the generated sources of a build
    /// directory (a root of their own, under a gitignored directory) loadable.
    fn ignored_by_owning_root(
        matchers: &mut [(AbsPathBuf, ignore::IncrementalIgnore)],
        path: &vfs::AbsPath,
    ) -> bool {
        matchers
            .iter_mut()
            .filter_map(|(root, matcher)| {
                let rel = path.strip_prefix(root.as_path())?;
                Some((
                    root.as_path().components().count(),
                    matcher.matched(rel, false).is_ignore(),
                ))
            })
            .max_by_key(|(depth, _)| *depth)
            .is_some_and(|(_, ignored)| ignored)
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
                        if Self::ignored_by_owning_root(
                            &mut self.source_root_matchers,
                            path.as_path(),
                        ) {
                            continue;
                        }
                        vfs.0.set_file_contents(path.into(), contents);
                    }
                }
                self.process_changes();
            }
            vfs::loader::Message::Changed { files } => {
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
                        if Self::ignored_by_owning_root(
                            &mut self.source_root_matchers,
                            path.as_path(),
                        ) {
                            continue;
                        }
                        vfs.0.set_file_contents(path.into(), contents);
                    }
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
        let mut change = Change::default();

        // Whether any change added or removed a file from the workspace: only
        // then must the source roots (and the `file → root` salsa inputs) be
        // rebuilt. A pure text `Modify` never changes the file set, so it must
        // not re-set every file's source root — salsa 0.28 records a write on
        // every `set`, each of which is a new revision that invalidates every
        // root-keyed memo (`source_root_symbols_query`, ...) workspace-wide.
        let mut roots_changed = false;
        {
            let mut vfs = self.vfs.write();
            let (vfs, line_endings_map) = &mut *vfs;
            let vfs_changes = vfs.take_changes();

            for (file_id, changed_file) in vfs_changes {
                match &changed_file.change {
                    vfs::Change::Create(..) | vfs::Change::Delete => roots_changed = true,
                    vfs::Change::Modify(..) => {}
                }
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
        };

        // Files were added to or removed from the vfs, so the source roots need
        // to be rebuilt to keep `file_language_kind` working.
        if roots_changed && self.file_set_config.is_some() {
            let roots = self.partition_source_roots();
            change.set_roots(roots);
        }

        self.analysis_host.apply_change(change);
    }
}

/// Reports the library-source preparation as a single Begin/End pair on the
/// sync's token (just ended, so this is a fresh cycle): the work is one `mkdir`
/// per library plus a prune of dead roots, so a per-library report would be
/// noise.
fn report_prepared_sources(
    task_sender: &crossbeam_channel::Sender<BackgroundTaskEvent>,
    token: &str,
    prepared: usize,
) {
    task_sender
        .send(BackgroundTaskEvent::Progress(ProgressEvent {
            token: token.to_owned(),
            title: "Indexing library sources".to_string(),
            message: None,
            percentage: None,
            state: ProgressState::Begin,
        }))
        .ok();
    task_sender
        .send(BackgroundTaskEvent::Progress(ProgressEvent {
            token: token.to_owned(),
            title: String::new(),
            message: Some(format!("Prepared {prepared} source archives")),
            percentage: None,
            state: ProgressState::End,
        }))
        .ok();
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

/// Aggregates structured [`SyncProgress`] events into a single phase-budgeted
/// percentage and a human-readable status message, so the LSP client sees a
/// moving bar (IntelliJ-style) instead of a spinner plus raw line flashes.
///
/// Budgets: each phase owns a fixed percentage window (see
/// [`PHASE_PERCENTAGE_RANGES`]). Within a window the bar advances on every
/// meaningful event; when no events arrive the last reported position is kept.
/// `on_model_end` forces the "Exporting" phase to its window end, and
/// `on_sync_complete(true)` reports exactly 100% — both guarantees that a
/// successful sync always reaches 100% right before the progress token ends.
struct PhaseProgressAggregator {
    phase: SyncPhase,
    /// Cumulative bytes downloaded across all `Download` events.
    bytes_downloaded: u64,
    /// Total bytes expected once known; `None` until a size-bearing download.
    bytes_total: Option<u64>,
    download_count: u64,
    /// Module the tool is currently working on, if any.
    current_project: Option<String>,
    current_project_index: u32,
    current_project_total: u32,
    model_parsed: bool,
    finished: bool,
    last_report: Option<(String, u32)>,
}

impl PhaseProgressAggregator {
    fn new() -> Self {
        Self {
            phase: SyncPhase::Resolving,
            bytes_downloaded: 0,
            bytes_total: None,
            download_count: 0,
            current_project: None,
            current_project_index: 0,
            current_project_total: 0,
            model_parsed: false,
            finished: false,
            last_report: None,
        }
    }

    fn on_event(&mut self, event: SyncProgress) {
        match event {
            SyncProgress::Phase(phase) => {
                // Terminal phases (Done/Failed) are set by the sync result, not
                // by tool output; ignore them here so a stray "BUILD SUCCESS"
                // line does not end the bar early.
                if phase != SyncPhase::Done && phase != SyncPhase::Failed {
                    self.phase = phase;
                }
            }
            SyncProgress::Download {
                bytes_downloaded,
                bytes_total,
                ..
            } => {
                self.phase = SyncPhase::Downloading;
                self.download_count += 1;
                // A "Downloaded …" completion line carries the full size; track
                // it as both the running total and the expected total so the
                // bar approaches its window end as transfers finish.
                if bytes_total.is_some() && bytes_downloaded > self.bytes_downloaded {
                    self.bytes_downloaded = bytes_downloaded;
                }
                if let Some(total) = bytes_total {
                    self.bytes_total = Some(total);
                }
            }
            SyncProgress::Project {
                name,
                index,
                total,
                action,
            } => {
                self.current_project = Some(name.clone());
                if index > 0 {
                    self.current_project_index = index;
                }
                if total > 0 {
                    self.current_project_total = total;
                }
                match action.as_str() {
                    "building" | "compileJava" | "compileTestJava" => {
                        self.phase = SyncPhase::Compiling;
                    }
                    "configuring" => {
                        self.phase = SyncPhase::Configuring;
                    }
                    _ => {}
                }
            }
            SyncProgress::Info(_) => {
                // Free-form text carries no phase semantics; leave the phase
                // as-is so the message is not misleading.
            }
        }
    }

    /// A non-model output line arrived; nudge the phase along so a tool that
    /// never emits structured events still shows progress.
    fn on_line(&mut self, _line: &str) {
        // No-op: keeping this hook gives future per-line nudging (e.g. task
        // counts) a single chokepoint without coupling the parser here.
    }

    /// The `WORKSPACE_MODEL_BEGIN` marker was printed: the tool is serializing
    /// the final model, so the Exporting phase is in progress.
    fn on_model_begin(&mut self) {
        self.phase = SyncPhase::Exporting;
    }

    /// The `WORKSPACE_MODEL_END` marker was printed: the model is fully on
    /// stdout and the sync is guaranteed to succeed from here.
    fn on_model_end(&mut self) {
        self.phase = SyncPhase::Exporting;
        self.model_parsed = true;
    }

    /// Marks the sync finished, driving the percentage to exactly 100% on
    /// success (per the guaranteed-completion contract).
    fn on_sync_complete(&mut self, success: bool) {
        self.finished = true;
        if success {
            self.phase = SyncPhase::Done;
        } else {
            self.phase = SyncPhase::Failed;
        }
    }

    /// The current (message, percentage) to report, or `None` while the phase
    /// is still Resolving with no information at all.
    fn current_status(&mut self) -> Option<(String, u32)> {
        let percentage = if self.finished {
            match self.phase {
                SyncPhase::Done => 100,
                SyncPhase::Failed => 100,
                _ => self.budgeted_percentage(),
            }
        } else if self.model_parsed {
            // Model fully serialized but sync not yet marked complete: hold at
            // the top of the Exporting window instead of jumping to 100 early.
            self.budgeted_percentage()
        } else {
            self.budgeted_percentage()
        };

        let message = if self.finished && self.phase == SyncPhase::Failed {
            "Sync failed".to_string()
        } else if self.finished {
            "Sync complete".to_string()
        } else {
            self.status_message()
        };

        if self.last_report.as_ref() == Some(&(message.clone(), percentage)) {
            return None;
        }
        self.last_report = Some((message.clone(), percentage));
        Some((message, percentage))
    }

    fn budgeted_percentage(&self) -> u32 {
        let range = PHASE_PERCENTAGE_RANGES
            .iter()
            .find(|(phase, _)| *phase == self.phase)
            .map(|(_, (lo, hi))| (*lo, *hi))
            .unwrap_or((0, 99));

        let (lo, hi) = range;
        match self.phase {
            SyncPhase::Downloading => {
                // Within the download window, advance proportional to bytes.
                let total = self.bytes_total.unwrap_or(0);
                if total > 0 && self.bytes_downloaded > 0 {
                    let ratio = self.bytes_downloaded.min(total) as f64 / total as f64;
                    lo + ((hi - lo) as f64 * ratio) as u32
                } else {
                    lo
                }
            }
            SyncPhase::Configuring | SyncPhase::Compiling => {
                // Advance through the window by module index when known.
                if self.current_project_total > 0 {
                    let ratio = (self.current_project_index as f64
                        / self.current_project_total as f64)
                        .min(1.0);
                    lo + ((hi - lo) as f64 * ratio) as u32
                } else {
                    lo
                }
            }
            SyncPhase::Exporting => hi,
            _ => lo,
        }
    }

    fn status_message(&self) -> String {
        match self.phase {
            SyncPhase::Resolving => "Resolving dependencies…".to_string(),
            SyncPhase::Downloading => {
                let downloads = if self.download_count > 0 {
                    format!("{} dependencies", self.download_count)
                } else {
                    "dependencies".to_string()
                };
                match (self.bytes_downloaded, self.bytes_total) {
                    (d, Some(t)) if t > 0 && d > 0 => {
                        format!(
                            "Downloading {downloads} · {} / {}",
                            human_bytes(d),
                            human_bytes(t)
                        )
                    }
                    (d, None) if d > 0 => format!("Downloading {downloads} · {}", human_bytes(d)),
                    _ => format!("Downloading {downloads}…"),
                }
            }
            SyncPhase::Configuring => {
                if let Some(project) = &self.current_project {
                    let idx = module_label(self.current_project_index, self.current_project_total);
                    format!("Configuring project {project}{idx}…")
                } else {
                    "Configuring projects…".to_string()
                }
            }
            SyncPhase::Compiling => {
                if let Some(project) = &self.current_project {
                    let idx = module_label(self.current_project_index, self.current_project_total);
                    format!("Compiling project {project}{idx}…")
                } else {
                    "Compiling projects…".to_string()
                }
            }
            SyncPhase::Exporting => {
                if self.model_parsed {
                    "Finalizing workspace model…".to_string()
                } else {
                    "Exporting workspace model…".to_string()
                }
            }
            SyncPhase::Done => "Sync complete".to_string(),
            SyncPhase::Failed => "Sync failed".to_string(),
        }
    }
}

/// Formats a `(index, total)` module position as ` (i/n)` when known, else "".
fn module_label(index: u32, total: u32) -> String {
    if total > 0 && index >= 1 {
        format!(" ({index}/{total})")
    } else {
        String::new()
    }
}

/// Formats a byte count for the status message.
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GiB", b / GB)
    } else if b >= MB {
        format!("{:.1} MiB", b / MB)
    } else if b >= KB {
        format!("{:.1} KiB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ignore verdict of a loaded file is the one of the source root that
    /// owns it. The ancestor's matcher comes first here, as it does in the
    /// loader's entry order (roots are sorted by path), so a file under the
    /// nested root must not be dropped by the ancestor's `.gitignore`.
    #[test]
    fn owning_root_decides_the_ignore_verdict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
        let nested = dir.path().join("build/generated");
        let file = nested.join("pkg/Foo.java");
        std::fs::create_dir_all(nested.join("pkg")).unwrap();
        std::fs::write(&file, "package pkg;\nclass Foo {}\n").unwrap();
        let outside = dir.path().join("build/Other.java");
        std::fs::write(&outside, "class Other {}\n").unwrap();

        let matcher = |root: &std::path::Path| {
            ignore::WalkBuilder::new(root)
                .standard_filters(true)
                .require_git(false)
                .build_matchers()
                .into_iter()
                .next()
                .expect("a matcher for a directory with ignore rules")
        };
        let mut matchers = vec![
            (
                AbsPathBuf::assert_utf8(dir.path().to_path_buf()),
                matcher(dir.path()),
            ),
            (AbsPathBuf::assert_utf8(nested.clone()), matcher(&nested)),
        ];

        assert!(
            !GlobalState::ignored_by_owning_root(&mut matchers, &AbsPathBuf::assert_utf8(file)),
            "the nested root's own rules do not ignore its file, and the \
             ancestor's `build/` entry must not reach it"
        );
        assert!(
            GlobalState::ignored_by_owning_root(&mut matchers, &AbsPathBuf::assert_utf8(outside)),
            "a file the nested root does not contain is the ancestor's, whose \
             rules ignore it"
        );
    }

    #[test]
    fn download_advances_within_window() {
        let mut agg = PhaseProgressAggregator::new();
        agg.on_event(SyncProgress::Phase(SyncPhase::Downloading));
        agg.on_event(SyncProgress::Download {
            dependency: "x.jar".into(),
            bytes_downloaded: 0,
            bytes_total: Some(100),
        });
        assert_eq!(
            agg.current_status(),
            Some(("Downloading 1 dependencies…".into(), 15))
        );

        agg.on_event(SyncProgress::Download {
            dependency: "x.jar".into(),
            bytes_downloaded: 50,
            bytes_total: Some(100),
        });
        assert_eq!(
            agg.current_status(),
            Some(("Downloading 2 dependencies · 50 B / 100 B".into(), 37))
        );

        agg.on_event(SyncProgress::Download {
            dependency: "x.jar".into(),
            bytes_downloaded: 100,
            bytes_total: Some(100),
        });
        assert_eq!(
            agg.current_status(),
            Some(("Downloading 3 dependencies · 100 B / 100 B".into(), 60))
        );
    }

    #[test]
    fn configuring_advances_by_module() {
        let mut agg = PhaseProgressAggregator::new();
        agg.on_event(SyncProgress::Project {
            name: ":app".into(),
            index: 1,
            total: 3,
            action: "configuring".into(),
        });
        assert_eq!(
            agg.current_status(),
            Some(("Configuring project :app (1/3)…".into(), 68))
        );
    }

    #[test]
    fn model_end_and_complete_reach_100() {
        let mut agg = PhaseProgressAggregator::new();
        agg.on_model_begin();
        assert_eq!(
            agg.current_status(),
            Some(("Exporting workspace model…".into(), 99))
        );
        agg.on_model_end();
        assert_eq!(
            agg.current_status(),
            Some(("Finalizing workspace model…".into(), 99))
        );
        agg.on_sync_complete(true);
        assert_eq!(agg.current_status(), Some(("Sync complete".into(), 100)));
    }

    #[test]
    fn failure_reports_failed() {
        let mut agg = PhaseProgressAggregator::new();
        agg.on_sync_complete(false);
        assert_eq!(agg.current_status(), Some(("Sync failed".into(), 100)));
    }

    #[test]
    fn duplicate_status_not_reemitted() {
        let mut agg = PhaseProgressAggregator::new();
        agg.on_model_begin();
        let first = agg.current_status();
        let second = agg.current_status();
        assert_eq!(first, Some(("Exporting workspace model…".into(), 99)));
        assert_eq!(second, None);
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(500), "500 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(12_897_485), "12.3 MiB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }
}
