use crate::config::ConfigErrors;
use project_model::WorkspaceGraph;
use std::{sync::Arc, time::Instant};
use vfs::loader::NotifyHandle;
use vfs::virtual_path::{JarHandler, JimageHandler};

use crossbeam_channel::{Receiver, Sender, unbounded};
use ide::{Analysis, AnalysisHost};
use lsp_server::{ErrorCode, Response};
use parking_lot::RwLock;

use vfs::{AbsPathBuf, Vfs};

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

        if let Some(err) = &resp.error {
            tracing::error!(?resp.id, "Client returned error response: {:?}", err);
            return;
        }

        match outgoing_req {
            OutgoingRequest::SelectBuildSystem { root, systems } => {
                self.handle_select_build_system_response(resp, root, systems);
            }

            OutgoingRequest::Generic(handler) => {
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
