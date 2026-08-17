use crate::{
    config::ConfigErrors,
    line_index::{LineEndings, LineIndex},
    lsp::from_proto,
    mem_docs::MemDocs,
};
use lsp_types::Uri;
use project_model::WorkspaceGraph;
use rustc_hash::FxHashMap;
use std::time::Instant;
use triomphe::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
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
    CreateProgress {
        token: String,
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

pub struct GlobalState {
    sender: Sender<lsp_server::Message>,
    req_queue: ReqQueue,

    pub(crate) task_sender: Sender<BackgroundTaskEvent>,
    pub(crate) task_receiver: Receiver<BackgroundTaskEvent>,
    pub(crate) thread_pool: threadpool::ThreadPool,

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

        let thread_pool = threadpool::ThreadPool::new(num_cpus::get());

        let loader = {
            let (sender, receiver) = unbounded::<vfs::loader::Message>();
            let handle: vfs_notify::NotifyHandle = vfs::loader::Handle::spawn(sender);
            let handle = Box::new(handle) as Box<dyn vfs::loader::Handle>;
            Handle { handle, receiver }
        };

        Self {
            sender,
            req_queue: ReqQueue::default(),

            task_sender,
            task_receiver,
            thread_pool,

            config: Arc::new(config),
            config_errors: None,

            analysis_host: AnalysisHost::new(),
            mem_docs: MemDocs::default(),

            shutdown_requested: false,
            exit_requested: false,

            loader,
            vfs: Arc::new(RwLock::new((vfs::Vfs::default(), Default::default()))),
            vfs_config_version: 0,
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
    mem_docs: MemDocs,
    vfs: Arc<RwLock<(vfs::Vfs, FxHashMap<FileId, LineEndings>)>>,
}

impl GlobalStateSnapshot {
    fn vfs_read(&self) -> MappedRwLockReadGuard<'_, vfs::Vfs> {
        RwLockReadGuard::map(self.vfs.read(), |(it, _)| it)
    }

    /// Returns `None` if the file was excluded.
    pub(crate) fn url_to_file_id(&self, url: &Uri) -> anyhow::Result<Option<FileId>> {
        url_to_file_id(&self.vfs_read(), url)
    }

    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {
        let endings = self.vfs.read().1[&file_id];
        let index = self.analysis.file_line_index(file_id)?;
        let res = LineIndex {
            index,
            endings,
            encoding: self.config.negotiated_encoding(),
        };
        Ok(res)
    }
}

/// Returns `None` if the file was excluded.
pub(crate) fn url_to_file_id(vfs: &vfs::Vfs, url: &Uri) -> anyhow::Result<Option<FileId>> {
    let path = from_proto::vfs_path(url)?;
    vfs_path_to_file_id(vfs, &path)
}

/// Returns `None` if the file was excluded.
pub(crate) fn vfs_path_to_file_id(
    vfs: &vfs::Vfs,
    vfs_path: &VfsPath,
) -> anyhow::Result<Option<FileId>> {
    let (file_id, excluded) = vfs
        .file_id(vfs_path)
        .ok_or_else(|| anyhow::format_err!("file not found: {vfs_path}"))?;
    match excluded {
        vfs::FileExcluded::Yes => Ok(None),
        vfs::FileExcluded::No => Ok(Some(file_id)),
    }
}
