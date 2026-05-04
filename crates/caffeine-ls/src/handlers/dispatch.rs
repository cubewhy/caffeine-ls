use lsp_server::{Notification, Request};
use serde::de::DeserializeOwned;

use crate::{
    GlobalState,
    global_state::{BackgroundTaskEvent, GlobalStateSnapshot},
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
        R: lsp_types::request::Request,
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

    pub(crate) fn on_async<R>(
        &mut self,
        worker: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + Send + 'static,
        R::Result: serde::Serialize + Send + 'static,
    {
        let (id, params) = match self.parse::<R>() {
            Some(it) => it,
            None => return self,
        };

        let snapshot = self.global_state.snapshot();
        let task_sender = self.global_state.task_sender.clone();

        self.global_state.thread_pool.execute(move || {
            let result = worker(snapshot, params);
            let _ = task_sender.send(BackgroundTaskEvent::AsyncRequestCompleted {
                id,
                result: result.map(|it| serde_json::to_value(it).unwrap()),
            });
        });

        self.req = None;
        self
    }

    fn parse<R>(&mut self) -> Option<(lsp_server::RequestId, R::Params)>
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned,
    {
        let req = self.req.as_ref()?;
        if req.method != R::METHOD {
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
        N: lsp_types::notification::Notification,
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
        N: lsp_types::notification::Notification,
        N::Params: DeserializeOwned,
    {
        let notif = self.notif.as_ref()?;
        if notif.method != N::METHOD {
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
