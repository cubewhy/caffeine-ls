use std::{
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use camino::Utf8PathBuf;
use crossbeam_channel::Receiver;
use hir::{LibraryId as HirLibraryId, LibraryKind};
use ide_db::base_db::{FileChange, SourceRoot, salsa::Cancelled};
use lsp_server::{Connection, ErrorCode, Notification, Request};
use lsp_types::*;
use project_model::ClasspathEntry;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use rustc_hash::FxHashSet;
use triomphe::Arc;
use vfs::AbsPathBuf;

use crate::{
    GlobalState,
    config::Config,
    global_state::{BackgroundTaskEvent, OutgoingRequest, ProgressEvent, ProgressState},
    handlers::{
        self,
        dispatch::{NotificationDispatcher, RequestDispatcher},
    },
    line_index::LineEndings,
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

            self.process_changes();

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

                    match system.get_executor().sync(root.as_ref(), &java_home) {
                        Ok(graph) => {
                            finish_progress();
                            task_sender
                                .send(BackgroundTaskEvent::WorkspaceLoaded { graph, root })
                                .ok();
                        }
                        Err(err) => {
                            tracing::error!(?root, "Metadata compilation failure: {}", err);
                            finish_progress();
                            task_sender
                                .send(BackgroundTaskEvent::NotifyUser {
                                    typ: MessageType::Error,
                                    message: format!("Failed to receive project metadata: {err}"),
                                })
                                .ok();
                        }
                    }
                });
            }

            BackgroundTaskEvent::WorkspaceLoaded { graph, root } => {
                tracing::info!("Project configuration graph successfully loaded: {graph:#?}");

                self.apply_loaded_graph(graph, root);
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
                self.apply_loaded_graph(project_model::WorkspaceGraph::plain(root.clone()), root);
            }
        }
    }

    /// Turns a loaded [`project_model::WorkspaceGraph`] into database source
    /// roots and configures the vfs loader with the source roots declared by
    /// the build system.
    fn apply_loaded_graph(&mut self, graph: project_model::WorkspaceGraph, root: AbsPathBuf) {
        tracing::info!(?root, "Applying workspace source roots and loader config");

        let mut source_roots: Vec<AbsPathBuf> = Vec::new();

        for project in graph.projects.values() {
            for source_set in project.source_sets.values() {
                source_roots.extend(source_set.source_roots.iter().cloned());
                source_roots.extend(source_set.generated_source_roots.iter().cloned());
            }
        }

        source_roots.sort();
        source_roots.dedup();

        let mut builder = vfs::file_set::FileSetConfig::builder();
        builder.add_file_set(
            source_roots
                .iter()
                .cloned()
                .map(vfs::VfsPath::from)
                .collect(),
        );
        let file_set_config = builder.build();

        // Load and watch the source roots declared by the build system,
        // skipping paths that gitignore rules exclude.
        self.vfs_config_version += 1;
        let mut matchers = Vec::new();
        let entries: Vec<vfs::loader::Entry> = source_roots
            .iter()
            .map(|source_root| {
                let mut builder = ignore::WalkBuilder::new(source_root);
                builder.standard_filters(true).require_git(false);
                if let Some(matcher) = builder.build_matchers().into_iter().next() {
                    matchers.push((source_root.clone(), matcher));
                }

                vfs::loader::Entry::Directories(vfs::loader::Directories {
                    extensions: vec!["java".into(), "kt".into(), "kts".into()],
                    include: vec![source_root.clone()],
                    exclude: collect_ignored_paths(source_root),
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
        self.apply_source_roots();
        self.register_libraries(&graph, &root);
        self.refresh_diagnostics();
    }

    /// Registers the JDK and classpath jars from the workspace graph with the
    /// stub index, then warms the indexes up on a background thread so the
    /// first type query does not pay the full JDK parse cost.
    fn register_libraries(&mut self, graph: &project_model::WorkspaceGraph, root: &AbsPathBuf) {
        let mut libraries: Vec<(HirLibraryId, LibraryKind, AbsPathBuf)> = Vec::new();

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
                libraries.push((HirLibraryId(id.0), kind, path));
            }
        }

        // Classpath jars referenced by any source set.
        for project in graph.projects.values() {
            for source_set in project.source_sets.values() {
                for entry in &source_set.compile_classpath {
                    if let ClasspathEntry::External(lib_id) = entry
                        && let Some(lib) = graph.library_paths.get(lib_id)
                    {
                        libraries.push((
                            HirLibraryId(lib_id.0),
                            LibraryKind::Jar,
                            lib.path.clone(),
                        ));
                    }
                }
            }
        }

        // Deduplicate: the same jar may be referenced from many source sets.
        libraries.sort_by_key(|(id, _, _)| id.0);
        libraries.dedup_by_key(|(id, _, _)| id.0);

        let db = self.analysis_host.raw_database_mut();
        for (id, kind, path) in &libraries {
            hir::register_library(db, *id, *kind, path.as_str().into());
        }

        let ids: Vec<HirLibraryId> = libraries.into_iter().map(|(id, _, _)| id).collect();
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
    fn apply_source_roots(&mut self) {
        let mut change = FileChange::default();
        change.set_roots(self.partition_source_roots());
        self.analysis_host.apply_change(change);
    }

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
            vfs::loader::Message::Loaded { files } | vfs::loader::Message::Changed { files } => {
                {
                    let mut vfs = self.vfs.write();
                    for (path, contents) in files {
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
            change.set_roots(self.partition_source_roots());
        }

        self.analysis_host.apply_change(change);
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

#[cfg(test)]
mod tests {
    use super::collect_ignored_paths;
    use vfs::AbsPathBuf;

    #[test]
    fn collect_ignored_paths_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::create_dir_all(dir.path().join("ignored_dir")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join(".gitignore"),
            "target/\nignored_dir/\n*.log\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("ignored_dir/a.java"), "").unwrap();
        std::fs::write(dir.path().join("src/Main.java"), "").unwrap();
        std::fs::write(dir.path().join("src/debug.log"), "").unwrap();

        let root = AbsPathBuf::assert_utf8(dir.path().to_path_buf());
        let ignored = collect_ignored_paths(&root);

        let mut ignored: Vec<String> = ignored
            .iter()
            .map(|path| path.as_str().to_owned())
            .collect();
        ignored.sort();

        let mut expected = vec![
            root.join(".gitignore").as_str().to_owned(),
            root.join("ignored_dir").as_str().to_owned(),
            root.join("src/debug.log").as_str().to_owned(),
            root.join("target").as_str().to_owned(),
        ];
        expected.sort();

        assert_eq!(ignored, expected);
    }
}
