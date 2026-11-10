//! Native bridge to the private task runtime shipped with an installed Thunder
//! client. Each platform adapter stays isolated, while task ownership and
//! progress handling are shared by this backend.

#[cfg(any(windows, target_os = "linux"))]
use std::collections::BTreeMap;
use std::{path::PathBuf, sync::Arc};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

use crate::{
    cloud::commands::ResolvedCloudDownload, DownloadAction, DownloadTask, TaskStatus, TaskStore,
    UriSubmissionOptions,
};

mod engine;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::checkpoint as macos_checkpoint_path;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use engine::native_gid;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use engine::NativeEngine;
use engine::{
    parse_native_gid, NativeEngineSession, NativeProgress, NativeSubmission, NativeTaskHandle,
    NativeTransferMeter, NATIVE_DOWNLOADING,
};

#[cfg(all(test, windows))]
#[path = "xunlei_native_live_tests.rs"]
mod live_tests;

/// An in-memory-only marker used to hand a drive response's opaque credential
/// to this backend. `ResolvedCloudDownload::into_uri_submission_options`
/// removes it before an aria2 request is constructed.
pub(crate) const ACCELERATION_TOKEN_MARKER_HEADER: &str = "X-Downloader-Xunlei-Acceleration-Token";
pub(crate) const ACCESS_TOKEN_MARKER_HEADER: &str = "X-Downloader-Xunlei-Access-Token";
pub(crate) const USER_ID_MARKER_HEADER: &str = "X-Downloader-Xunlei-User-Id";
pub(crate) const PEER_ID_MARKER_HEADER: &str = "X-Downloader-Xunlei-Peer-Id";
pub(crate) const CONTENT_LENGTH_MARKER_HEADER: &str = "X-Downloader-Xunlei-Content-Length";
pub(crate) const CID_MARKER_HEADER: &str = "X-Downloader-Xunlei-Cid";
pub(crate) const GCID_MARKER_HEADER: &str = "X-Downloader-Xunlei-Gcid";
pub(crate) const VIP_TYPE_MARKER_HEADER: &str = "X-Downloader-Xunlei-Vip-Type";
pub(crate) const PLUGIN_UPDATE_MESSAGE: &str = "迅雷云盘插件需要更新";
pub(crate) const NATIVE_RESUME_BACKEND_UNAVAILABLE_MESSAGE: &str =
    "迅雷原生下载后端不可用，当前断点无法由普通下载继续；请安装或更新迅雷后重试，或选择重新下载";

pub(crate) fn is_native_gid(gid: Option<&str>) -> bool {
    gid.and_then(parse_native_gid).is_some()
}

#[cfg(target_os = "linux")]
pub(crate) fn run_linux_worker_from_args() -> Result<(), ()> {
    linux::run_worker_from_args()
}

pub struct XunleiNativeBackend {
    engine_session: NativeEngineSession,
    #[cfg(windows)]
    api: std::sync::Mutex<Option<WindowsApi>>,
    #[cfg(any(windows, target_os = "linux"))]
    vip_tokens: std::sync::Mutex<BTreeMap<u32, String>>,
    #[cfg(target_os = "linux")]
    linux_engine: std::sync::Mutex<Option<linux::LinuxWorkerEngine>>,
    #[cfg(target_os = "macos")]
    macos_engine: std::sync::Mutex<Option<macos::MacWorkerEngine>>,
    #[cfg(any(windows, target_os = "macos"))]
    plugin_config_path: Option<PathBuf>,
}

impl Default for XunleiNativeBackend {
    fn default() -> Self {
        Self {
            engine_session: NativeEngineSession::new(),
            #[cfg(windows)]
            api: std::sync::Mutex::new(None),
            #[cfg(any(windows, target_os = "linux"))]
            vip_tokens: std::sync::Mutex::new(BTreeMap::new()),
            #[cfg(target_os = "linux")]
            linux_engine: std::sync::Mutex::new(None),
            #[cfg(target_os = "macos")]
            macos_engine: std::sync::Mutex::new(None),
            #[cfg(any(windows, target_os = "macos"))]
            plugin_config_path: None,
        }
    }
}

impl XunleiNativeBackend {
    pub(crate) fn with_plugin_config(plugin_config_path: PathBuf) -> Self {
        Self {
            engine_session: NativeEngineSession::new(),
            #[cfg(windows)]
            api: std::sync::Mutex::new(None),
            #[cfg(any(windows, target_os = "linux"))]
            vip_tokens: std::sync::Mutex::new(BTreeMap::new()),
            #[cfg(target_os = "linux")]
            linux_engine: std::sync::Mutex::new(None),
            #[cfg(target_os = "macos")]
            macos_engine: std::sync::Mutex::new(None),
            #[cfg(any(windows, target_os = "macos"))]
            plugin_config_path: Some(plugin_config_path),
        }
    }

    /// Discovers the installed SDK and persists its path without loading the
    /// native module. Download submission later uses this saved, validated
    /// location directly.
    pub(crate) fn initialize_plugin_config(&self) {
        #[cfg(windows)]
        let _ = configured_or_installed_plugin_config(self.plugin_config_path.as_deref());
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::supported_versions as macos_supported_versions;

#[cfg(target_os = "macos")]
pub(crate) fn macos_fallback_notice() -> String {
    format!(
        "download.xunlei.unsupportedFallback:{}",
        macos_supported_versions()
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_checkpoint_notice() -> &'static str {
    static NOTICE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
            "download.xunlei.unsupportedCheckpoint:{}",
            macos_supported_versions()
        )
    });
    NOTICE.as_str()
}

#[derive(Debug)]
pub enum NativeSubmitError {
    #[cfg(target_os = "macos")]
    UnsupportedClientVersion,
    Incompatible,
    #[cfg(target_os = "macos")]
    ProxyCheckpoint,
    #[cfg(target_os = "macos")]
    SubmitFailed,
    Failed(String),
}

impl NativeSubmitError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Incompatible => PLUGIN_UPDATE_MESSAGE.into(),
            #[cfg(target_os = "macos")]
            Self::UnsupportedClientVersion => macos_fallback_notice(),
            #[cfg(target_os = "macos")]
            Self::ProxyCheckpoint => "download.xunlei.proxyCheckpoint".into(),
            #[cfg(target_os = "macos")]
            Self::SubmitFailed => "download.xunlei.submitFailed".into(),
            Self::Failed(message) => message.clone(),
        }
    }
}

impl XunleiNativeBackend {
    /// Returns `Ok(None)` when Thunder is unavailable or the resolved file
    /// lacks both login identity and a file-level acceleration credential.
    pub fn submit(
        self: &Arc<Self>,
        app: &AppHandle,
        store: &Arc<TaskStore>,
        task_id: Uuid,
        resolved: &ResolvedCloudDownload,
        options: &UriSubmissionOptions,
    ) -> Result<Option<DownloadTask>, NativeSubmitError> {
        // Existing aria2 checkpoints must stay with aria2 after installing or
        // upgrading the Mac native adapter. Only new tasks may select an engine.
        #[cfg(target_os = "macos")]
        if options.proxy.is_some() && is_native_gid(options.native_resume_gid.as_deref()) {
            return Err(NativeSubmitError::ProxyCheckpoint);
        }
        #[cfg(target_os = "macos")]
        let legacy_checkpoint = options.continue_download
            && store
                .find(task_id)
                .is_ok_and(|task| crate::aria2::native_resume::has_legacy_control(store, &task));
        #[cfg(target_os = "macos")]
        if !macos_can_select_native(options, legacy_checkpoint) {
            return Ok(None);
        }
        let Some((user_id, peer_id, vip_token)) = native_vip_credentials(resolved) else {
            return Ok(None);
        };
        let vip_token = Some(vip_token.to_owned());

        #[cfg(all(not(windows), not(target_os = "linux"), not(target_os = "macos")))]
        {
            let _ = (app, store, task_id, resolved, options, user_id, peer_id);
            return Ok(None);
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            return self.submit_unix(
                app, store, task_id, resolved, options, user_id, peer_id, vip_token,
            );
        }

        #[cfg(windows)]
        {
            let output_directory = options
                .directory
                .clone()
                .ok_or_else(|| NativeSubmitError::Failed("迅雷原生下载缺少保存目录".into()))?;
            let requested_output_name = options
                .output_name
                .clone()
                .unwrap_or_else(|| resolved.name.clone());
            if requested_output_name.is_empty() || requested_output_name.contains(['/', '\\']) {
                return Err(NativeSubmitError::Failed("迅雷原生下载文件名无效".into()));
            }
            std::fs::create_dir_all(&output_directory)
                .map_err(|_| NativeSubmitError::Failed("无法创建迅雷下载目录".into()))?;
            let output_path = crate::reserve_unique_output_path(
                store,
                task_id,
                &output_directory,
                &requested_output_name,
            )
            .map_err(|_| NativeSubmitError::Failed("无法分配迅雷下载文件名".into()))?;
            let output_name = output_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| NativeSubmitError::Failed("迅雷原生下载文件名无效".into()))?
                .to_owned();
            let referer = resolved
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("referer"))
                .map(|(_, value)| value.as_str())
                .unwrap_or("");
            let user_agent = resolved
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
                .map(|(_, value)| value.as_str())
                .unwrap_or("Mozilla/5.0");
            let cid = marker_header(resolved, CID_MARKER_HEADER).unwrap_or("");
            let gcid = marker_header(resolved, GCID_MARKER_HEADER).unwrap_or("");
            let native_id = {
                let mut api = self
                    .api
                    .lock()
                    .map_err(|_| NativeSubmitError::Incompatible)?;
                if api.is_none() {
                    *api = WindowsApi::load(
                        self.plugin_config_path.as_deref(),
                        Some((user_id, peer_id)),
                    )?;
                }
                let Some(api) = api.as_mut() else {
                    return Ok(None);
                };
                // SDK IDs belong to this process, not the persisted task DB.
                // Release only the live owner of this output, including before
                // an overwrite. A restored GID may now identify another task.
                api.release_output(&output_path)?;
                // Native checkpoint files are meaningful only after the
                // official backend has actually been selected. Without the
                // installed SDK, return `None` above and let aria2 apply its
                // own `.aria2` continuation and overwrite rules.
                prepare_native_output(
                    &output_path,
                    options.allow_overwrite,
                    options.continue_download,
                )?;
                api.set_identity(user_id, peer_id)?;
                let size = resolved.size_hint().unwrap_or(0);
                let native_id = api.create_p2sp_task(
                    &resolved.url,
                    &output_path,
                    &output_name,
                    &output_directory,
                    referer,
                    size,
                    user_agent,
                    cid,
                    gcid,
                )?;
                let start_result = (|| {
                    if let Some(token) = vip_token.as_deref() {
                        api.enable_vip_with_file_token(native_id, token)?;
                    }
                    api.start(native_id)
                })();
                if let Err(error) = start_result {
                    let _ = api.delete(native_id);
                    return Err(error);
                }
                native_id
            };
            let native_handle = self
                .engine_session
                .allocate_handle(native_id)
                .ok_or(NativeSubmitError::Incompatible)?;

            let mut task = store
                .find(task_id)
                .map_err(|_| NativeSubmitError::Failed("下载任务已不存在".into()))?;
            if task.status != TaskStatus::Queued {
                // Cancellation can deliberately bypass a blocked native
                // submission. Never publish this late SDK result over the
                // user's durable terminal state.
                self.spawn_task_cleanup(native_handle);
                return Ok(Some(task));
            }
            if let Some(token) = vip_token {
                self.vip_tokens
                    .lock()
                    .map_err(|_| NativeSubmitError::Incompatible)?
                    .insert(native_id, token);
            }
            task.name.clone_from(&output_name);
            task.aria_gid = Some(native_gid(native_handle));
            task.followed_by_gid = None;
            task.status = TaskStatus::Downloading;
            task.error = None;
            task.status_message = if self
                .vip_tokens
                .lock()
                .is_ok_and(|tokens| tokens.contains_key(&native_id))
            {
                Some("正在启用迅雷加速".into())
            } else {
                Some("迅雷SVIP加速凭据不可用，正在使用普通通道".into())
            };
            task.failure_code = None;
            task.aria_error_code = None;
            task.total_length = resolved.size_hint();
            task.download_speed = 0;
            task.connections = 0;
            task = store
                .apply_transfer_snapshot_with_files(
                    task,
                    &[(
                        0,
                        output_path.to_string_lossy().into_owned(),
                        resolved.size_hint().unwrap_or(0),
                        0,
                        true,
                    )],
                )
                .map_err(|_| NativeSubmitError::Failed("无法保存迅雷原生任务".into()))?;
            emit_task(app, store, &task);
            self.spawn_progress_monitor(
                app.clone(),
                Arc::clone(store),
                task_id,
                native_handle,
                output_path,
            );
            Ok(Some(task))
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn submit_unix(
        self: &Arc<Self>,
        app: &AppHandle,
        store: &Arc<TaskStore>,
        task_id: Uuid,
        resolved: &ResolvedCloudDownload,
        options: &UriSubmissionOptions,
        user_id: &str,
        peer_id: &str,
        vip_token: Option<String>,
    ) -> Result<Option<DownloadTask>, NativeSubmitError> {
        let output_directory = options
            .directory
            .clone()
            .ok_or_else(|| NativeSubmitError::Failed("迅雷原生下载缺少保存目录".into()))?;
        let requested_output_name = options
            .output_name
            .clone()
            .unwrap_or_else(|| resolved.name.clone());
        if requested_output_name.is_empty() || requested_output_name.contains(['/', '\\']) {
            return Err(NativeSubmitError::Failed("迅雷原生下载文件名无效".into()));
        }
        std::fs::create_dir_all(&output_directory)
            .map_err(|_| NativeSubmitError::Failed("无法创建迅雷下载目录".into()))?;
        let output_path = crate::reserve_unique_output_path(
            store,
            task_id,
            &output_directory,
            &requested_output_name,
        )
        .map_err(|_| NativeSubmitError::Failed("无法分配迅雷下载文件名".into()))?;
        let output_name = output_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| NativeSubmitError::Failed("迅雷原生下载文件名无效".into()))?
            .to_owned();
        #[cfg(target_os = "linux")]
        prepare_native_output(
            &output_path,
            options.allow_overwrite,
            options.continue_download,
        )?;

        #[cfg(target_os = "linux")]
        let native_handle = {
            let mut engine = self
                .linux_engine
                .lock()
                .map_err(|_| NativeSubmitError::Incompatible)?;
            if engine.is_none() {
                match linux::start_engine(self.engine_session) {
                    Ok(started) => *engine = Some(started),
                    Err(error) => {
                        linux_native_start_fallback(error)?;
                        return Ok(None);
                    }
                }
            }
            let engine = engine.as_mut().ok_or(NativeSubmitError::Incompatible)?;
            let headers = resolved
                .headers
                .iter()
                .filter(|(name, _)| !name.starts_with("X-Downloader-Xunlei-"))
                .cloned()
                .collect();
            engine
                .submit(NativeSubmission {
                    url: resolved.url.clone(),
                    headers,
                    output_path: output_path.clone(),
                    output_name: output_name.clone(),
                    user_id: user_id.to_owned(),
                    peer_id: peer_id.to_owned(),
                    acceleration_token: vip_token.clone(),
                    cid: marker_header(resolved, CID_MARKER_HEADER).map(str::to_owned),
                    gcid: marker_header(resolved, GCID_MARKER_HEADER).map(str::to_owned),
                    total_length: resolved.size_hint(),
                })
                .map_err(|_| NativeSubmitError::Incompatible)?
        };

        #[cfg(target_os = "macos")]
        let native_handle = {
            let mut engine = self
                .macos_engine
                .lock()
                .map_err(|_| NativeSubmitError::Incompatible)?;
            if !engine.as_mut().is_some_and(|engine| engine.is_alive()) {
                let root = self
                    .plugin_config_path
                    .as_deref()
                    .and_then(|path| path.parent())
                    .ok_or(NativeSubmitError::Incompatible)?
                    .join("xunlei-macos");
                match macos::MacWorkerEngine::start(&root, self.engine_session, peer_id) {
                    Ok(mut started) => {
                        started.prune_removed_tasks(store);
                        *engine = Some(started);
                    }
                    Err(engine::NativeEngineError::Unavailable) => return Ok(None),
                    Err(engine::NativeEngineError::UnsupportedClientVersion) => {
                        return Err(NativeSubmitError::UnsupportedClientVersion);
                    }
                    Err(_) => return Err(NativeSubmitError::Incompatible),
                }
            }
            let engine = engine.as_mut().ok_or(NativeSubmitError::Incompatible)?;
            let settings = app
                .state::<crate::commands::AppState>()
                .settings
                .lock()
                .map_err(|_| NativeSubmitError::Incompatible)?
                .aria2
                .clone();
            engine
                .set_limits(settings.overall_download_limit, settings.p2p_upload_limit)
                .map_err(|_| NativeSubmitError::Incompatible)?;
            engine
                .submit_with_options(
                    task_id,
                    NativeSubmission {
                        url: resolved.url.clone(),
                        headers: resolved
                            .headers
                            .iter()
                            .filter(|(name, _)| !name.starts_with("X-Downloader-Xunlei-"))
                            .cloned()
                            .collect(),
                        output_path: output_path.clone(),
                        output_name: output_name.clone(),
                        user_id: user_id.to_owned(),
                        peer_id: peer_id.to_owned(),
                        acceleration_token: vip_token.clone(),
                        cid: marker_header(resolved, CID_MARKER_HEADER).map(str::to_owned),
                        gcid: marker_header(resolved, GCID_MARKER_HEADER).map(str::to_owned),
                        total_length: resolved.size_hint(),
                    },
                    marker_header(resolved, VIP_TYPE_MARKER_HEADER)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    options.allow_overwrite,
                    options.continue_download && options.native_resume_gid.is_some(),
                )
                .map_err(|_| NativeSubmitError::SubmitFailed)?
        };

        let mut task = store
            .find(task_id)
            .map_err(|_| NativeSubmitError::Failed("下载任务已不存在".into()))?;
        if task.status != TaskStatus::Queued {
            self.spawn_task_cleanup(native_handle);
            return Ok(Some(task));
        }
        #[cfg(target_os = "linux")]
        if let Some(token) = vip_token {
            self.vip_tokens
                .lock()
                .map_err(|_| NativeSubmitError::Incompatible)?
                .insert(native_handle.task_id, token);
        }
        task.name.clone_from(&output_name);
        task.aria_gid = Some(native_gid(native_handle));
        task.followed_by_gid = None;
        task.status = TaskStatus::Downloading;
        task.error = None;
        #[cfg(target_os = "linux")]
        {
            task.status_message = if self
                .vip_tokens
                .lock()
                .is_ok_and(|tokens| tokens.contains_key(&native_handle.task_id))
            {
                Some("正在启用迅雷加速".into())
            } else {
                Some("迅雷SVIP加速凭据不可用，正在使用普通通道".into())
            };
        }
        #[cfg(target_os = "macos")]
        {
            task.status_message = Some("download.xunlei.native".into());
        }
        task.failure_code = None;
        task.aria_error_code = None;
        task.total_length = resolved.size_hint();
        task.download_speed = 0;
        task.connections = 0;
        task = store
            .apply_transfer_snapshot_with_files(
                task,
                &[(
                    0,
                    output_path.to_string_lossy().into_owned(),
                    resolved.size_hint().unwrap_or(0),
                    0,
                    true,
                )],
            )
            .map_err(|_| NativeSubmitError::Failed("无法保存迅雷原生任务".into()))?;
        emit_task(app, store, &task);
        self.spawn_progress_monitor(
            app.clone(),
            Arc::clone(store),
            task_id,
            native_handle,
            output_path,
        );
        Ok(Some(task))
    }

    pub fn control(
        self: &Arc<Self>,
        app: &AppHandle,
        store: &Arc<TaskStore>,
        task: DownloadTask,
        action: DownloadAction,
    ) -> Result<DownloadTask, String> {
        let native_handle =
            native_handle(&task).ok_or_else(|| "迅雷原生任务 ID 无效".to_string())?;
        if !self.engine_session.owns(native_handle) {
            return Err(NATIVE_RESUME_BACKEND_UNAVAILABLE_MESSAGE.into());
        }
        #[cfg(not(target_os = "macos"))]
        let native_id = native_handle.task_id;
        #[cfg(all(not(windows), not(target_os = "linux"), not(target_os = "macos")))]
        {
            let _ = (app, store, action, native_id);
            return Err(PLUGIN_UPDATE_MESSAGE.into());
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            let engine = &self.linux_engine;
            #[cfg(target_os = "macos")]
            let engine = &self.macos_engine;
            let mut engine = engine
                .lock()
                .map_err(|_| PLUGIN_UPDATE_MESSAGE.to_string())?;
            let engine = engine
                .as_mut()
                .ok_or_else(|| NATIVE_RESUME_BACKEND_UNAVAILABLE_MESSAGE.to_string())?;
            engine
                .control(native_handle, action)
                .map_err(|_| PLUGIN_UPDATE_MESSAGE.to_string())?;
            #[cfg(target_os = "linux")]
            if action == DownloadAction::Cancel {
                if let Ok(mut tokens) = self.vip_tokens.lock() {
                    tokens.remove(&native_id);
                }
            }

            let mut updated = task;
            match action {
                DownloadAction::Pause => {
                    updated.status = TaskStatus::Paused;
                    updated.status_message = None;
                }
                DownloadAction::Resume => {
                    updated.status = TaskStatus::Downloading;
                    #[cfg(target_os = "linux")]
                    {
                        updated.status_message = self
                            .vip_tokens
                            .lock()
                            .is_ok_and(|tokens| tokens.contains_key(&native_id))
                            .then(|| "正在启用迅雷加速".into());
                    }
                    #[cfg(target_os = "macos")]
                    {
                        updated.status_message = Some("download.xunlei.native".into());
                    }
                }
                DownloadAction::Cancel => {
                    updated.status = TaskStatus::Cancelled;
                    updated.status_message = None;
                }
            }
            updated.download_speed = 0;
            updated.error = None;
            updated.aria_error_code = None;
            let updated = store
                .apply_transfer_snapshot(updated)
                .map_err(|error| error.to_string())?;
            emit_task(app, store, &updated);
            if action == DownloadAction::Cancel {
                release_cloud_slot(app, store, &updated);
            }
            if action == DownloadAction::Resume {
                if let Some(path) = store
                    .files(updated.id)
                    .ok()
                    .and_then(|files| files.into_iter().next())
                    .map(|file| PathBuf::from(file.path))
                {
                    self.spawn_progress_monitor(
                        app.clone(),
                        Arc::clone(store),
                        updated.id,
                        native_handle,
                        path,
                    );
                }
            }
            return Ok(updated);
        }
        #[cfg(windows)]
        {
            {
                let mut api = self
                    .api
                    .lock()
                    .map_err(|_| PLUGIN_UPDATE_MESSAGE.to_string())?;
                if api.is_none() {
                    *api = WindowsApi::load(self.plugin_config_path.as_deref(), None)
                        .map_err(|error| error.user_message())?;
                }
                let api = api
                    .as_mut()
                    .ok_or_else(|| "迅雷原生下载组件未安装".to_string())?;
                match action {
                    DownloadAction::Pause => api.stop(native_id),
                    DownloadAction::Resume => (|| -> Result<(), NativeSubmitError> {
                        if let Some(token) = self
                            .vip_tokens
                            .lock()
                            .map_err(|_| NativeSubmitError::Incompatible)?
                            .get(&native_id)
                            .cloned()
                        {
                            api.enable_vip_with_file_token(native_id, &token)?;
                        }
                        api.start(native_id)
                    })(),
                    DownloadAction::Cancel => api.delete(native_id),
                }
                .map_err(|error| error.user_message())?;
            }

            if action == DownloadAction::Cancel {
                if let Ok(mut tokens) = self.vip_tokens.lock() {
                    tokens.remove(&native_id);
                }
            }

            let mut updated = task;
            match action {
                DownloadAction::Pause => {
                    updated.status = TaskStatus::Paused;
                    updated.status_message = None;
                }
                DownloadAction::Resume => {
                    updated.status = TaskStatus::Downloading;
                    updated.status_message = self
                        .vip_tokens
                        .lock()
                        .is_ok_and(|tokens| tokens.contains_key(&native_id))
                        .then(|| "正在启用迅雷加速".into());
                }
                DownloadAction::Cancel => {
                    updated.status = TaskStatus::Cancelled;
                    updated.status_message = None;
                }
            }
            updated.download_speed = 0;
            updated.error = None;
            updated.aria_error_code = None;
            let updated = store
                .apply_transfer_snapshot(updated)
                .map_err(|error| error.to_string())?;
            emit_task(app, store, &updated);
            if action == DownloadAction::Cancel {
                release_cloud_slot(app, store, &updated);
            }
            if action == DownloadAction::Resume {
                if let Some(path) = store
                    .files(updated.id)
                    .ok()
                    .and_then(|files| files.into_iter().next())
                    .map(|file| PathBuf::from(file.path))
                {
                    self.spawn_progress_monitor(
                        app.clone(),
                        Arc::clone(store),
                        updated.id,
                        native_handle,
                        path,
                    );
                }
            }
            Ok(updated)
        }
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    fn spawn_task_cleanup(self: &Arc<Self>, native_handle: NativeTaskHandle) {
        let backend = Arc::clone(self);
        tauri::async_runtime::spawn_blocking(move || {
            #[cfg(windows)]
            let native_id = native_handle.task_id;
            #[cfg(windows)]
            let Ok(mut api) = backend.api.lock() else {
                return;
            };
            #[cfg(windows)]
            {
                // A persisted SDK id from an earlier application process is not
                // safe to delete: the new process may have reused that numeric id
                // for another task. Only clean tasks owned by this SDK instance.
                if let Some(api) = api.as_mut() {
                    if api.task_paths.values().any(|id| *id == native_id) {
                        if let Err(error) = api.delete(native_id) {
                            eprintln!("迅雷原生任务后台清理失败：{}", error.user_message());
                        }
                    }
                }
            }
            #[cfg(target_os = "linux")]
            let engine = &backend.linux_engine;
            #[cfg(target_os = "macos")]
            let engine = &backend.macos_engine;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if let Ok(mut engine) = engine.lock() {
                if let Some(engine) = engine.as_mut() {
                    let _ = engine.control(native_handle, DownloadAction::Cancel);
                    let _ = engine.release(native_handle);
                }
            }
        });
    }

    fn spawn_progress_monitor(
        self: &Arc<Self>,
        app: AppHandle,
        store: Arc<TaskStore>,
        id: Uuid,
        expected_native_handle: NativeTaskHandle,
        output_path: PathBuf,
    ) {
        let backend = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let mut meter = NativeTransferMeter::new(std::time::Instant::now());
            let mut progress_failures = 0u8;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let Ok(mut task) = store.find(id) else { return };
                if native_handle(&task) != Some(expected_native_handle)
                    || !backend.engine_session.owns(expected_native_handle)
                    || task.status != TaskStatus::Downloading
                {
                    return;
                }
                #[cfg(not(target_os = "macos"))]
                let native_task_id = expected_native_handle.task_id;
                #[cfg(windows)]
                let native_progress = backend
                    .api
                    .lock()
                    .ok()
                    .and_then(|api| api.as_ref().map(|api| api.progress(native_task_id)));
                #[cfg(target_os = "linux")]
                let engine = &backend.linux_engine;
                #[cfg(target_os = "macos")]
                let engine = &backend.macos_engine;
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                let native_progress = engine.lock().ok().and_then(|mut engine| {
                    engine.as_mut().map(|engine| {
                        engine
                            .poll(expected_native_handle)
                            .map_err(|_| NativeSubmitError::Incompatible)
                    })
                });
                #[cfg(all(not(windows), not(target_os = "linux"), not(target_os = "macos")))]
                let native_progress: Option<
                    Result<NativeProgress, NativeSubmitError>,
                > = None;
                let native_progress = match native_progress {
                    Some(Ok(progress)) => {
                        progress_failures = 0;
                        progress
                    }
                    Some(Err(_)) | None => {
                        progress_failures = progress_failures.saturating_add(1);
                        if !pause_after_native_progress_failures(&mut task, progress_failures) {
                            continue;
                        }
                        #[cfg(target_os = "macos")]
                        if !store.find(id).is_ok_and(|current| {
                            current.status == TaskStatus::Downloading
                                && native_handle(&current) == Some(expected_native_handle)
                        }) {
                            return;
                        }
                        let Ok(updated) = store.apply_transfer_snapshot(task) else {
                            return;
                        };
                        emit_task(&app, &store, &updated);
                        release_cloud_slot(&app, &store, &updated);
                        return;
                    }
                };
                let completed = native_progress.completed;
                let native_total = native_progress.total;
                let native_status = native_progress.status;
                let native_error_code = native_progress.error_code;
                let total = (native_total > 0)
                    .then_some(native_total)
                    .or(task.total_length);
                task.total_length = total;
                let now = std::time::Instant::now();
                #[cfg(not(target_os = "macos"))]
                let has_vip_token = {
                    #[cfg(windows)]
                    {
                        backend
                            .vip_tokens
                            .lock()
                            .is_ok_and(|tokens| tokens.contains_key(&native_task_id))
                    }
                    #[cfg(target_os = "linux")]
                    {
                        backend
                            .vip_tokens
                            .lock()
                            .is_ok_and(|tokens| tokens.contains_key(&native_task_id))
                    }
                    #[cfg(all(not(windows), not(target_os = "linux"), not(target_os = "macos")))]
                    {
                        false
                    }
                };
                // The macOS path accepts only a file-level acceleration token.
                #[cfg(target_os = "macos")]
                let has_vip_token = true;
                let (speed, svip_confirmed) = meter.sample(native_progress, has_vip_token, now);
                task.completed_length = completed;
                task.download_speed = speed;
                task.connections = native_progress.connections;
                #[cfg(not(target_os = "macos"))]
                update_svip_status_message(&mut task.status_message, has_vip_token, svip_confirmed);
                #[cfg(target_os = "macos")]
                {
                    task.status_message = Some(
                        if svip_confirmed {
                            "download.xunlei.accelerating"
                        } else {
                            "download.xunlei.native"
                        }
                        .into(),
                    );
                }
                task.progress = total
                    .filter(|total| *total > 0)
                    .map(|total| ((completed.saturating_mul(100) / total).min(100)) as u8)
                    .unwrap_or(0);
                if native_status == NATIVE_COMPLETE
                    && output_path.is_file()
                    && total.is_some_and(|total| total > 0 && completed == total)
                {
                    task.status = TaskStatus::Completed;
                    task.progress = 100;
                    task.download_speed = 0;
                    task.error = None;
                    task.status_message = None;
                    let _ = backend.stop_completed_task(&task);
                } else if native_status == NATIVE_ERROR {
                    task.status = TaskStatus::Failed;
                    task.download_speed = 0;
                    task.error = Some(native_failure_message(native_error_code));
                    task.status_message = None;
                } else if native_status == NATIVE_STOPPED {
                    task.status = TaskStatus::Paused;
                    task.download_speed = 0;
                    task.status_message = Some("迅雷原生任务已停止，请继续下载".into());
                } else if now.duration_since(meter.last_data_at).as_secs() >= 120 {
                    task.status = TaskStatus::Failed;
                    task.download_speed = 0;
                    task.status_message = None;
                    task.error = Some("迅雷原生任务长时间未返回数据，请重试".into());
                    let _ = backend.stop_completed_task(&task);
                }
                // A pause/resubmit can finish while QueryTaskInfo is in flight.
                if !store.find(id).is_ok_and(|current| {
                    current.status == TaskStatus::Downloading
                        && native_handle(&current) == Some(expected_native_handle)
                }) {
                    return;
                }
                let Ok(updated) = store.apply_transfer_snapshot_with_files(
                    task.clone(),
                    &[(
                        0,
                        output_path.to_string_lossy().into_owned(),
                        total.unwrap_or(0),
                        completed,
                        true,
                    )],
                ) else {
                    return;
                };
                emit_task(&app, &store, &updated);
                // Native transfers do not emit aria2 completion events. Use
                // the same opt-in cleanup for this Mac engine's share tasks.
                #[cfg(target_os = "macos")]
                if updated.status == TaskStatus::Completed {
                    crate::cloud::commands::schedule_cloud_share_cleanup(app.clone(), &updated);
                }
                if matches!(
                    updated.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Paused
                ) {
                    release_cloud_slot(&app, &store, &updated);
                    return;
                }
            }
        });
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn shutdown(&self) {
        if let Ok(mut engine) = self.macos_engine.lock() {
            if let Some(engine) = engine.as_mut() {
                engine.shutdown();
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn set_limits(&self, download: u64, upload: u64) -> Result<(), String> {
        if let Ok(mut engine) = self.macos_engine.lock() {
            if let Some(engine) = engine.as_mut() {
                if !engine.is_alive() {
                    return Ok(());
                }
                engine
                    .set_limits(download, upload)
                    .map_err(|_| PLUGIN_UPDATE_MESSAGE.to_string())?;
            }
        }
        Ok(())
    }

    fn stop_completed_task(&self, task: &DownloadTask) -> Result<(), NativeSubmitError> {
        #[cfg(target_os = "macos")]
        if let Some(handle) = native_handle(task) {
            if let Ok(mut engine) = self.macos_engine.lock() {
                if let Some(engine) = engine.as_mut() {
                    if task.status == TaskStatus::Completed {
                        let _ = engine.finish(handle);
                    } else {
                        let _ = engine.control(handle, DownloadAction::Pause);
                        let _ = engine.release(handle);
                    }
                }
            }
        }
        #[cfg(windows)]
        if let Some(handle) = native_handle(task).filter(|handle| self.engine_session.owns(*handle))
        {
            let id = handle.task_id;
            if let Ok(mut api) = self.api.lock() {
                if let Some(api) = api.as_mut() {
                    let _ = api.stop(id);
                }
            }
        }
        Ok(())
    }
}

impl ResolvedCloudDownload {
    fn size_hint(&self) -> Option<u64> {
        self.headers.iter().find_map(|(name, value)| {
            name.eq_ignore_ascii_case(CONTENT_LENGTH_MARKER_HEADER)
                .then(|| value.parse().ok())
                .flatten()
        })
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn native_checkpoint_path(output: &std::path::Path, suffix: &str) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

#[cfg(any(windows, target_os = "linux"))]
fn prepare_native_output(
    output: &std::path::Path,
    allow_overwrite: bool,
    continue_download: bool,
) -> Result<(), NativeSubmitError> {
    let checkpoint = native_checkpoint_path(output, ".xltd");
    let checkpoint_config = native_checkpoint_path(output, ".xltd.cfg");
    if allow_overwrite {
        for path in [output, checkpoint.as_path(), checkpoint_config.as_path()] {
            if path.exists() {
                std::fs::remove_file(path)
                    .map_err(|_| NativeSubmitError::Failed("无法覆盖已有的迅雷下载文件".into()))?;
            }
        }
        return Ok(());
    }
    // A native resume is safe only with the SDK's own checkpoint. The payload
    // may already exist (and is often preallocated), so it must not be
    // mistaken for a duplicate once that checkpoint is present.
    if continue_download && checkpoint.is_file() {
        return Ok(());
    }
    if output.exists() {
        return Err(NativeSubmitError::Failed(
            "目标文件已存在，请选择覆盖或更换下载位置".into(),
        ));
    }
    if !continue_download && (checkpoint.exists() || checkpoint_config.exists()) {
        return Err(NativeSubmitError::Failed(
            "目标位置存在未完成的迅雷文件，请继续原任务或选择覆盖".into(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_can_select_native(options: &UriSubmissionOptions, legacy_checkpoint: bool) -> bool {
    options.proxy.is_none()
        && !legacy_checkpoint
        && options
            .native_resume_gid
            .as_deref()
            .is_none_or(|gid| is_native_gid(Some(gid)))
}

fn marker_header<'a>(resolved: &'a ResolvedCloudDownload, marker: &str) -> Option<&'a str> {
    resolved
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(marker))
        .map(|(_, value)| value.as_str())
}

fn native_vip_token(resolved: &ResolvedCloudDownload) -> Option<&str> {
    marker_header(resolved, ACCELERATION_TOKEN_MARKER_HEADER).filter(|token| !token.is_empty())
}

fn native_vip_credentials(resolved: &ResolvedCloudDownload) -> Option<(&str, &str, &str)> {
    marker_header(resolved, ACCESS_TOKEN_MARKER_HEADER).filter(|value| !value.is_empty())?;
    Some((
        marker_header(resolved, USER_ID_MARKER_HEADER).filter(|value| !value.is_empty())?,
        marker_header(resolved, PEER_ID_MARKER_HEADER).filter(|value| !value.is_empty())?,
        native_vip_token(resolved)?,
    ))
}

#[cfg(target_os = "linux")]
fn linux_native_start_fallback(error: engine::NativeEngineError) -> Result<(), NativeSubmitError> {
    match error {
        engine::NativeEngineError::Unavailable => Ok(()),
        _ => Err(NativeSubmitError::Incompatible),
    }
}

fn native_handle(task: &DownloadTask) -> Option<NativeTaskHandle> {
    task.aria_gid.as_deref().and_then(parse_native_gid)
}

fn emit_task(app: &AppHandle, store: &TaskStore, task: &DownloadTask) {
    let _ = crate::commands::emit_task_update_for_task(app, store, None, task.clone());
}

fn release_cloud_slot(app: &AppHandle, store: &TaskStore, task: &DownloadTask) {
    let state = app.state::<crate::commands::AppState>();
    release_cloud_admission(&state.cloud_download_dispatch, store, task);
}

fn release_cloud_admission(
    dispatch: &crate::cloud::download_dispatch::CloudDownloadDispatch,
    store: &TaskStore,
    task: &DownloadTask,
) {
    if task.kind != "cloud" {
        return;
    }
    let directory_finished = store
        .cloud_directory_group_for_task(task.id)
        .ok()
        .flatten()
        .is_some_and(|group| {
            crate::commands::cloud_directory_group_queue_task(store, group.id).is_ok_and(|root| {
                matches!(
                    root.status,
                    TaskStatus::Completed
                        | TaskStatus::Failed
                        | TaskStatus::Cancelled
                        | TaskStatus::Recycled
                )
            })
        });
    dispatch.release_terminal(task.id, directory_finished);
}

#[cfg(windows)]
struct WindowsApi {
    // The native module owns worker threads for the lifetime of the process.
    // Keep the module loaded instead of calling FreeLibrary while they may run.
    _module: usize,
    task_paths: BTreeMap<PathBuf, u32>,
    create_p2sp_task: XlCreateP2spTask,
    redirect_original_resource: XlRedirectOriginalResource,
    delete_task: XlTaskAction,
    query_task_info: XlQueryTaskInfo,
    set_app_guid: XlSetString,
    set_global_ext_info: XlSetGlobalExtInfo,
    set_origin_connect_count: XlTaskInteger,
    enable_dcdn_with_vip_cert: XlTaskStringKind,
    set_task_trace_id: XlTaskStringKind,
    set_task_user_agent: XlTaskString,
    set_token_mode: XlSetInteger,
    set_user_info: XlSetUserInfo,
    start_task: XlTaskAction,
    stop_task: XlTaskAction,
}

#[cfg(windows)]
type XlInit = unsafe extern "system" fn(*const std::ffi::c_char, *const u8) -> i32;
#[cfg(windows)]
type XlCreateP2spTask = unsafe extern "system" fn(
    *const u16,
    *const u16,
    *const u16,
    *const u16,
    *const u16,
    *mut u32,
) -> i32;
#[cfg(windows)]
type XlTaskAction = unsafe extern "system" fn(u32) -> i32;
#[cfg(windows)]
type XlQueryTaskInfo = unsafe extern "system" fn(u32, *mut u8) -> i32;
#[cfg(windows)]
type XlSetString = unsafe extern "system" fn(*const std::ffi::c_char) -> i32;
#[cfg(windows)]
type XlSetInteger = unsafe extern "system" fn(i32) -> i32;
#[cfg(windows)]
type XlSetUserInfo =
    unsafe extern "system" fn(*const std::ffi::c_char, *const std::ffi::c_char) -> i32;
#[cfg(windows)]
type XlSetGlobalExtInfo = unsafe extern "system" fn(*const std::ffi::c_char, u8) -> i32;
#[cfg(windows)]
type XlTaskInteger = unsafe extern "system" fn(u32, i32) -> i32;
#[cfg(windows)]
type XlTaskString = unsafe extern "system" fn(u32, *const std::ffi::c_char) -> i32;
#[cfg(windows)]
type XlTaskStringKind = unsafe extern "system" fn(u32, i32, *const std::ffi::c_char) -> i32;

#[cfg(windows)]
type XlRedirectOriginalResource = unsafe extern "system" fn(u32, *const u16, u32) -> i32;

const NATIVE_STOPPED: i32 = 5;
const NATIVE_COMPLETE: i32 = 7;
const NATIVE_ERROR: i32 = 8;

fn update_svip_status_message(
    message: &mut Option<String>,
    has_vip_token: bool,
    svip_confirmed: bool,
) {
    if has_vip_token {
        *message = Some(
            if svip_confirmed {
                "正在使用迅雷svip加速"
            } else {
                "正在启用迅雷加速"
            }
            .into(),
        );
    } else if matches!(
        message.as_deref(),
        Some("正在启用迅雷加速" | "正在使用迅雷svip加速")
    ) {
        *message = None;
    }
}

const NATIVE_PROGRESS_FAILURE_LIMIT: u8 = 3;

fn pause_after_native_progress_failures(task: &mut DownloadTask, failures: u8) -> bool {
    if failures < NATIVE_PROGRESS_FAILURE_LIMIT {
        return false;
    }
    task.status = TaskStatus::Paused;
    task.download_speed = 0;
    task.upload_speed = 0;
    task.error = None;
    task.failure_code = None;
    task.aria_error_code = None;
    task.status_message = Some("迅雷原生任务已停止，点击开始可继续下载".into());
    true
}

#[cfg(windows)]
fn c_string(value: &str) -> Result<std::ffi::CString, NativeSubmitError> {
    std::ffi::CString::new(value).map_err(|_| NativeSubmitError::Incompatible)
}

#[cfg(windows)]
fn wide_string(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

#[cfg(windows)]
fn wide_os(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().chain([0]).collect()
}

#[cfg(windows)]
fn native_call(code: i32, message: &'static str) -> Result<(), NativeSubmitError> {
    match code {
        0 => Ok(()),
        0x4E20 => Err(NativeSubmitError::Incompatible),
        _ => Err(NativeSubmitError::Failed(format!(
            "{message}（错误码 {code}）"
        ))),
    }
}

#[cfg(windows)]
fn native_stale_task_action_call(
    code: i32,
    message: &'static str,
) -> Result<(), NativeSubmitError> {
    match code {
        // Pause and cancellation are idempotent. If the SDK server restarted,
        // it no longer knows IDs owned by the previous server instance.
        0 | 0x4E20 | 0x4E21 => Ok(()),
        _ => native_call(code, message),
    }
}

#[cfg(windows)]
impl WindowsApi {
    fn load(
        plugin_config_path: Option<&std::path::Path>,
        identity: Option<(&str, &str)>,
    ) -> Result<Option<Self>, NativeSubmitError> {
        use windows::{
            core::{PCSTR, PCWSTR},
            Win32::System::LibraryLoader::{
                GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            },
        };

        let Some(plugin) = configured_or_installed_plugin_config(plugin_config_path) else {
            return Ok(None);
        };
        let plugin = isolated_plugin_config(plugin_config_path, plugin)?;
        let wide_path = wide_os(plugin.sdk_path.as_os_str());
        let module = unsafe {
            LoadLibraryExW(
                PCWSTR(wide_path.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        }
        .map_err(|_| NativeSubmitError::Incompatible)?;

        unsafe fn symbol<T: Copy>(
            module: windows::Win32::Foundation::HMODULE,
            name: &'static [u8],
        ) -> Result<T, NativeSubmitError> {
            let address = GetProcAddress(module, PCSTR(name.as_ptr()))
                .ok_or(NativeSubmitError::Incompatible)?;
            Ok(std::mem::transmute_copy(&address))
        }

        let init: XlInit = unsafe { symbol(module, b"XL_Init\0")? };
        let mut init_data = [0u8; 40];
        init_data[..4].copy_from_slice(&40u32.to_le_bytes());
        init_data[4..8].copy_from_slice(&(-1i32).to_le_bytes());
        let version = plugin.client_version.as_bytes();
        if version.len() > 29 {
            return Err(NativeSubmitError::Incompatible);
        }
        init_data[10..10 + version.len()].copy_from_slice(version);
        let app_key = c_string("xzcGMudGh1bmRlclg7MA^^SDK==edee53fd0b15e8d65dbfe7824f5f^a23")?;
        if unsafe { init(app_key.as_ptr(), init_data.as_ptr()) } != 0 {
            return Err(NativeSubmitError::Incompatible);
        }

        let mut api = Self {
            _module: module.0 as usize,
            task_paths: BTreeMap::new(),
            create_p2sp_task: unsafe { symbol(module, b"XL_CreateP2spTask\0")? },
            redirect_original_resource: unsafe {
                symbol(module, b"XL_RedirectOriginalResource\0")?
            },
            delete_task: unsafe { symbol(module, b"XL_DeleteTask\0")? },
            query_task_info: unsafe { symbol(module, b"XL_QueryTaskInfo\0")? },
            set_app_guid: unsafe { symbol(module, b"XL_SetAppGuid\0")? },
            set_global_ext_info: unsafe { symbol(module, b"XL_SetGlobalExtInfo\0")? },
            set_origin_connect_count: unsafe { symbol(module, b"XL_SetOriginConnectCount\0")? },
            enable_dcdn_with_vip_cert: unsafe { symbol(module, b"XL_EnableDcdnWithVipCert\0")? },
            set_task_trace_id: unsafe { symbol(module, b"XL_SetTaskTraceID\0")? },
            set_task_user_agent: unsafe { symbol(module, b"XL_SetTaskUserAgent\0")? },
            set_token_mode: unsafe { symbol(module, b"XL_SetTokenMode\0")? },
            set_user_info: unsafe { symbol(module, b"XL_SetUserInfo\0")? },
            start_task: unsafe { symbol(module, b"XL_StartTask\0")? },
            stop_task: unsafe { symbol(module, b"XL_StopTask\0")? },
        };
        if let Some((user_id, peer_id)) = identity {
            api.set_identity(user_id, peer_id)?;
        }
        Ok(Some(api))
    }

    fn create_p2sp_task(
        &mut self,
        url: &str,
        full_path: &std::path::Path,
        display_name: &str,
        save_path: &std::path::Path,
        ref_url: &str,
        file_size: u64,
        user_agent: &str,
        cid: &str,
        gcid: &str,
    ) -> Result<u32, NativeSubmitError> {
        self.release_output(full_path)?;
        let _ = (file_size, cid);
        // The SDK hashes the creation URL into its checkpoint identity. Signed
        // cloud URLs change on every resolution and otherwise produce error 201
        // when reopening the same file. This URL is only an SDK resource key;
        // redirect to the current real URL below BEFORE the task can be started.
        let creation_url = if gcid.is_empty() {
            url.to_owned()
        } else {
            format!(
                "https://pan.xunlei.com/?gcid={}",
                url::form_urlencoded::byte_serialize(gcid.to_ascii_lowercase().as_bytes())
                    .collect::<String>()
            )
        };
        let creation_url = wide_string(&creation_url);
        let current_url = wide_string(url);
        let referer = wide_string(ref_url);
        let cookie = wide_string("");
        let save_path = wide_os(save_path.as_os_str());
        let name = wide_string(display_name);
        let mut task_id = 0u32;
        let result = unsafe {
            (self.create_p2sp_task)(
                creation_url.as_ptr(),
                referer.as_ptr(),
                cookie.as_ptr(),
                save_path.as_ptr(),
                name.as_ptr(),
                &mut task_id,
            )
        };
        if result != 0 || task_id == 0 {
            return Err(NativeSubmitError::Failed(format!(
                "迅雷原生任务创建失败（错误码 {result}）"
            )));
        }
        // Proxy ABI includes the UTF-16 length (without NUL); the direct SDK
        // DLL's same-named export has a different, two-argument ABI.
        if let Err(error) = native_call(
            unsafe {
                (self.redirect_original_resource)(
                    task_id,
                    current_url.as_ptr(),
                    (current_url.len() - 1) as u32,
                )
            },
            "迅雷原生任务下载地址更新失败",
        ) {
            let _ = unsafe { (self.delete_task)(task_id) };
            return Err(error);
        }
        let user_agent = c_string(user_agent)?;
        native_call(
            unsafe { (self.set_task_user_agent)(task_id, user_agent.as_ptr()) },
            "迅雷原生任务 User-Agent 设置失败",
        )?;
        native_call(
            unsafe { (self.set_origin_connect_count)(task_id, 2) },
            "迅雷原生任务连接数设置失败",
        )?;
        let trace_id = Uuid::new_v4().to_string();
        let trace = c_string(&trace_id)?;
        native_call(
            unsafe { (self.set_task_trace_id)(task_id, 0, trace.as_ptr()) },
            "迅雷原生任务加速标识设置失败",
        )?;
        self.task_paths.insert(full_path.to_owned(), task_id);
        Ok(task_id)
    }

    fn release_output(&mut self, output: &std::path::Path) -> Result<(), NativeSubmitError> {
        let Some(id) = self.task_paths.get(output).copied() else {
            return Ok(());
        };
        native_call(unsafe { (self.stop_task)(id) }, "迅雷原生任务暂停失败")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let progress = self.progress(id)?;
            if matches!(
                progress.status,
                NATIVE_STOPPED | NATIVE_COMPLETE | NATIVE_ERROR
            ) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(NativeSubmitError::Failed(
                    "迅雷原生任务尚未停止，请稍后重试".into(),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Stop is asynchronous and does not release the SDK's open checkpoint.
        // DeleteTask releases the handle, but preserves .xltd/.xltd.cfg on disk.
        native_call(unsafe { (self.delete_task)(id) }, "迅雷原生断点释放失败")?;
        self.task_paths.remove(output);
        Ok(())
    }

    fn enable_vip_with_file_token(
        &mut self,
        id: u32,
        token: &str,
    ) -> Result<(), NativeSubmitError> {
        let token = c_string(token)?;
        native_call(
            unsafe { (self.enable_dcdn_with_vip_cert)(id, -1, token.as_ptr()) },
            "迅雷会员加速令牌注入失败",
        )
    }
    fn set_identity(&mut self, user_id: &str, peer_id: &str) -> Result<(), NativeSubmitError> {
        let user_id = c_string(user_id)?;
        let empty = c_string("")?;
        let peer_id = c_string(peer_id)?;
        let ext = c_string(&format!(
            "isvip=1,viptype=super,viplevel=,userchannel=,client_peerid={}",
            peer_id.to_string_lossy()
        ))?;
        native_call(
            unsafe { (self.set_app_guid)(peer_id.as_ptr()) },
            "迅雷原生设备标识设置失败",
        )?;
        native_call(
            unsafe { (self.set_user_info)(user_id.as_ptr(), empty.as_ptr()) },
            "迅雷原生账号设置失败",
        )?;
        native_call(
            unsafe { (self.set_global_ext_info)(ext.as_ptr(), 0) },
            "迅雷会员身份设置失败",
        )?;
        native_call(
            unsafe { (self.set_token_mode)(2) },
            "迅雷会员令牌模式设置失败",
        )
    }
    fn start(&mut self, id: u32) -> Result<(), NativeSubmitError> {
        native_call(unsafe { (self.start_task)(id) }, "迅雷原生任务启动失败")
    }
    fn stop(&mut self, id: u32) -> Result<(), NativeSubmitError> {
        native_stale_task_action_call(unsafe { (self.stop_task)(id) }, "迅雷原生任务暂停失败")
    }
    fn delete(&mut self, id: u32) -> Result<(), NativeSubmitError> {
        // The official engine expects an active task to be stopped before it
        // is deleted. Ignore the stop result here: delete below is the
        // authoritative operation, and both calls may report task-not-found
        // for a stale ID restored from an earlier application process.
        let _ = unsafe { (self.stop_task)(id) };
        let result = unsafe { (self.delete_task)(id) };
        self.task_paths.retain(|_, task_id| *task_id != id);
        native_stale_task_action_call(result, "迅雷原生任务取消失败")
    }

    fn progress(&self, id: u32) -> Result<NativeProgress, NativeSubmitError> {
        let mut info = [0u8; 924];
        info[..4].copy_from_slice(&924u32.to_le_bytes());
        native_call(
            unsafe { (self.query_task_info)(id, info.as_mut_ptr()) },
            "迅雷原生任务进度暂不可用",
        )?;
        Ok(NativeProgress::from_windows_query_info(&info))
    }
}

#[cfg(test)]
mod cross_platform_compile_tests {
    use super::native_failure_message;

    #[test]
    fn native_failure_message_is_available_without_windows_bindings() {
        assert_eq!(native_failure_message(0), "迅雷原生任务失败，请重试");
    }
}

fn native_failure_message(error_code: i32) -> String {
    if error_code == 0 {
        "迅雷原生任务失败，请重试".into()
    } else {
        format!("迅雷原生任务失败（错误码 {error_code}），请重试")
    }
}

#[cfg(all(windows, test))]
fn configured_or_installed_sdk_path(
    plugin_config_path: Option<&std::path::Path>,
) -> Option<PathBuf> {
    configured_or_installed_plugin_config(plugin_config_path).map(|config| config.sdk_path)
}

#[cfg(windows)]
fn configured_or_installed_plugin_config(
    plugin_config_path: Option<&std::path::Path>,
) -> Option<XunleiPluginConfig> {
    let configured = plugin_config_path
        .and_then(read_plugin_config)
        .filter(|config| {
            is_download_sdk(&config.sdk_path) && valid_client_version(&config.client_version)
        });
    if let Some(config) = configured {
        if let Some(path) = plugin_config_path {
            let _ = write_plugin_config(path, &config);
        }
        return Some(config);
    }

    let discovered = discover_installed_plugin_config()?;
    if let Some(plugin_config_path) = plugin_config_path {
        if let Err(error) = write_plugin_config(plugin_config_path, &discovered) {
            eprintln!("无法保存迅雷云盘插件配置（{plugin_config_path:?}）: {error}");
        }
    }
    Some(discovered)
}

#[cfg(windows)]
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct XunleiPluginConfig {
    sdk_path: PathBuf,
    #[serde(default)]
    client_version: String,
}

#[cfg(windows)]
fn read_plugin_config(path: &std::path::Path) -> Option<XunleiPluginConfig> {
    std::fs::read(path)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
}

#[cfg(windows)]
fn write_plugin_config(path: &std::path::Path, config: &XunleiPluginConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(config).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

#[cfg(windows)]
fn is_download_sdk(path: &std::path::Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("DownloadSDKProxy.dll"))
        && path.with_file_name("DownloadSDK.dll").is_file()
        && path.with_file_name("DownloadSDKServer.exe").is_file()
}

#[cfg(windows)]
fn valid_client_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 29
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

#[cfg(windows)]
fn discover_installed_plugin_config() -> Option<XunleiPluginConfig> {
    use winreg::{
        enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
        RegKey,
    };

    let uninstall_roots = [
        (
            HKEY_LOCAL_MACHINE,
            "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
        ),
        (
            HKEY_LOCAL_MACHINE,
            "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
        ),
        (
            HKEY_CURRENT_USER,
            "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
        ),
    ];
    uninstall_roots.into_iter().find_map(|(hive, subkey)| {
        let root = RegKey::predef(hive).open_subkey(subkey).ok()?;
        root.enum_keys().filter_map(Result::ok).find_map(|name| {
            let key = root.open_subkey(name).ok()?;
            let display_name: String = key.get_value("DisplayName").ok()?;
            if !is_thunder_installation(&display_name) {
                return None;
            }
            let install_location: String = key.get_value("InstallLocation").ok()?;
            let client_version: String = key.get_value("DisplayVersion").ok()?;
            if !valid_client_version(&client_version) {
                return None;
            }
            sdk_path_from_install_location(std::path::Path::new(install_location.trim())).map(
                |sdk_path| XunleiPluginConfig {
                    sdk_path,
                    client_version,
                },
            )
        })
    })
}

#[cfg(windows)]
fn is_thunder_installation(display_name: &str) -> bool {
    let name = display_name.trim().to_ascii_lowercase();
    name == "迅雷" || name.contains("thunder") || name.contains("xunlei")
}

#[cfg(windows)]
fn sdk_path_from_install_location(install_location: &std::path::Path) -> Option<PathBuf> {
    let path = install_location
        .join("Program")
        .join("SDK")
        .join("DownloadSDKProxy.dll");
    is_download_sdk(&path).then_some(path)
}

#[cfg(windows)]
const OFFICIAL_SDK_SERVER_NAME: &str = "DownloadSDKServer.exe";
#[cfg(windows)]
const ISOLATED_SDK_SERVER_NAME: &str = "AllDownSDKServer.exe";

#[cfg(windows)]
fn patch_proxy_server_name(image: &mut [u8]) -> Result<(), NativeSubmitError> {
    // The official proxy kills every process named DownloadSDKServer.exe and
    // then polls for up to five minutes before starting its broker. Thunder
    // supervises that same process, so both clients otherwise fight forever.
    // Patch only the private runtime copy; the installed official files remain
    // byte-for-byte untouched. An unknown proxy layout is a plugin update,
    // never a best-effort binary rewrite.
    let official = OFFICIAL_SDK_SERVER_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let private = ISOLATED_SDK_SERVER_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    if private.len() > official.len() {
        return Err(NativeSubmitError::Incompatible);
    }
    let matches = image
        .windows(official.len())
        .enumerate()
        .filter_map(|(offset, window)| (window == official).then_some(offset))
        .collect::<Vec<_>>();
    let [offset] = matches.as_slice() else {
        return Err(NativeSubmitError::Incompatible);
    };
    image[*offset..*offset + official.len()].fill(0);
    image[*offset..*offset + private.len()].copy_from_slice(&private);
    Ok(())
}

#[cfg(windows)]
fn materialize_isolated_sdk_runtime(
    source_directory: &std::path::Path,
    runtime_directory: &std::path::Path,
) -> Result<(), NativeSubmitError> {
    std::fs::create_dir_all(runtime_directory)
        .map_err(|_| NativeSubmitError::Failed("无法准备迅雷原生下载组件".into()))?;
    let entries = std::fs::read_dir(source_directory)
        .map_err(|_| NativeSubmitError::Failed("无法读取迅雷原生下载组件".into()))?;
    for entry in entries {
        let entry =
            entry.map_err(|_| NativeSubmitError::Failed("无法读取迅雷原生下载组件".into()))?;
        let source = entry.path();
        let file_name = entry.file_name();
        let destination = runtime_directory.join(&file_name);
        if source.is_dir() {
            materialize_isolated_sdk_runtime(&source, &destination)?;
            continue;
        }
        if !source.is_file() {
            continue;
        }
        let name = file_name.to_string_lossy();
        if name.eq_ignore_ascii_case("DownloadSDKProxy.dll") {
            let mut image = std::fs::read(&source)
                .map_err(|_| NativeSubmitError::Failed("无法读取迅雷原生下载组件".into()))?;
            patch_proxy_server_name(&mut image)?;
            std::fs::write(&destination, image)
                .map_err(|_| NativeSubmitError::Failed("无法准备迅雷原生下载组件".into()))?;
        } else {
            let destination = if name.eq_ignore_ascii_case(OFFICIAL_SDK_SERVER_NAME) {
                runtime_directory.join(ISOLATED_SDK_SERVER_NAME)
            } else {
                destination
            };
            std::fs::copy(&source, destination)
                .map_err(|_| NativeSubmitError::Failed("无法准备迅雷原生下载组件".into()))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn isolated_plugin_config(
    plugin_config_path: Option<&std::path::Path>,
    installed: XunleiPluginConfig,
) -> Result<XunleiPluginConfig, NativeSubmitError> {
    // Keep the persisted path pointed at the installed SDK so official client
    // updates are still discoverable. Only the module loaded by this process
    // comes from the fingerprinted, non-conflicting runtime copy.
    let source_directory = installed
        .sdk_path
        .parent()
        .ok_or(NativeSubmitError::Incompatible)?;
    let proxy_image = std::fs::read(&installed.sdk_path)
        .map_err(|_| NativeSubmitError::Failed("无法读取迅雷原生下载组件".into()))?;
    let server_image = std::fs::read(source_directory.join(OFFICIAL_SDK_SERVER_NAME))
        .map_err(|_| NativeSubmitError::Failed("无法读取迅雷原生下载组件".into()))?;
    let mut identity = Vec::with_capacity(proxy_image.len() + server_image.len());
    identity.extend_from_slice(&proxy_image);
    identity.extend_from_slice(&server_image);
    let fingerprint = format!("{:x}", md5::compute(identity));
    let base_directory = plugin_config_path
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("versadown-plugins"));
    let runtime_root = base_directory.join("xunlei-native-runtime");
    let runtime_directory =
        runtime_root.join(format!("{}-{}", installed.client_version, fingerprint));
    let runtime_proxy = runtime_directory.join("DownloadSDKProxy.dll");
    let ready = runtime_directory.join(".ready");
    if !(ready.is_file()
        && runtime_proxy.is_file()
        && runtime_directory.join(ISOLATED_SDK_SERVER_NAME).is_file()
        && runtime_directory.join("DownloadSDK.dll").is_file())
    {
        std::fs::create_dir_all(&runtime_root)
            .map_err(|_| NativeSubmitError::Failed("无法准备迅雷原生下载组件".into()))?;
        let staging = runtime_root.join(format!(
            ".staging-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        materialize_isolated_sdk_runtime(source_directory, &staging)?;
        std::fs::write(staging.join(".ready"), fingerprint.as_bytes())
            .map_err(|_| NativeSubmitError::Failed("无法准备迅雷原生下载组件".into()))?;
        match std::fs::rename(&staging, &runtime_directory) {
            Ok(()) => {}
            Err(_) if ready.is_file() => {
                let _ = std::fs::remove_dir_all(staging);
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(staging);
                return Err(NativeSubmitError::Failed("无法准备迅雷原生下载组件".into()));
            }
        }
    }
    Ok(XunleiPluginConfig {
        sdk_path: runtime_proxy,
        client_version: installed.client_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn process_ids(image_name: &str) -> Vec<u32> {
        let process_name = image_name.trim_end_matches(".exe");
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Get-Process -Name '{}' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id",
                    process_name.replace('\'', "''")
                ),
            ])
            .output()
            .expect("PowerShell should inspect SDK server processes");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|pid| pid.trim().parse::<u32>().ok())
            .collect()
    }

    #[test]
    fn native_gid_is_not_an_aria_gid() {
        assert!(is_native_gid(Some(
            "xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:42"
        )));
        assert!(!is_native_gid(Some("xunlei-native:42")));
        assert!(!is_native_gid(Some("0123456789abcdef")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_engine_selection_preserves_existing_aria2_resumes() {
        let mut options = UriSubmissionOptions::default();
        assert!(macos_can_select_native(&options, false));
        assert!(!macos_can_select_native(&options, true));
        options.proxy = Some("http://localhost:8080".into());
        assert!(!macos_can_select_native(&options, false));
        options.proxy = None;
        options.native_resume_gid = Some("12ab345678abcdef".into());
        assert!(!macos_can_select_native(&options, false));
        options.native_resume_gid =
            Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:7".into());
        assert!(macos_can_select_native(&options, false));
    }

    #[test]
    fn native_credentials_never_reach_aria2_options() {
        let resolved = ResolvedCloudDownload {
            url: "https://cdn.example/file".into(),
            name: "file.zip".into(),
            headers: vec![
                (ACCELERATION_TOKEN_MARKER_HEADER.into(), "secret".into()),
                (ACCESS_TOKEN_MARKER_HEADER.into(), "access-secret".into()),
                (USER_ID_MARKER_HEADER.into(), "user-id".into()),
                (PEER_ID_MARKER_HEADER.into(), "peer-id".into()),
                (CONTENT_LENGTH_MARKER_HEADER.into(), "42".into()),
                (CID_MARKER_HEADER.into(), "cid".into()),
                (GCID_MARKER_HEADER.into(), "gcid".into()),
                (VIP_TYPE_MARKER_HEADER.into(), "5".into()),
            ],
        };
        let options = resolved.into_uri_submission_options();
        assert!(options.headers.is_empty());
    }

    #[test]
    fn native_vip_uses_the_file_acceleration_token() {
        let resolved = ResolvedCloudDownload {
            url: "https://cdn.example/file".into(),
            name: "file.zip".into(),
            headers: vec![
                (ACCESS_TOKEN_MARKER_HEADER.into(), "account-token".into()),
                (
                    ACCELERATION_TOKEN_MARKER_HEADER.into(),
                    "file-acceleration-token".into(),
                ),
            ],
        };

        assert_eq!(native_vip_token(&resolved), Some("file-acceleration-token"));
    }

    #[test]
    fn native_submission_without_file_acceleration_credential_uses_aria2_fallback() {
        let resolved = ResolvedCloudDownload {
            url: "https://cdn.example/file".into(),
            name: "file.zip".into(),
            headers: vec![
                (ACCESS_TOKEN_MARKER_HEADER.into(), "login-access".into()),
                (USER_ID_MARKER_HEADER.into(), "account-id".into()),
                (PEER_ID_MARKER_HEADER.into(), "device-id".into()),
            ],
        };

        assert!(native_vip_credentials(&resolved).is_none());
    }

    #[test]
    fn native_submission_without_login_access_uses_aria2_fallback() {
        let resolved = ResolvedCloudDownload {
            url: "https://cdn.example/file".into(),
            name: "file.zip".into(),
            headers: vec![
                (USER_ID_MARKER_HEADER.into(), "account-id".into()),
                (PEER_ID_MARKER_HEADER.into(), "device-id".into()),
                (
                    ACCELERATION_TOKEN_MARKER_HEADER.into(),
                    "file-acceleration-token".into(),
                ),
            ],
        };

        assert!(native_vip_credentials(&resolved).is_none());
    }

    #[test]
    fn native_submission_with_login_and_file_acceleration_uses_native_backend() {
        let resolved = ResolvedCloudDownload {
            url: "https://cdn.example/file".into(),
            name: "file.zip".into(),
            headers: vec![
                (ACCESS_TOKEN_MARKER_HEADER.into(), "login-access".into()),
                (USER_ID_MARKER_HEADER.into(), "account-id".into()),
                (PEER_ID_MARKER_HEADER.into(), "device-id".into()),
                (
                    ACCELERATION_TOKEN_MARKER_HEADER.into(),
                    "file-acceleration-token".into(),
                ),
            ],
        };

        assert_eq!(
            native_vip_credentials(&resolved),
            Some(("account-id", "device-id", "file-acceleration-token"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_linux_client_falls_back_without_plugin_update_warning() {
        assert!(linux_native_start_fallback(engine::NativeEngineError::Unavailable).is_ok());
        assert!(matches!(
            linux_native_start_fallback(engine::NativeEngineError::Incompatible),
            Err(NativeSubmitError::Incompatible)
        ));
    }

    #[test]
    fn native_progress_uses_received_bytes_not_the_sdk_connection_counter() {
        // Captured from a stopped 16 MiB real SDK download; retained bytes are
        // a checkpoint, and the historical connection count remains 1.
        let mut info = [0u8; 924];
        info[4..8].copy_from_slice(&5i32.to_le_bytes());
        info[12..20].copy_from_slice(&16_777_216u64.to_le_bytes());
        info[20..28].copy_from_slice(&1_638_400u64.to_le_bytes());
        info[44..48].copy_from_slice(&1u32.to_le_bytes());
        let progress = NativeProgress::from_windows_query_info(&info);
        assert_eq!(progress.completed, 1_638_400);
        assert_eq!(progress.received, 0);
        let now = std::time::Instant::now();
        let mut meter = NativeTransferMeter::new(now);
        assert_eq!(
            meter.sample(progress, true, now + std::time::Duration::from_secs(1)),
            (0, false)
        );
    }

    #[test]
    fn svip_confirmation_survives_transient_native_status_for_the_same_task() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let mut meter = NativeTransferMeter::new(now);
        let mut progress = NativeProgress {
            status: 3,
            completed: 20_000_000,
            received: 600_000,
            ..Default::default()
        };
        // The first sample establishes a baseline. Historical bytes from a
        // restored task must not become current speed or SVIP evidence.
        assert_eq!(
            meter.sample(progress, true, now + Duration::from_secs(2)),
            (0, false)
        );
        progress.received += 800_000;
        progress.peer_received = 800_000;
        assert_eq!(
            meter.sample(progress, true, now + Duration::from_secs(4)),
            (400_000, true)
        );

        // The SDK can briefly report a connecting/scheduling state while the
        // same native task remains active. That instantaneous state must not
        // revoke an already observed SVIP transfer.
        progress.status = 1;
        assert_eq!(
            meter.sample(progress, true, now + Duration::from_secs(5)),
            (0, true)
        );

        progress.status = NATIVE_DOWNLOADING;
        assert_eq!(
            meter.sample(progress, true, now + Duration::from_secs(10)),
            (0, true)
        );
    }

    #[test]
    fn svip_confirmation_does_not_cross_native_monitor_lifetimes() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let progress = NativeProgress {
            status: NATIVE_DOWNLOADING,
            received: 512_000,
            peer_received: 512_000,
            ..Default::default()
        };

        let mut first_task = NativeTransferMeter::new(now);
        assert_eq!(
            first_task.sample(progress, true, now + Duration::from_secs(1)),
            (0, false)
        );
        assert_eq!(
            first_task.sample(
                NativeProgress {
                    status: NATIVE_DOWNLOADING,
                    received: 640_000,
                    peer_received: 640_000,
                    ..Default::default()
                },
                true,
                now + Duration::from_secs(2),
            ),
            (128_000, true)
        );

        let mut resumed_task = NativeTransferMeter::new(now + Duration::from_secs(2));
        assert_eq!(
            resumed_task.sample(
                NativeProgress {
                    status: NATIVE_DOWNLOADING,
                    received: 512_000,
                    peer_received: 512_000,
                    ..Default::default()
                },
                true,
                now + Duration::from_secs(3),
            ),
            (0, false)
        );
        assert_eq!(
            resumed_task.sample(
                NativeProgress {
                    status: NATIVE_DOWNLOADING,
                    received: 768_000,
                    peer_received: 768_000,
                    ..Default::default()
                },
                true,
                now + Duration::from_secs(4),
            ),
            (256_000, true)
        );
    }

    #[test]
    fn svip_status_message_follows_the_current_native_task_lifecycle() {
        let mut message = Some("正在启用迅雷加速".to_string());

        update_svip_status_message(&mut message, true, false);
        assert_eq!(message.as_deref(), Some("正在启用迅雷加速"));

        update_svip_status_message(&mut message, true, true);
        assert_eq!(message.as_deref(), Some("正在使用迅雷svip加速"));

        update_svip_status_message(&mut message, false, false);
        assert!(message.is_none());
    }

    #[test]
    fn repeated_native_progress_failures_switch_the_task_to_resumable_pause() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(crate::NewTask::direct("cloud://xunlei/file-id"))
            .unwrap();
        task.status = TaskStatus::Downloading;
        task.download_speed = 876_544;
        task.status_message = Some("正在使用迅雷svip加速".into());

        assert!(!pause_after_native_progress_failures(&mut task, 2));
        assert_eq!(task.status, TaskStatus::Downloading);

        assert!(pause_after_native_progress_failures(&mut task, 3));
        assert_eq!(task.status, TaskStatus::Paused);
        assert_eq!(task.download_speed, 0);
        assert!(task.error.is_none());
        assert!(task.status_message.is_some());
    }

    #[cfg(windows)]
    #[test]
    fn isolated_proxy_uses_a_private_server_process_name() {
        let official = "DownloadSDKServer.exe"
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let mut image = [
            b"prefix".as_slice(),
            official.as_slice(),
            b"suffix".as_slice(),
        ]
        .concat();

        patch_proxy_server_name(&mut image).unwrap();

        let private = "AllDownSDKServer.exe"
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert!(image.windows(private.len()).any(|window| window == private));
        assert!(!image
            .windows(official.len())
            .any(|window| window == official));
    }

    #[cfg(windows)]
    #[test]
    fn isolated_sdk_runtime_renames_only_its_server_and_keeps_dependencies() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(source.join("xar")).unwrap();
        let official = OFFICIAL_SDK_SERVER_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        std::fs::write(
            source.join("DownloadSDKProxy.dll"),
            [b"proxy".as_slice(), official.as_slice()].concat(),
        )
        .unwrap();
        std::fs::write(source.join(OFFICIAL_SDK_SERVER_NAME), b"server").unwrap();
        std::fs::write(source.join("DownloadSDK.dll"), b"engine").unwrap();
        std::fs::write(source.join("xar").join("resource.bin"), b"resource").unwrap();

        materialize_isolated_sdk_runtime(&source, &runtime).unwrap();

        assert!(runtime.join("DownloadSDKProxy.dll").is_file());
        assert!(runtime.join(ISOLATED_SDK_SERVER_NAME).is_file());
        assert!(!runtime.join(OFFICIAL_SDK_SERVER_NAME).exists());
        assert_eq!(
            std::fs::read(runtime.join("xar").join("resource.bin")).unwrap(),
            b"resource"
        );
    }

    #[cfg(windows)]
    #[test]
    fn plugin_config_keeps_the_installed_source_but_loads_an_isolated_runtime() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("official");
        std::fs::create_dir_all(&source).unwrap();
        let official = OFFICIAL_SDK_SERVER_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let source_proxy = source.join("DownloadSDKProxy.dll");
        std::fs::write(
            &source_proxy,
            [b"proxy".as_slice(), official.as_slice()].concat(),
        )
        .unwrap();
        std::fs::write(source.join(OFFICIAL_SDK_SERVER_NAME), b"server").unwrap();
        std::fs::write(source.join("DownloadSDK.dll"), b"engine").unwrap();
        let persisted_path = root.path().join("plugins").join("xunlei-native.json");
        let installed = XunleiPluginConfig {
            sdk_path: source_proxy.clone(),
            client_version: "25.1.13.1634".into(),
        };

        let runtime = isolated_plugin_config(Some(&persisted_path), installed).unwrap();

        assert_ne!(runtime.sdk_path, source_proxy);
        assert!(runtime.sdk_path.is_file());
        assert!(runtime
            .sdk_path
            .with_file_name(ISOLATED_SDK_SERVER_NAME)
            .is_file());
        assert_eq!(runtime.client_version, "25.1.13.1634");
    }

    #[tokio::test]
    async fn cancelling_native_cloud_task_releases_its_provider_admission() {
        use crate::cloud::{
            download_dispatch::CloudDownloadDispatch, download_scheduler::AdmissionOutcome,
            CloudProvider,
        };
        use std::time::Duration;

        let store = TaskStore::open_in_memory().unwrap();
        let tasks = (0..5)
            .map(|index| {
                store
                    .create(crate::NewTask {
                        source: format!("cloud://xunlei/file-{index}"),
                        name: format!("file-{index}.zip"),
                        kind: "cloud".into(),
                        is_private: false,
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let dispatch = CloudDownloadDispatch::default();
        for task in tasks.iter().take(4) {
            assert_eq!(
                dispatch
                    .scheduler
                    .enqueue(CloudProvider::Xunlei, task.id)
                    .await,
                AdmissionOutcome::Granted
            );
        }

        let waiting_dispatch = dispatch.clone();
        let waiting_id = tasks[4].id;
        let waiting = tokio::spawn(async move {
            waiting_dispatch
                .scheduler
                .enqueue(CloudProvider::Xunlei, waiting_id)
                .await
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        let mut cancelled = tasks[0].clone();
        cancelled.status = TaskStatus::Cancelled;
        let cancelled = store.apply_transfer_snapshot(cancelled).unwrap();
        release_cloud_admission(&dispatch, &store, &cancelled);

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(250), waiting)
                .await
                .expect("the queued Xunlei task should be admitted after cancellation")
                .unwrap(),
            AdmissionOutcome::Granted
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_stop_and_cancel_are_idempotent_after_sdk_server_restart() {
        assert!(native_stale_task_action_call(0, "action").is_ok());
        assert!(native_stale_task_action_call(0x4E20, "action").is_ok());
        assert!(native_stale_task_action_call(0x4E21, "action").is_ok());
        assert_eq!(
            native_stale_task_action_call(0x4E23, "迅雷原生任务取消失败")
                .unwrap_err()
                .user_message(),
            "迅雷原生任务取消失败（错误码 20003）"
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_output_conflicts_require_an_explicit_resume_or_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("file.zip");
        let checkpoint = native_checkpoint_path(&output, ".xltd");
        let checkpoint_config = native_checkpoint_path(&output, ".xltd.cfg");

        std::fs::write(&output, b"complete").unwrap();
        let error = prepare_native_output(&output, false, false).unwrap_err();
        assert_eq!(
            error.user_message(),
            "目标文件已存在，请选择覆盖或更换下载位置"
        );

        std::fs::remove_file(&output).unwrap();
        std::fs::write(&output, b"partial payload").unwrap();
        assert_eq!(
            prepare_native_output(&output, false, true)
                .unwrap_err()
                .user_message(),
            "目标文件已存在，请选择覆盖或更换下载位置"
        );
        std::fs::remove_file(&output).unwrap();
        std::fs::write(&checkpoint, b"partial").unwrap();
        std::fs::write(&checkpoint_config, b"checkpoint").unwrap();
        assert!(prepare_native_output(&output, false, true).is_ok());
        std::fs::write(&output, b"partial payload").unwrap();
        assert!(prepare_native_output(&output, false, true).is_ok());
        std::fs::remove_file(&output).unwrap();
        assert_eq!(
            prepare_native_output(&output, false, false)
                .unwrap_err()
                .user_message(),
            "目标位置存在未完成的迅雷文件，请继续原任务或选择覆盖"
        );

        prepare_native_output(&output, true, false).unwrap();
        assert!(!checkpoint.exists());
        assert!(!checkpoint_config.exists());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an installed Thunder client"]
    fn installed_download_sdk_loads_official_proxy() {
        let root = tempfile::tempdir().unwrap();
        let plugin_config_path = root.path().join("plugins").join("xunlei-native.json");
        let api = WindowsApi::load(Some(&plugin_config_path), None)
            .expect("installed proxy should be compatible")
            .expect("Thunder should be installed");
        let config = read_plugin_config(&plugin_config_path)
            .expect("the discovered SDK path should be persisted for later calls");
        assert!(is_download_sdk(&config.sdk_path));
        drop(api);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an installed official Thunder SDK"]
    fn installed_sdk_reuses_thunder_server_without_restarting_it() {
        let root = tempfile::tempdir().unwrap();
        let plugin_config_path = root.path().join("plugins").join("xunlei-native.json");
        let server_pids_before = process_ids(OFFICIAL_SDK_SERVER_NAME);
        assert!(
            !server_pids_before.is_empty(),
            "this coexistence probe requires Thunder's SDK server to be running"
        );
        let started = std::time::Instant::now();

        let api = WindowsApi::load(Some(&plugin_config_path), None)
            .expect("isolated proxy should remain compatible")
            .expect("Thunder should be installed");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "isolated SDK initialization must not enter the official proxy's five-minute server wait"
        );
        assert!(
            server_pids_before
                .iter()
                .any(|pid| process_ids(OFFICIAL_SDK_SERVER_NAME).contains(pid)),
            "VersaDown must reuse the running official SDK server without terminating it"
        );
        drop(api);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the official Thunder SDK with its shared server stopped"]
    fn installed_sdk_starts_private_server_without_thunder_server() {
        assert!(
            process_ids(OFFICIAL_SDK_SERVER_NAME).is_empty(),
            "stop the shared Thunder SDK server before running this cold-start probe"
        );
        let root = tempfile::tempdir().unwrap();
        let plugin_config_path = root.path().join("plugins").join("xunlei-native.json");
        let started = std::time::Instant::now();

        let api = WindowsApi::load(Some(&plugin_config_path), None)
            .expect("private proxy should remain compatible")
            .expect("Thunder should be installed");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "private SDK initialization must not enter the five-minute server wait"
        );
        assert!(
            !process_ids(ISOLATED_SDK_SERVER_NAME).is_empty(),
            "the private runtime must start its renamed official SDK server"
        );
        drop(api);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "read-only cloud source plus an isolated native download and resume probe"]
    fn live_xunlei_native_vip_download_and_resume() {
        use crate::cloud::CredentialStore;
        use std::io::Read as _;

        assert_eq!(
            std::env::var("VERSADOWN_LIVE_CLOUD").as_deref(),
            Ok("1")
        );
        let app_data = std::env::var_os("VERSADOWN_XUNLEI_SESSION_ROOT")
            .map(PathBuf::from)
            .expect("use an explicitly isolated session directory for the live test");
        let session_root = app_data.join("cloud-sessions");
        let load_session = || {
            if let Ok(json) = std::env::var("VERSADOWN_XUNLEI_SESSION_JSON") {
                serde_json::from_str(&json).expect("in-memory Xunlei session should be valid")
            } else {
                let credentials =
                    crate::cloud::SystemCredentialStore::new(session_root.clone()).unwrap();
                serde_json::from_slice(
                    &credentials
                        .load("xunlei")
                        .expect("saved Xunlei session should be readable")
                        .expect("saved Xunlei session should exist"),
                )
                .unwrap()
            }
        };
        let http = crate::cloud::ReqwestTransport::new().unwrap();
        let client = crate::cloud::xunlei::XunleiClient::new(load_session(), &http);
        // This test has no WebView. Prepare its isolated session through the
        // app's complete recovery flow, including the persistent web profile.
        let root_directory = client.directory("0").expect(
            "prepare a current session with the app recovery flow before this SDK-only test",
        );
        let refreshed_credentials =
            crate::cloud::SystemCredentialStore::new(session_root.clone()).unwrap();
        refreshed_credentials
            .save("xunlei", &serde_json::to_vec(&client.session()).unwrap())
            .expect("the refreshed live session should be persisted for the download probe");
        let file_id = std::env::var("VERSADOWN_XUNLEI_FILE_ID").unwrap_or_else(|_| {
            root_directory
                .items
                .iter()
                .find(|file| {
                    file.name == "midysj.zip"
                        && matches!(file.kind, crate::cloud::CloudFileKind::File)
                })
                .expect("provide the scoped midysj.zip file ID when it is inside a folder")
                .id
                .clone()
        });
        let expected_name =
            std::env::var("VERSADOWN_XUNLEI_FILE_NAME").unwrap_or_else(|_| "midysj.zip".into());
        let download = client
            .download_with_acceleration(&file_id)
            .expect("Xunlei should return a fresh cloud download contract");
        assert_eq!(
            download.name, expected_name,
            "only the scoped test file may be downloaded"
        );
        let root = std::env::var_os("VERSADOWN_XUNLEI_DOWNLOAD_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join(".tools")
                    .join(format!("xunlei-native-live-{}", uuid::Uuid::new_v4()))
            });
        let restore = std::env::var("VERSADOWN_XUNLEI_RESTORE").as_deref() == Ok("1");
        std::fs::create_dir_all(&root).unwrap();
        let source_hash = format!("{:x}", md5::compute(download.url.as_bytes()));
        if restore {
            let previous_hash = std::fs::read_to_string(root.join("source-url-hash")).unwrap();
            eprintln!(
                "cloud restore URL unchanged: {}",
                previous_hash == source_hash
            );
        } else {
            std::fs::write(root.join("source-url-hash"), source_hash).unwrap();
        }
        let output_directory = root.join("downloads");
        std::fs::create_dir_all(&output_directory).unwrap();
        let plugin_config_path = root.join("plugins").join("xunlei-native.json");
        let session = client.session();
        let peer_id = if session.web_context.peer_id.is_empty() {
            if session.web_context.device_id.is_empty() {
                &session.profile.device_id
            } else {
                &session.web_context.device_id
            }
        } else {
            &session.web_context.peer_id
        };
        let mut api =
            WindowsApi::load(Some(&plugin_config_path), Some((&session.user_id, peer_id)))
                .expect("official Xunlei proxy should load")
                .expect("Thunder should be installed");
        api.set_identity(&session.user_id, peer_id)
            .expect("native proxy should accept the current Xunlei identity");
        let output_name = "xunlei-native-live.bin";
        let output_path = output_directory.join(output_name);
        eprintln!(
            "native live contract: size={}, cid_len={}, gcid_len={}",
            download.size.unwrap_or(0),
            download.cid.as_deref().map(str::len).unwrap_or(0),
            download.gcid.as_deref().map(str::len).unwrap_or(0)
        );
        let task_id = api
            .create_p2sp_task(
                &download.url,
                &output_path,
                output_name,
                &output_directory,
                "https://pan.xunlei.com/",
                download.size.unwrap_or(0),
                &client.session().profile.user_agent,
                download.cid.as_deref().unwrap_or(""),
                download.gcid.as_deref().unwrap_or(""),
            )
            .expect("official P2SP task should be created");
        eprintln!("native live task id: {task_id}");
        let token = download
            .acceleration_token
            .as_deref()
            .expect("Xunlei should return the file acceleration token");
        api.enable_vip_with_file_token(task_id, token)
            .expect("the file SVIP token should be accepted");
        api.start(task_id).expect("native task should start");

        if restore {
            let saved: u64 = std::fs::read_to_string(root.join("verified-checkpoint-bytes"))
                .unwrap()
                .parse()
                .unwrap();
            let started = std::time::Instant::now();
            let mut checkpoint_checked = false;
            let mut peer_data = false;
            let mut last_report = 0;
            loop {
                let progress = api.progress(task_id).unwrap();
                assert_ne!(
                    progress.status, NATIVE_ERROR,
                    "restored cloud task failed: {progress:?}"
                );
                if progress.total > 0 && !checkpoint_checked {
                    assert!(
                        progress.completed >= saved,
                        "process restart discarded cloud checkpoint"
                    );
                    checkpoint_checked = true;
                }
                peer_data |= progress.peer_received > 0;
                let elapsed = started.elapsed().as_secs();
                if elapsed >= last_report + 10 {
                    eprintln!(
                        "cloud restore: completed={}, received={}, peer_received={}, elapsed={}s",
                        progress.completed, progress.received, progress.peer_received, elapsed
                    );
                    last_report = elapsed;
                }
                if progress.status == NATIVE_COMPLETE {
                    assert_eq!(progress.completed, download.size.unwrap());
                    assert_eq!(
                        std::fs::metadata(&output_path).unwrap().len(),
                        progress.completed
                    );
                    assert!(
                        peer_data,
                        "restored cloud task had no new accelerated resource bytes"
                    );
                    eprintln!(
                        "cloud restore complete: bytes={}, new_received={}, peer_received={}",
                        progress.completed, progress.received, progress.peer_received
                    );
                    api.delete(task_id).unwrap();
                    return;
                }
                assert!(
                    elapsed < 900,
                    "restored cloud download timed out: {progress:?}"
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        let paused_at = loop {
            let progress = api
                .progress(task_id)
                .expect("native progress should be readable");
            assert_ne!(
                progress.status, NATIVE_ERROR,
                "native task failed: {progress:?}"
            );
            if progress.completed >= 512 * 1024 {
                eprintln!(
                    "native live progress before pause: {}/{}, status={}, received={}",
                    progress.completed, progress.total, progress.status, progress.received
                );
                api.stop(task_id).expect("native task should pause");
                break progress.completed;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native download did not produce data"
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
        };
        let stop_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let stopped = loop {
            let progress = api.progress(task_id).unwrap();
            if progress.status == NATIVE_STOPPED {
                break progress;
            }
            assert!(
                std::time::Instant::now() < stop_deadline,
                "pause was not acknowledged"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        let resumed_task_id = api
            .create_p2sp_task(
                &download.url,
                &output_path,
                output_name,
                &output_directory,
                "https://pan.xunlei.com/",
                download.size.unwrap_or(0),
                &client.session().profile.user_agent,
                download.cid.as_deref().unwrap_or(""),
                download.gcid.as_deref().unwrap_or(""),
            )
            .expect("native task should reopen the existing breakpoint data");
        api.enable_vip_with_file_token(resumed_task_id, token)
            .expect("the file SVIP token should be restored before resume");
        api.start(resumed_task_id)
            .expect("recreated native task should resume");
        let resume_deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        let mut saw_restored_checkpoint = false;
        let resumed_at = loop {
            let progress = api
                .progress(resumed_task_id)
                .expect("resumed progress should be readable");
            assert_ne!(
                progress.status, NATIVE_ERROR,
                "resumed native task failed: {progress:?}"
            );
            if progress.total > 0 && !saw_restored_checkpoint {
                assert!(
                    progress.completed >= stopped.completed,
                    "resumed task discarded existing ranges"
                );
                saw_restored_checkpoint = true;
            }
            if progress.completed >= paused_at + 512 * 1024 && progress.peer_received > 0 {
                eprintln!(
                    "native live progress after resume: {}/{}, status={}, received={}",
                    progress.completed, progress.total, progress.status, progress.received
                );
                break progress.completed;
            }
            assert!(
                std::time::Instant::now() < resume_deadline,
                "native resume produced no current accelerated peer data"
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
        };
        assert!(resumed_at > paused_at);
        api.release_output(&output_path).unwrap();
        std::fs::write(
            root.join("verified-checkpoint-bytes"),
            resumed_at.to_string(),
        )
        .unwrap();
        let data_path = if output_path.is_file() {
            output_path.clone()
        } else {
            native_checkpoint_path(&output_path, ".xltd")
        };
        let mut prefix = vec![0_u8; 64 * 1024];
        let mut file = std::fs::File::open(data_path).unwrap();
        file.read_exact(&mut prefix).unwrap();
        assert!(
            prefix.iter().any(|byte| *byte != 0),
            "downloaded prefix must contain real data"
        );
        eprintln!(
            "native token accepted; restored checkpoint and new peer data verified (partial download only) in {}",
            root.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn configured_proxy_path_is_used_without_registry_access() {
        let root = tempfile::tempdir().unwrap();
        let proxy_path = root.path().join("DownloadSDKProxy.dll");
        let engine_path = root.path().join("DownloadSDK.dll");
        let server_path = root.path().join("DownloadSDKServer.exe");
        std::fs::write(&proxy_path, b"proxy fixture").unwrap();
        std::fs::write(&engine_path, b"engine fixture").unwrap();
        std::fs::write(&server_path, b"server fixture").unwrap();
        let plugin_config_path = root.path().join("xunlei-native.json");
        std::fs::write(
            &plugin_config_path,
            serde_json::json!({
                "sdkPath": proxy_path,
                "clientVersion": "25.1.13.1634"
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            configured_or_installed_sdk_path(Some(&plugin_config_path)),
            Some(proxy_path)
        );
        assert!(!is_download_sdk(&engine_path));
    }
}
