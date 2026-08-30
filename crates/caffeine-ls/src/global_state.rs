use crate::{
    config::ConfigErrors,
    line_index::{LineEndings, LineIndex},
    lsp::from_proto,
    mem_docs::MemDocs,
    task_pool::TaskPool,
};
use lsp_types::Uri;
use project_model::WorkspaceGraph;
use rustc_hash::FxHashMap;
use std::time::Instant;
use triomphe::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use hir::enable_persistent_stub_cache;
use ide::{Analysis, AnalysisHost, Cancellable};
use lsp_server::{ErrorCode, Response};
use parking_lot::{MappedRwLockReadGuard, RwLock, RwLockReadGuard};

use vfs::{AbsPathBuf, FileId, VfsPath};

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
    SyncFailed {
        message: String,
        log_file: Option<std::path::PathBuf>,
    },
    Progress(ProgressEvent),
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
pub(crate) struct CancellationToken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Default for CancellationToken {
    fn default() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
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
    /// Gitignore-aware matchers for the loaded source roots, used to filter
    /// out ignored files delivered by the loader.
    pub(crate) source_root_matchers: Vec<(AbsPathBuf, ignore::IncrementalIgnore)>,
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
        if enable_persistent_stub_cache(analysis_host.raw_database()) {
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
            source_root_matchers: Vec::new(),
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
        }
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
}

impl GlobalStateSnapshot {
    fn vfs_read(&self) -> MappedRwLockReadGuard<'_, vfs::Vfs> {
        RwLockReadGuard::map(self.vfs.read(), |(it, _)| it)
    }

    /// Returns `None` if the file was excluded.
    pub(crate) fn url_to_file_id(&self, url: &Uri) -> anyhow::Result<Option<FileId>> {
        url_to_file_id(&self.vfs_read(), url)
    }

    /// The URI of a file known to the vfs.
    pub(crate) fn file_id_to_url(&self, file_id: FileId) -> anyhow::Result<Uri> {
        let vfs = self.vfs_read();
        let path = vfs.file_path(file_id);
        let path = path
            .as_path()
            .ok_or_else(|| anyhow::format_err!("file has no absolute path: {file_id:?}"))?;
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
    /// that are not open.
    pub(crate) fn open_document_version(&self, path: &vfs::VfsPath) -> Option<i32> {
        self.mem_docs.get(path).map(|doc| doc.version)
    }
}

/// Returns `None` if the file was excluded.
pub(crate) fn url_to_file_id(vfs: &vfs::Vfs, url: &Uri) -> anyhow::Result<Option<FileId>> {
    let path = from_proto::vfs_path(url)?;
    vfs_path_to_file_id(vfs, &path)
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
