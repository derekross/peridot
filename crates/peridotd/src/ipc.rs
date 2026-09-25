//! Peridot on the control socket (served by opal-kit).

use std::sync::Arc;

use futures::future::BoxFuture;
use opal_core::ipc::IpcEvent;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::api;
use crate::app::App;

impl opal_kit::ipc::Service for App {
    fn dispatch(
        self: Arc<Self>,
        method: String,
        params: Value,
    ) -> BoxFuture<'static, anyhow::Result<Value>> {
        Box::pin(async move { api::dispatch(&self, &method, params).await })
    }

    fn snapshot(self: Arc<Self>) -> BoxFuture<'static, Value> {
        // The app's own snapshot, not this trait method (same name).
        Box::pin(async move { App::snapshot(&self).await })
    }

    fn events(&self) -> broadcast::Receiver<IpcEvent> {
        self.events.subscribe()
    }
}
