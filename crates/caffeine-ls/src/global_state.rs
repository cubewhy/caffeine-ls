use crate::{
    config::ConfigErrors,
    line_index::{LineEndings, LineIndex},
    lsp::{from_proto, semantic_tokens},
    mem_docs::MemDocs,
    task_pool::TaskPool,
};
use lsp_types::Uri;
use project_model::WorkspaceGraph;
use rustc_hash::FxHashMap;
use std::time::Instant;
use triomphe::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use ide::{Analysis, AnalysisHost, Cancellable, LibraryId, LibraryInfo, LibrarySources};
use lsp_server::{ErrorCode, Response};
use parking_lot::{MappedRwLockReadGuard, RwLock, RwLockReadGuard};

use vfs::{AbsPathBuf, FileId, VfsPath};

use crate::config::Config;
use crate::library_view;

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
        /// Library → the materialized source roots the driver prepared.
        sources: FxHashMap<LibraryId, LibrarySources>,
        /// Library → the decompiled-output root the driver prepared. Empty when
        /// no decompiler is configured.
        decompiled: FxHashMap<LibraryId, AbsPathBuf>,
    },
    SyncFailed {
        message: String,
        log_file: Option<std::path::PathBuf>,
    },
    Progress(ProgressEvent),
    /// Every registered library has been indexed — classfile stubs and the
    /// source layouts the parameter-name hints read names through. Sent once
    /// per workspace load, and also when there was nothing to index.
    ///
    /// It is what triggers the inlay-hint refresh: a hint request sent before
    /// it would race the stage for the very archives it is filling, and pay for
    /// them itself.
    LibrariesIndexed,
    VfsLoaded,
    AsyncRequestCompleted {
        id: lsp_server::RequestId,
        result: Result<serde_json::Value, anyhow::Error>,
    },
    /// An async request was cancelled by a pending salsa write. The closure
    /// re-runs the request once the write has been applied; it must not hold a
    /// database snapshot, since the writer blocks until every snapshot clone
    /// is released.
    AsyncRequestRetry {
        id: lsp_server::RequestId,
        run: PendingRequest,
    },
    /// An async request observed its `$/cancelRequest` token and stopped early.
    /// The main loop has already replied `RequestCancelled`; no further
    /// response is sent and the worker's snapshot is dropped.
    AsyncRequestAborted {
        id: lsp_server::RequestId,
    },
    /// A request handler needs library files in the database before it can
    /// answer: the main loop reads each archive entry into the cache and
    /// decompiles each class, loads the results into the vfs, and re-runs the
    /// request on a fresh snapshot (see `handlers::dispatch::DeferForLibraryFiles`).
    LoadLibraryFiles {
        files: Vec<ide::LibraryFileRef>,
        retry: (lsp_server::RequestId, PendingRequest),
    },
    /// The decompiled files the worker produced, and the per-class failures.
    /// The retry is pushed once the files are in the vfs, exactly as the
    /// inline source path does.
    LibraryDecompiled {
        files: Vec<(vfs::VfsPath, Vec<u8>)>,
        failed: Vec<(LibraryId, Arc<str>, String)>,
        retry: (lsp_server::RequestId, PendingRequest),
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

/// The kind of each registered source root, in `SourceRootId` order — the same
/// order `Change::apply` assigns ids in, so `partition_source_roots` can
/// tag each partitioned `FileSet` with its owner.
pub(crate) enum SourceRootKind {
    /// A build-system source root of an owning source set.
    SourceSet,
    /// A read-only root holding a library's materialized sources.
    Library(LibraryId),
    /// A read-only root holding a library's decompiled output. Read-only for
    /// the same reason as [`SourceRootKind::Library`]: the decompiler wrote it,
    /// the client only reads it.
    DecompiledLibrary(LibraryId),
}

pub(crate) type ReqHandler = fn(&mut GlobalState, lsp_server::Response);
pub(crate) enum OutgoingRequest {
    Generic(ReqHandler),
    SelectBuildSystem {
        root: AbsPathBuf,
        systems: Vec<project_model::BuildSystemType>,
    },
    CreateProgress {
        token: String,
    },
    OpenBuildToolLog {
        log_file: std::path::PathBuf,
    },
}

/// Lifecycle of a `$/progress` token on the client side.
pub(crate) enum ProgressTokenState {
    /// A `window/workDoneProgress/create` request is in flight; events for
    /// this token are buffered until the client acknowledges it.
    Creating(Vec<ProgressEvent>),
    /// The client acknowledged the token; progress flows straight through.
    Active,
}

type ReqQueue = lsp_server::ReqQueue<(String, Instant), OutgoingRequest>;

/// An async request that was cancelled by a pending write, ready to be re-run
/// on a fresh snapshot once the write has been applied. The request id is
/// captured inside the closure.
pub(crate) type PendingRequest = Box<dyn FnOnce(GlobalStateSnapshot) + Send>;

/// The client-cancellation token of an in-flight async request. Flipped by
/// `$/cancelRequest` so the running worker stops instead of burning CPU to
/// completion; salsa's cancellation only fires for pending writes.
#[derive(Clone)]
pub(crate) struct CancellationToken(triomphe::Arc<std::sync::atomic::AtomicBool>);

impl Default for CancellationToken {
    fn default() -> Self {
        Self(triomphe::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )))
    }
}

impl CancellationToken {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Error a handler returns once it observes its [`CancellationToken`] flipped.
/// The main loop has already replied `RequestCancelled`; the worker just drops
/// its snapshot — no re-queue, no duplicate response.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientCancelled;

impl std::fmt::Display for ClientCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request cancelled by client")
    }
}

impl std::error::Error for ClientCancelled {}

pub struct GlobalState {
    sender: Sender<lsp_server::Message>,
    req_queue: ReqQueue,

    pub(crate) task_sender: Sender<BackgroundTaskEvent>,
    pub(crate) task_receiver: Receiver<BackgroundTaskEvent>,
    pub(crate) thread_pool: TaskPool,

    pub(crate) config: Arc<Config>,
    pub(crate) config_errors: Option<ConfigErrors>,
    pub(crate) analysis_host: AnalysisHost,
    pub(crate) mem_docs: MemDocs,

    pub(crate) shutdown_requested: bool,
    pub(crate) exit_requested: bool,

    // Vfs
    pub(crate) loader: Handle<Box<dyn vfs::loader::Handle>, Receiver<vfs::loader::Message>>,
    pub(crate) vfs: Arc<RwLock<(vfs::Vfs, FxHashMap<FileId, LineEndings>)>>,
    pub(crate) vfs_config_version: u32,
    /// Async requests cancelled by a pending salsa write, re-run once the
    /// write is applied. See [`BackgroundTaskEvent::AsyncRequestRetry`].
    pub(crate) pending_requests: Vec<PendingRequest>,
    /// Cancellation tokens of in-flight async requests, keyed by request id, so
    /// `$/cancelRequest` aborts the running worker instead of letting it finish
    /// a full pull.
    pub(crate) inflight_cancellations: FxHashMap<lsp_server::RequestId, CancellationToken>,
    /// Tracks the loader config version whose VFS scan progress is currently
    /// being reported to the client, so stale `Message::Progress` updates from
    /// a previous (reload) config are ignored.
    pub(crate) scan_config_version: Option<u32>,
    /// Lifecycle state of `$/progress` tokens awaiting (or acknowledged by)
    /// the client's `window/workDoneProgress/create` handshake.
    pub(crate) progress_tokens: FxHashMap<String, ProgressTokenState>,
    /// Partitions the vfs into source roots. `None` until a workspace has been loaded.
    pub(crate) file_set_config: Option<vfs::file_set::FileSetConfig>,
    /// The kind of each registered source root, in `SourceRootId` order.
    pub(crate) source_root_kinds: Vec<SourceRootKind>,
    /// Gitignore-aware matchers for the loaded source roots, used to filter
    /// out ignored files delivered by the loader.
    pub(crate) source_root_matchers: Vec<(AbsPathBuf, ignore::IncrementalIgnore)>,
    /// The registered libraries of the loaded workspace: the classfile archive
    /// of each, which is what a decompile reads a class's bytes out of and what
    /// it hands the decompiler as a classpath. Empty before the first load.
    pub(crate) library_archives: FxHashMap<LibraryId, LibraryInfo>,
    /// Whether a decompiler failure has already been reported to the user. A
    /// broken JDK or jar fails every navigation, and one warning is a diagnosis
    /// where one per navigation is noise.
    pub(crate) decompiler_error_reported: bool,
    /// The semantic-token stream last sent for each document, so a
    /// `textDocument/semanticTokens/full/delta` request can be answered with the
    /// edit that turns it into the current one (see
    /// [`crate::lsp::semantic_tokens::DeltaCache`]).
    pub(crate) semantic_tokens: Arc<RwLock<semantic_tokens::DeltaCache>>,
}

impl GlobalState {
    pub fn new(sender: Sender<lsp_server::Message>, config: Config) -> Self {
        let (task_sender, task_receiver) = unbounded();

        let thread_pool = TaskPool::new("caffeine-task", num_cpus::get());

        let loader = {
            let (sender, receiver) = unbounded::<vfs::loader::Message>();
            let handle: vfs_notify::NotifyHandle = vfs::loader::Handle::spawn(sender);
            let handle = Box::new(handle) as Box<dyn vfs::loader::Handle>;
            Handle { handle, receiver }
        };

        let analysis_host = AnalysisHost::new();
        if analysis_host.enable_persistent_stub_cache(&config.get_cache_dir()) {
            tracing::debug!("persistent stub cache enabled");
        }

        Self {
            sender,
            req_queue: ReqQueue::default(),

            task_sender,
            task_receiver,
            thread_pool,

            config: Arc::new(config),
            config_errors: None,

            analysis_host,
            mem_docs: MemDocs::default(),

            shutdown_requested: false,
            exit_requested: false,

            loader,
            vfs: Arc::new(RwLock::new((vfs::Vfs::default(), Default::default()))),
            vfs_config_version: 0,
            pending_requests: Vec::new(),
            inflight_cancellations: FxHashMap::default(),
            scan_config_version: None,
            progress_tokens: FxHashMap::default(),
            file_set_config: None,
            source_root_kinds: Vec::new(),
            source_root_matchers: Vec::new(),
            library_archives: FxHashMap::default(),
            decompiler_error_reported: false,
            semantic_tokens: Arc::new(RwLock::new(semantic_tokens::DeltaCache::default())),
        }
    }

    // Helper to send response back to client
    pub(crate) fn handle_result<R>(
        &mut self,
        id: lsp_server::RequestId,
        result: anyhow::Result<R::Result>,
    ) where
        R: lsp_types::Request,
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

    #[track_caller]
    fn send(&self, msg: lsp_server::Message) {
        self.sender.send(msg).unwrap();
    }

    pub(crate) fn respond_ok<R>(&mut self, id: lsp_server::RequestId, result: R)
    where
        R: serde::Serialize,
    {
        // The entry may already be gone if `$/cancelRequest` replied first;
        // drop the late result instead of sending a duplicate response.
        let Some((method, start)) = self.req_queue.incoming.complete(&id) else {
            return;
        };
        tracing::info!("handled {} in {:?}", method, start.elapsed());
        let resp = lsp_server::Response::new_ok(id, result);
        self.send(resp.into());
    }

    pub(crate) fn respond_err(
        &mut self,
        id: lsp_server::RequestId,
        code: ErrorCode,
        message: String,
    ) {
        // See [`Self::respond_ok`]: a cancelled request already has its
        // `RequestCancelled` response, so a late error is dropped silently.
        let Some((method, _)) = self.req_queue.incoming.complete(&id) else {
            return;
        };
        tracing::error!("failed {}: {}", method, message);
        let resp = lsp_server::Response::new_err(id, code as i32, message);
        self.send(resp.into());
    }

    pub(crate) fn notify<N>(&self, params: N::Params)
    where
        N: lsp_types::Notification,
    {
        let notif = lsp_server::Notification::new(N::METHOD.to_string(), params);
        self.send(notif.into());
    }

    pub(crate) fn send_request<R>(&mut self, params: R::Params, state: OutgoingRequest)
    where
        R: lsp_types::Request,
    {
        let req = self
            .req_queue
            .outgoing
            .register(R::METHOD.to_string(), params, state);
        self.send(req.into());
    }

    pub(crate) fn register_request(
        &mut self,
        req: &lsp_server::Request,
        request_received: Instant,
    ) {
        self.req_queue
            .incoming
            .register(req.id.clone(), (req.method.clone(), request_received));
    }

    /// Registers a fresh cancellation token for an in-flight async request and
    /// returns it, so the worker carries the token the client flips.
    pub(crate) fn register_async_cancellation(
        &mut self,
        id: lsp_server::RequestId,
    ) -> CancellationToken {
        let token = CancellationToken::default();
        self.inflight_cancellations.insert(id, token.clone());
        token
    }

    /// Drops the cancellation token of a finished async request.
    pub(crate) fn remove_async_cancellation(&mut self, id: &lsp_server::RequestId) {
        self.inflight_cancellations.remove(id);
    }

    pub(crate) fn complete_request(&mut self, resp: lsp_server::Response) {
        let Some(outgoing_req) = self.req_queue.outgoing.complete(resp.id.clone()) else {
            tracing::warn!(?resp.id, "Received response for an unknown or untracked request");
            return;
        };

        match outgoing_req {
            OutgoingRequest::CreateProgress { token } => {
                if let Some(err) = &resp.error {
                    tracing::warn!(?resp.id, "Client rejected progress token {token}: {err:?}");
                    self.progress_tokens.remove(&token);
                } else {
                    self.flush_progress(&token);
                }
            }

            OutgoingRequest::SelectBuildSystem { root, systems } => {
                if let Some(err) = &resp.error {
                    tracing::error!(?resp.id, "Client returned error response: {:?}", err);
                    return;
                }
                self.handle_select_build_system_response(resp, root, systems);
            }

            OutgoingRequest::OpenBuildToolLog { log_file } => {
                if let Some(err) = &resp.error {
                    tracing::error!(?resp.id, "Client returned error response: {:?}", err);
                    return;
                }
                self.handle_open_build_tool_log_response(resp, log_file);
            }

            OutgoingRequest::Generic(handler) => {
                if let Some(err) = &resp.error {
                    tracing::error!(?resp.id, "Client returned error response: {:?}", err);
                    return;
                }
                handler(self, resp);
            }
        }
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
            mem_docs: self.mem_docs.clone(),
            cancelled: CancellationToken::default(),
            semantic_tokens: Arc::clone(&self.semantic_tokens),
        }
    }

    /// The vfs path a *library view* URI names, `None` for every other URI. The
    /// notification handlers need the same resolution the request handlers get
    /// from [`GlobalStateSnapshot::url_to_file_id`], without a snapshot.
    pub(crate) fn view_vfs_path(&self, uri: &Uri) -> Option<VfsPath> {
        Some(VfsPath::from(library_view_path(&self.config, uri)?))
    }

    /// Re-runs async requests that were cancelled by a pending salsa write, on
    /// a fresh snapshot that observes the change just applied to the database.
    /// Called from the main loop after `process_changes`.
    pub(crate) fn run_pending_requests(&mut self) {
        let pending = std::mem::take(&mut self.pending_requests);
        for run in pending {
            let snapshot = self.snapshot();
            self.thread_pool.execute(move || {
                run(snapshot);
            });
        }
    }

    pub(crate) fn cancel(&mut self, request_id: lsp_server::RequestId) {
        // Flip the in-flight worker's token so it stops at the next checkpoint
        // instead of running the whole pull to completion.
        if let Some(token) = self.inflight_cancellations.get(&request_id) {
            token.cancel();
        }
        if let Some(response) = self.req_queue.incoming.cancel(request_id) {
            self.send(response.into());
        }
    }
}

#[derive(Clone)]
pub struct GlobalStateSnapshot {
    pub(crate) config: Arc<Config>,
    pub(crate) analysis: Analysis,
    mem_docs: MemDocs,
    vfs: Arc<RwLock<(vfs::Vfs, FxHashMap<FileId, LineEndings>)>>,
    /// Set by `$/cancelRequest` for the request this snapshot serves; workers
    /// checkpoint it and abort early instead of finishing a full pull.
    pub(crate) cancelled: CancellationToken,
    /// The semantic-token streams the server last sent, shared with the main
    /// loop's state (see [`GlobalState::semantic_tokens`]).
    semantic_tokens: Arc<RwLock<semantic_tokens::DeltaCache>>,
}

impl GlobalStateSnapshot {
    fn vfs_read(&self) -> MappedRwLockReadGuard<'_, vfs::Vfs> {
        RwLockReadGuard::map(self.vfs.read(), |(it, _)| it)
    }

    /// The semantic-token delta cache: the streams a `full/delta` request can
    /// be diffed against, written by the `full` and `full/delta` handlers.
    pub(crate) fn semantic_tokens(&self) -> &RwLock<semantic_tokens::DeltaCache> {
        &self.semantic_tokens
    }

    /// Returns `None` if the file was excluded.
    pub(crate) fn url_to_file_id(&self, url: &Uri) -> anyhow::Result<Option<FileId>> {
        // A library view has no path the client could resolve on its own: the
        // view scheme is what names it on the way in, exactly as
        // [`Self::file_id_to_url`] spells it on the way out.
        if let Some(path) = library_view_path(&self.config, url) {
            return vfs_path_to_file_id(&self.vfs_read(), &VfsPath::from(path));
        }
        url_to_file_id(&self.vfs_read(), url)
    }

    /// The URI of a file known to the vfs.
    pub(crate) fn file_id_to_url(&self, file_id: FileId) -> anyhow::Result<Uri> {
        let vfs = self.vfs_read();
        let path = vfs.file_path(file_id);
        let path = path
            .as_path()
            .ok_or_else(|| anyhow::format_err!("file has no absolute path: {file_id:?}"))?;
        // A materialized library file is an implementation detail of this
        // server: when the client serves library views itself, it is named by
        // its view — `<scheme>://<library>/…/<rel>` — and never by the cache
        // path it happens to live at. Only a client that configured a scheme
        // pays for the layout lookup.
        if let Some(scheme) = self.config.library_uri_scheme()
            && let Some((view, library, rel)) =
                library_view::relative_to_view(&self.config.get_cache_dir(), path)
            && let Some(uri) = library_view::uri(scheme, &view, &library, &rel)
        {
            return Ok(uri);
        }
        Ok(crate::lsp::to_proto::url(path))
    }

    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {
        // A file deleted mid-pull (removed from the source set, then reverted
        // to empty text) may have lost its line-endings row; fall back rather
        // than panic, so a workspace pull racing a deletion stays graceful.
        let endings = self
            .vfs
            .read()
            .1
            .get(&file_id)
            .copied()
            .unwrap_or(LineEndings::Unix);
        let index = self.analysis.file_line_index(file_id)?;
        let res = LineIndex {
            index,
            endings,
            encoding: self.config.negotiated_encoding(),
        };
        Ok(res)
    }

    /// The version of an open document (client-maintained), `None` for files
    /// that are not open. Resolved directly from the `FileId`, avoiding a
    /// `FileId -> Uri -> VfsPath` round-trip per file in workspace pulls.
    pub(crate) fn open_document_version_by_file(&self, file_id: FileId) -> Option<i32> {
        let path = self.vfs_read().file_path(file_id).clone();
        self.mem_docs.get(&path).map(|doc| doc.version)
    }

    /// FileIds of the documents the client has open (`didOpen`), dropping files
    /// excluded from the vfs. Order is unspecified; callers sort.
    pub(crate) fn opened_file_ids(&self) -> Vec<FileId> {
        let vfs = self.vfs_read();
        self.mem_docs
            .paths()
            .filter_map(|path| vfs_path_to_file_id(&vfs, path).ok().flatten())
            .collect()
    }
}

/// Returns `None` if the file was excluded.
pub(crate) fn url_to_file_id(vfs: &vfs::Vfs, url: &Uri) -> anyhow::Result<Option<FileId>> {
    let path = from_proto::vfs_path(url)?;
    vfs_path_to_file_id(vfs, &path)
}

/// The cache path a *library view* URI names: `None` when the client configured
/// no view scheme, or when the URI is not one of this server's views (another
/// scheme, an unknown backend, a `..` out of the cache). The one place both the
/// request snapshot and the notification handlers resolve a view from, so a
/// document the client opened is the same file a definition answers with.
fn library_view_path(config: &Config, uri: &Uri) -> Option<AbsPathBuf> {
    let scheme = config.library_uri_scheme()?;
    library_view::view_path(
        &config.get_cache_dir(),
        scheme,
        uri,
        crate::decompiler::is_backend,
    )
}

/// Returns `None` if the file was excluded or is no longer known to the vfs
/// (e.g. it was deleted); callers respond with an empty result rather than an
/// internal error.
pub(crate) fn vfs_path_to_file_id(
    vfs: &vfs::Vfs,
    vfs_path: &VfsPath,
) -> anyhow::Result<Option<FileId>> {
    let Some((file_id, excluded)) = vfs.file_id(vfs_path) else {
        return Ok(None);
    };
    match excluded {
        vfs::FileExcluded::Yes => Ok(None),
        vfs::FileExcluded::No => Ok(Some(file_id)),
    }
}
