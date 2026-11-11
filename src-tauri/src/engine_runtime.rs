use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex, RwLock,
    },
    time::Duration,
};

use serde::Serialize;
use thiserror::Error;
use tokio::sync::{watch, Mutex};

use crate::{
    aria2::{
        Aria2GlobalStat, Aria2LaunchConfig, Aria2Options, Aria2Process, Aria2RpcClient,
        DownloadAction, DownloadService, EventSink, TransferSynchronizer, UriSubmissionOptions,
    },
    bandwidth::BandwidthLimiter,
    DownloadTask, FtpDownloadEngine, TaskStatus, TaskStore,
};

#[cfg(test)]
const TEST_CLEANUP_OWNER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[cfg(test)]
fn cleanup_completed_share(
    plans: &crate::cloud::share::CloudSharePlanStore,
    tasks: &TaskStore,
    task: &crate::DownloadTask,
    enabled: bool,
    retry: bool,
    trash: impl FnOnce(crate::cloud::CloudProvider, &[String]) -> Result<(), crate::cloud::QuarkError>,
) -> Result<Option<crate::DownloadTask>, crate::cloud::QuarkError> {
    cleanup_completed_share_with_owner(
        plans,
        tasks,
        task,
        enabled,
        retry,
        Some(TEST_CLEANUP_OWNER),
        trash,
    )
}

/// Production callers pass the current account-v2 owner fingerprint. Missing
/// stable identity leaves the transfer pending and never invokes deletion.
pub(crate) fn cleanup_completed_share_for_owner(
    plans: &crate::cloud::share::CloudSharePlanStore,
    tasks: &TaskStore,
    task: &crate::DownloadTask,
    enabled: bool,
    retry: bool,
    owner_fingerprint: Option<&str>,
    trash: impl FnOnce(crate::cloud::CloudProvider, &[String]) -> Result<(), crate::cloud::QuarkError>,
) -> Result<Option<crate::DownloadTask>, crate::cloud::QuarkError> {
    cleanup_completed_share_with_owner(plans, tasks, task, enabled, retry, owner_fingerprint, trash)
}

fn cleanup_completed_share_with_owner(
    plans: &crate::cloud::share::CloudSharePlanStore,
    tasks: &TaskStore,
    task: &crate::DownloadTask,
    enabled: bool,
    retry: bool,
    owner_fingerprint: Option<&str>,
    trash: impl FnOnce(crate::cloud::CloudProvider, &[String]) -> Result<(), crate::cloud::QuarkError>,
) -> Result<Option<crate::DownloadTask>, crate::cloud::QuarkError> {
    use crate::cloud::{share::parse_stable_cloud_share_source, QuarkError};
    if !enabled
        || task.status != TaskStatus::Completed
        || !task.source.starts_with("cloud-share://")
    {
        return Ok(None);
    }
    let (provider, _, _) = parse_stable_cloud_share_source(&task.source)?;
    let current = tasks.find(task.id).map_err(|_| QuarkError::Storage)?;
    if current.status != TaskStatus::Completed || current.source != task.source {
        return Ok(None);
    }
    if owner_fingerprint.is_none() {
        return tasks
            .set_completed_cleanup_message(task.id, true)
            .map(Some)
            .map_err(|_| QuarkError::Storage);
    }
    let attempt = (|| {
        let ids = plans.mark_local_completion_for_owner(
            &task.source,
            retry,
            owner_fingerprint,
            |id, source| {
                tasks.find(id).is_ok_and(|local| {
                    local.status == TaskStatus::Completed && local.source == source
                })
            },
        )?;
        if ids.is_empty() {
            // aria2 may publish another completion without the cleanup message.
            // Re-project the journal outcome without replaying a provider call.
            use crate::cloud::share::CloudShareCleanupState;
            let plan = plans.resolve_entry(&task.source)?.plan;
            let pending = plan.transfers.iter().any(|transfer| {
                plan.linked_local_tasks
                    .contains_key(&transfer.entry_fingerprint)
                    && matches!(
                        transfer.cleanup_state,
                        CloudShareCleanupState::Failed | CloudShareCleanupState::Attempted
                    )
            });
            if pending
                && !current
                    .error
                    .as_deref()
                    .is_some_and(|message| message.starts_with("cleanup_pending："))
            {
                return Ok(Some(false));
            }
            if !pending
                && current
                    .error
                    .as_deref()
                    .is_some_and(|message| message.starts_with("cleanup_pending："))
                && plan
                    .transfers
                    .iter()
                    .any(|transfer| transfer.cleanup_state == CloudShareCleanupState::Succeeded)
            {
                return Ok(Some(true));
            }
            return Ok(None);
        }
        let succeeded = trash(provider, &ids).is_ok();
        plans.finish_cleanup(&task.source, &ids, succeeded)?;
        Ok::<_, QuarkError>(Some(succeeded))
    })();
    let pending = match attempt {
        Ok(None) => return Ok(None),
        Ok(Some(succeeded)) => !succeeded,
        Err(_) => true,
    };
    tasks
        .set_completed_cleanup_message(task.id, pending)
        .map(Some)
        .map_err(|_| QuarkError::Storage)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineAvailability {
    pub available: bool,
    pub retrying: bool,
    pub error: Option<String>,
}

impl EngineAvailability {
    fn starting() -> Self {
        Self {
            available: false,
            retrying: true,
            error: None,
        }
    }

    fn available() -> Self {
        Self {
            available: true,
            retrying: false,
            error: None,
        }
    }

    fn unavailable(error: impl Into<String>) -> Self {
        Self {
            available: false,
            retrying: false,
            error: Some(format!("下载引擎不可用：{}", error.into())),
        }
    }
}

pub trait EngineStateSink: Send + Sync {
    fn engine_state_updated(&self, state: &EngineAvailability);
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct EngineUnavailableError {
    message: String,
}

struct EngineSlot {
    service: Option<Arc<DownloadService>>,
    client: Option<Aria2RpcClient>,
    sync_cancel: Option<watch::Sender<bool>>,
}

/// Keep peer connections alive while cloud uploads take precedence.
/// aria2-next applies this value globally to BT, magnet, and ED2K uploads.
const CLOUD_UPLOAD_P2P_LIMIT: u64 = 1_024;

#[derive(Debug)]
struct CloudUploadP2pState {
    active_uploads: u64,
    configured_limit: u64,
}

impl CloudUploadP2pState {
    fn new(configured_limit: u64) -> Self {
        Self {
            active_uploads: 0,
            configured_limit,
        }
    }

    fn acquire(&mut self) -> Option<u64> {
        self.active_uploads += 1;
        (self.active_uploads == 1).then_some(CLOUD_UPLOAD_P2P_LIMIT)
    }

    fn release(&mut self) -> Option<u64> {
        self.active_uploads = self.active_uploads.saturating_sub(1);
        (self.active_uploads == 0).then_some(self.configured_limit)
    }

    fn set_configured_limit(&mut self, configured_limit: u64) {
        self.configured_limit = configured_limit;
    }

    fn has_active_uploads(&self) -> bool {
        self.active_uploads > 0
    }
}

pub(crate) struct CloudUploadLease {
    runtime: Arc<EngineRuntime>,
    released: AtomicBool,
}

impl CloudUploadLease {
    pub(crate) fn limiter(&self) -> Arc<BandwidthLimiter> {
        Arc::clone(&self.runtime.cloud_upload_limiter)
    }

    pub(crate) async fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.runtime.release_cloud_upload().await;
        }
    }
}

impl Drop for CloudUploadLease {
    fn drop(&mut self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            let runtime = Arc::clone(&self.runtime);
            tauri::async_runtime::spawn(async move {
                runtime.release_cloud_upload().await;
            });
        }
    }
}

pub struct EngineRuntime {
    launch_config: RwLock<Aria2LaunchConfig>,
    store: Arc<TaskStore>,
    download_events: Arc<dyn EventSink>,
    ftp: Arc<FtpDownloadEngine>,
    cloud_upload_limiter: Arc<BandwidthLimiter>,
    cloud_upload_p2p: Mutex<CloudUploadP2pState>,
    state_events: Arc<dyn EngineStateSink>,
    state: RwLock<EngineAvailability>,
    slot: Mutex<EngineSlot>,
    // Kept separate from `slot` so an explicit application exit can always
    // terminate the owned aria2 child, even while a recovery holds `slot`.
    active_service: StdMutex<Option<Arc<DownloadService>>>,
    exiting: AtomicBool,
    monitor_started: AtomicBool,
    automatic_retry_used: AtomicBool,
}

impl EngineRuntime {
    pub fn new(
        launch_config: Aria2LaunchConfig,
        store: Arc<TaskStore>,
        download_events: Arc<dyn EventSink>,
        state_events: Arc<dyn EngineStateSink>,
    ) -> Self {
        let configured_p2p_upload_limit = launch_config.aria2.p2p_upload_limit;
        let ftp = Arc::new(FtpDownloadEngine::new(
            &launch_config.download_dir,
            Arc::clone(&store),
            Arc::clone(&download_events),
        ));
        ftp.set_limits(
            launch_config.aria2.max_concurrent_downloads,
            launch_config.aria2.max_connections_per_server,
        );
        ftp.set_download_limit(launch_config.aria2.overall_download_limit);
        Self {
            launch_config: RwLock::new(launch_config),
            store,
            download_events,
            ftp,
            cloud_upload_limiter: Arc::new(BandwidthLimiter::new(0)),
            cloud_upload_p2p: Mutex::new(CloudUploadP2pState::new(configured_p2p_upload_limit)),
            state_events,
            state: RwLock::new(EngineAvailability::starting()),
            slot: Mutex::new(EngineSlot {
                service: None,
                client: None,
                sync_cancel: None,
            }),
            active_service: StdMutex::new(None),
            exiting: AtomicBool::new(false),
            monitor_started: AtomicBool::new(false),
            automatic_retry_used: AtomicBool::new(false),
        }
    }

    pub fn status(&self) -> EngineAvailability {
        self.state
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn register_active_service(&self, service: Arc<DownloadService>) {
        *self
            .active_service
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(service);
    }

    fn active_service(&self) -> Option<Arc<DownloadService>> {
        self.active_service
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn clear_active_service(&self, service: &Arc<DownloadService>) {
        let mut active = self
            .active_service
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, service))
        {
            *active = None;
        }
    }

    pub async fn initialize(&self) -> EngineAvailability {
        self.retry().await
    }

    pub async fn retry(&self) -> EngineAvailability {
        if self.exiting.load(Ordering::Acquire) {
            return self.set_state(EngineAvailability::unavailable("应用正在退出"));
        }
        self.set_state(EngineAvailability::starting());
        let _ = self.ftp.recover().await;
        let mut slot = self.slot.lock().await;
        if self.exiting.load(Ordering::Acquire) {
            self.stop_slot_for_exit(&mut slot).await;
            return self.set_state(EngineAvailability::unavailable("应用正在退出"));
        }
        self.retry_locked(&mut slot).await
    }

    async fn retry_locked(&self, slot: &mut EngineSlot) -> EngineAvailability {
        if let Some(service) = slot.service.clone() {
            if service.engine_running().await.ok().flatten() == Some(true) {
                let mut resume_after_recovery = self.recovery_resume_candidates();
                let download_root = self
                    .launch_config
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .download_dir
                    .clone();
                let recovered = service.recover_native(&download_root).await;
                if self.exiting.load(Ordering::Acquire) {
                    self.stop_slot_for_exit(slot).await;
                    return self.set_state(EngineAvailability::unavailable("应用正在退出"));
                }
                return match recovered {
                    Ok(recovered) => {
                        resume_after_recovery.extend(recovered.resumable_task_ids);
                        self.resume_recovered_non_cloud_tasks(&service, &resume_after_recovery)
                            .await;
                        let client = slot.client.clone();
                        Self::start_synchronizer_if_needed(slot, service, client);
                        self.set_state(EngineAvailability::available())
                    }
                    Err(error) => {
                        self.mark_tasks_unavailable(&error.to_string());
                        self.set_state(EngineAvailability::unavailable(error.to_string()))
                    }
                };
            }
        }

        if let Some(cancel) = slot.sync_cancel.take() {
            let _ = cancel.send(true);
        }
        if let Some(previous) = slot.service.take() {
            self.clear_active_service(&previous);
        }
        slot.client.take();
        let launch_config = self
            .launch_config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let download_root = launch_config.download_dir.clone();
        let mut resume_after_recovery = self.recovery_resume_candidates();
        let external_rpc_slot_base = launch_config.aria2.max_concurrent_downloads;
        let (process, client) = match Aria2Process::start(launch_config).await {
            Ok(engine) => engine,
            Err(error) => {
                self.mark_tasks_unavailable(&error.to_string());
                return self.set_state(EngineAvailability::unavailable(error.to_string()));
            }
        };
        if self.exiting.load(Ordering::Acquire) {
            drop(process);
            return self.set_state(EngineAvailability::unavailable("应用正在退出"));
        }
        let service = Arc::new(
            DownloadService::with_process(
                Arc::new(client.clone()),
                Arc::clone(&self.store),
                process,
            )
            .with_events(Arc::clone(&self.download_events))
            .with_external_rpc_slots(external_rpc_slot_base),
        );
        slot.service = Some(Arc::clone(&service));
        self.register_active_service(Arc::clone(&service));
        slot.client = Some(client.clone());
        let recovered = service.recover_native(&download_root).await;
        if self.exiting.load(Ordering::Acquire) {
            self.stop_slot_for_exit(slot).await;
            return self.set_state(EngineAvailability::unavailable("应用正在退出"));
        }
        match recovered {
            Ok(recovered) => {
                resume_after_recovery.extend(recovered.resumable_task_ids);
                self.resume_recovered_non_cloud_tasks(&service, &resume_after_recovery)
                    .await;
                Self::start_synchronizer_if_needed(slot, service, Some(client));
                self.set_state(EngineAvailability::available())
            }
            Err(error) => {
                self.mark_tasks_unavailable(&error.to_string());
                self.set_state(EngineAvailability::unavailable(error.to_string()))
            }
        }
    }

    /// `--pause=true` makes aria2 load every saved GID without immediately
    /// touching a saved provider URL. Restore the normal automatic behavior
    /// for non-cloud work after reconciliation; direct cloud tasks are resumed
    /// only by the authorization-completion path.
    fn recovery_resume_candidates(&self) -> Vec<uuid::Uuid> {
        self.store
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|task| {
                task.aria_gid.is_some()
                    && !crate::cloud::download_dispatch::is_cloud_download_source(&task.source)
                    && !crate::xunlei_native::is_native_gid(task.aria_gid.as_deref())
                    && matches!(task.status, TaskStatus::Queued | TaskStatus::Downloading)
            })
            .map(|task| task.id)
            .collect()
    }

    async fn resume_recovered_non_cloud_tasks(
        &self,
        service: &Arc<DownloadService>,
        candidates: &[uuid::Uuid],
    ) {
        for &id in candidates {
            let should_resume = self.store.find(id).is_ok_and(|task| {
                task.status == TaskStatus::Paused
                    && task.aria_gid.is_some()
                    && !crate::cloud::download_dispatch::is_cloud_download_source(&task.source)
            });
            if should_resume {
                // A failed unpause leaves the durable task paused for the
                // normal engine retry/control path; it must not make startup
                // itself unavailable.
                let _ = service.control(id, DownloadAction::Resume).await;
            }
        }
    }

    pub async fn service(&self) -> Result<Arc<DownloadService>, EngineUnavailableError> {
        let state = self.status();
        if !state.available {
            return Err(EngineUnavailableError {
                message: state
                    .error
                    .unwrap_or_else(|| "下载引擎不可用，请稍后重试".into()),
            });
        }
        self.slot
            .lock()
            .await
            .service
            .clone()
            .ok_or_else(|| EngineUnavailableError {
                message: "下载引擎不可用，请稍后重试".into(),
            })
    }

    pub(crate) fn set_cloud_upload_limit(&self, bytes_per_second: u64) {
        self.cloud_upload_limiter.set_limit(bytes_per_second);
    }

    pub(crate) async fn begin_cloud_upload(self: &Arc<Self>) -> CloudUploadLease {
        let mut state = self.cloud_upload_p2p.lock().await;
        if state.acquire().is_some() {
            // Cloud uploads must remain usable while the local download engine
            // is starting or recovering. In that state it cannot keep a P2P
            // connection alive, so best-effort suppression is sufficient.
            let _ = self
                .set_aria2_global_upload_limit(CLOUD_UPLOAD_P2P_LIMIT)
                .await;
        }
        CloudUploadLease {
            runtime: Arc::clone(self),
            released: AtomicBool::new(false),
        }
    }

    async fn release_cloud_upload(&self) {
        let mut state = self.cloud_upload_p2p.lock().await;
        if let Some(limit) = state.release() {
            let _ = self.set_aria2_global_upload_limit(limit).await;
        }
    }

    async fn set_aria2_global_upload_limit(&self, bytes_per_second: u64) -> Result<(), String> {
        let service = self.service().await.map_err(|error| error.to_string())?;
        service
            .api()
            .change_global_option(Aria2Options::from([(
                "max-overall-upload-limit".into(),
                bytes_per_second.to_string(),
            )]))
            .await
            .map_err(|error| format!("无法设置 P2P 上传限速：{error}"))?;
        Ok(())
    }

    pub async fn global_stat(&self) -> Result<Aria2GlobalStat, EngineUnavailableError> {
        let ftp = self.ftp.global_stat().ok();
        let mut stat =
            match self.service().await {
                Ok(service) => service.api().get_global_stat().await.map_err(|error| {
                    EngineUnavailableError {
                        message: format!("下载引擎不可用：{error}"),
                    }
                })?,
                Err(_)
                    if ftp
                        .as_ref()
                        .is_some_and(|stat| stat.num_active > 0 || stat.num_waiting > 0) =>
                {
                    Aria2GlobalStat {
                        download_speed: 0,
                        upload_speed: 0,
                        num_active: 0,
                        num_waiting: 0,
                        num_stopped: 0,
                        num_stopped_total: 0,
                    }
                }
                Err(error) => return Err(error),
            };
        if let Some(ftp) = ftp {
            stat.download_speed = stat.download_speed.saturating_add(ftp.download_speed);
            stat.upload_speed = stat.upload_speed.saturating_add(ftp.upload_speed);
            stat.num_active = stat.num_active.saturating_add(ftp.num_active);
            stat.num_waiting = stat.num_waiting.saturating_add(ftp.num_waiting);
            stat.num_stopped = stat.num_stopped.saturating_add(ftp.num_stopped);
            stat.num_stopped_total = stat.num_stopped_total.saturating_add(ftp.num_stopped_total);
        }
        Ok(stat)
    }

    pub async fn submit_uri_with_options(
        self: &Arc<Self>,
        task_id: uuid::Uuid,
        uris: Vec<String>,
        options: UriSubmissionOptions,
    ) -> Result<DownloadTask, String> {
        if uris.len() == 1 && uris[0].to_ascii_lowercase().starts_with("ftp://") {
            return self
                .ftp
                .submit_uri(task_id, &uris[0], options)
                .await
                .map_err(|error| error.to_string());
        }
        let service = self.service().await.map_err(|error| error.to_string())?;
        let task = self
            .store
            .find(task_id)
            .map_err(|error| error.to_string())?;
        if uris.len() == 1 && matches!(task.kind.as_str(), "hls" | "dash") {
            return service
                .submit_native_media_with_options(task_id, uris, options, &task.kind)
                .await
                .map_err(|error| error.to_string());
        }
        service
            .submit_uri_with_options(task_id, uris, options)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn resubmit_uri_with_options(
        self: &Arc<Self>,
        task_id: uuid::Uuid,
        uris: Vec<String>,
        options: UriSubmissionOptions,
    ) -> Result<DownloadTask, String> {
        if uris.len() == 1 && uris[0].to_ascii_lowercase().starts_with("ftp://") {
            return self
                .ftp
                .submit_uri(task_id, &uris[0], options)
                .await
                .map_err(|error| error.to_string());
        }
        let service = self.service().await.map_err(|error| error.to_string())?;
        let task = self
            .store
            .find(task_id)
            .map_err(|error| error.to_string())?;
        if uris.len() == 1 && matches!(task.kind.as_str(), "hls" | "dash") {
            return service
                .resubmit_native_media_with_options(task_id, uris, options, &task.kind)
                .await
                .map_err(|error| error.to_string());
        }
        service
            .resubmit_uri_with_options(task_id, uris, options)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn control(
        self: &Arc<Self>,
        task_id: uuid::Uuid,
        action: DownloadAction,
    ) -> Result<DownloadTask, String> {
        let task = self
            .store
            .find(task_id)
            .map_err(|error| error.to_string())?;
        if task.kind == "ftp" {
            return self
                .ftp
                .control(task_id, action)
                .await
                .map_err(|error| error.to_string());
        }
        let service = self.service().await.map_err(|error| error.to_string())?;
        service
            .control(task_id, action)
            .await
            .map_err(|error| error.to_string())
    }

    /// Applies the complete aria2 configuration immediately. The RPC endpoint and
    /// authentication token are startup-only options, so the owned local process
    /// is restarted after its session has been saved. aria2 restores active work
    /// from that session before this call resolves successfully.
    pub async fn reconfigure(
        &self,
        launch_config: Aria2LaunchConfig,
    ) -> Result<EngineAvailability, String> {
        if self.exiting.load(Ordering::Acquire) {
            return Err("应用正在退出，无法更新 aria2 设置".into());
        }
        let current_config = self
            .launch_config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if launch_config.aria2.rpc_port != current_config.aria2.rpc_port {
            std::net::TcpListener::bind(("127.0.0.1", launch_config.aria2.rpc_port))
                .map_err(|_| format!("aria2 RPC 端口 {} 已被占用", launch_config.aria2.rpc_port))?;
        }

        self.set_state(EngineAvailability::starting());
        let mut slot = self.slot.lock().await;
        if self.exiting.load(Ordering::Acquire) {
            self.stop_slot_for_exit(&mut slot).await;
            return Err("应用正在退出，无法更新 aria2 设置".into());
        }
        if let Some(cancel) = slot.sync_cancel.take() {
            let _ = cancel.send(true);
        }
        slot.client.take();
        if let Some(service) = slot.service.take() {
            let report = service.shutdown_report(Duration::from_secs(5)).await;
            self.clear_active_service(&service);
            if !report.child_stopped {
                return Err("aria2 未能在保存会话后停止，设置未应用".into());
            }
        }
        *self
            .launch_config
            .write()
            .unwrap_or_else(|error| error.into_inner()) = launch_config;
        self.ftp.set_default_directory(
            &self
                .launch_config
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .download_dir,
        );
        let state = self.retry_locked(&mut slot).await;
        if state.available {
            let (max_active_tasks, max_connections_per_task) = {
                let launch_config = self
                    .launch_config
                    .read()
                    .unwrap_or_else(|error| error.into_inner());
                (
                    launch_config.aria2.max_concurrent_downloads,
                    launch_config.aria2.max_connections_per_server,
                )
            };
            self.ftp
                .set_limits(max_active_tasks, max_connections_per_task);
            let overall_download_limit = self
                .launch_config
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .aria2
                .overall_download_limit;
            self.ftp.set_download_limit(overall_download_limit);
            Ok(state)
        } else {
            let error = state.error.unwrap_or_else(|| "aria2 配置应用失败".into());
            *self
                .launch_config
                .write()
                .unwrap_or_else(|error| error.into_inner()) = current_config;
            self.ftp.set_default_directory(
                &self
                    .launch_config
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .download_dir,
            );
            let restored = self.retry_locked(&mut slot).await;
            if restored.available {
                Err(error)
            } else {
                Err(format!("{error}；且无法恢复原 aria2 配置"))
            }
        }
    }

    pub async fn reconfigure_aria2_settings(
        &self,
        aria2: crate::Aria2Settings,
    ) -> Result<EngineAvailability, String> {
        let mut upload_state = self.cloud_upload_p2p.lock().await;
        let previous_limit = upload_state.configured_limit;
        upload_state.set_configured_limit(aria2.p2p_upload_limit);
        let mut launch_config = self
            .launch_config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        launch_config.aria2 = aria2;
        let availability = self.reconfigure(launch_config).await;
        if availability.is_err() {
            upload_state.set_configured_limit(previous_limit);
            return availability;
        }
        if upload_state.has_active_uploads() {
            self.set_aria2_global_upload_limit(CLOUD_UPLOAD_P2P_LIMIT)
                .await?;
        }
        availability
    }

    pub fn start_monitor(self: &Arc<Self>, interval: Duration) {
        if self.monitor_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let runtime = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if runtime.exiting.load(Ordering::Acquire) {
                    return;
                }
                let mut slot = runtime.slot.lock().await;
                if runtime.exiting.load(Ordering::Acquire) {
                    return;
                }
                // A public retry publishes this state before waiting for the slot.
                // Recheck it under the same mutex used to start a process so the
                // monitor cannot race ahead and spend its one automatic attempt.
                if runtime.status().retrying {
                    continue;
                }
                let service = slot.service.clone();
                let child_exited = match service {
                    Some(service) => service.engine_running().await.ok().flatten() == Some(false),
                    None => true,
                };
                let recovery_pending = !runtime.status().available;
                if (child_exited || recovery_pending)
                    && runtime
                        .automatic_retry_used
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    runtime.set_state(EngineAvailability::starting());
                    let _ = runtime.retry_locked(&mut slot).await;
                }
            }
        });
    }

    pub async fn shutdown(&self, timeout: Duration) -> crate::aria2::recovery::ShutdownReport {
        self.exiting.store(true, Ordering::Release);
        self.ftp.shutdown().await;
        // `retry_locked` and recovery deliberately retain `slot` while awaiting
        // RPC.  Exit cannot depend on that lock: its service registry gives the
        // shutdown path direct access to the owned child and guarantees the
        // timeout path calls `terminate_fallback`.
        let report = if let Some(service) = self.active_service() {
            let report = service.shutdown_report(timeout).await;
            self.clear_active_service(&service);
            report
        } else {
            crate::aria2::recovery::ShutdownReport {
                session_saved: false,
                shutdown_requested: false,
                force_killed: false,
                child_stopped: true,
            }
        };
        // Cleanup bookkeeping when the slot is currently idle.  A retry that
        // owns it observes `exiting` and performs the same cleanup itself.
        if let Ok(mut slot) = self.slot.try_lock() {
            if let Some(cancel) = slot.sync_cancel.take() {
                let _ = cancel.send(true);
            }
            slot.client.take();
            slot.service.take();
        }
        self.set_state(EngineAvailability::unavailable("应用正在退出"));
        report
    }

    async fn stop_slot_for_exit(&self, slot: &mut EngineSlot) {
        if let Some(cancel) = slot.sync_cancel.take() {
            let _ = cancel.send(true);
        }
        slot.client.take();
        if let Some(service) = slot.service.take() {
            let _ = service.shutdown_report(Duration::ZERO).await;
            self.clear_active_service(&service);
        }
    }

    fn start_synchronizer_if_needed(
        slot: &mut EngineSlot,
        service: Arc<DownloadService>,
        client: Option<Aria2RpcClient>,
    ) {
        if slot.sync_cancel.is_some() {
            return;
        }
        let (cancel, cancel_rx) = watch::channel(false);
        slot.sync_cancel = Some(cancel);
        let synchronizer = match client {
            Some(client) => TransferSynchronizer::new(service).with_websocket(client),
            None => TransferSynchronizer::new(service),
        };
        tauri::async_runtime::spawn(async move {
            synchronizer.run(cancel_rx).await;
        });
    }

    fn mark_tasks_unavailable(&self, cause: &str) {
        let Ok(tasks) = self.store.list() else {
            return;
        };
        for mut task in tasks {
            if matches!(task.kind.as_str(), "ftp" | "web_video") {
                continue;
            }
            if !matches!(
                task.status,
                TaskStatus::Downloading | TaskStatus::Queued | TaskStatus::Parsing
            ) {
                continue;
            }
            let can_resume = task.aria_gid.is_some()
                && !crate::xunlei_native::is_native_gid(task.aria_gid.as_deref());
            task.status = if can_resume {
                TaskStatus::Paused
            } else {
                TaskStatus::Failed
            };
            task.download_speed = 0;
            task.upload_speed = 0;
            task.eta_seconds = None;
            task.error = Some(if can_resume {
                format!("下载引擎不可用：{cause}")
            } else {
                format!("下载引擎不可用，解析已中断，请重新添加任务：{cause}")
            });
            if let Ok(task) = self.store.apply_transfer_snapshot(task) {
                self.download_events.task_updated(&task);
            }
        }
    }

    fn set_state(&self, next: EngineAvailability) -> EngineAvailability {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if *state != next {
            *state = next.clone();
            self.state_events.engine_state_updated(&next);
        }
        next
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn cloud_upload_state_suppresses_once_and_restores_the_latest_user_limit() {
        let mut state = CloudUploadP2pState::new(1_048_576);

        assert_eq!(state.acquire(), Some(CLOUD_UPLOAD_P2P_LIMIT));
        assert_eq!(state.acquire(), None);
        state.set_configured_limit(2_097_152);
        assert!(state.has_active_uploads());
        assert_eq!(state.release(), None);
        assert_eq!(state.release(), Some(2_097_152));
    }

    fn set_status(tasks: &TaskStore, id: uuid::Uuid, status: TaskStatus) -> crate::DownloadTask {
        let mut task = tasks.find(id).unwrap();
        task.status = status;
        tasks.apply_transfer_snapshot(task).unwrap()
    }

    fn cleanup_fixture() -> (
        tempfile::TempDir,
        crate::cloud::share::CloudSharePlanStore,
        TaskStore,
        Vec<crate::DownloadTask>,
    ) {
        use crate::cloud::{share::*, CloudProvider, SystemCredentialStore};
        let root = tempfile::tempdir().unwrap();
        let plans =
            CloudSharePlanStore::new(SystemCredentialStore::new(root.path().into()).unwrap());
        let plan = CloudSharePlan::new(
            CloudProvider::Baidu,
            "share".into(),
            None,
            ["first", "second"]
                .into_iter()
                .map(|id| CloudShareEntry::new("share", id.into(), "file".into(), vec![], Some(1)))
                .collect(),
            serde_json::json!({}),
        );
        plans.upsert(&plan).unwrap();
        let sources = selected_share_sources(
            &plans,
            &plan.source_for("first"),
            &plan
                .entries
                .iter()
                .map(|e| e.entry_fingerprint.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        plans
            .record_transfer_with_owner(&sources[0], "created-id", TEST_CLEANUP_OWNER)
            .unwrap();
        let tasks = TaskStore::open_in_memory().unwrap();
        let locals = sources
            .into_iter()
            .map(|source| {
                let task = tasks
                    .create(crate::NewTask {
                        source,
                        name: "file".into(),
                        kind: "cloud".into(),
                        is_private: false,
                    })
                    .unwrap();
                plans.reserve_local_task(&task.source, task.id).unwrap();
                tasks.transition(task.id, TaskStatus::Queued).unwrap();
                tasks.transition(task.id, TaskStatus::Downloading).unwrap()
            })
            .collect();
        (root, plans, tasks, locals)
    }

    #[test]
    fn last_completed_local_task_trashes_only_recorded_transfer_when_enabled() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        cleanup_completed_share(&plans, &tasks, &first, true, false, |_, _| {
            panic!("sibling incomplete")
        })
        .unwrap();
        let second = set_status(&tasks, locals[1].id, TaskStatus::Completed);
        let mut actions = vec![];
        cleanup_completed_share(&plans, &tasks, &second, true, false, |provider, ids| {
            actions.push((provider, ids.to_vec()));
            // The claim must already survive a reload before the network call.
            assert!(plans
                .mark_cleanup_attempted(&first.source)
                .unwrap()
                .is_empty());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            actions,
            vec![(
                crate::cloud::CloudProvider::Baidu,
                vec!["created-id".to_string()]
            )]
        );
        cleanup_completed_share(&plans, &tasks, &second, true, false, |_, _| {
            panic!("duplicate cleanup")
        })
        .unwrap();
    }

    #[test]
    fn cleanup_with_a_different_credential_never_calls_provider_delete() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let source = locals[0].source.clone();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        set_status(&tasks, locals[1].id, TaskStatus::Completed);
        plans
            .record_transfer_with_owner(&source, "owned-created-id", &"a".repeat(64))
            .unwrap();

        cleanup_completed_share_for_owner(
            &plans,
            &tasks,
            &first,
            true,
            false,
            Some(&"b".repeat(64)),
            |_, _| panic!("a different credential must not delete transferred files"),
        )
        .unwrap();

        let transfers = plans.resolve_entry(&source).unwrap().plan.transfers;
        assert!(transfers.iter().all(|transfer| {
            transfer.cleanup_state == crate::cloud::share::CloudShareCleanupState::Pending
        }));
    }

    #[test]
    fn cleanup_without_a_stable_owner_keeps_the_legacy_transfer_pending() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        set_status(&tasks, locals[1].id, TaskStatus::Completed);

        let updated =
            cleanup_completed_share_for_owner(&plans, &tasks, &first, true, false, None, |_, _| {
                panic!("identity-unavailable cleanup must not call provider delete")
            })
            .unwrap()
            .unwrap();

        assert!(updated
            .error
            .as_deref()
            .is_some_and(|message| message.starts_with("cleanup_pending：")));
        let transfers = plans.resolve_entry(&first.source).unwrap().plan.transfers;
        assert_eq!(
            transfers[0].cleanup_state,
            crate::cloud::share::CloudShareCleanupState::Pending
        );
    }

    #[test]
    fn cleanup_with_a_renewed_session_for_the_same_account_deletes_transfer() {
        use crate::cloud::{share::stable_owner_fingerprint, CloudProvider};

        let (_root, plans, tasks, locals) = cleanup_fixture();
        let source = locals[0].source.clone();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        set_status(&tasks, locals[1].id, TaskStatus::Completed);
        let before = stable_owner_fingerprint(CloudProvider::Baidu, "account-7").unwrap();
        let after = stable_owner_fingerprint(CloudProvider::Baidu, "account-7").unwrap();
        assert_eq!(before, after);
        plans
            .record_transfer_with_owner(&source, "owned-created-id", &before)
            .unwrap();
        let mut deleted = Vec::new();

        cleanup_completed_share_for_owner(
            &plans,
            &tasks,
            &first,
            true,
            false,
            Some(&after),
            |provider, ids| {
                deleted.push((provider, ids.to_vec()));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            deleted,
            vec![(CloudProvider::Baidu, vec!["owned-created-id".into()])]
        );
    }

    #[test]
    fn disabled_cleanup_or_failed_sibling_keeps_transfer() {
        for sibling_status in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let (_root, plans, tasks, locals) = cleanup_fixture();
            set_status(&tasks, locals[0].id, sibling_status);
            let last = set_status(&tasks, locals[1].id, TaskStatus::Completed);
            cleanup_completed_share(
                &plans,
                &tasks,
                &last,
                sibling_status != TaskStatus::Completed,
                false,
                |_, _| panic!("cleanup blocked"),
            )
            .unwrap();
            assert_eq!(
                plans
                    .resolve_entry(&locals[0].source)
                    .unwrap()
                    .plan
                    .transfers[0]
                    .cleanup_state,
                crate::cloud::share::CloudShareCleanupState::Pending
            );
        }
    }

    #[test]
    fn cleanup_failure_keeps_completed_and_manual_retry_only_retries_cleanup() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        set_status(&tasks, locals[1].id, TaskStatus::Completed);
        let result = cleanup_completed_share(&plans, &tasks, &first, true, false, |_, _| {
            Err(crate::cloud::QuarkError::Provider("secret-token".into()))
        })
        .unwrap()
        .unwrap();
        assert_eq!(result.status, TaskStatus::Completed);
        assert!(result.error.as_ref().unwrap().contains("cleanup_pending"));
        assert!(!result.error.as_ref().unwrap().contains("secret-token"));
        cleanup_completed_share(&plans, &tasks, &result, true, false, |_, _| {
            panic!("automatic retry")
        })
        .unwrap();
        let retry =
            cleanup_completed_share(&plans, &tasks, &result, true, true, |provider, ids| {
                assert_eq!(provider, crate::cloud::CloudProvider::Baidu);
                assert_eq!(ids, &["created-id"]);
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(retry.status, TaskStatus::Completed);
        assert!(retry.error.is_none());
    }

    #[test]
    fn cleanup_rechecks_current_status_and_ignores_non_share_sources() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let stale = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        set_status(&tasks, locals[1].id, TaskStatus::Completed);
        let mut restarted = stale.clone();
        restarted.status = TaskStatus::Downloading;
        tasks.apply_transfer_snapshot(restarted).unwrap();
        cleanup_completed_share(&plans, &tasks, &stale, true, true, |_, _| {
            panic!("stale completion")
        })
        .unwrap();
        for source in [
            "cloud://baidu/existing-personal-id",
            "https://example.com/file",
            "cloud-share://baidu/invalid/invalid",
        ] {
            let mut task = stale.clone();
            task.source = source.into();
            let _ = cleanup_completed_share(&plans, &tasks, &task, true, true, |_, _| {
                panic!("untracked source")
            });
        }
    }

    #[test]
    fn cleanup_result_cannot_restore_a_task_changed_during_network_work() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        set_status(&tasks, locals[1].id, TaskStatus::Completed);
        let result = cleanup_completed_share(&plans, &tasks, &first, true, false, |_, _| {
            set_status(&tasks, first.id, TaskStatus::Cancelled);
            Err(crate::cloud::QuarkError::Transport(
                "private-provider-error".into(),
            ))
        })
        .unwrap()
        .unwrap();
        assert_eq!(result.status, TaskStatus::Cancelled);
        assert!(result.error.is_none());
    }

    #[test]
    fn repeated_completion_restores_cleanup_pending_without_repeating_network_action() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        set_status(&tasks, locals[1].id, TaskStatus::Completed);
        cleanup_completed_share(&plans, &tasks, &first, true, false, |_, _| {
            Err(crate::cloud::QuarkError::Storage)
        })
        .unwrap();
        // A repeated aria2 completion snapshot carries no provider cleanup error.
        tasks.apply_transfer_snapshot(first.clone()).unwrap();
        cleanup_completed_share(&plans, &tasks, &first, true, false, |_, _| {
            panic!("automatic replay")
        })
        .unwrap();
        assert!(tasks
            .find(first.id)
            .unwrap()
            .error
            .unwrap()
            .contains("cleanup_pending"));
    }

    #[tokio::test]
    async fn cleanup_waiting_on_resubmission_gate_rechecks_restarted_sibling() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        set_status(&tasks, locals[1].id, TaskStatus::Completed);
        let gates = crate::cloud::commands::CloudDownloadSourceGate::default();
        let retry_guard = gates.acquire(&first.source).await.unwrap();
        let cleanup_gate = gates.acquire(&first.source);
        tokio::pin!(cleanup_gate);
        assert!(futures_util::poll!(&mut cleanup_gate).is_pending());
        set_status(&tasks, first.id, TaskStatus::Downloading);
        drop(retry_guard);
        let _cleanup_guard = cleanup_gate.await.unwrap();
        cleanup_completed_share(&plans, &tasks, &first, true, false, |_, _| {
            panic!("restarted sibling was deleted")
        })
        .unwrap();
    }

    #[tokio::test]
    async fn cleanup_plan_gate_blocks_new_sibling_transfer_until_aria2_rebind() {
        use crate::cloud::{share::*, CloudProvider, SystemCredentialStore};
        let root = tempfile::tempdir().unwrap();
        let plans =
            CloudSharePlanStore::new(SystemCredentialStore::new(root.path().into()).unwrap());
        let plan = CloudSharePlan::new(
            CloudProvider::Baidu,
            "share".into(),
            None,
            ["a", "b"]
                .into_iter()
                .map(|id| CloudShareEntry::new("share", id.into(), "file".into(), vec![], Some(1)))
                .collect(),
            serde_json::json!({}),
        );
        plans.upsert(&plan).unwrap();
        let a = plan.source_for("a");
        let b = plan.source_for("b");
        let tasks = TaskStore::open_in_memory().unwrap();
        let first = tasks
            .create(crate::NewTask {
                source: a.clone(),
                name: "a".into(),
                kind: "cloud".into(),
                is_private: false,
            })
            .unwrap();
        let second = tasks
            .create(crate::NewTask {
                source: b.clone(),
                name: "b".into(),
                kind: "cloud".into(),
                is_private: false,
            })
            .unwrap();
        let first = set_status(&tasks, first.id, TaskStatus::Completed);
        set_status(&tasks, second.id, TaskStatus::Completed);
        plans.link_local_sources(std::slice::from_ref(&a)).unwrap();
        plans.link_local_task(&a, first.id).unwrap();
        plans.record_transfer(&a, "created-a").unwrap();
        let old_plan = plans.resolve_entry(&a).unwrap().plan;
        assert_eq!(old_plan.linked_local_tasks.len(), 1);

        let gates = crate::cloud::commands::CloudDownloadSourceGate::default();
        let overwrite_guard = gates.acquire(&b).await.unwrap();
        let independent = gates
            .acquire(&stable_share_source(
                CloudProvider::Baidu,
                "other-share",
                "b",
            ))
            .await
            .unwrap();
        drop(independent);
        // B is linked and its new transfer journalled while aria2 still reports
        // its previous Completed task. A's earlier plan snapshot omits B.
        plans.link_local_sources(std::slice::from_ref(&b)).unwrap();
        plans.link_local_task(&b, second.id).unwrap();
        plans.record_transfer(&b, "new-b-transfer").unwrap();
        let cleanup_gate = gates.acquire(&a);
        tokio::pin!(cleanup_gate);
        assert!(
            futures_util::poll!(&mut cleanup_gate).is_pending(),
            "cleanup used the old entry set and bypassed the in-flight sibling overwrite"
        );
        set_status(&tasks, second.id, TaskStatus::Downloading);
        drop(overwrite_guard);
        let _cleanup_guard = cleanup_gate.await.unwrap();
        cleanup_completed_share(&plans, &tasks, &first, true, false, |_, _| {
            panic!("deleted the newly transferred sibling before completion")
        })
        .unwrap();
        assert!(plans
            .resolve_entry(&a)
            .unwrap()
            .plan
            .transfers
            .iter()
            .all(|transfer| transfer.cleanup_state == CloudShareCleanupState::Pending));
    }

    #[test]
    fn task_listing_restores_attempted_cleanup_after_restart_without_aria2_or_network() {
        use crate::cloud::{share::*, CloudProvider, SystemCredentialStore};
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("tasks.sqlite");
        let plans =
            CloudSharePlanStore::new(SystemCredentialStore::new(root.path().into()).unwrap());
        let tasks = TaskStore::open_path(&db).unwrap();
        let plan = CloudSharePlan::new(
            CloudProvider::Baidu,
            "secret-share-id".into(),
            Some("secret-access-code".into()),
            vec![CloudShareEntry::new(
                "secret-share-id",
                "entry".into(),
                "file".into(),
                vec![],
                None,
            )],
            serde_json::json!({"cookie":"secret-cookie"}),
        );
        plans.upsert(&plan).unwrap();
        let source = plan.source_for("entry");
        plans
            .link_local_sources(std::slice::from_ref(&source))
            .unwrap();
        let task = tasks
            .create(crate::NewTask {
                source: source.clone(),
                name: "file".into(),
                kind: "cloud".into(),
                is_private: false,
            })
            .unwrap();
        let completed = set_status(&tasks, task.id, TaskStatus::Completed);
        assert!(completed.aria_gid.is_none());
        plans.link_local_task(&source, task.id).unwrap();
        plans.record_transfer(&source, "secret-created-id").unwrap();
        assert_eq!(
            plans
                .mark_local_completion(&source, false, |_, _| true)
                .unwrap(),
            vec!["secret-created-id"]
        );
        assert!(tasks.find(task.id).unwrap().error.is_none());
        drop(plans);
        drop(tasks);

        let reopened_plans =
            CloudSharePlanStore::new(SystemCredentialStore::new(root.path().into()).unwrap());
        let reopened_tasks = TaskStore::open_path(&db).unwrap();
        let credentials = SystemCredentialStore::new(root.path().into()).unwrap();
        let before = crate::cloud::CredentialStore::load(&credentials, SHARE_PLAN_SECRET).unwrap();
        let listed = crate::commands::list_tasks_with_cleanup_projection(
            &reopened_tasks,
            Some(&reopened_plans),
        )
        .unwrap();
        assert_eq!(
            crate::cloud::CredentialStore::load(&credentials, SHARE_PLAN_SECRET).unwrap(),
            before,
            "listing must not modify or rearm the cleanup journal"
        );
        assert_eq!(listed[0].status, TaskStatus::Completed);
        assert!(listed[0]
            .error
            .as_deref()
            .is_some_and(|message| message.starts_with("cleanup_pending：")));
        let projection = serde_json::to_string(&listed).unwrap();
        for secret in [
            "secret-share-id",
            "secret-access-code",
            "secret-cookie",
            "secret-created-id",
        ] {
            assert!(!projection.contains(secret));
        }
        // Listing has no provider action dependency and never rearms a claim.
        assert!(reopened_plans
            .mark_local_completion(&source, false, |_, _| true)
            .unwrap()
            .is_empty());
        assert_eq!(
            reopened_plans
                .resolve_entry(&source)
                .unwrap()
                .plan
                .transfers[0]
                .cleanup_state,
            CloudShareCleanupState::Attempted
        );
    }

    #[test]
    fn cleanup_projection_preserves_cancelled_tasks_and_rejects_mismatched_sources() {
        let (_root, plans, tasks, locals) = cleanup_fixture();
        let completed = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        let cancelled = set_status(&tasks, locals[1].id, TaskStatus::Cancelled);
        let personal = tasks
            .create(crate::NewTask {
                source: "cloud://baidu/personal-file".into(),
                name: "personal".into(),
                kind: "cloud".into(),
                is_private: false,
            })
            .unwrap();
        let personal = set_status(&tasks, personal.id, TaskStatus::Completed);
        plans
            .link_local_task(&completed.source, personal.id)
            .unwrap();
        plans.mark_cleanup_attempted(&completed.source).unwrap();
        let listed =
            crate::commands::list_tasks_with_cleanup_projection(&tasks, Some(&plans)).unwrap();
        assert!(listed
            .iter()
            .find(|task| task.id == completed.id)
            .unwrap()
            .error
            .as_deref()
            .is_some_and(|message| message.starts_with("cleanup_pending：")));
        for original in [cancelled, personal] {
            let restored = listed.iter().find(|task| task.id == original.id).unwrap();
            assert_eq!(restored.status, original.status);
            assert_eq!(restored.error, original.error);
        }
    }

    #[test]
    fn cleanup_projection_converges_all_siblings_after_one_successful_retry() {
        use crate::cloud::{share::SHARE_PLAN_SECRET, CredentialStore, SystemCredentialStore};
        let (root, plans, tasks, locals) = cleanup_fixture();
        let first = set_status(&tasks, locals[0].id, TaskStatus::Completed);
        let second = set_status(&tasks, locals[1].id, TaskStatus::Completed);
        assert!(first.aria_gid.is_none() && second.aria_gid.is_none());
        plans
            .mark_local_completion(&first.source, false, |_, _| true)
            .unwrap();
        let pending =
            crate::commands::list_tasks_with_cleanup_projection(&tasks, Some(&plans)).unwrap();
        assert!(pending.iter().all(|task| task
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("cleanup_pending："))));

        let mut network_calls = 0;
        cleanup_completed_share(&plans, &tasks, &first, true, true, |_, ids| {
            network_calls += 1;
            assert_eq!(ids, &["created-id"]);
            Ok(())
        })
        .unwrap();
        assert_eq!(network_calls, 1);
        assert!(tasks.find(first.id).unwrap().error.is_none());
        assert!(tasks.find(second.id).unwrap().error.is_some());
        let credentials = SystemCredentialStore::new(root.path().into()).unwrap();
        let journal = credentials.load(SHARE_PLAN_SECRET).unwrap();
        let listed =
            crate::commands::list_tasks_with_cleanup_projection(&tasks, Some(&plans)).unwrap();
        assert!(
            listed
                .iter()
                .all(|task| task.status == TaskStatus::Completed && task.error.is_none()),
            "a sibling retained cleanup_pending after the plan cleanup succeeded"
        );
        assert_eq!(network_calls, 1);
        assert_eq!(credentials.load(SHARE_PLAN_SECRET).unwrap(), journal);

        // Only the literal cleanup_pending prefix is owned by this projection.
        // An underscore in SQL LIKE must not turn it into a wildcard match.
        let mut other_error = tasks.find(first.id).unwrap();
        other_error.error = Some("cleanupXpending：unrelated message".into());
        tasks.apply_transfer_snapshot(other_error.clone()).unwrap();
        let mut cancelled = tasks.find(second.id).unwrap();
        cancelled.status = TaskStatus::Cancelled;
        cancelled.error = Some("cleanup_pending：preserve cancelled task".into());
        tasks.apply_transfer_snapshot(cancelled.clone()).unwrap();
        let listed =
            crate::commands::list_tasks_with_cleanup_projection(&tasks, Some(&plans)).unwrap();
        for expected in [other_error, cancelled] {
            let actual = listed.iter().find(|task| task.id == expected.id).unwrap();
            assert_eq!(actual.status, expected.status);
            assert_eq!(actual.error, expected.error);
        }
        assert_eq!(network_calls, 1);
        assert_eq!(credentials.load(SHARE_PLAN_SECRET).unwrap(), journal);
    }

    struct NoEngineEvents;
    impl EngineStateSink for NoEngineEvents {
        fn engine_state_updated(&self, _: &EngineAvailability) {}
    }

    fn runtime(root: &std::path::Path) -> Arc<EngineRuntime> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut aria2 = crate::Aria2Settings::default();
        aria2.rpc_port = listener.local_addr().unwrap().port();
        aria2.rpc_token = format!("engine-runtime-unit-test-{}", aria2.rpc_port);
        drop(listener);
        Arc::new(EngineRuntime::new(
            Aria2LaunchConfig {
                binary: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
                data_dir: root.join("data"),
                download_dir: root.join("downloads"),
                parent_pid: std::process::id(),
                peer_discovery: false,
                aria2,
            },
            Arc::new(TaskStore::open_in_memory().unwrap()),
            Arc::new(crate::aria2::sync::NoEvents),
            Arc::new(NoEngineEvents),
        ))
    }

    #[tokio::test]
    async fn shutdown_deadline_includes_waiting_for_a_retry_that_owns_the_slot() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let slot = runtime.slot.lock().await;
        let retry_runtime = Arc::clone(&runtime);
        let retry = tokio::spawn(async move { retry_runtime.retry().await });
        tokio::task::yield_now().await;

        let began = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(150),
            runtime.shutdown(Duration::from_millis(40)),
        )
        .await;

        assert!(result.is_ok(), "shutdown waited indefinitely for the slot");
        assert!(began.elapsed() < Duration::from_millis(150));
        drop(slot);
        assert!(!retry.await.unwrap().available);
        assert!(runtime.slot.lock().await.service.is_none());
    }

    #[tokio::test]
    async fn monitor_does_not_spend_restart_budget_while_manual_retry_is_queued() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let slot = runtime.slot.lock().await;
        runtime.start_monitor(Duration::from_millis(10));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let retry_runtime = Arc::clone(&runtime);
        let manual = tokio::spawn(async move { retry_runtime.retry().await });
        tokio::task::yield_now().await;
        drop(slot);

        assert!(manual.await.unwrap().available);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !runtime.automatic_retry_used.load(Ordering::Acquire),
            "monitor spent its only budget while a manual retry was already pending"
        );

        let old = runtime.service().await.unwrap();
        old.api().shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(7), async {
            loop {
                if let Ok(service) = runtime.service().await {
                    if !Arc::ptr_eq(&old, &service)
                        && service.engine_running().await.unwrap() == Some(true)
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("real child exit did not receive the automatic restart budget");
        assert!(runtime.shutdown(Duration::from_secs(5)).await.child_stopped);
    }
}
