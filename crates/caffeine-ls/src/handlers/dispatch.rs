use crossbeam_channel::Sender;
use ide_db::base_db::salsa::Cancelled;
use lsp_server::{Notification, Request};
use serde::de::DeserializeOwned;

use crate::{
    GlobalState,
    global_state::{BackgroundTaskEvent, GlobalStateSnapshot, PendingRequest},
};

pub(crate) struct RequestDispatcher<'a> {
    pub(crate) req: Option<Request>,
    pub(crate) global_state: &'a mut GlobalState,
}

impl<'a> RequestDispatcher<'a> {
    /// Dispatches the request to a handler function if the method matches.
    pub(crate) fn on<R>(
        &mut self,
        f: fn(&mut GlobalState, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::Request,
        R::Params: DeserializeOwned + serde::Serialize,
        R::Result: serde::Serialize,
    {
        let (id, params) = match self.parse::<R>() {
            Some(it) => it,
            None => return self,
        };

        let result = f(self.global_state, params);
        self.global_state.handle_result::<R>(id, result);

        self.req = None;
        self
    }

    /// Dispatches a request to a handler on the thread pool. Currently unused,
    /// but kept as the building block for future expensive (async) requests.
    pub(crate) fn on_async<R>(
        &mut self,
        worker: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::Request,
        R::Params: DeserializeOwned + Send + Clone + 'static,
        R::Result: serde::Serialize + Send + 'static,
    {
        let (id, params) = match self.parse::<R>() {
            Some(it) => it,
            None => return self,
        };

        let snapshot = self.global_state.snapshot();
        let task_sender = self.global_state.task_sender.clone();

        self.global_state.thread_pool.execute(move || {
            run_and_report::<R>(&task_sender, id, worker, snapshot, params);
        });

        self.req = None;
        self
    }

    fn parse<R>(&mut self) -> Option<(lsp_server::RequestId, R::Params)>
    where
        R: lsp_types::Request,
        R::Params: DeserializeOwned,
    {
        let req = self.req.as_ref()?;
        if req.method != R::METHOD.as_str() {
            return None;
        }
        let req = self.req.take().unwrap();
        match serde_json::from_value(req.params) {
            Ok(params) => Some((req.id, params)),
            Err(err) => {
                self.global_state.respond_err(
                    req.id,
                    lsp_server::ErrorCode::InvalidParams,
                    format!("Invalid params for {}: {}", R::METHOD, err),
                );
                None
            }
        }
    }

    pub(crate) fn finish(&mut self) {
        if let Some(req) = self.req.take() {
            tracing::warn!("unknown request: {}", req.method);
            self.global_state.reply_not_implemented(req.id, req.method);
        }
    }
}

/// Runs an async request worker once and reports its outcome to the main loop.
///
/// Salsa cancels in-flight queries with [`Cancelled::PendingWrite`] whenever a
/// write (e.g. a `didChange`) arrives while the request is running. The worker
/// must then drop its snapshot immediately — salsa's writer blocks until every
/// clone of the database is released — and hand the request back to the main
/// loop, which re-runs it on a fresh snapshot once the write has been applied.
fn run_and_report<R>(
    task_sender: &Sender<BackgroundTaskEvent>,
    id: lsp_server::RequestId,
    worker: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    snapshot: GlobalStateSnapshot,
    params: R::Params,
) where
    R: lsp_types::Request,
    R::Params: Send + Clone + 'static,
    R::Result: serde::Serialize + Send + 'static,
{
    let run = retry_closure::<R>(task_sender.clone(), id, worker, params);
    run(snapshot);
}

/// Builds a closure that runs `worker` once, owning `params`. On success it
/// reports the serialized result; on a pending-write cancellation it reports
/// an [`BackgroundTaskEvent::AsyncRequestRetry`] carrying a fresh closure that
/// owns `params` again, so the request can be re-run after the write lands.
fn retry_closure<R>(
    task_sender: Sender<BackgroundTaskEvent>,
    id: lsp_server::RequestId,
    worker: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    params: R::Params,
) -> PendingRequest
where
    R: lsp_types::Request,
    R::Params: Send + Clone + 'static,
    R::Result: serde::Serialize + Send + 'static,
{
    Box::new(move |snapshot| {
        let retry_params = params.clone();
        let retry_id = id.clone();
        let result = worker(snapshot, params);
        match result {
            Ok(value) => {
                let _ = task_sender.send(BackgroundTaskEvent::AsyncRequestCompleted {
                    id,
                    result: Ok(serde_json::to_value(value).unwrap()),
                });
            }
            Err(err)
                if matches!(
                    err.downcast_ref::<Cancelled>(),
                    Some(Cancelled::PendingWrite)
                ) =>
            {
                let run = retry_closure::<R>(task_sender.clone(), retry_id, worker, retry_params);
                let _ = task_sender.send(BackgroundTaskEvent::AsyncRequestRetry { id, run });
            }
            Err(err) => {
                let _ = task_sender.send(BackgroundTaskEvent::AsyncRequestCompleted {
                    id,
                    result: Err(err),
                });
            }
        }
    })
}

pub(crate) struct NotificationDispatcher<'a> {
    pub(crate) notif: Option<Notification>,
    pub(crate) global_state: &'a mut GlobalState,
}

impl<'a> NotificationDispatcher<'a> {
    pub(crate) fn on<N>(
        &mut self,
        f: fn(&mut GlobalState, N::Params) -> anyhow::Result<()>,
    ) -> &mut Self
    where
        N: lsp_types::Notification,
        N::Params: DeserializeOwned,
    {
        let params = match self.parse::<N>() {
            Some(it) => it,
            None => return self,
        };
        if let Err(e) = f(self.global_state, params) {
            tracing::error!("failed to handle notification {}: {}", N::METHOD, e);
        }
        self.notif = None;
        self
    }

    fn parse<N>(&mut self) -> Option<N::Params>
    where
        N: lsp_types::Notification,
        N::Params: DeserializeOwned,
    {
        let notif = self.notif.as_ref()?;
        if notif.method != N::METHOD.as_str() {
            return None;
        }
        let notif = self.notif.take().unwrap();
        serde_json::from_value(notif.params).ok()
    }

    pub(crate) fn finish(&mut self) {
        if let Some(notif) = self.notif.take() {
            tracing::warn!("unknown notification: {}", notif.method);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crossbeam_channel::unbounded;
    use lsp_server::RequestId;
    use lsp_types::{ClientCapabilities, LspRequestMethod, MessageDirection, Request};

    use crate::{GlobalState, config::Config};

    use super::*;

    struct FakeRequest;

    impl Request for FakeRequest {
        type Params = ();
        type Result = ();
        const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("fake/request");
        const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
    }

    fn snapshot() -> GlobalStateSnapshot {
        let (msg_tx, _msg_rx) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let config = Config::new(ClientCapabilities::default(), Vec::new(), None, None);
        GlobalState::new(msg_tx, config).snapshot()
    }

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn flaky_worker(_snapshot: GlobalStateSnapshot, _params: ()) -> anyhow::Result<()> {
        // First invocation is cancelled by a pending salsa write, later ones
        // succeed (the query re-runs once the write has been applied).
        if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(anyhow::Error::new(Cancelled::PendingWrite));
        }
        Ok(())
    }

    #[test]
    fn cancelled_request_is_requeued_and_retried() {
        CALLS.store(0, Ordering::SeqCst);
        let (tx, rx) = unbounded();
        let id = RequestId::from(1);

        let run = retry_closure::<FakeRequest>(tx.clone(), id.clone(), flaky_worker, ());
        run(snapshot());

        // The cancelled attempt must hand the request back to the main loop
        // (dropping its snapshot) rather than report an internal error.
        let event = rx.recv().unwrap();
        let BackgroundTaskEvent::AsyncRequestRetry {
            id: retry_id,
            run: retry,
        } = event
        else {
            panic!("expected AsyncRequestRetry");
        };
        assert_eq!(retry_id, id);

        // The main loop re-runs the request on a fresh snapshot after the
        // write is applied; the second attempt succeeds.
        retry(snapshot());

        let event = rx.recv().unwrap();
        let BackgroundTaskEvent::AsyncRequestCompleted { id, result } = event else {
            panic!("expected AsyncRequestCompleted");
        };
        assert_eq!(id, id);
        assert!(result.is_ok());

        assert!(rx.is_empty());
    }
}
