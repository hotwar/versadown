//! Durable acceptance is independent of any CLI/MCP connection lifetime.
//! One host-owned worker owns pending dispatch and shares its control gate with
//! the GUI's ordinary action router. Engine errors never erase the intent.
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use crate::{DownloadAction, DownloadTask, EngineRuntime, StoreError, TaskStore};

type Authorize = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

pub(crate) struct CliDispatcher {
    store: Arc<TaskStore>,
    runtime: Arc<EngineRuntime>,
    authorize: Authorize,
    gate: Mutex<()>,
    wake: Arc<Notify>,
}

impl CliDispatcher {
    pub(crate) fn start(store: Arc<TaskStore>, runtime: Arc<EngineRuntime>, authorize: Authorize) -> Arc<Self> {
        let dispatcher = Arc::new(Self { store, runtime, authorize, gate: Mutex::new(()), wake: Arc::new(Notify::new()) });
        let weak = Arc::downgrade(&dispatcher);
        let wake = dispatcher.wake.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let Some(dispatcher) = weak.upgrade() else { break; };
                dispatcher.dispatch_pending().await;
                drop(dispatcher);
                tokio::select! {
                    _ = wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                }
            }
        });
        dispatcher
    }

    pub(crate) fn notify(&self) { self.wake.notify_one(); }

    async fn dispatch_pending(&self) {
        let Ok(service) = self.runtime.service().await else { return; };
        let Ok(intents) = self.store.pending_cli_intents() else { return; };
        for intent in intents {
            let _gate = self.gate.lock().await;
            // Errors may mean that an RPC was accepted. Leave the intent and
            // its fixed GID for reconciliation, and do not log raw RPC errors.
            if let Err(error) = service.dispatch_cli_intent(&intent, self.authorize.as_ref()).await {
                let code = match error {
                    crate::DownloadServiceError::InvalidNativeCheckpoint => Some("CLI_RECOVERY_REQUIRED"),
                    crate::DownloadServiceError::InvalidUriOption(_) | crate::DownloadServiceError::Input(_) => Some("INVALID_ARGUMENT"),
                    _ => None,
                };
                if let Some(code) = code {
                    // A deterministic validation/recovery failure is not a
                    // transient RPC outage. Make the required action visible.
                    let _ = service.fail_cli_dispatch(intent.task_id, code).await;
                }
            }
        }
    }

    /// Invoke before the generic GUI action router can resubmit a paused task.
    /// This also works while the engine is unavailable or starting.
    pub(crate) async fn control_pending(&self, id: Uuid, action: DownloadAction) -> Result<Option<DownloadTask>, StoreError> {
        let _gate = self.gate.lock().await;
        let task = self.store.control_pending_cli(id, action)?;
        if task.is_some() { self.notify(); }
        Ok(task)
    }
}

#[cfg(test)]
#[path = "cli_dispatch_tests.rs"]
mod tests;
