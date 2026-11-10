use std::{
    collections::{HashMap, HashSet},
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use crate::downloader::load_torrent_base64;
use crate::models::{CloudDirectoryGroup, CloudDirectoryGroupItem, QueueTaskPayload};
use crate::{
    cloud::{share::CloudShareConfirmation, CloudProvider, CredentialStore, SystemCredentialStore},
    confirm_download, duplicate_state, prepare_download, resolve_magnet_metadata,
    retry_duplicate_state, AiSettings, AiSettingsInput, AppSettings, DownloadAction,
    DownloadServiceError, DownloadTask, DuplicateDownloadState, LauncherVisibility, ParseOutcome,
    ParseResource, ProxySettings, TaskStatus, TaskStore, TransferFile, UriSubmissionOptions,
};

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParseDownloadOutcome {
    Task(ParseOutcome),
    ShareConfirmation(CloudShareConfirmation),
    AccessCodeRequired {
        provider: CloudProvider,
        message: &'static str,
    },
}

pub struct AppState {
    pub store: Arc<TaskStore>,
    pub runtime: Arc<crate::EngineRuntime>,
    pub settings: Mutex<AppSettings>,
    pub settings_path: PathBuf,
    pub ai_secrets: SystemCredentialStore,
    pub privacy_session: crate::privacy::PrivacySession,
    pub cloud_download_sources: crate::cloud::commands::CloudDownloadSourceGate,
    pub cloud_download_dispatch: crate::cloud::download_dispatch::CloudDownloadDispatch,
    pub baidu_range_gateway:
        Arc<tokio::sync::OnceCell<Arc<crate::cloud::baidu_range_gateway::BaiduRangeGateway>>>,
    pub xunlei_native_backend: std::sync::Arc<crate::xunlei_native::XunleiNativeBackend>,
}

#[derive(Clone, Serialize)]
pub struct DownloadTaskPayload {
    #[serde(flatten)]
    pub task: DownloadTask,
    pub files: Vec<TransferFile>,
    pub cleanup_pending: bool,
    /// A queue-safe action hint for callers that receive a task-action result
    /// before the next list projection arrives.
    pub reauthorization_required: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivacyTaskVisibility {
    pub enabled: bool,
    pub unlocked: bool,
}

pub(crate) fn privacy_task_visibility(state: &AppState) -> Result<PrivacyTaskVisibility, String> {
    let accepting_new_private_downloads = state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())?
        .privacy_download_enabled;
    // Disabling new private downloads must never unlock historical private
    // tasks. Keep their protection and unlock control available until every
    // private task has been removed.
    let enabled = accepting_new_private_downloads
        || state
            .store
            .has_private_tasks()
            .map_err(|error| error.to_string())?;
    Ok(PrivacyTaskVisibility {
        enabled,
        unlocked: enabled && state.privacy_session.is_unlocked(),
    })
}

/// Private task names must not cross the event boundary while the session is
/// locked. Fail closed if settings state cannot be read.
pub(crate) fn should_emit_task_update(state: &AppState, is_private: bool) -> bool {
    !is_private
        || privacy_task_visibility(state)
            .map(|visibility| visibility.unlocked)
            .unwrap_or(false)
}

pub(crate) fn ensure_private_task_access(
    state: &AppState,
    task: &DownloadTask,
) -> Result<(), String> {
    if task.is_private && !privacy_task_visibility(state)?.unlocked {
        return Err("隐私任务已锁定，请先解锁".into());
    }
    Ok(())
}

pub(crate) fn ensure_private_group_access(
    state: &AppState,
    group: &CloudDirectoryGroup,
) -> Result<(), String> {
    if group.is_private && !privacy_task_visibility(state)?.unlocked {
        return Err("隐私任务已锁定，请先解锁".into());
    }
    Ok(())
}

pub(crate) fn ensure_private_download_enabled(
    state: &AppState,
    private_download: bool,
) -> Result<(), String> {
    if private_download
        && !state
            .settings
            .lock()
            .map_err(|_| "设置锁不可用".to_string())?
            .privacy_download_enabled
    {
        return Err("请先在设置中开启隐私下载".into());
    }
    Ok(())
}

/// Persists a privacy conversion before producing the same queue projection
/// consumed by the task list. A cloud directory is one durable unit: selecting
/// either its root or any linked leaf converts the whole group.
fn set_task_private_with_visibility(
    store: &TaskStore,
    task_id: Uuid,
    is_private: bool,
    privacy_download_enabled: bool,
    privacy_unlocked: bool,
) -> Result<QueueTaskPayload, String> {
    let ensure_transition = |currently_private: bool| {
        if currently_private && !privacy_unlocked {
            return Err("隐私任务已锁定，请先解锁".to_string());
        }
        if !currently_private && is_private && !privacy_download_enabled {
            return Err("请先在设置中开启隐私下载".to_string());
        }
        Ok(())
    };

    if let Some(group) = store
        .cloud_directory_group(task_id)
        .map_err(|error| error.to_string())?
    {
        ensure_transition(group.is_private)?;
        store
            .set_cloud_directory_group_private(group.id, is_private)
            .map_err(|error| error.to_string())?;
        return cloud_directory_group_queue_task(store, group.id);
    }

    let task = store.find(task_id).map_err(|error| error.to_string())?;
    if let Some(group) = store
        .cloud_directory_group_for_task(task.id)
        .map_err(|error| error.to_string())?
    {
        ensure_transition(group.is_private)?;
        store
            .set_cloud_directory_group_private(group.id, is_private)
            .map_err(|error| error.to_string())?;
        return cloud_directory_group_queue_task(store, group.id);
    }

    ensure_transition(task.is_private)?;
    let task = store
        .set_task_private(task.id, is_private)
        .map_err(|error| error.to_string())?;
    queue_task_payload_from_task(store, task)
}

#[tauri::command]
pub fn set_task_private(
    task_id: String,
    is_private: bool,
    app: AppHandle,
) -> Result<QueueTaskPayload, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let state = app.state::<AppState>();
    let privacy_download_enabled = state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())?
        .privacy_download_enabled;
    let privacy_unlocked = privacy_task_visibility(&state)?.unlocked;
    let payload = set_task_private_with_visibility(
        &state.store,
        task_id,
        is_private,
        privacy_download_enabled,
        privacy_unlocked,
    )?;

    // A newly-private projection is never broadcast into a locked session.
    // The caller still receives the result for its immediate local update.
    emit_task_update(&app, TaskUpdatePayload::Queue(payload.clone()));
    Ok(payload)
}

#[derive(Clone, Serialize)]
pub struct TaskActionOutcome {
    #[serde(flatten)]
    pub payload: DownloadTaskPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate: Option<DuplicateDownloadState>,
    /// Present only when an action affects a durable cloud-directory group.
    /// Ordinary task and torrent action JSON remains unchanged.
    #[serde(rename = "queueTask", skip_serializing_if = "Option::is_none")]
    pub queue_task: Option<QueueTaskPayload>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudShareBatchFailure {
    pub selected_id: String,
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudShareBatchOutcome {
    pub outcomes: Vec<TaskActionOutcome>,
    pub failures: Vec<CloudShareBatchFailure>,
}

pub(crate) fn task_payload(
    store: &TaskStore,
    task: DownloadTask,
) -> Result<DownloadTaskPayload, String> {
    task_payload_with_cleanup(store, None, task)
}

pub(crate) fn task_payload_with_cleanup(
    store: &TaskStore,
    plans: Option<&crate::cloud::share::CloudSharePlanStore>,
    mut task: DownloadTask,
) -> Result<DownloadTaskPayload, String> {
    let cleanup_pending =
        if task.status == TaskStatus::Completed && task.source.starts_with("cloud-share://") {
            match plans {
                Some(plans) => plans
                    .cleanup_pending_for_task(&task.source, task.id)
                    .map_err(|_| "无法读取网盘转存清理状态".to_string())?,
                None => false,
            }
        } else {
            false
        };
    let files = store.files(task.id).map_err(|error| error.to_string())?;
    let reauthorization_required = crate::cloud::commands::requires_cloud_reauthorization_failure(
        task.failure_code.as_deref(),
    );
    redact_ftp_credentials(&mut task);
    Ok(DownloadTaskPayload {
        task,
        files,
        cleanup_pending,
        reauthorization_required,
    })
}

pub(crate) fn task_action_outcome(
    store: &TaskStore,
    task: DownloadTask,
    duplicate: Option<DuplicateDownloadState>,
) -> Result<TaskActionOutcome, String> {
    let queue_task = store
        .cloud_directory_group_for_task(task.id)
        .map_err(|error| error.to_string())?
        .map(|group| cloud_directory_group_queue_task(store, group.id))
        .transpose()?;
    Ok(TaskActionOutcome {
        payload: task_payload(store, task)?,
        duplicate,
        queue_task,
    })
}

fn share_action_parse_outcome(action: TaskActionOutcome) -> ParseDownloadOutcome {
    ParseDownloadOutcome::Task(ParseOutcome {
        task: action.payload.task,
        requires_confirmation: false,
        resources: Vec::new(),
        duplicate: action.duplicate,
    })
}

fn proxy_url(proxy: &ProxySettings) -> Result<Option<String>, String> {
    if !proxy.enabled {
        return Ok(None);
    }
    if !matches!(proxy.protocol.as_str(), "http" | "https") {
        return Err("aria2 仅支持 HTTP/HTTPS 代理".into());
    }
    if proxy.host.trim().is_empty() || proxy.host.contains("://") || proxy.port == 0 {
        return Err("代理服务器地址或端口无效".into());
    }
    let host = proxy.host.trim();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let mut value = url::Url::parse(&format!("{}://{}:{}", proxy.protocol, host, proxy.port))
        .map_err(|_| "代理服务器地址无效".to_string())?;
    if !proxy.username.is_empty() {
        value
            .set_username(&proxy.username)
            .map_err(|_| "代理用户名无效".to_string())?;
    }
    if !proxy.password.is_empty() {
        value
            .set_password(Some(&proxy.password))
            .map_err(|_| "代理密码无效".to_string())?;
    }
    Ok(Some(value.to_string()))
}

fn validated_download_directory(value: &str) -> Result<PathBuf, String> {
    let directory = PathBuf::from(value.trim());
    if directory.as_os_str().is_empty() || !directory.is_absolute() {
        return Err("下载目录必须是绝对路径".into());
    }
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建下载目录：{error}"))?;
    Ok(directory)
}

fn magnet_preview_task_directory_name(task_name: &str) -> String {
    #[cfg(windows)]
    {
        task_name
            .chars()
            .map(|character| {
                if character.is_control()
                    || matches!(
                        character,
                        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                    )
                {
                    '_'
                } else {
                    character
                }
            })
            .collect::<String>()
            .trim_end_matches(|character| character == ' ' || character == '.')
            .to_owned()
    }
    #[cfg(not(windows))]
    {
        task_name.to_owned()
    }
}

fn magnet_preview_file_path(
    download_directory: &Path,
    task_name: &str,
    metadata_path: &str,
    multi_file: bool,
) -> Result<PathBuf, String> {
    let unsafe_path = || "磁链文件路径不安全，无法创建任务".to_string();
    if !download_directory.is_absolute() {
        return Err(unsafe_path());
    }
    let relative = Path::new(metadata_path.trim());
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(unsafe_path());
    }

    let mut path = download_directory.to_path_buf();
    if multi_file {
        let torrent_root = PathBuf::from(magnet_preview_task_directory_name(task_name.trim()));
        if torrent_root.as_os_str().is_empty()
            || !torrent_root
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            || torrent_root.components().count() != 1
        {
            return Err(unsafe_path());
        }
        let original_root = Path::new(task_name.trim());
        if relative.starts_with(original_root) && !relative.starts_with(&torrent_root) {
            let suffix = relative
                .strip_prefix(original_root)
                .map_err(|_| unsafe_path())?;
            path.push(&torrent_root);
            path.push(suffix);
        } else if !relative.starts_with(&torrent_root) {
            path.push(torrent_root);
            path.push(relative);
        } else {
            path.push(relative);
        }
    } else {
        path.push(relative);
    }
    if !path.is_absolute()
        || !path.strip_prefix(download_directory).is_ok_and(|suffix| {
            suffix
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        })
    {
        return Err(unsafe_path());
    }
    Ok(path)
}

fn magnet_preview_relative_path(
    task_name: &str,
    stored_path: &Path,
    multi_file: bool,
) -> Result<PathBuf, String> {
    let unsafe_path = || "磁链文件路径不安全，无法创建任务".to_string();
    if !stored_path.is_absolute() {
        return stored_path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            .then(|| stored_path.to_path_buf())
            .ok_or_else(unsafe_path);
    }
    if !multi_file {
        return stored_path
            .file_name()
            .map(PathBuf::from)
            .ok_or_else(unsafe_path);
    }

    let components = stored_path.components().collect::<Vec<_>>();
    let original_root = task_name.trim();
    let normalized_root = magnet_preview_task_directory_name(original_root);
    let Some(root_index) = components.iter().rposition(|component| {
        matches!(component, Component::Normal(name) if {
            let name = name.to_string_lossy();
            name == original_root || name == normalized_root
        })
    }) else {
        return Err(unsafe_path());
    };
    let mut relative = PathBuf::new();
    for component in &components[root_index + 1..] {
        let Component::Normal(name) = component else {
            return Err(unsafe_path());
        };
        relative.push(name);
    }
    (!relative.as_os_str().is_empty())
        .then_some(relative)
        .ok_or_else(unsafe_path)
}

fn rebase_magnet_preview_files(
    store: &TaskStore,
    task: &DownloadTask,
    download_directory: &Path,
) -> Result<(), String> {
    if !matches!(task.kind.as_str(), "magnet" | "torrent") {
        return Ok(());
    }
    let files = store.files(task.id).map_err(|error| error.to_string())?;
    let multi_file = files.len() > 1;
    let rebased = files
        .into_iter()
        .map(|file| {
            let relative =
                magnet_preview_relative_path(&task.name, Path::new(&file.path), multi_file)?;
            let path = magnet_preview_file_path(
                download_directory,
                &task.name,
                &relative.to_string_lossy(),
                multi_file,
            )?;
            Ok((
                file.index,
                path.to_string_lossy().into_owned(),
                file.length,
                file.completed_length,
                file.selected,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    store
        .upsert_files(task.id, &rebased)
        .map_err(|error| error.to_string())
}

pub(crate) fn download_submission_options_for_directory(
    state: &AppState,
    destination_directory: Option<&str>,
) -> Result<UriSubmissionOptions, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())?;
    let directory = destination_directory
        .map(str::to_owned)
        .unwrap_or_else(|| settings.download_directory.clone());
    let proxy = proxy_url(&settings.proxy)?;
    drop(settings);
    Ok(UriSubmissionOptions {
        directory: Some(validated_download_directory(&directory)?),
        proxy,
        ..Default::default()
    })
}

pub(crate) fn download_submission_options(
    state: &AppState,
) -> Result<UriSubmissionOptions, String> {
    download_submission_options_for_directory(state, None)
}

fn fail_invalid_task_submission(store: &TaskStore, task: &DownloadTask) -> Result<(), String> {
    match crate::validate_task_source(&task.kind, &task.source) {
        Ok(()) => Ok(()),
        Err(error) => {
            let message = error.to_string();
            let _ = store.fail(task.id, message.clone());
            Err(message)
        }
    }
}

fn download_engine_submission_message(error: &DownloadServiceError) -> String {
    match error {
        DownloadServiceError::Input(error) => error.to_string(),
        DownloadServiceError::Aria2(crate::Aria2Error::HttpStatus(400)) => {
            "下载引擎拒绝请求（HTTP 400）".into()
        }
        DownloadServiceError::Aria2(crate::Aria2Error::Rpc(fault))
            if fault.code == -32602
                && ["uri", "magnet"]
                    .iter()
                    .any(|term| fault.message.to_ascii_lowercase().contains(term)) =>
        {
            "下载引擎拒绝该链接：链接格式无效".into()
        }
        _ => "下载引擎未能接受下载任务".into(),
    }
}

fn safe_engine_submission_message(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("http status 400") {
        return "下载引擎拒绝请求（HTTP 400）".into();
    }
    "下载引擎未能接受下载任务".into()
}

fn fail_engine_submission(
    store: &TaskStore,
    task: &DownloadTask,
    error: DownloadServiceError,
) -> String {
    let message = download_engine_submission_message(&error);
    let _ = store.fail(task.id, message.clone());
    message
}

fn fail_engine_submission_text(store: &TaskStore, task: &DownloadTask, error: String) -> String {
    let message = safe_engine_submission_message(&error);
    let _ = store.fail(task.id, message.clone());
    message
}

#[tauri::command]
pub fn scan_download_inputs(input: String) -> Vec<crate::ScannedInput> {
    crate::scan_download_inputs(&input)
}

const MAX_DROPPED_TORRENT_BYTES: usize = 16 * 1024 * 1024;

fn stage_dropped_torrent_file(
    data_directory: &Path,
    file_name: &str,
    contents_base64: &str,
) -> Result<PathBuf, String> {
    let name = Path::new(file_name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty() && name.to_ascii_lowercase().ends_with(".torrent"))
        .ok_or_else(|| "只能接收 .torrent 种子文件".to_string())?;
    let contents = STANDARD
        .decode(contents_base64)
        .map_err(|_| "种子文件内容无效".to_string())?;
    if contents.len() > MAX_DROPPED_TORRENT_BYTES {
        return Err("种子文件超过 16 MiB 安全限制".to_string());
    }
    let directory = data_directory
        .join("dropped-torrents")
        .join(Uuid::new_v4().to_string());
    fs::create_dir_all(&directory).map_err(|error| format!("无法暂存种子文件：{error}"))?;
    let path = directory.join(name);
    fs::write(&path, contents).map_err(|error| format!("无法暂存种子文件：{error}"))?;
    Ok(path)
}

/// WebView drag/drop intentionally exposes file bytes rather than the source
/// path so HTML text and link drops work consistently. Keep the selected
/// torrent in application data for the task's later confirmation/retry flow.
#[tauri::command]
pub fn stage_dropped_torrent(
    file_name: String,
    contents_base64: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let data_directory = state
        .settings_path
        .parent()
        .ok_or_else(|| "无法确定应用数据目录".to_string())?;
    let path = stage_dropped_torrent_file(data_directory, &file_name, &contents_base64)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Xunlei verification failures cross the IPC boundary as their stable reason
/// code so the frontend can localize them; everything else keeps its message.
pub(crate) fn parse_share_error_string(error: crate::cloud::QuarkError) -> String {
    match error {
        crate::cloud::QuarkError::XunleiVerificationUnavailable(reason) => reason.to_owned(),
        other => other.to_string(),
    }
}

#[tauri::command]
pub async fn parse_download(
    app: AppHandle,
    input: String,
    access_code: Option<String>,
    require_confirmation: Option<bool>,
    state: State<'_, AppState>,
) -> Result<ParseDownloadOutcome, String> {
    if let Some(candidate) =
        crate::parser::extract_cloud_share_candidate(&input, access_code.as_deref())
            .map_err(|error| error.to_string())?
    {
        let provider = candidate.provider;
        return match crate::cloud::commands::inspect_cloud_share(app.clone(), candidate)
            .await
            .map_err(parse_share_error_string)?
        {
            crate::cloud::share::ShareConfirmationOutcome::Confirmation(plan) => {
                Ok(ParseDownloadOutcome::ShareConfirmation(plan))
            }
            crate::cloud::share::ShareConfirmationOutcome::AutoSubmit {
                source,
                confirmation,
            } => {
                if require_confirmation.unwrap_or(false) {
                    return Ok(ParseDownloadOutcome::ShareConfirmation(confirmation));
                }
                let cloud = app
                    .try_state::<Arc<crate::cloud::commands::CloudState>>()
                    .ok_or_else(|| "网盘分享下载状态不可用，请重新解析分享链接".to_string())?;
                let selected_ids = confirmation
                    .resources
                    .iter()
                    .map(|resource| resource.id.clone())
                    .collect::<Vec<_>>();
                let selection = crate::cloud::share::selected_share_download(
                    &cloud.share_plans,
                    &confirmation.plan_source,
                    &selected_ids,
                )
                .map_err(|error| error.to_string())?;
                crate::cloud::commands::preflight_cloud_share_download(app.clone(), &selection)
                    .await
                    .map_err(|error| error.to_string())?;
                let configured = download_submission_options(&state)?;
                let action = crate::cloud::commands::submit_cloud_share_file(
                    app, &state, source, configured, false, None,
                )
                .await
                .map_err(|error| error.to_string())?;
                Ok(share_action_parse_outcome(action))
            }
            crate::cloud::share::ShareConfirmationOutcome::AccessCodeRequired => {
                Ok(ParseDownloadOutcome::AccessCodeRequired {
                    provider,
                    message: "此分享需要访问码，请输入后重新解析",
                })
            }
        };
    }
    let mut outcome = prepare_download(&input, &state.store).map_err(|error| error.to_string())?;
    if outcome.duplicate.is_some() {
        redact_ftp_credentials(&mut outcome.task);
        ensure_private_task_access(&state, &outcome.task)?;
        return Ok(ParseDownloadOutcome::Task(outcome));
    }
    if require_confirmation.unwrap_or(false) && !outcome.requires_confirmation {
        outcome.task = state
            .store
            .transition(outcome.task.id, TaskStatus::AwaitingUser)
            .map_err(|error| error.to_string())?;
        outcome.requires_confirmation = true;
    }
    if outcome.task.kind == "torrent" {
        let service = state
            .runtime
            .service()
            .await
            .map_err(|error| error.to_string())?;
        let options = download_submission_options(&state)?
            .into_aria2_options()
            .map_err(|error| error.to_string())?;
        let encoded = match load_torrent_base64(&outcome.task.source).await {
            Ok(encoded) => encoded,
            Err(error) => {
                let message = error.to_string();
                let _ = state.store.fail(outcome.task.id, message.clone());
                return Err(message);
            }
        };
        outcome.task = match service
            .submit_torrent(outcome.task.id, encoded, vec![], options)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                let message = fail_engine_submission(&state.store, &outcome.task, error);
                return Err(message);
            }
        };
        outcome.resources = parse_resources(&state.store, outcome.task.id)?;
    } else if !outcome.requires_confirmation {
        let options = download_submission_options(&state)?;
        outcome.task = match state
            .runtime
            .submit_uri_with_options(outcome.task.id, vec![outcome.task.source.clone()], options)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                return Err(fail_engine_submission_text(
                    &state.store,
                    &outcome.task,
                    error,
                ))
            }
        };
    } else if outcome.task.kind == "magnet" {
        let service = state
            .runtime
            .service()
            .await
            .map_err(|error| error.to_string())?;
        let options = download_submission_options(&state)?;
        let metadata_settings = state
            .settings
            .lock()
            .map_err(|_| "设置锁不可用".to_string())?
            .magnet_metadata
            .clone();
        if let Ok(Some(resolved)) =
            resolve_magnet_metadata(&metadata_settings, &outcome.task.source).await
        {
            if let Some(title) = resolved.title {
                outcome.task.name = title;
                outcome.task = state
                    .store
                    .apply_transfer_snapshot(outcome.task)
                    .map_err(|error| error.to_string())?;
            }
            if let Some(torrent) = resolved.torrent {
                outcome.task = service
                    .submit_torrent(
                        outcome.task.id,
                        torrent,
                        vec![],
                        options
                            .into_aria2_options()
                            .map_err(|error| error.to_string())?,
                    )
                    .await
                    .map_err(|error| fail_engine_submission(&state.store, &outcome.task, error))?;
            } else {
                let download_directory = options
                    .directory
                    .as_deref()
                    .ok_or_else(|| "磁链任务缺少下载目录".to_string())?;
                let multi_file = resolved.files.len() > 1;
                let preview_files = resolved
                    .files
                    .iter()
                    .map(|file| {
                        magnet_preview_file_path(
                            download_directory,
                            &outcome.task.name,
                            &file.path,
                            multi_file,
                        )
                        .map(|path| {
                            (
                                file.index,
                                path.to_string_lossy().into_owned(),
                                file.length,
                                0,
                                true,
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                state
                    .store
                    .upsert_files(outcome.task.id, &preview_files)
                    .map_err(|error| error.to_string())?;
                outcome.task = service
                    .prepare_magnet_with_options(
                        outcome.task.id,
                        outcome.task.source.clone(),
                        options,
                    )
                    .await
                    .map_err(|error| fail_engine_submission(&state.store, &outcome.task, error))?;
                if preview_files.is_empty() {
                    match service
                        .wait_for_metadata(outcome.task.id, std::time::Duration::from_secs(12))
                        .await
                    {
                        Ok(task) => outcome.task = task,
                        Err(DownloadServiceError::MetadataTimeout) => {}
                        Err(error) => {
                            return Err(fail_engine_submission(&state.store, &outcome.task, error))
                        }
                    }
                }
            }
        } else {
            outcome.task = service
                .prepare_magnet_with_options(outcome.task.id, outcome.task.source.clone(), options)
                .await
                .map_err(|error| fail_engine_submission(&state.store, &outcome.task, error))?;
            match service
                .wait_for_metadata(outcome.task.id, std::time::Duration::from_secs(12))
                .await
            {
                Ok(task) => outcome.task = task,
                Err(DownloadServiceError::MetadataTimeout) => {}
                Err(error) => {
                    return Err(fail_engine_submission(&state.store, &outcome.task, error))
                }
            }
        }
        outcome.resources = parse_resources(&state.store, outcome.task.id)?;
    }
    redact_ftp_credentials(&mut outcome.task);
    Ok(ParseDownloadOutcome::Task(outcome))
}

fn prepared_task_can_be_discarded(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Draft | TaskStatus::Parsing | TaskStatus::AwaitingUser
    )
}

#[tauri::command]
pub async fn discard_prepared_download(
    task_id: String,
    state: State<'_, AppState>,
    cloud: State<'_, Arc<crate::cloud::commands::CloudState>>,
) -> Result<(), String> {
    if task_id.starts_with("cloud-share://") {
        return cloud
            .share_plans
            .discard_plan(&task_id)
            .map_err(|error| error.to_string());
    }
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let task = state.store.find(id).map_err(|error| error.to_string())?;
    ensure_private_task_access(&state, &task)?;
    if !prepared_task_can_be_discarded(task.status) {
        return Err("只能清理尚未提交的下载任务".to_string());
    }
    if task.aria_gid.is_some() {
        let service = state
            .runtime
            .service()
            .await
            .map_err(|error| error.to_string())?;
        service
            .control(id, DownloadAction::Cancel)
            .await
            .map_err(|error| error.to_string())?;
    }
    state
        .store
        .remove_task_record(id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn confirm_cloud_share_download(
    app: AppHandle,
    plan_source: String,
    selected_ids: Vec<String>,
    destination_directory: Option<String>,
    private_download: Option<bool>,
    post_processing_note: Option<String>,
    on_created: tauri::ipc::Channel<QueueTaskPayload>,
    state: State<'_, AppState>,
) -> Result<CloudShareBatchOutcome, crate::cloud::QuarkError> {
    let cloud = app
        .try_state::<Arc<crate::cloud::commands::CloudState>>()
        .ok_or(crate::cloud::QuarkError::Storage)?;
    let selection = crate::cloud::share::selected_share_download(
        &cloud.share_plans,
        &plan_source,
        &selected_ids,
    )?;
    let configured =
        download_submission_options_for_directory(&state, destination_directory.as_deref())
            .map_err(crate::cloud::QuarkError::InvalidInput)?;
    let private_download = private_download.unwrap_or(false);
    ensure_private_download_enabled(&state, private_download)
        .map_err(crate::cloud::QuarkError::InvalidInput)?;
    crate::cloud::commands::preflight_cloud_share_download(app.clone(), &selection).await?;
    if selection.directory_name.is_some() {
        let plan = crate::cloud::commands::plan_cloud_share_directory_download(&selection)?;
        let store = Arc::clone(&state.store);
        let note_store = Arc::clone(&state.store);
        let root_note = post_processing_note.clone();
        let provider = selection.provider;
        let group_source = selection.plan_source.clone();
        let submit_app = app.clone();
        let root_event_app = app.clone();
        let outcome = crate::cloud::commands::submit_cloud_directory_plan_with_group_event(
            &store,
            provider,
            &group_source,
            plan,
            configured,
            private_download,
            move |mut root| {
                let root_id =
                    Uuid::parse_str(&root.id).map_err(|_| crate::cloud::QuarkError::Storage)?;
                let group = note_store
                    .set_cloud_directory_group_post_processing_note(root_id, root_note.as_deref())
                    .map_err(|_| crate::cloud::QuarkError::Storage)?;
                root.post_processing_note = group.post_processing_note;
                let _ = on_created.send(root.clone());
                emit_task_update(&root_event_app, TaskUpdatePayload::Queue(root));
                Ok(())
            },
            move |_, source, options, context| {
                let app = submit_app.clone();
                async move {
                    let state = app.state::<AppState>();
                    crate::cloud::commands::submit_cloud_share_directory_leaf(
                        app.clone(),
                        &state,
                        source,
                        options,
                        context,
                        private_download,
                    )
                    .await
                }
            },
        )
        .await?;
        let mut result = CloudShareBatchOutcome {
            outcomes: vec![outcome.task],
            failures: Vec::new(),
        };
        if private_download {
            mark_cloud_share_outcomes_private(&state.store, &mut result)?;
        }
        return Ok(result);
    }
    let selections = selection
        .entries
        .into_iter()
        .map(|entry| (entry.selected_id, entry.source))
        .collect();
    let state = &*state;
    let mut result = submit_cloud_share_batch_with(selections, |source| {
        let app = app.clone();
        let configured = configured.clone();
        let on_created = on_created.clone();
        let post_processing_note = post_processing_note.clone();
        async move {
            let outcome = crate::cloud::commands::submit_cloud_share_file(
                app,
                state,
                source,
                configured,
                private_download,
                None,
            )
            .await?;
            let duplicate = outcome.duplicate;
            let task = state
                .store
                .set_post_processing_note(outcome.payload.task.id, post_processing_note.as_deref())
                .map_err(|_| crate::cloud::QuarkError::Storage)?;
            let updated = task_action_outcome(&state.store, task, duplicate)
                .map_err(crate::cloud::QuarkError::Transport)?;
            let task = match updated.queue_task.clone() {
                Some(task) => task,
                None => queue_task_payload_from_task(&state.store, updated.payload.task.clone())
                    .map_err(crate::cloud::QuarkError::Transport)?,
            };
            let _ = on_created.send(task);
            Ok(updated)
        }
    })
    .await;
    if private_download {
        mark_cloud_share_outcomes_private(&state.store, &mut result)?;
    }
    Ok(result)
}

fn mark_cloud_share_outcomes_private(
    store: &TaskStore,
    result: &mut CloudShareBatchOutcome,
) -> Result<(), crate::cloud::QuarkError> {
    for outcome in &mut result.outcomes {
        let id = outcome.payload.task.id;
        if let Some(group) = store
            .cloud_directory_group(id)
            .map_err(|_| crate::cloud::QuarkError::Storage)?
        {
            store
                .set_cloud_directory_group_private(group.id, true)
                .map_err(|_| crate::cloud::QuarkError::Storage)?;
            outcome.payload.task.is_private = true;
            outcome.queue_task = Some(
                cloud_directory_group_queue_task(store, group.id)
                    .map_err(crate::cloud::QuarkError::Transport)?,
            );
        } else {
            let task = store
                .set_task_private(id, true)
                .map_err(|_| crate::cloud::QuarkError::Storage)?;
            outcome.payload =
                task_payload(store, task).map_err(crate::cloud::QuarkError::Transport)?;
        }
    }
    Ok(())
}

async fn submit_cloud_share_batch_with<F, Fut>(
    selections: Vec<(String, String)>,
    mut submit: F,
) -> CloudShareBatchOutcome
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<TaskActionOutcome, crate::cloud::QuarkError>>,
{
    let mut result = CloudShareBatchOutcome {
        outcomes: Vec::new(),
        failures: Vec::new(),
    };
    for (selected_id, source) in selections {
        match submit(source).await {
            Ok(outcome) => result.outcomes.push(outcome),
            Err(error) => result.failures.push(CloudShareBatchFailure {
                selected_id,
                code: crate::cloud::commands::cloud_failure_code(&error),
                message: crate::cloud::commands::share_resolution_failure_message(&error),
            }),
        }
    }
    result
}

fn parse_resources(store: &TaskStore, task_id: Uuid) -> Result<Vec<ParseResource>, String> {
    Ok(store
        .files(task_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|file| ParseResource {
            id: file.index.to_string(),
            index: file.index,
            name: file.path,
            size: format!("{} B", file.length),
            selected: file.selected,
        })
        .collect())
}

#[tauri::command]
pub async fn confirm_download_command(
    task_id: String,
    selected_ids: Vec<String>,
    destination_directory: Option<String>,
    private_download: Option<bool>,
    post_processing_note: Option<String>,
    state: State<'_, AppState>,
) -> Result<DownloadTaskPayload, String> {
    let private_download = private_download.unwrap_or(false);
    ensure_private_download_enabled(&state, private_download)?;
    let service = state
        .runtime
        .service()
        .await
        .map_err(|error| error.to_string())?;
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let current = state.store.find(id).map_err(|error| error.to_string())?;
    ensure_private_task_access(&state, &current)?;
    fail_invalid_task_submission(&state.store, &current)?;
    state
        .store
        .set_private(id, private_download)
        .map_err(|error| error.to_string())?;
    let current = state
        .store
        .set_post_processing_note(id, post_processing_note.as_deref())
        .map_err(|error| error.to_string())?;
    let options =
        download_submission_options_for_directory(&state, destination_directory.as_deref())?;
    if current.aria_gid.is_some() {
        let indices = selected_ids
            .into_iter()
            .map(|value| value.parse::<u32>().map_err(|_| "文件索引无效".to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let download_directory = options
            .directory
            .clone()
            .ok_or_else(|| "磁链任务缺少下载目录".to_string())?;
        rebase_magnet_preview_files(&state.store, &current, &download_directory)?;
        let task = service
            .select_files_with_options(id, indices, options)
            .await
            .map_err(|error| fail_engine_submission(&state.store, &current, error))?;
        return task_payload(&state.store, task);
    }
    if let Some(error) = unbound_torrent_confirmation_error(&current) {
        return Err(error.to_string());
    }
    let task = confirm_download(id, &state.store).map_err(|error| error.to_string())?;
    let task = service
        .submit_uri_with_options(task.id, vec![task.source.clone()], options)
        .await
        .map_err(|error| fail_engine_submission(&state.store, &task, error))?;
    task_payload(&state.store, task)
}

fn unbound_torrent_confirmation_error(task: &DownloadTask) -> Option<&'static str> {
    (task.kind == "torrent" && task.aria_gid.is_none())
        .then_some("种子文件尚未完成解析，请重新添加任务")
}

#[tauri::command]
pub async fn list_tasks(
    state: State<'_, AppState>,
    app: AppHandle,
    scope: Option<String>,
) -> Result<Vec<QueueTaskPayload>, String> {
    let cloud = app
        .try_state::<Arc<crate::cloud::commands::CloudState>>()
        .map(|cloud| Arc::clone(cloud.inner()));
    let visibility = privacy_task_visibility(&state)?;
    let include_private = visibility.unlocked;
    list_queue_tasks_in_background(Arc::clone(&state.store), cloud, scope, include_private).await
}

async fn list_queue_tasks_in_background(
    store: Arc<TaskStore>,
    cloud: Option<Arc<crate::cloud::commands::CloudState>>,
    scope: Option<String>,
    include_private: bool,
) -> Result<Vec<QueueTaskPayload>, String> {
    // SQLite and the encrypted share journal can block. Keep the complete
    // projection off the window thread, including focus-triggered refreshes.
    tauri::async_runtime::spawn_blocking(move || {
        // Restore share-cleanup messages before reading the same projected list that
        // the command returns. The reusable helper below deliberately has no
        // application-state dependency, so the durable directory grouping boundary
        // can be exercised outside a running Tauri webview.
        if let Some(cloud) = cloud.as_ref() {
            let _ = cloud.share_plans.restore_cleanup_pending_messages(&store);
        }
        let mut tasks =
            list_queue_tasks_for_scope_with_privacy(&store, scope.as_deref(), include_private)?;
        if let Some(plans) = cloud.as_ref().map(|cloud| &cloud.share_plans) {
            apply_queue_cleanup_projection(&mut tasks, plans)?;
        }
        Ok(tasks)
    })
    .await
    .map_err(|error| format!("后台读取任务失败: {error}"))?
}

/// Returns the durable queue projection used by [`list_tasks`] for a scope.
/// Cloud-directory children are removed from the top level and nested beneath
/// their one synthetic folder root.
pub fn list_queue_tasks_for_scope(
    store: &TaskStore,
    scope: Option<&str>,
) -> Result<Vec<QueueTaskPayload>, String> {
    list_queue_tasks_for_scope_with_privacy(store, scope, true)
}

pub fn list_queue_tasks_for_scope_with_privacy(
    store: &TaskStore,
    scope: Option<&str>,
    include_private: bool,
) -> Result<Vec<QueueTaskPayload>, String> {
    let leaves = list_tasks_with_cleanup_projection_for_scope(store, None, None)?;
    let tasks = project_cloud_directory_groups(store, leaves, scope)?;
    Ok(if include_private {
        tasks
    } else {
        without_private_queue_tasks(tasks)
    })
}

fn without_private_queue_tasks(tasks: Vec<QueueTaskPayload>) -> Vec<QueueTaskPayload> {
    tasks
        .into_iter()
        .filter(|task| !task.is_private)
        .map(|mut task| {
            task.children = without_private_queue_tasks(task.children);
            task
        })
        .collect()
}

/// Produces the single durable queue root for one cloud-directory group.
/// This is intentionally derived through the same projection used by
/// `list_tasks`, so actions and task-update events cannot publish a stale or
/// flat representation of a directory member.
pub fn cloud_directory_group_queue_task(
    store: &TaskStore,
    group_id: Uuid,
) -> Result<QueueTaskPayload, String> {
    project_cloud_directory_groups_inner(store, Vec::new(), None, Some(group_id))?
        .into_iter()
        .find(|task| task.id == group_id.to_string())
        .ok_or_else(|| "网盘目录下载任务不存在".to_string())
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum TaskUpdatePayload {
    Queue(QueueTaskPayload),
    Task(DownloadTaskPayload),
}

impl TaskUpdatePayload {
    fn is_private(&self) -> bool {
        match self {
            Self::Queue(payload) => payload.is_private,
            Self::Task(payload) => payload.task.is_private,
        }
    }
}

/// Builds the event form of a durable cloud-directory root. Keeping this
/// conversion here prevents root events from bypassing the common event DTO.
pub(crate) fn task_update_payload_for_directory_root(
    store: &TaskStore,
    group_id: Uuid,
) -> Result<TaskUpdatePayload, String> {
    cloud_directory_group_queue_task(store, group_id).map(TaskUpdatePayload::Queue)
}

/// The sole `download-task-updated` event boundary. Every payload is wrapped
/// in `TaskUpdatePayload`, so ordinary tasks and recursive directory roots
/// retain their distinct projections while sharing privacy filtering and the
/// JSON serialization contract.
pub(crate) fn emit_task_update(app: &AppHandle, payload: TaskUpdatePayload) {
    let visible = app
        .try_state::<AppState>()
        .map(|state| should_emit_task_update(&state, payload.is_private()))
        .unwrap_or(!payload.is_private());
    if visible {
        let _ = app.emit("download-task-updated", payload);
    }
}

/// Projects an updated durable task before passing it through the sole event
/// boundary. Directory members automatically project their synthetic root.
pub(crate) fn emit_task_update_for_task(
    app: &AppHandle,
    store: &TaskStore,
    plans: Option<&crate::cloud::share::CloudSharePlanStore>,
    task: DownloadTask,
) -> Result<(), String> {
    let payload = task_update_payload(store, plans, task)?;
    emit_task_update(app, payload);
    Ok(())
}

/// Reprojects a directory root before emitting it, so callers cannot publish
/// a stale, flat, or leaf-shaped directory event.
pub(crate) fn emit_task_update_for_directory_root(
    app: &AppHandle,
    store: &TaskStore,
    group_id: Uuid,
) -> Result<(), String> {
    emit_task_update(
        app,
        task_update_payload_for_directory_root(store, group_id)?,
    );
    Ok(())
}

/// Selects the externally visible event representation for one task update.
/// A member of a cloud-directory group always updates its synthetic root.
pub(crate) fn task_update_payload(
    store: &TaskStore,
    plans: Option<&crate::cloud::share::CloudSharePlanStore>,
    task: DownloadTask,
) -> Result<TaskUpdatePayload, String> {
    if let Some(group) = store
        .cloud_directory_group_for_task(task.id)
        .map_err(|error| error.to_string())?
    {
        return cloud_directory_group_queue_task(store, group.id).map(TaskUpdatePayload::Queue);
    }
    task_payload_with_cleanup(store, plans, task).map(TaskUpdatePayload::Task)
}

fn apply_queue_cleanup_projection(
    tasks: &mut [QueueTaskPayload],
    plans: &crate::cloud::share::CloudSharePlanStore,
) -> Result<(), String> {
    fn collect(
        tasks: &[QueueTaskPayload],
        sources: &mut Vec<(String, Uuid)>,
    ) -> Result<(), String> {
        for task in tasks {
            if task.status == TaskStatus::Completed && task.source.starts_with("cloud-share://") {
                sources.push((
                    task.source.clone(),
                    Uuid::parse_str(&task.id).map_err(|_| "任务 ID 无效".to_string())?,
                ));
            }
            collect(&task.children, sources)?;
        }
        Ok(())
    }
    fn apply(tasks: &mut [QueueTaskPayload], pending: &HashMap<(String, String), bool>) {
        for task in tasks {
            if task.status == TaskStatus::Completed && task.source.starts_with("cloud-share://") {
                task.cleanup_pending = pending
                    .get(&(task.source.clone(), task.id.clone()))
                    .copied()
                    .unwrap_or(false);
            }
            apply(&mut task.children, pending);
        }
    }
    let mut sources = Vec::new();
    collect(tasks, &mut sources)?;
    let pending = plans
        .cleanup_pending_for_tasks(&sources)
        .map_err(|_| "无法读取网盘转存清理状态".to_string())?;
    let pending = sources
        .into_iter()
        .zip(pending)
        .map(|((source, id), pending)| ((source, id.to_string()), pending))
        .collect();
    apply(tasks, &pending);
    Ok(())
}

pub(crate) fn queue_task_payload_from_task(
    store: &TaskStore,
    mut task: DownloadTask,
) -> Result<QueueTaskPayload, String> {
    let files = store.files(task.id).map_err(|error| error.to_string())?;
    let reauthorization_required = crate::cloud::commands::requires_cloud_reauthorization_failure(
        task.failure_code.as_deref(),
    );
    redact_ftp_credentials(&mut task);
    Ok(QueueTaskPayload {
        id: task.id.to_string(),
        source: task.source,
        name: task.name,
        kind: task.kind,
        status: task.status,
        progress: task.progress,
        error: task.error,
        status_message: task.status_message,
        failure_code: task.failure_code,
        reauthorization_required,
        aria_gid: task.aria_gid,
        followed_by_gid: task.followed_by_gid,
        metadata_gid: task.metadata_gid,
        total_length: task.total_length,
        completed_length: task.completed_length,
        download_speed: task.download_speed,
        upload_speed: task.upload_speed,
        connections: task.connections,
        discovered_nodes: task.discovered_nodes,
        eta_seconds: task.eta_seconds,
        aria_error_code: task.aria_error_code,
        created_at: task.created_at,
        updated_at: task.updated_at,
        completed_at: task.completed_at,
        p2p_sharing: task.p2p_sharing,
        is_private: task.is_private,
        post_processing_note: task.post_processing_note,
        files,
        cleanup_pending: false,
        children: Vec::new(),
    })
}

fn task_is_active(status: TaskStatus) -> bool {
    !matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Recycled
    )
}

fn status_in_scope(status: TaskStatus, scope: Option<&str>) -> Result<bool, String> {
    match scope {
        None | Some("all") => Ok(true),
        Some("active") => Ok(task_is_active(status)),
        Some("terminal") => Ok(!task_is_active(status)),
        Some(_) => Err("任务列表范围无效".to_string()),
    }
}

fn virtual_cloud_directory_item_payload(
    group: &CloudDirectoryGroup,
    item: &CloudDirectoryGroupItem,
    error: Option<String>,
    failure_code: Option<String>,
) -> QueueTaskPayload {
    let status = if matches!(
        group.status,
        TaskStatus::Paused | TaskStatus::Cancelled | TaskStatus::Recycled
    ) {
        group.status
    } else if error.is_some() {
        TaskStatus::Failed
    } else {
        TaskStatus::Queued
    };
    let reauthorization_required =
        crate::cloud::commands::requires_cloud_reauthorization_failure(failure_code.as_deref());
    QueueTaskPayload {
        id: item.id.to_string(),
        source: format!(
            "cloud-directory-item://{}/{}",
            group.provider.key(),
            item.id
        ),
        name: item.output_name.clone(),
        kind: "file".to_string(),
        status,
        progress: 0,
        error,
        status_message: None,
        failure_code,
        reauthorization_required,
        aria_gid: None,
        followed_by_gid: None,
        metadata_gid: None,
        total_length: item.size,
        completed_length: 0,
        download_speed: 0,
        upload_speed: 0,
        connections: 0,
        discovered_nodes: 0,
        eta_seconds: None,
        aria_error_code: None,
        created_at: group.created_at,
        updated_at: group.updated_at,
        completed_at: None,
        p2p_sharing: false,
        is_private: group.is_private,
        post_processing_note: None,
        files: Vec::new(),
        cleanup_pending: false,
        children: Vec::new(),
    }
}

fn cloud_directory_folder_payload(
    group: &CloudDirectoryGroup,
    components: &[String],
) -> QueueTaskPayload {
    let encoded_path: String =
        url::form_urlencoded::byte_serialize(components.join("/").as_bytes()).collect();
    QueueTaskPayload {
        id: format!("group:{}:dir:{encoded_path}", group.id),
        source: format!("cloud-directory://{}/{}", group.provider.key(), group.id),
        name: components
            .last()
            .cloned()
            .unwrap_or_else(|| group.relative_directory.clone()),
        kind: "folder".to_string(),
        status: TaskStatus::Queued,
        progress: 0,
        error: None,
        status_message: None,
        failure_code: None,
        reauthorization_required: false,
        aria_gid: None,
        followed_by_gid: None,
        metadata_gid: None,
        total_length: None,
        completed_length: 0,
        download_speed: 0,
        upload_speed: 0,
        connections: 0,
        discovered_nodes: 0,
        eta_seconds: None,
        aria_error_code: None,
        created_at: group.created_at,
        updated_at: group.updated_at,
        completed_at: None,
        p2p_sharing: false,
        is_private: group.is_private,
        post_processing_note: None,
        files: Vec::new(),
        cleanup_pending: false,
        children: Vec::new(),
    }
}

fn linked_cloud_directory_item_payload(
    store: &TaskStore,
    item: &CloudDirectoryGroupItem,
    task: DownloadTask,
) -> Result<QueueTaskPayload, String> {
    let mut payload = queue_task_payload_from_task(store, task)?;
    if payload.total_length.is_none() {
        payload.total_length = selected_transfer_files_total_length(&payload.files)
            .or(item.size)
            .or_else(|| {
                (payload.status == TaskStatus::Completed)
                    .then(|| completed_cloud_directory_item_file_size(store, item))
                    .flatten()
            });
    }
    if payload.status == TaskStatus::Completed {
        // Older snapshots could retain a cloud-share failure after aria2 later
        // reported the same local task as complete. The authoritative local
        // completion wins when rendering the durable directory membership.
        payload.error = None;
        payload.failure_code = None;
        payload.aria_error_code = None;
        if let Some(total_length) = payload.total_length {
            payload.completed_length = total_length;
        }
    }
    Ok(payload)
}

fn selected_transfer_files_total_length(files: &[TransferFile]) -> Option<u64> {
    files
        .iter()
        .filter(|file| file.selected)
        .map(|file| file.length.max(file.completed_length))
        .reduce(u64::saturating_add)
        .filter(|total_length| *total_length > 0)
}

fn completed_cloud_directory_item_file_size(
    store: &TaskStore,
    item: &CloudDirectoryGroupItem,
) -> Option<u64> {
    let group = store.cloud_directory_group(item.group_id).ok()??;
    let (root, descendants) = item.relative_directory.split_first()?;
    if root != &group.relative_directory {
        return None;
    }
    let mut output_path = cloud_directory_group_output_root(&group).ok()?;
    for component in descendants.iter().chain(std::iter::once(&item.output_name)) {
        let mut path_components = Path::new(component).components();
        let Some(Component::Normal(component)) = path_components.next() else {
            return None;
        };
        if path_components.next().is_some() {
            return None;
        }
        output_path.push(component);
    }
    let metadata = fs::metadata(output_path).ok()?;
    metadata.is_file().then_some(metadata.len())
}

fn insert_cloud_directory_leaf(
    group: &CloudDirectoryGroup,
    parent: &mut QueueTaskPayload,
    components: &[String],
    prefix: &mut Vec<String>,
    leaf: QueueTaskPayload,
) {
    let Some((component, rest)) = components.split_first() else {
        parent.children.push(leaf);
        return;
    };
    prefix.push(component.clone());
    let encoded: String =
        url::form_urlencoded::byte_serialize(prefix.join("/").as_bytes()).collect();
    let folder_id = format!("group:{}:dir:{encoded}", group.id);
    let index = parent
        .children
        .iter()
        .position(|child| child.id == folder_id)
        .unwrap_or_else(|| {
            parent
                .children
                .push(cloud_directory_folder_payload(group, prefix));
            parent.children.len() - 1
        });
    insert_cloud_directory_leaf(group, &mut parent.children[index], rest, prefix, leaf);
    prefix.pop();
}

fn aggregate_cloud_directory_root(
    group: &CloudDirectoryGroup,
    children: Vec<QueueTaskPayload>,
) -> QueueTaskPayload {
    let mut root = cloud_directory_folder_payload(group, &[]);
    root.id = group.id.to_string();
    root.post_processing_note = group.post_processing_note.clone();
    root.children = children;
    aggregate_cloud_directory_folder(group, root)
}

fn aggregate_cloud_directory_folder(
    group: &CloudDirectoryGroup,
    folder: QueueTaskPayload,
) -> QueueTaskPayload {
    // Every directory, not just the queue root, derives its transfer state
    // from all descendant files. Keep directory identities stable across updates.
    let children: Vec<_> = folder
        .children
        .into_iter()
        .map(|child| {
            if child.kind == "folder" {
                aggregate_cloud_directory_folder(group, child)
            } else {
                child
            }
        })
        .collect();
    fn collect_leaves<'a>(node: &'a QueueTaskPayload, leaves: &mut Vec<&'a QueueTaskPayload>) {
        if node.children.is_empty() {
            leaves.push(node);
        } else {
            for child in &node.children {
                collect_leaves(child, leaves);
            }
        }
    }
    let mut leaves = Vec::new();
    for child in &children {
        collect_leaves(child, &mut leaves);
    }
    // A directory group is a virtual queue root.  Once every real download
    // member has been recycled, it must move with those members even though a
    // retained virtual failure can still be shown below it.
    let linked_leaves = leaves
        .iter()
        .filter(|item| !item.source.starts_with("cloud-directory-item://"))
        .collect::<Vec<_>>();
    let all_linked_recycled = !linked_leaves.is_empty()
        && linked_leaves
            .iter()
            .all(|item| item.status == TaskStatus::Recycled);
    let has_active = leaves.iter().any(|item| task_is_active(item.status));
    let has_failure = leaves.iter().any(|item| {
        matches!(
            item.status,
            TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Recycled
        )
    });
    let all_completed = !leaves.is_empty()
        && leaves
            .iter()
            .all(|item| item.status == TaskStatus::Completed);
    let status = if group.status == TaskStatus::Recycled || all_linked_recycled {
        TaskStatus::Recycled
    } else if group.status == TaskStatus::Cancelled {
        // Cancellation is durable user intent. Native/provider cleanup may
        // still be finishing in the background, but an active child must not
        // make the cancelled directory appear queued or downloading again.
        TaskStatus::Cancelled
    } else if group.status == TaskStatus::Paused && !all_completed {
        TaskStatus::Paused
    } else if has_active {
        // A preparing/queued child is part of the directory's submission
        // pipeline, not the lifecycle of the aggregate task. Pick by activity,
        // never by filename/order; keep a running directory stable between files.
        if leaves
            .iter()
            .any(|item| item.status == TaskStatus::Downloading)
            || (group.status != TaskStatus::Cancelled
                && !linked_leaves.is_empty()
                && leaves
                    .iter()
                    .any(|item| matches!(item.status, TaskStatus::Parsing | TaskStatus::Queued)))
        {
            TaskStatus::Downloading
        } else {
            [
                TaskStatus::Processing,
                TaskStatus::Parsing,
                TaskStatus::AwaitingUser,
                TaskStatus::Draft,
                TaskStatus::Paused,
                TaskStatus::Queued,
            ]
            .into_iter()
            .find(|status| leaves.iter().any(|item| item.status == *status))
            .unwrap_or(TaskStatus::Queued)
        }
    } else if all_completed {
        TaskStatus::Completed
    } else if has_failure {
        TaskStatus::Failed
    } else {
        TaskStatus::Failed
    };
    let has_known_total = leaves.iter().any(|item| item.total_length.is_some());
    let total_length = leaves
        .iter()
        .filter_map(|item| item.total_length)
        .sum::<u64>();
    let completed_length = leaves
        .iter()
        .filter(|item| item.total_length.is_some())
        .map(|item| item.completed_length)
        .sum::<u64>();
    let progress = if all_completed {
        100
    } else if total_length == 0 {
        0
    } else {
        // Bytes alone cannot confirm completion: unknown-size files or
        // finalization may still be pending after all known bytes arrive.
        ((completed_length.saturating_mul(100) / total_length).min(99)) as u8
    };
    let updated_at = leaves
        .iter()
        .map(|item| item.updated_at)
        .max()
        .unwrap_or(group.updated_at)
        .max(group.updated_at);
    QueueTaskPayload {
        id: folder.id,
        source: folder.source,
        name: folder.name,
        kind: "folder".to_string(),
        status,
        progress,
        error: matches!(status, TaskStatus::Failed | TaskStatus::Paused)
            .then(|| {
                leaves
                    .iter()
                    .find(|item| item.status == status && item.error.is_some())
                    .and_then(|item| item.error.clone())
            })
            .flatten(),
        status_message: leaves
            .iter()
            .find(|item| item.status == status && item.status_message.is_some())
            .and_then(|item| item.status_message.clone()),
        failure_code: matches!(status, TaskStatus::Failed | TaskStatus::Paused)
            .then(|| {
                leaves
                    .iter()
                    .find(|item| item.status == status && item.failure_code.is_some())
                    .and_then(|item| item.failure_code.clone())
            })
            .flatten(),
        reauthorization_required: matches!(status, TaskStatus::Failed | TaskStatus::Paused)
            && leaves.iter().any(|item| item.reauthorization_required),
        aria_gid: None,
        followed_by_gid: None,
        metadata_gid: None,
        total_length: has_known_total.then_some(total_length),
        completed_length,
        download_speed: leaves.iter().map(|item| item.download_speed).sum(),
        upload_speed: leaves.iter().map(|item| item.upload_speed).sum(),
        connections: leaves.iter().map(|item| item.connections).sum(),
        discovered_nodes: 0,
        eta_seconds: leaves.iter().filter_map(|item| item.eta_seconds).max(),
        aria_error_code: None,
        created_at: group.created_at,
        updated_at,
        completed_at: (status == TaskStatus::Completed).then(|| {
            leaves
                .iter()
                .filter_map(|leaf| leaf.completed_at)
                .max()
                .unwrap_or(updated_at)
        }),
        p2p_sharing: false,
        is_private: group.is_private,
        post_processing_note: folder.post_processing_note,
        files: Vec::new(),
        cleanup_pending: false,
        children,
    }
}

fn legacy_share_directory_root(
    group: &CloudDirectoryGroup,
    items: &[CloudDirectoryGroupItem],
) -> Option<String> {
    if group.relative_directory
        != crate::cloud::share::provider_share_directory_name(group.provider)
    {
        return None;
    }
    let root = items.first()?.relative_directory.get(1)?;
    (!root.trim().is_empty()
        && items.iter().all(|item| {
            item.relative_directory.first() == Some(&group.relative_directory)
                && item.relative_directory.get(1) == Some(root)
        }))
    .then(|| root.to_owned())
}

pub(crate) fn project_cloud_directory_groups(
    store: &TaskStore,
    leaves: Vec<DownloadTask>,
    scope: Option<&str>,
) -> Result<Vec<QueueTaskPayload>, String> {
    project_cloud_directory_groups_inner(store, leaves, scope, None)
}

fn project_cloud_directory_groups_inner(
    store: &TaskStore,
    leaves: Vec<DownloadTask>,
    scope: Option<&str>,
    requested_group: Option<Uuid>,
) -> Result<Vec<QueueTaskPayload>, String> {
    let group_ids = if let Some(id) = requested_group {
        vec![id]
    } else {
        store
            .cloud_directory_groups()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|group| group.id)
            .collect()
    };
    let mut group_items = Vec::with_capacity(group_ids.len());
    let mut grouped_task_ids = HashSet::new();
    for group_id in group_ids {
        let Some((group, items, tasks)) = store
            .cloud_directory_snapshot(group_id)
            .map_err(|error| error.to_string())?
        else {
            continue;
        };
        grouped_task_ids.extend(items.iter().filter_map(|item| item.task_id));
        group_items.push((group, items, tasks));
    }
    let mut projected = leaves
        .into_iter()
        .filter(|task| !grouped_task_ids.contains(&task.id))
        .filter(|task| status_in_scope(task.status, scope).unwrap_or(false))
        .map(|task| queue_task_payload_from_task(store, task))
        .collect::<Result<Vec<_>, _>>()?;
    for (group, items, group_tasks) in group_items {
        let mut task_by_id = group_tasks
            .into_iter()
            .map(|task| (task.id, task))
            .collect::<HashMap<_, _>>();
        let legacy_share_root = legacy_share_directory_root(&group, &items);
        let mut root = cloud_directory_folder_payload(&group, &[]);
        root.id = group.id.to_string();
        root.name = legacy_share_root
            .clone()
            .unwrap_or_else(|| group.relative_directory.clone());
        for item in items {
            let leaf = match item.task_id {
                Some(task_id) => match task_by_id.remove(&task_id) {
                    Some(task) => linked_cloud_directory_item_payload(store, &item, task)?,
                    None => virtual_cloud_directory_item_payload(
                        &group,
                        &item,
                        Some("关联的下载任务不存在，需重新提交网盘文件".to_string()),
                        None,
                    ),
                },
                None => virtual_cloud_directory_item_payload(
                    &group,
                    &item,
                    item.failure_message.clone(),
                    item.failure_code.clone(),
                ),
            };
            // Planned paths include the selected directory itself because that
            // component is needed for the local output path. The synthetic
            // group root already represents it in the queue, so omit exactly
            // that leading component from its visible child tree.
            let visible_components = item
                .relative_directory
                .strip_prefix(&[group.relative_directory.clone()])
                .unwrap_or(&item.relative_directory);
            let visible_components = legacy_share_root
                .as_ref()
                .and_then(|root| visible_components.strip_prefix(std::slice::from_ref(root)))
                .unwrap_or(visible_components);
            insert_cloud_directory_leaf(
                &group,
                &mut root,
                visible_components,
                &mut Vec::new(),
                leaf,
            );
        }
        let mut root = aggregate_cloud_directory_root(&group, root.children);
        if let Some(root_name) = legacy_share_root {
            root.name = root_name;
        }
        if status_in_scope(root.status, scope)? {
            projected.push(root);
        }
    }
    Ok(projected)
}

fn list_tasks_with_cleanup_projection_for_scope(
    store: &TaskStore,
    plans: Option<&crate::cloud::share::CloudSharePlanStore>,
    scope: Option<&str>,
) -> Result<Vec<DownloadTask>, String> {
    // Listing remains available if credentials are unavailable. The projection
    // only reads the journal; even an Attempted record never triggers deletion.
    if let Some(plans) = plans {
        let _ = plans.restore_cleanup_pending_messages(store);
    }
    match scope {
        None | Some("all") => store.list(),
        Some("active") => store.list_active(),
        Some("terminal") => store.list_terminal(),
        Some(_) => return Err("任务列表范围无效".to_string()),
    }
    .map_err(|error| error.to_string())
}

#[cfg(test)]
pub(crate) fn list_tasks_with_cleanup_projection(
    store: &TaskStore,
    plans: Option<&crate::cloud::share::CloudSharePlanStore>,
) -> Result<Vec<DownloadTask>, String> {
    list_tasks_with_cleanup_projection_for_scope(store, plans, None)
}

fn force_cancel_unsubmitted_task(
    store: &TaskStore,
    mut task: DownloadTask,
) -> Result<DownloadTask, String> {
    if task.aria_gid.is_some()
        || !matches!(
            task.status,
            TaskStatus::Draft
                | TaskStatus::Parsing
                | TaskStatus::AwaitingUser
                | TaskStatus::Queued
                | TaskStatus::Downloading
                | TaskStatus::Processing
                | TaskStatus::Paused
        )
    {
        return Err("当前任务不能强制取消".to_string());
    }
    task.status = TaskStatus::Cancelled;
    task.download_speed = 0;
    task.upload_speed = 0;
    task.eta_seconds = None;
    store
        .apply_transfer_snapshot(task)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn task_action(
    task_id: String,
    action: String,
    app: AppHandle,
) -> Result<TaskActionOutcome, String> {
    if task_id.starts_with("group:") {
        return Err("目录节点仅用于展开，不能执行下载操作".to_string());
    }
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let state = app.state::<AppState>();
    if let Some(group) = state
        .store
        .cloud_directory_group(id)
        .map_err(|error| error.to_string())?
    {
        ensure_private_group_access(&state, &group)?;
        return run_cloud_directory_group_action(app.clone(), &state, &group, &action).await;
    }
    for group in state
        .store
        .cloud_directory_groups()
        .map_err(|error| error.to_string())?
    {
        if let Some(item) = state
            .store
            .cloud_directory_group_items(group.id)
            .map_err(|error| error.to_string())?
            .iter()
            .find(|item| item.id == id && item.task_id.is_none())
        {
            ensure_private_group_access(&state, &group)?;
            return match action.as_str() {
                "retry" => crate::cloud::commands::retry_cloud_directory_group_item(
                    app.clone(),
                    group.id,
                    item.id,
                )
                .await
                .map_err(|error| error.to_string()),
                _ => Err("未入队的网盘目录文件不能执行此操作".to_string()),
            };
        }
    }
    run_local_task_action(task_id, action, app).await
}

async fn run_cloud_directory_group_action(
    app: AppHandle,
    state: &AppState,
    group: &CloudDirectoryGroup,
    action: &str,
) -> Result<TaskActionOutcome, String> {
    ensure_private_group_access(state, group)?;
    if action == "cancel" {
        return crate::cloud::commands::cancel_cloud_directory_group_with(
            &state.store,
            group.id,
            |id| {
                let app = app.clone();
                async move {
                    run_local_task_action(id.to_string(), "cancel".to_string(), app)
                        .await
                        .map(|_| ())
                }
            },
        )
        .await;
    }
    if !matches!(action, "pause" | "resume" | "cancel" | "retry") {
        return Err("当前任务不能执行此操作".to_string());
    }
    if matches!(action, "pause" | "resume") {
        state
            .store
            .set_cloud_directory_paused(group.id, action == "pause")
            .map_err(|error| error.to_string())?;
        emit_task_update_for_directory_root(&app, &state.store, group.id)?;
        let items = state
            .store
            .cloud_directory_group_items(group.id)
            .map_err(|e| e.to_string())?;
        for id in items.iter().filter_map(|item| item.task_id) {
            if action == "pause" {
                let _ = state
                    .cloud_download_dispatch
                    .control_unsubmitted(&state.store, id, DownloadAction::Pause)
                    .await;
            }
        }
        let targets = items
            .iter()
            .filter_map(|item| item.task_id)
            .map(|id| state.store.find(id))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|task| task.aria_gid.is_some() && task_is_active(task.status));
        // Dispatch every interrupt before waiting for any child's reconciliation.
        let targets = targets.collect::<Vec<_>>();
        let (native_targets, aria_targets): (Vec<_>, Vec<_>) = targets
            .into_iter()
            .partition(|task| crate::xunlei_native::is_native_gid(task.aria_gid.as_deref()));
        let control = if action == "pause" {
            DownloadAction::Pause
        } else {
            DownloadAction::Resume
        };
        let native_action = action.to_owned();
        let mut errors = futures_util::future::join_all(native_targets.into_iter().map(|task| {
            run_local_task_action(task.id.to_string(), native_action.clone(), app.clone())
        }))
        .await
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
        if !aria_targets.is_empty() {
            let service = state.runtime.service().await.map_err(|e| e.to_string())?;
            errors.extend(
                futures_util::future::join_all(
                    aria_targets
                        .iter()
                        .map(|task| service.control_directory_member(task.id, control)),
                )
                .await
                .into_iter()
                .filter_map(Result::err)
                .map(|error| error.to_string()),
            );
        }
        if action == "resume" {
            let app = app.clone();
            let group_id = group.id;
            tauri::async_runtime::spawn(async move {
                let state = app.state::<AppState>();
                // Linked preparations can survive an application exit without
                // ever receiving a GID. resubmit_task waits for any live resolver
                // and rechecks the record before doing provider work.
                for id in items.iter().filter_map(|item| item.task_id) {
                    if let Ok(task) = state.store.find(id) {
                        if task.aria_gid.is_none()
                            && matches!(
                                task.status,
                                TaskStatus::Parsing | TaskStatus::Paused | TaskStatus::Queued
                            )
                        {
                            if let Err(error) = resubmit_task(
                                app.clone(),
                                &state,
                                task,
                                UriSubmissionOptions {
                                    continue_download: true,
                                    ..Default::default()
                                },
                            )
                            .await
                            {
                                if let Ok(current) = state.store.find(id) {
                                    let still_running = state
                                        .store
                                        .cloud_directory_group(group_id)
                                        .ok()
                                        .flatten()
                                        .is_some_and(|group| {
                                            !matches!(
                                                group.status,
                                                TaskStatus::Paused
                                                    | TaskStatus::Cancelled
                                                    | TaskStatus::Recycled
                                            )
                                        });
                                    if still_running
                                        && current.aria_gid.is_none()
                                        && matches!(
                                            current.status,
                                            TaskStatus::Parsing | TaskStatus::Paused
                                        )
                                    {
                                        let _ = state.store.fail(id, &error);
                                    }
                                }
                            }
                        }
                    }
                }
                if items.iter().any(|item| item.task_id.is_none()) {
                    let _ =
                        crate::cloud::commands::retry_cloud_directory_group(app.clone(), group_id)
                            .await;
                }
                let _ = emit_task_update_for_directory_root(&app, &state.store, group_id);
            });
        }
        if action == "resume" && group.provider == CloudProvider::Quark {
            // Existing directory members resume their current GIDs without
            // returning through URL submission. Wake Quark's private
            // coordinator after those GIDs are unpaused so it reapplies the
            // current connection budget after a long pause.
            crate::cloud::activate_quark_download_scheduler(app.clone());
        }
        if let Some(error) = errors.into_iter().next() {
            return Err(format!("部分目录文件操作失败，请重试：{error}"));
        }
        return crate::cloud::commands::cloud_directory_group_task_outcome(&state.store, group)
            .map_err(|error| error.to_string());
    }
    let items = state
        .store
        .cloud_directory_group_items(group.id)
        .map_err(|error| error.to_string())?;
    let mut directory_retry =
        if action == "retry" && items.iter().any(|item| item.task_id.is_none()) {
            Some(
                crate::cloud::commands::retry_cloud_directory_group(app.clone(), group.id)
                    .await
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
    let mut first_outcome = None;
    for item in items {
        let Some(task_id) = item.task_id else {
            continue;
        };
        let current = state
            .store
            .find(task_id)
            .map_err(|error| error.to_string())?;
        let applicable = match action {
            "pause" => matches!(
                current.status,
                TaskStatus::Queued | TaskStatus::Downloading | TaskStatus::Processing
            ),
            "resume" => current.status == TaskStatus::Paused,
            "cancel" => task_is_active(current.status),
            "retry" => matches!(current.status, TaskStatus::Failed | TaskStatus::Cancelled),
            _ => false,
        };
        if applicable {
            let outcome =
                run_local_task_action(task_id.to_string(), action.to_string(), app.clone()).await?;
            first_outcome.get_or_insert(outcome);
        }
    }
    if let Some(outcome) = first_outcome {
        let mut outcome = directory_retry.take().unwrap_or(outcome);
        outcome.queue_task = Some(cloud_directory_group_queue_task(&state.store, group.id)?);
        return Ok(outcome);
    }
    if let Some(outcome) = directory_retry {
        return Ok(outcome);
    }
    Err("目录内没有可执行此操作的下载任务".to_string())
}

async fn run_local_task_action(
    task_id: String,
    action: String,
    app: AppHandle,
) -> Result<TaskActionOutcome, String> {
    let state = app.state::<AppState>();
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let mut current = state.store.find(id).map_err(|error| error.to_string())?;
    ensure_private_task_access(&state, &current)?;
    if action == "retry_cleanup" {
        let task = crate::cloud::commands::cleanup_cloud_share_task(app.clone(), id, true)
            .await
            .map_err(|_| "网盘转存文件清理未完成，可重试清理".to_string())?;
        let cloud = app.try_state::<Arc<crate::cloud::commands::CloudState>>();
        return Ok(TaskActionOutcome {
            payload: task_payload_with_cleanup(
                &state.store,
                cloud.as_ref().map(|cloud| &cloud.share_plans),
                task,
            )?,
            duplicate: None,
            queue_task: None,
        });
    }
    if current.kind == "cloud" && matches!(action.as_str(), "pause" | "cancel") {
        let control = if action == "pause" {
            DownloadAction::Pause
        } else {
            DownloadAction::Cancel
        };
        if let Some(task) = state
            .cloud_download_dispatch
            .control_unsubmitted(&state.store, id, control)
            .await
            .map_err(|error| error.to_string())?
        {
            crate::cloud::commands::emit_cloud_dispatch_update(&app, &state, &task);
            return task_action_outcome(&state.store, task, None);
        }
        // addUri may have completed while control waited for the submission
        // guard. Use its fresh GID instead of locally cancelling a stale copy.
        current = state.store.find(id).map_err(|error| error.to_string())?;
    }
    if crate::xunlei_native::is_native_gid(current.aria_gid.as_deref())
        && matches!(action.as_str(), "pause" | "resume" | "cancel")
    {
        if action == "resume" {
            let task = resubmit_task(
                app.clone(),
                &state,
                current,
                UriSubmissionOptions {
                    continue_download: true,
                    ..Default::default()
                },
            )
            .await?;
            return task_action_outcome(&state.store, task, None);
        }
        let action = match action.as_str() {
            "pause" => DownloadAction::Pause,
            "cancel" => DownloadAction::Cancel,
            _ => unreachable!(),
        };
        let task = state
            .xunlei_native_backend
            .control(&app, &state.store, current, action)?;
        return task_action_outcome(&state.store, task, None);
    }
    if action == "resume"
        && current.status == TaskStatus::Paused
        && ((current.kind == "cloud"
            && crate::cloud::download_dispatch::is_cloud_download_source(&current.source))
            || (current.aria_gid.is_none()
                && matches!(
                    current.kind.as_str(),
                    "file" | "magnet" | "ed2k" | "hls" | "dash" | "cloud"
                )))
    {
        let task = resubmit_task(
            app.clone(),
            &state,
            current,
            UriSubmissionOptions {
                continue_download: true,
                ..Default::default()
            },
        )
        .await?;
        return task_action_outcome(&state.store, task, None);
    }
    if action == "cancel" && current.aria_gid.is_none() && current.kind != "ftp" {
        let task = force_cancel_unsubmitted_task(&state.store, current)?;
        return task_action_outcome(&state.store, task, None);
    }
    if action == "retry" {
        if !matches!(current.status, TaskStatus::Failed | TaskStatus::Cancelled) {
            return Err("当前任务不能重新下载".to_string());
        }
        if !cloud_download_requires_signed_url_refresh(&current) {
            if let Some(duplicate) =
                retry_duplicate_state(&state.store, &current).map_err(|error| error.to_string())?
            {
                return task_action_outcome(&state.store, current, Some(duplicate));
            }
        }
        let options = cloud_retry_submission_options(&current);
        let task = resubmit_task(app.clone(), &state, current, options).await?;
        return task_action_outcome(&state.store, task, None);
    }
    let action = match action.as_str() {
        "pause" => DownloadAction::Pause,
        "resume" => DownloadAction::Resume,
        "cancel" => DownloadAction::Cancel,
        _ => return Err("当前任务不能执行此操作".to_string()),
    };
    let task = state
        .runtime
        .control(id, action)
        .await
        .map_err(|error| error.to_string())?;
    task_action_outcome(&state.store, task, None)
}

#[tauri::command]
pub async fn resolve_duplicate_download(
    task_id: String,
    action: String,
    app: AppHandle,
) -> Result<DownloadTaskPayload, String> {
    let state = app.state::<AppState>();
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let current = state.store.find(id).map_err(|error| error.to_string())?;
    ensure_private_task_access(&state, &current)?;
    let resolution = duplicate_state(&state.store, &current).map_err(|error| error.to_string())?;
    let task = match (action.as_str(), resolution) {
        ("resume", DuplicateDownloadState::ResumeAvailable)
            if current.status == TaskStatus::Paused
                && current.kind == "cloud"
                && crate::cloud::download_dispatch::is_cloud_download_source(&current.source) =>
        {
            resubmit_task(
                app.clone(),
                &state,
                current,
                UriSubmissionOptions {
                    continue_download: true,
                    ..Default::default()
                },
            )
            .await?
        }
        ("resume", DuplicateDownloadState::ResumeAvailable)
            if current.status == TaskStatus::Paused && current.aria_gid.is_some() =>
        {
            if crate::xunlei_native::is_native_gid(current.aria_gid.as_deref()) {
                resubmit_task(
                    app.clone(),
                    &state,
                    current,
                    UriSubmissionOptions {
                        continue_download: true,
                        ..Default::default()
                    },
                )
                .await?
            } else {
                state
                    .runtime
                    .control(current.id, DownloadAction::Resume)
                    .await
                    .map_err(|error| error.to_string())?
            }
        }
        ("resume", DuplicateDownloadState::ResumeAvailable) => {
            resubmit_task(
                app.clone(),
                &state,
                current,
                UriSubmissionOptions {
                    continue_download: true,
                    ..Default::default()
                },
            )
            .await?
        }
        ("restart_missing", DuplicateDownloadState::CompletedFilesMissing) => {
            resubmit_task(app.clone(), &state, current, Default::default()).await?
        }
        ("overwrite", DuplicateDownloadState::OverwriteRequired) => {
            resubmit_task(
                app.clone(),
                &state,
                current,
                UriSubmissionOptions {
                    allow_overwrite: true,
                    ..Default::default()
                },
            )
            .await?
        }
        _ => return Err("重复下载状态已变化，请重新提交下载链接".to_string()),
    };
    task_payload(&state.store, task)
}

pub(crate) async fn resubmit_task(
    app: AppHandle,
    state: &AppState,
    mut task: DownloadTask,
    mut options: UriSubmissionOptions,
) -> Result<DownloadTask, String> {
    fail_invalid_task_submission(&state.store, &task)?;
    // Match the cleanup worker's source gate for both retry and overwrite.
    let _directory_submission =
        crate::cloud::commands::lock_directory_member_submission(&state.store, task.id)
            .await
            .map_err(|error| error.to_string())?;
    let _share_submission = if task.source.starts_with("cloud-share://") {
        Some(
            state
                .cloud_download_sources
                .acquire(&task.source)
                .await
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    if _directory_submission.is_some()
        && task.aria_gid.is_none()
        && matches!(task.status, TaskStatus::Parsing | TaskStatus::Paused)
    {
        task = crate::cloud::commands::recover_interrupted_directory_preparation(
            &state.store,
            task.id,
        )
        .map_err(|error| error.to_string())?;
        if task.aria_gid.is_some() || task.status != TaskStatus::Paused {
            return Ok(task);
        }
    }
    if task.source.starts_with("cloud-share://") {
        let cloud = app
            .try_state::<Arc<crate::cloud::commands::CloudState>>()
            .ok_or_else(|| "网盘分享下载状态不可用，请重新解析分享链接".to_string())?;
        reserve_cloud_share_resubmission(&cloud.share_plans, &task)?;
    }
    options = crate::cloud::commands::cloud_directory_member_retry_options(
        &state.store,
        task.id,
        options,
    )
    .map_err(|error| error.to_string())?;
    let configured = download_submission_options(state)?;
    if options.directory.is_none() {
        options.directory = configured.directory;
    }
    if options.proxy.is_none() {
        options.proxy = configured.proxy;
    }
    if task.source.starts_with("cloud://") || task.source.starts_with("cloud-share://") {
        options = state
            .cloud_download_dispatch
            .retry_submission_options(task.id, options);
        if state
            .store
            .cloud_directory_group_for_task(task.id)
            .map_err(|e| e.to_string())?
            .is_none()
            && task.source.starts_with("cloud-share://")
        {
            let cloud = app.state::<Arc<crate::cloud::commands::CloudState>>();
            let entry = cloud
                .share_plans
                .resolve_entry(&task.source)
                .map_err(|e| e.to_string())?;
            if let Some(directory) = options.directory.as_ref() {
                options.directory = Some(
                    crate::cloud::commands::directory_download_output_directory(
                        directory,
                        &entry.entry.relative_directory,
                    )
                    .map_err(|e| e.to_string())?,
                );
            }
        }
        if let Some(file) = state
            .store
            .files(task.id)
            .map_err(|e| e.to_string())?
            .first()
        {
            let path = Path::new(&file.path);
            if path.is_absolute() {
                options.directory = path.parent().map(Path::to_path_buf);
                options.output_name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned);
            }
        }
        return crate::cloud::commands::queue_cloud_dispatch(app.clone(), state, task, options)
            .await
            .map_err(|e| e.to_string());
    }
    if task.kind == "torrent" {
        return Err("种子任务请重新添加后再选择文件下载".to_string());
    }
    if task.kind == "magnet" {
        let service = state
            .runtime
            .service()
            .await
            .map_err(|error| error.to_string())?;
        // The engine keeps magnet metadata in its native state directory. A
        // degraded task must therefore return its original magnet to aria2
        // instead of requiring the optional metadata service to export a
        // separate .torrent file. aria2 reuses local state when available and
        // otherwise performs normal P2P metadata discovery.
        return service
            .prepare_magnet_with_options(task.id, task.source, options)
            .await
            .map_err(|error| error.to_string());
    }
    options.output_name = None;
    state
        .runtime
        .resubmit_uri_with_options(task.id, vec![task.source], options)
        .await
        .map_err(|error| error.to_string())
}

fn redact_ftp_credentials(task: &mut DownloadTask) {
    if task.kind != "ftp" {
        return;
    }
    if let Ok(target) = crate::parse_ftp_url(&task.source) {
        task.source = target.display_source().to_owned();
    }
}

fn reserve_cloud_share_resubmission(
    plans: &crate::cloud::share::CloudSharePlanStore,
    task: &DownloadTask,
) -> Result<(), String> {
    plans
        .reserve_local_task(&task.source, task.id)
        .map_err(|_| "无法准备网盘分享重试，请重新解析分享链接".to_string())
}

fn task_output_directory(store: &TaskStore, task_id: Uuid) -> Result<std::path::PathBuf, String> {
    store
        .files(task_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter_map(|file| {
            std::path::PathBuf::from(file.path)
                .parent()
                .map(|path| path.to_path_buf())
        })
        .find(|directory| directory.is_dir())
        .ok_or_else(|| "下载文件已不存在，无法打开所在目录".to_string())
}

/// A cloud retry obtains a fresh provider URL, but the partial local payload
/// remains the same managed download. Request continuation so DownloadService
/// validates its native checkpoint before aria2 sees the new URL. A rejected
/// Range request refers to the expiring provider URL, not the local checkpoint.
fn cloud_retry_submission_options(task: &DownloadTask) -> UriSubmissionOptions {
    UriSubmissionOptions {
        continue_download: task.source.starts_with("cloud://")
            || task.source.starts_with("cloud-share://"),
        ..Default::default()
    }
}

/// A cloud provider's signed URL can expire while a local payload remains
/// resumable.  Do not make the user decide between the existing checkpoint
/// and a fresh URL: re-resolve the same stable cloud task and let aria2 resume
/// it.  Error 8 is separately changed to an overwrite below because that
/// particular endpoint rejected the range request itself.
fn cloud_download_requires_signed_url_refresh(task: &DownloadTask) -> bool {
    (task.source.starts_with("cloud://") || task.source.starts_with("cloud-share://"))
        && matches!(
            task.aria_error_code.as_deref(),
            Some("6" | "8" | "22" | "24")
        )
}

/// Called after event publication; the task worker never blocks the aria2
/// synchronization callback. The source gate is released on every return path.
pub(crate) fn schedule_cloud_download_refresh(app: AppHandle, task: &DownloadTask) {
    if task.status != TaskStatus::Failed {
        return;
    }
    let Some(cloud) = app.try_state::<Arc<crate::cloud::commands::CloudState>>() else {
        return;
    };
    let share = task.source.starts_with("cloud-share://");
    let claimed = if share {
        cloud
            .share_refresh_budget
            .claim(&task.source, task.aria_error_code.as_deref())
            .unwrap_or(false)
    } else {
        cloud
            .cloud_download_refresh_budget
            .claim(&task.source, task.aria_error_code.as_deref())
    };
    if !claimed {
        return;
    }
    let failed = task.clone();
    tauri::async_runtime::spawn(async move {
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };
        let Ok(_directory_submission) =
            crate::cloud::commands::lock_directory_member_submission(&state.store, failed.id).await
        else {
            return;
        };
        let Ok(_guard) = state.cloud_download_sources.acquire(&failed.source).await else {
            return;
        };
        let Ok(current) = state.store.find(failed.id) else {
            return;
        };
        // A user retry, cancellation, or recycle while waiting wins.
        if current.status != TaskStatus::Failed || current.aria_gid != failed.aria_gid {
            return;
        }
        if !cloud_download_requires_signed_url_refresh(&current)
            && !matches!(retry_duplicate_state(&state.store, &current), Ok(None))
        {
            return;
        }
        let Ok(options) = download_submission_options(&state) else {
            return;
        };
        let Ok(mut options) = crate::cloud::commands::cloud_directory_member_retry_options(
            &state.store,
            current.id,
            options,
        ) else {
            return;
        };
        if let Ok(files) = state.store.files(current.id) {
            if let Some(file) = files.first() {
                let path = Path::new(&file.path);
                if path.is_absolute() {
                    options.directory = path.parent().map(Path::to_path_buf);
                    options.output_name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned);
                }
            }
        }
        let _ = crate::cloud::commands::queue_cloud_dispatch(app.clone(), &state, current, options)
            .await;
    });
}

const PLAYABLE_MEDIA_EXTENSIONS: &[&str] = &[
    "3gp", "asf", "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts", "ts",
    "webm", "wmv", "aac", "aiff", "alac", "ape", "flac", "m4a", "mka", "mp3", "oga", "ogg", "opus",
    "wav", "wma",
];

const IMAGE_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "gif", "heic", "ico", "jpeg", "jpg", "png", "svg", "tif", "tiff", "webp",
];

const ASSOCIATED_FILE_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "cfg", "conf", "csv", "css", "doc", "docm", "docx", "dot", "dotm", "dotx",
    "gif", "heic", "htm", "html", "ico", "ini", "jpeg", "jpg", "json", "log", "markdown", "md",
    "odp", "ods", "odt", "pdf", "png", "pot", "potm", "potx", "pps", "ppsm", "ppsx", "ppt", "pptm",
    "pptx", "rst", "rtf", "svg", "text", "tif", "tiff", "toml", "tsv", "txt", "webp", "wps", "xls",
    "xlsb", "xlsm", "xlsx", "xlt", "xltm", "xltx", "xml", "yaml", "yml",
];

fn is_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            PLAYABLE_MEDIA_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

fn is_associated_file_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ASSOCIATED_FILE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

fn completed_task_file_paths(store: &TaskStore, task_id: Uuid) -> Result<Vec<PathBuf>, String> {
    if store
        .cloud_directory_group(task_id)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        let items = store
            .cloud_directory_group_items(task_id)
            .map_err(|error| error.to_string())?;
        if items.is_empty() {
            return Err("下载文件已不存在，无法打开".to_string());
        }
        let mut paths = Vec::new();
        for item in items {
            let member_id = item
                .task_id
                .ok_or_else(|| "仅已完成任务可以打开文件".to_string())?;
            let member = store.find(member_id).map_err(|error| error.to_string())?;
            if member.status != TaskStatus::Completed {
                return Err("仅已完成任务可以打开文件".to_string());
            }
            let files = store.files(member_id).map_err(|error| error.to_string())?;
            let selected = files
                .into_iter()
                .filter(|file| file.selected)
                .map(|file| PathBuf::from(file.path))
                .collect::<Vec<_>>();
            if selected.is_empty() {
                return Err("下载文件已不存在，无法打开".to_string());
            }
            paths.extend(selected);
        }
        return Ok(paths);
    }
    let task = store.find(task_id).map_err(|error| error.to_string())?;
    if task.status != TaskStatus::Completed {
        return Err("仅已完成任务可以打开文件".to_string());
    }
    Ok(store
        .files(task_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|file| file.selected)
        .map(|file| PathBuf::from(file.path))
        .collect())
}

fn task_video_paths(store: &TaskStore, task_id: Uuid) -> Result<Vec<PathBuf>, String> {
    Ok(completed_task_file_paths(store, task_id)?
        .into_iter()
        .filter(|path| path.is_file() && is_video_path(path))
        .collect())
}

fn video_playlist_path(task_id: Uuid, videos: &[PathBuf]) -> Result<PathBuf, String> {
    let directory = std::env::temp_dir().join("VersaDown").join("playlists");
    fs::create_dir_all(&directory).map_err(|error| format!("无法准备播放列表目录：{error}"))?;
    let playlist = directory.join(format!("{task_id}-{}.m3u", Uuid::new_v4()));
    let entries = videos
        .iter()
        .map(|path| path.to_string_lossy().replace(['\r', '\n'], ""))
        .collect::<Vec<_>>()
        .join("\r\n");
    fs::write(&playlist, format!("#EXTM3U\r\n{entries}\r\n"))
        .map_err(|error| format!("无法创建视频播放列表：{error}"))?;
    Ok(playlist)
}

#[tauri::command]
pub fn open_task_directory(task_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let directory = match state
        .store
        .cloud_directory_group(id)
        .map_err(|error| error.to_string())?
    {
        Some(group) => {
            ensure_private_group_access(&state, &group)?;
            cloud_directory_group_output_root(&group)?
        }
        None => {
            let task = state.store.find(id).map_err(|error| error.to_string())?;
            ensure_private_task_access(&state, &task)?;
            task_output_directory(&state.store, id)?
        }
    };
    tauri_plugin_opener::open_path(&directory, None::<&str>)
        .map_err(|error| format!("无法打开下载目录：{error}"))
}

fn task_associated_file_path(store: &TaskStore, task_id: Uuid) -> Result<PathBuf, String> {
    let files = completed_task_file_paths(store, task_id)?;
    let path = match files.as_slice() {
        [path] => path,
        [first, ..] if files.iter().all(|path| is_image_path(path)) => first,
        _ => return Err("该任务包含多个文件，请打开下载目录".to_string()),
    };
    if !path.is_absolute() {
        return Err("下载文件路径不是本地绝对路径，无法打开".to_string());
    }
    if !is_associated_file_path(path) {
        return Err("该文件类型不支持使用系统默认应用打开".to_string());
    }
    if !path.is_file() {
        return Err("下载文件已不存在，无法打开".to_string());
    }
    Ok(path.to_path_buf())
}

#[tauri::command]
pub fn open_task_file(task_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    if let Some(group) = state
        .store
        .cloud_directory_group(id)
        .map_err(|error| error.to_string())?
    {
        ensure_private_group_access(&state, &group)?;
    } else {
        let task = state.store.find(id).map_err(|error| error.to_string())?;
        ensure_private_task_access(&state, &task)?;
    }
    let path = task_associated_file_path(&state.store, id)?;
    tauri_plugin_opener::open_path(&path, None::<&str>)
        .map_err(|error| format!("无法使用系统默认应用打开文件：{error}"))
}

fn cloud_directory_group_output_root(group: &CloudDirectoryGroup) -> Result<PathBuf, String> {
    let relative = Path::new(&group.relative_directory);
    if relative.is_absolute() || relative.components().count() != 1 {
        return Err("目录下载任务的输出目录不安全，无法打开".to_string());
    }
    let Some(name) = relative.file_name().filter(|name| !name.is_empty()) else {
        return Err("目录下载任务缺少输出目录名称".to_string());
    };
    let root = group.output_directory.join(name);
    if !root.starts_with(&group.output_directory) {
        return Err("目录下载任务的输出目录不安全，无法打开".to_string());
    }
    Ok(root)
}

#[tauri::command]
pub fn play_task_videos(task_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    if let Some(group) = state
        .store
        .cloud_directory_group(id)
        .map_err(|error| error.to_string())?
    {
        ensure_private_group_access(&state, &group)?;
    } else {
        let task = state.store.find(id).map_err(|error| error.to_string())?;
        ensure_private_task_access(&state, &task)?;
        if task.status != TaskStatus::Completed {
            return Err("仅已完成任务可以播放视频".to_string());
        }
    }
    let videos = task_video_paths(&state.store, id)?;
    if videos.is_empty() {
        return Err("此任务中没有可播放的视频文件".to_string());
    }
    let target = if videos.len() == 1 {
        videos.into_iter().next().expect("checked non-empty")
    } else {
        video_playlist_path(id, &videos)?
    };
    tauri_plugin_opener::open_path(&target, None::<&str>)
        .map_err(|error| format!("无法调用系统默认播放器：{error}"))
}

#[tauri::command]
pub async fn recycle_task(
    task_id: String,
    state: State<'_, AppState>,
) -> Result<QueueTaskPayload, String> {
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    if let Some(group) = state
        .store
        .cloud_directory_group(id)
        .map_err(|error| error.to_string())?
    {
        ensure_private_group_access(&state, &group)?;
        let _submission = crate::cloud::commands::lock_directory_submission(id)
            .await
            .map_err(|error| error.to_string())?;
        return recycle_cloud_directory_group(&state.store, id);
    }
    let task = state.store.find(id).map_err(|error| error.to_string())?;
    ensure_private_task_access(&state, &task)?;
    if !matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    ) {
        return Err("请先取消活动下载任务，再移入回收站".to_string());
    }
    let recycled = state
        .store
        .transition(id, TaskStatus::Recycled)
        .map_err(|error| error.to_string())?;
    queue_task_payload_from_task(&state.store, recycled)
}

fn recycle_cloud_directory_group(
    store: &TaskStore,
    group_id: Uuid,
) -> Result<QueueTaskPayload, String> {
    let group = store
        .cloud_directory_group(group_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "目录下载任务不存在".to_string())?;
    let members = store
        .cloud_directory_group_items(group.id)
        .map_err(|error| error.to_string())?;
    let mut tasks = Vec::new();
    for member in members {
        let Some(task_id) = member.task_id else {
            if member.failure_message.is_none()
                && !matches!(group.status, TaskStatus::Cancelled | TaskStatus::Recycled)
            {
                return Err("请先取消目录中的活动下载任务，再移入回收站".to_string());
            }
            continue;
        };
        let task = store.find(task_id).map_err(|error| error.to_string())?;
        if !matches!(
            task.status,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Recycled
        ) {
            return Err("请先取消目录中的活动下载任务，再移入回收站".to_string());
        }
        tasks.push(task);
    }
    for task in tasks {
        if task.status != TaskStatus::Recycled {
            store
                .transition(task.id, TaskStatus::Recycled)
                .map_err(|error| error.to_string())?;
        }
    }
    store
        .recycle_cloud_directory_group(group.id)
        .map_err(|error| error.to_string())?;
    cloud_directory_group_queue_task(store, group.id)
}

fn recycle_local_task_for_permanent_deletion(
    store: &TaskStore,
    task_id: Uuid,
) -> Result<(), String> {
    let task = store.find(task_id).map_err(|error| error.to_string())?;
    match task.status {
        TaskStatus::Recycled | TaskStatus::Cancelled => Ok(()),
        TaskStatus::Completed | TaskStatus::Failed => {
            store
                .transition(task_id, TaskStatus::Recycled)
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        _ => Err("任务仍在运行，无法删除".to_string()),
    }
}

#[tauri::command]
pub async fn delete_tasks(
    task_ids: Vec<String>,
    delete_files: bool,
    app: AppHandle,
    state: State<'_, AppState>,
    cloud: State<'_, Arc<crate::cloud::commands::CloudState>>,
) -> Result<(), String> {
    let mut local_task_ids = Vec::new();
    let mut group_ids = Vec::new();
    let mut seen_local_task_ids = HashSet::new();
    let mut seen_group_ids = HashSet::new();

    for task_id in &task_ids {
        let id = Uuid::parse_str(task_id).map_err(|_| "任务 ID 无效".to_string())?;
        if let Some(group) = state
            .store
            .cloud_directory_group(id)
            .map_err(|error| error.to_string())?
        {
            ensure_private_group_access(&state, &group)?;
            if seen_group_ids.insert(group.id) {
                group_ids.push(group.id);
            }
            continue;
        }
        if let Some(group) = state
            .store
            .cloud_directory_group_for_task(id)
            .map_err(|error| error.to_string())?
        {
            ensure_private_group_access(&state, &group)?;
            if seen_group_ids.insert(group.id) {
                group_ids.push(group.id);
            }
            continue;
        }
        let task = state.store.find(id).map_err(|error| error.to_string())?;
        ensure_private_task_access(&state, &task)?;
        if seen_local_task_ids.insert(id) {
            local_task_ids.push(id);
        }
    }

    for group_id in &group_ids {
        let group = state
            .store
            .cloud_directory_group(*group_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "目录下载任务不存在".to_string())?;
        if task_is_active(group.status) {
            run_cloud_directory_group_action(app.clone(), &state, &group, "cancel").await?;
        }
        recycle_cloud_directory_group(&state.store, *group_id)?;
    }

    for task_id in &local_task_ids {
        let task = state
            .store
            .find(*task_id)
            .map_err(|error| error.to_string())?;
        if task_is_active(task.status) {
            run_local_task_action(task_id.to_string(), "cancel".to_string(), app.clone()).await?;
        }
        recycle_local_task_for_permanent_deletion(&state.store, *task_id)?;
    }

    let purge_ids = group_ids
        .into_iter()
        .chain(local_task_ids)
        .map(|id| id.to_string())
        .collect();
    purge_recycled_task_batch(
        Arc::clone(&state.store),
        Arc::clone(cloud.inner()),
        purge_ids,
        delete_files,
    )
    .await
}

fn remove_recorded_download_file(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("拒绝删除非绝对路径的下载文件：{}", path.display()));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)
                .map_err(|error| format!("无法删除下载文件 {}：{error}", path.display()))
        }
        Ok(_) => Err(format!("拒绝删除非文件下载项：{}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("无法读取下载文件 {}：{error}", path.display())),
    }
}

fn recorded_task_payload_paths(store: &TaskStore, task_id: Uuid) -> Result<Vec<PathBuf>, String> {
    let task = store.find(task_id).map_err(|error| error.to_string())?;
    let mut paths = Vec::new();
    for file in store.files(task_id).map_err(|error| error.to_string())? {
        if file.path.trim().is_empty() {
            continue;
        }
        let path = PathBuf::from(file.path);
        if !path.is_absolute() {
            if matches!(task.kind.as_str(), "magnet" | "torrent") && file.completed_length == 0 {
                continue;
            }
            return Err(format!("拒绝删除非绝对路径的下载文件：{}", path.display()));
        }
        paths.push(path.clone());
        let mut sidecar = path.into_os_string();
        sidecar.push(".aria2");
        paths.push(PathBuf::from(sidecar));
    }
    for path in &paths {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(format!("拒绝删除非文件下载项：{}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("无法读取下载文件 {}：{error}", path.display())),
        }
    }
    Ok(paths)
}

fn remove_task_payload_files(store: &TaskStore, task_id: Uuid) -> Result<(), String> {
    for path in recorded_task_payload_paths(store, task_id)? {
        remove_recorded_download_file(&path)?;
    }
    Ok(())
}

fn recorded_xunlei_checkpoint_paths(
    store: &TaskStore,
    task: &DownloadTask,
) -> Result<Vec<PathBuf>, String> {
    if !crate::xunlei_native::is_native_gid(task.aria_gid.as_deref()) {
        return Ok(Vec::new());
    }
    // The Mac service removes its checkpoint on completion. A hidden script
    // created next to the finished file later does not belong to this task.
    #[cfg(target_os = "macos")]
    if task.completed_at.is_some() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for file in store.files(task.id).map_err(|error| error.to_string())? {
        if file.path.trim().is_empty() {
            continue;
        }
        let output = PathBuf::from(file.path);
        if !output.is_absolute() {
            return Err(format!(
                "拒绝删除非绝对路径的迅雷断点文件：{}",
                output.display()
            ));
        }
        #[cfg(target_os = "macos")]
        let checkpoints = [crate::xunlei_native::macos_checkpoint_path(&output)];
        #[cfg(not(target_os = "macos"))]
        let checkpoints = [".xltd", ".xltd.cfg"].map(|suffix| {
            let mut checkpoint = output.as_os_str().to_os_string();
            checkpoint.push(suffix);
            PathBuf::from(checkpoint)
        });
        for checkpoint in checkpoints {
            match fs::symlink_metadata(&checkpoint) {
                Ok(metadata)
                    if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {}
                Ok(_) => return Err(format!("拒绝删除非文件下载项：{}", checkpoint.display())),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "无法读取下载文件 {}：{error}",
                        checkpoint.display()
                    ))
                }
            }
            paths.push(checkpoint);
        }
    }
    Ok(paths)
}

// Engine snapshots can be missing after an interrupted submission/recovery.
// Use durable membership only when there is no authoritative engine filename;
// never guess from the display name or recursively remove a shared directory.
fn directory_payload_fallback_paths(
    store: &TaskStore,
    tasks: &[DownloadTask],
) -> Result<Vec<PathBuf>, String> {
    let mut groups = HashMap::new();
    let mut paths = Vec::new();
    for task in tasks {
        if store
            .files(task.id)
            .map_err(|error| error.to_string())?
            .iter()
            .any(|file| !file.path.trim().is_empty())
        {
            continue;
        }
        let Some(group) = store
            .cloud_directory_group_for_task(task.id)
            .map_err(|error| error.to_string())?
        else {
            continue;
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = groups.entry(group.id) {
            let members = store
                .cloud_directory_group_items(group.id)
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter_map(|item| item.task_id.map(|id| (id, item)))
                .collect::<HashMap<_, _>>();
            entry.insert(members);
        }
        let item = groups
            .get(&group.id)
            .and_then(|members| members.get(&task.id))
            .ok_or_else(|| "目录下载任务缺少文件归属记录，未删除本地文件".to_string())?;
        if let Some(path) = directory_payload_fallback_path(&group, item)? {
            let mut sidecar = path.clone().into_os_string();
            sidecar.push(".aria2");
            let sidecar = PathBuf::from(sidecar);
            // An unstarted task may share a planned name with a user's existing file.
            if task.completed_length == 0
                && (fs::symlink_metadata(&path).is_ok() || fs::symlink_metadata(&sidecar).is_ok())
            {
                return Err(
                    "无法确认目录中的文件是否由该任务下载，已保留文件和任务记录".to_string()
                );
            }
            paths.extend([path, sidecar]);
        }
    }
    // Validate every fallback before deleting any of them. Errors retain task
    // records, allowing the user to fix permissions/paths and retry the purge.
    for path in &paths {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(format!("拒绝删除非文件下载项：{}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("无法读取下载文件 {}：{error}", path.display())),
        }
    }
    Ok(paths)
}

fn remove_directory_payload_fallbacks(
    store: &TaskStore,
    tasks: &[DownloadTask],
) -> Result<(), String> {
    for path in directory_payload_fallback_paths(store, tasks)? {
        remove_recorded_download_file(&path)?;
    }
    Ok(())
}

fn directory_payload_directories(
    store: &TaskStore,
    tasks: &[DownloadTask],
) -> Result<Vec<PathBuf>, String> {
    let mut directories = HashSet::new();
    for task in tasks {
        let Some(group) = store
            .cloud_directory_group_for_task(task.id)
            .map_err(|error| error.to_string())?
        else {
            continue;
        };
        let item = store
            .cloud_directory_group_items(group.id)
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|item| item.task_id == Some(task.id))
            .ok_or_else(|| "目录下载任务缺少文件归属记录，未删除本地目录".to_string())?;
        let Some(path) = directory_payload_fallback_path(&group, &item)? else {
            continue;
        };
        let mut parent = path.parent().map(Path::to_path_buf);
        while let Some(directory) = parent {
            if directory == group.output_directory {
                break;
            }
            if !directory.starts_with(&group.output_directory) {
                return Err("目录下载文件路径不安全，已保留目录和任务记录".to_string());
            }
            parent = directory.parent().map(Path::to_path_buf);
            directories.insert(directory);
        }
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| right.cmp(left))
    });
    Ok(directories)
}

fn remove_empty_download_directory(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("无法读取下载目录 {}：{error}", path.display())),
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("目录下载文件路径不安全，已保留目录和任务记录".to_string());
        }
    }
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("拒绝删除非目录下载项：{}", path.display()));
    }
    let mut entries = fs::read_dir(path)
        .map_err(|error| format!("无法读取下载目录 {}：{error}", path.display()))?;
    if entries.next().is_some() {
        return Ok(());
    }
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("无法删除下载目录 {}：{error}", path.display())),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TianyiLocalStatePurgeReport {
    pub removed_tasks: usize,
    pub removed_directory_groups: usize,
    pub removed_share_plans: usize,
}

fn is_tianyi_task_source(source: &str) -> bool {
    source.starts_with("cloud://tianyi/") || source.starts_with("cloud-share://tianyi/")
}

/// Removes application-owned Tianyi downloads and durable records. Every
/// payload path is validated before any file, task or credential is deleted.
pub(crate) fn purge_tianyi_local_state(
    store: &TaskStore,
    cloud: &crate::cloud::commands::CloudState,
) -> Result<TianyiLocalStatePurgeReport, String> {
    cloud
        .preflight_tianyi_persisted_state()
        .map_err(|error| error.to_string())?;
    let groups = store
        .cloud_directory_groups()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|group| group.provider == crate::cloud::CloudProvider::Tianyi)
        .collect::<Vec<_>>();
    let mut task_ids = store
        .list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|task| is_tianyi_task_source(&task.source))
        .map(|task| task.id)
        .collect::<std::collections::HashSet<_>>();
    for group in &groups {
        for item in store
            .cloud_directory_group_items(group.id)
            .map_err(|error| error.to_string())?
        {
            if let Some(task_id) = item.task_id {
                task_ids.insert(task_id);
            }
        }
    }
    let tasks = task_ids
        .iter()
        .map(|id| store.find(*id).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;

    let mut paths = directory_payload_fallback_paths(store, &tasks)?;
    for task in &tasks {
        paths.extend(recorded_task_payload_paths(store, task.id)?);
    }
    for path in &paths {
        remove_recorded_download_file(path)?;
    }

    let removed_share_plans = cloud
        .purge_tianyi_persisted_state()
        .map_err(|error| error.to_string())?;
    let ids = task_ids.into_iter().collect::<Vec<_>>();
    store
        .remove_task_records(&ids)
        .map_err(|error| error.to_string())?;
    for group in &groups {
        store
            .remove_cloud_directory_group(group.id)
            .map_err(|error| error.to_string())?;
    }
    Ok(TianyiLocalStatePurgeReport {
        removed_tasks: ids.len(),
        removed_directory_groups: groups.len(),
        removed_share_plans,
    })
}

fn directory_payload_fallback_path(
    group: &CloudDirectoryGroup,
    item: &CloudDirectoryGroupItem,
) -> Result<Option<PathBuf>, String> {
    let unsafe_path = || "目录下载文件路径不安全，已保留文件和任务记录".to_string();
    if !group.output_directory.is_absolute()
        || group
            .output_directory
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || item.relative_directory.first() != Some(&group.relative_directory)
    {
        return Err(unsafe_path());
    }
    for component in item
        .relative_directory
        .iter()
        .chain(std::iter::once(&item.output_name))
    {
        if crate::cloud::commands::provider_directory_component(component)
            .map_err(|_| unsafe_path())?
            != *component
        {
            return Err(unsafe_path());
        }
    }
    let mut parent = group.output_directory.clone();
    for component in &item.relative_directory {
        parent.push(component);
    }
    // Include the configured output root and its ancestors: a junction there
    // redirects descendants just as a replaced member directory would.
    for ancestor in parent.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("无法读取下载目录 {}：{error}", ancestor.display())),
        };
        // Reject junctions as well as symbolic links; a replaced subdirectory
        // must not redirect deletion outside the recorded download tree.
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(unsafe_path());
            }
        }
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(unsafe_path());
        }
    }
    Ok(Some(parent.join(&item.output_name)))
}

#[tauri::command]
pub async fn purge_recycled_tasks(
    task_ids: Vec<String>,
    delete_files: bool,
    state: State<'_, AppState>,
    cloud: State<'_, Arc<crate::cloud::commands::CloudState>>,
) -> Result<(), String> {
    for task_id in &task_ids {
        let id = Uuid::parse_str(task_id).map_err(|_| "任务 ID 无效".to_string())?;
        if let Some(group) = state
            .store
            .cloud_directory_group(id)
            .map_err(|error| error.to_string())?
        {
            ensure_private_group_access(&state, &group)?;
        } else {
            let task = state.store.find(id).map_err(|error| error.to_string())?;
            ensure_private_task_access(&state, &task)?;
        }
    }
    purge_recycled_task_batch(
        Arc::clone(&state.store),
        Arc::clone(cloud.inner()),
        task_ids,
        delete_files,
    )
    .await
}

async fn purge_recycled_task_batch(
    store: Arc<TaskStore>,
    cloud: Arc<crate::cloud::commands::CloudState>,
    task_ids: Vec<String>,
    delete_files: bool,
) -> Result<(), String> {
    // A cancelled root can be visible before its in-flight URL resolution
    // returns. Do not delete membership while that submission is still linking.
    let mut locked_groups = Vec::new();
    for task_id in &task_ids {
        let id = Uuid::parse_str(task_id).map_err(|_| "任务 ID 无效".to_string())?;
        if store
            .cloud_directory_group(id)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            locked_groups.push(id);
        } else if let Some(group) = store
            .cloud_directory_group_for_task(id)
            .map_err(|error| error.to_string())?
        {
            locked_groups.push(group.id);
        }
    }
    locked_groups.sort_unstable();
    locked_groups.dedup();
    let mut submission_guards = Vec::new();
    for id in locked_groups {
        submission_guards.push(
            crate::cloud::commands::lock_directory_submission(id)
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _submission_guards = submission_guards;
        let mut ids = Vec::with_capacity(task_ids.len());
        let mut group_ids = Vec::new();
        let mut seen_task_ids = HashSet::with_capacity(task_ids.len());
        let mut seen_group_ids = HashSet::new();
        for task_id in task_ids {
            let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
            if store
                .cloud_directory_group(id)
                .map_err(|error| error.to_string())?
                .is_some()
            {
                if seen_group_ids.insert(id) {
                    group_ids.push(id);
                }
            } else if seen_task_ids.insert(id) {
                ids.push(id);
            }
        }
        let mut groups = Vec::with_capacity(group_ids.len());
        for group_id in group_ids {
            let (group, tasks) = prepare_cloud_directory_group_for_purge(&store, group_id)?;
            for task in tasks {
                if seen_task_ids.insert(task.id) {
                    ids.push(task.id);
                }
            }
            groups.push(group);
        }
        purge_recycled_task_records(&store, &cloud.share_plans, &ids, delete_files)?;
        for group in groups {
            store
                .remove_cloud_directory_group(group.id)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|_| "后台删除任务中断".to_string())?
}

fn prepare_cloud_directory_group_for_purge(
    store: &TaskStore,
    group_id: Uuid,
) -> Result<(CloudDirectoryGroup, Vec<DownloadTask>), String> {
    let mut group = store
        .cloud_directory_group(group_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "目录下载任务不存在".to_string())?;
    if matches!(group.status, TaskStatus::Cancelled | TaskStatus::Recycled) {
        // Only runs after an explicit purge request; completion records/files
        // stay untouched by cancellation itself. Re-run the recycle transition
        // for already-recycled roots as well so databases written by older
        // builds cannot strand completed or failed member rows outside the
        // recycle state.
        recycle_cloud_directory_group(store, group_id)?;
        group = store
            .cloud_directory_group(group_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "目录下载任务不存在".to_string())?;
    }
    let mut tasks = Vec::new();
    for item in store
        .cloud_directory_group_items(group.id)
        .map_err(|error| error.to_string())?
    {
        let Some(task_id) = item.task_id else {
            continue;
        };
        let task = store.find(task_id).map_err(|error| error.to_string())?;
        if !matches!(task.status, TaskStatus::Recycled | TaskStatus::Cancelled) {
            return Err("只有回收站中的目录下载任务可以彻底删除".to_string());
        }
        tasks.push(task);
    }
    if tasks.is_empty() && group.status != TaskStatus::Recycled {
        return Err("目录下载任务中没有可彻底删除的文件".to_string());
    }
    Ok((group, tasks))
}

#[cfg(test)]
fn purge_cloud_directory_group(
    store: &TaskStore,
    plans: &crate::cloud::share::CloudSharePlanStore,
    group_id: Uuid,
    delete_files: bool,
) -> Result<(), String> {
    let (group, tasks) = prepare_cloud_directory_group_for_purge(store, group_id)?;
    let task_ids = tasks.iter().map(|task| task.id).collect::<Vec<_>>();
    purge_recycled_task_records(store, plans, &task_ids, delete_files)?;
    store
        .remove_cloud_directory_group(group.id)
        .map_err(|error| error.to_string())
}

fn purge_recycled_task_records(
    store: &TaskStore,
    plans: &crate::cloud::share::CloudSharePlanStore,
    ids: &[Uuid],
    delete_files: bool,
) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tasks = Vec::with_capacity(ids.len());
    for id in ids {
        let task = store.find(*id).map_err(|error| error.to_string())?;
        if !matches!(task.status, TaskStatus::Recycled | TaskStatus::Cancelled) {
            return Err("只有回收站中的任务可以彻底删除".to_string());
        }
        tasks.push(task);
    }
    let native_checkpoints = tasks
        .iter()
        .map(|task| recorded_xunlei_checkpoint_paths(store, task))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if delete_files {
        let directories = directory_payload_directories(store, &tasks)?;
        remove_directory_payload_fallbacks(store, &tasks)?;
        for id in ids {
            remove_task_payload_files(store, *id)?;
        }
        for path in &native_checkpoints {
            remove_recorded_download_file(path)?;
        }
        for directory in directories {
            remove_empty_download_directory(&directory)?;
        }
    } else {
        for path in &native_checkpoints {
            remove_recorded_download_file(path)?;
        }
    }
    let linked_cloud_tasks = tasks
        .iter()
        .filter(|task| task.source.starts_with("cloud-share://"))
        .map(|task| (task.source.clone(), task.id))
        .collect::<Vec<_>>();
    plans
        .unlink_local_tasks(&linked_cloud_tasks)
        .map_err(|error| error.to_string())?;
    if let Err(error) = store.remove_task_records(ids) {
        let message = error.to_string();
        if let Err(restore_error) = plans.link_local_tasks(&linked_cloud_tasks) {
            return Err(format!("{message}；网盘关联恢复失败：{restore_error}"));
        }
        return Err(message);
    }
    Ok(())
}

#[tauri::command]
pub async fn select_files(
    task_id: String,
    indices: Vec<u32>,
    state: State<'_, AppState>,
) -> Result<DownloadTaskPayload, String> {
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let current = state.store.find(id).map_err(|error| error.to_string())?;
    ensure_private_task_access(&state, &current)?;
    let service = state
        .runtime
        .service()
        .await
        .map_err(|error| error.to_string())?;
    let task = service
        .select_files(id, indices)
        .await
        .map_err(|error| error.to_string())?;
    task_payload(&state.store, task)
}

#[tauri::command]
pub async fn cancel_download_file(
    task_id: String,
    file_index: u32,
    state: State<'_, AppState>,
) -> Result<DownloadTaskPayload, String> {
    let id = Uuid::parse_str(&task_id).map_err(|_| "任务 ID 无效".to_string())?;
    let current = state.store.find(id).map_err(|error| error.to_string())?;
    ensure_private_task_access(&state, &current)?;
    let service = state
        .runtime
        .service()
        .await
        .map_err(|error| error.to_string())?;
    if !state
        .store
        .files(id)
        .map_err(|error| error.to_string())?
        .iter()
        .any(|file| file.index == file_index)
    {
        return Err("文件索引无效".to_string());
    }
    let task = service
        .set_file_selected(id, file_index, false)
        .await
        .map_err(|error| error.to_string())?;
    task_payload(&state.store, task)
}

#[tauri::command]
pub fn get_download_engine_state(state: State<'_, AppState>) -> crate::EngineAvailability {
    state.runtime.status()
}

#[tauri::command]
pub async fn retry_download_engine(
    state: State<'_, AppState>,
) -> Result<crate::EngineAvailability, String> {
    let result = state.runtime.retry().await;
    if result.available {
        Ok(result)
    } else {
        Err(result
            .error
            .unwrap_or_else(|| "下载引擎不可用，请稍后重试".into()))
    }
}

#[tauri::command]
pub async fn get_download_global_stat(
    state: State<'_, AppState>,
) -> Result<crate::Aria2GlobalStat, String> {
    state
        .runtime
        .global_stat()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<AppSettings, String> {
    state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())
        .map(|settings| settings.clone())
}

#[tauri::command]
pub fn get_privacy_task_visibility(
    state: State<'_, AppState>,
) -> Result<PrivacyTaskVisibility, String> {
    privacy_task_visibility(&state)
}

fn privacy_change_requires_current_password(
    was_configured: bool,
    new_password: Option<&str>,
) -> bool {
    was_configured && new_password.is_some()
}

#[tauri::command]
pub fn configure_privacy_download(
    enabled: bool,
    current_password: Option<String>,
    new_password: Option<String>,
    state: State<'_, AppState>,
) -> Result<AppSettings, String> {
    let was_configured = crate::privacy::password_is_configured(&state.ai_secrets)?;
    if privacy_change_requires_current_password(was_configured, new_password.as_deref()) {
        let current_password = current_password.ok_or_else(|| "请输入旧密码".to_string())?;
        if !state
            .privacy_session
            .unlock(&state.ai_secrets, current_password)?
        {
            return Err("旧密码不正确".into());
        }
    }
    if let Some(password) = new_password {
        crate::privacy::configure_password(&state.ai_secrets, password)?;
    }
    let configured = crate::privacy::password_is_configured(&state.ai_secrets)?;
    if enabled && !configured {
        return Err("开启隐私下载前请先设置隐私模式密码".into());
    }
    state.privacy_session.lock();
    let mut settings = state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())?;
    settings.privacy_download_enabled = enabled;
    settings.privacy_password_configured = configured;
    persist_settings(&state.settings_path, &settings)?;
    Ok(settings.clone())
}

#[tauri::command]
pub fn unlock_privacy_tasks(
    password: String,
    state: State<'_, AppState>,
) -> Result<PrivacyTaskVisibility, String> {
    if !privacy_task_visibility(&state)?.enabled {
        return Err("请先开启隐私下载".into());
    }
    if !state.privacy_session.unlock(&state.ai_secrets, password)? {
        return Err("隐私模式密码不正确".into());
    }
    privacy_task_visibility(&state)
}

#[tauri::command]
pub fn lock_privacy_tasks(state: State<'_, AppState>) -> Result<PrivacyTaskVisibility, String> {
    state.privacy_session.lock();
    privacy_task_visibility(&state)
}

fn persist_settings(path: &Path, settings: &AppSettings) -> Result<(), String> {
    let json =
        serde_json::to_vec_pretty(settings).map_err(|error| format!("无法保存设置：{error}"))?;
    fs::write(path, json).map_err(|error| format!("无法保存设置：{error}"))
}

pub fn update_launcher_visibility_from_native_menu(
    app: &AppHandle,
    visibility: LauncherVisibility,
) -> Result<(), String> {
    let state = app
        .try_state::<AppState>()
        .ok_or_else(|| "应用设置尚未初始化".to_string())?;
    let mut settings = state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())?;
    if settings.launcher_visibility == visibility {
        return Ok(());
    }
    settings.launcher_visibility = visibility;
    persist_settings(&state.settings_path, &settings)?;
    // Window creation must never run under the settings lock.
    drop(settings);
    crate::launcher_window::sync_launcher_window(app, visibility);
    for label in ["launcher", "launcher-popup"] {
        let _ = app.emit_to(label, "launcher-visibility-updated", visibility);
    }
    Ok(())
}

#[tauri::command]
pub fn update_ai_settings(
    mut settings: AiSettingsInput,
    state: State<'_, AppState>,
) -> Result<AiSettings, String> {
    settings.base_url = settings.base_url.trim().to_string();
    settings.model = settings.model.trim().to_string();
    let mut api_key = settings.api_key.trim().to_string();
    let has_new_key = !api_key.is_empty();
    let has_saved_key = state
        .ai_secrets
        .load(crate::ai::API_KEY_SECRET_NAME)
        .map_err(|_| "无法读取本机保存的 AI API Key".to_string())?
        .is_some();
    if settings.enabled {
        crate::ai::normalize_base_url(&settings.base_url)?;
        if settings.model.is_empty() {
            return Err("请选择 AI 模型".into());
        }
    }
    if has_new_key {
        state
            .ai_secrets
            .save(crate::ai::API_KEY_SECRET_NAME, api_key.as_bytes())
            .map_err(|_| "无法安全保存 AI API Key".to_string())?;
    }
    zeroize::Zeroize::zeroize(&mut api_key);
    zeroize::Zeroize::zeroize(&mut settings.api_key);
    let configured = has_saved_key || has_new_key;
    if settings.enabled && !configured {
        return Err("启用 AI 智能分析前请先配置 API Key".into());
    }
    let saved = AiSettings {
        enabled: settings.enabled,
        base_url: settings.base_url,
        model: settings.model,
        api_key_configured: configured,
    };
    let mut app_settings = state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())?;
    app_settings.ai = saved.clone();
    persist_settings(&state.settings_path, &app_settings)?;
    Ok(saved)
}

#[tauri::command]
pub async fn list_ai_models(
    settings: AiSettingsInput,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let mut api_key = settings.api_key.trim().to_string();
    if api_key.is_empty() {
        api_key = state
            .ai_secrets
            .load(crate::ai::API_KEY_SECRET_NAME)
            .map_err(|_| "无法读取本机保存的 AI API Key".to_string())?
            .and_then(|value| String::from_utf8(value).ok())
            .ok_or_else(|| "请先输入或保存 AI API Key".to_string())?;
    }
    let result = crate::ai::fetch_models(&settings.base_url, &api_key).await;
    zeroize::Zeroize::zeroize(&mut api_key);
    result
}

#[tauri::command]
pub async fn update_settings(
    mut settings: AppSettings,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    settings.download_directory = settings.download_directory.trim().to_string();
    let download_directory = PathBuf::from(&settings.download_directory);
    if settings.download_directory.is_empty() || !download_directory.is_absolute() {
        return Err("下载目录必须是绝对路径".into());
    }
    std::fs::create_dir_all(&download_directory)
        .map_err(|error| format!("无法创建下载目录：{error}"))?;
    settings.proxy.host = settings.proxy.host.trim().to_string();
    settings.proxy.username = settings.proxy.username.trim().to_string();
    proxy_url(&settings.proxy)?;
    settings.aria2.rpc_token = settings.aria2.rpc_token.trim().to_string();
    settings.aria2.validate()?;

    let previous = state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())?
        .clone();
    settings.ai = previous.ai;
    settings.privacy_download_enabled = previous.privacy_download_enabled;
    settings.privacy_password_configured = previous.privacy_password_configured;
    if settings.aria2 != previous.aria2 {
        state
            .runtime
            .reconfigure_aria2_settings(settings.aria2.clone())
            .await?;
    }
    #[cfg(target_os = "macos")]
    if settings.aria2.overall_download_limit != previous.aria2.overall_download_limit
        || settings.aria2.p2p_upload_limit != previous.aria2.p2p_upload_limit
    {
        let native = std::sync::Arc::clone(&state.xunlei_native_backend);
        let download = settings.aria2.overall_download_limit;
        let upload = settings.aria2.p2p_upload_limit;
        let sync = tauri::async_runtime::spawn_blocking(move || native.set_limits(download, upload))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        if let Err(error) = sync {
            state.runtime.reconfigure_aria2_settings(previous.aria2.clone()).await?;
            return Err(error);
        }
    }
    if settings.associate_download_links != previous.associate_download_links {
        let enabled = settings.associate_download_links;
        let data_dir = state
            .settings_path
            .parent()
            .ok_or_else(|| "数据目录不可用".to_string())?
            .to_path_buf();
        tauri::async_runtime::spawn_blocking(move || {
            crate::link_association::sync(enabled, &data_dir)
        })
        .await
        .map_err(|error| error.to_string())??;
    }
    state
        .runtime
        .set_cloud_upload_limit(settings.cloud_upload_limit);
    persist_settings(&state.settings_path, &settings)?;
    *state
        .settings
        .lock()
        .map_err(|_| "设置锁不可用".to_string())? = settings.clone();
    crate::launcher_window::sync_launcher_window(&app, settings.launcher_visibility);
    for label in ["launcher", "launcher-popup"] {
        let _ = app.emit_to(
            label,
            "launcher-visibility-updated",
            settings.launcher_visibility,
        );
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        aria2::{Aria2Error, RpcFault},
        cloud::{
            commands::CloudState, share::CloudShareEntry, share::CloudSharePlan, CloudProvider,
            CredentialStore, MemoryCredentialStore,
        },
        models::{NewCloudDirectoryGroup, NewCloudDirectoryGroupItem},
        DuplicateDownloadState, NewTask,
    };

    #[test]
    fn stages_dropped_torrent_bytes_under_application_data() {
        let root = tempfile::tempdir().unwrap();
        let staged = stage_dropped_torrent_file(
            root.path(),
            "Collection.torrent",
            &STANDARD.encode(b"torrent-bytes"),
        )
        .unwrap();

        assert!(staged.is_absolute());
        assert_eq!(
            staged.file_name().and_then(|name| name.to_str()),
            Some("Collection.torrent")
        );
        assert!(staged.starts_with(root.path().join("dropped-torrents")));
        assert_eq!(fs::read(staged).unwrap(), b"torrent-bytes");
        assert!(stage_dropped_torrent_file(root.path(), "not-a-torrent.zip", "eA==").is_err());
    }

    #[tokio::test]
    async fn staged_torrent_scans_and_loads_from_a_path_with_spaces() {
        let root = tempfile::tempdir().unwrap();
        let staged = stage_dropped_torrent_file(
            &root.path().join("Application Support"),
            "测试 file.torrent",
            &STANDARD.encode(b"torrent-bytes"),
        ).unwrap();
        let input = staged.to_str().unwrap();
        let scanned = crate::scan_download_inputs(input);
        assert_eq!(scanned.len(), 1);
        assert!(scanned[0].valid);
        assert_eq!(scanned[0].input, input);
        let loaded = crate::downloader::load_torrent_base64(&scanned[0].input).await.unwrap();
        assert_eq!(STANDARD.decode(loaded).unwrap(), b"torrent-bytes");
    }

    #[test]
    fn discard_prepared_accepts_only_pre_submission_statuses() {
        assert!(prepared_task_can_be_discarded(TaskStatus::Draft));
        assert!(prepared_task_can_be_discarded(TaskStatus::Parsing));
        assert!(prepared_task_can_be_discarded(TaskStatus::AwaitingUser));
        assert!(!prepared_task_can_be_discarded(TaskStatus::Queued));
        assert!(!prepared_task_can_be_discarded(TaskStatus::Downloading));
        assert!(!prepared_task_can_be_discarded(TaskStatus::Completed));
    }

    #[test]
    fn task_update_events_use_the_same_status_message_contract_for_files_and_directory_roots() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud://xunlei/file-id"))
            .unwrap();
        task.status_message = Some("正在使用迅雷svip加速".into());

        let file_event = task_update_payload(&store, None, task).unwrap();
        let file_json = serde_json::to_value(file_event).unwrap();

        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-directory",
                    "课程",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-file",
                    &[] as &[&str],
                    "讲义.pdf",
                    Some(100),
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let mut member = NewTask::direct("cloud://quark/remote-file");
        member.name = "讲义.pdf".into();
        let member = store.create(member).unwrap();
        let member = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Downloading,
                total_length: Some(100),
                completed_length: 50,
                progress: 50,
                status_message: Some("正在获取网盘下载地址".into()),
                ..member
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, member.id)
            .unwrap();

        let directory_event = task_update_payload_for_directory_root(&store, group.id).unwrap();
        let directory_json = serde_json::to_value(directory_event).unwrap();

        assert_eq!(file_json["statusMessage"], "正在使用迅雷svip加速");
        assert!(file_json.get("status_message").is_none());
        assert_eq!(directory_json["statusMessage"], "正在获取网盘下载地址");
        assert!(directory_json.get("status_message").is_none());
    }

    #[test]
    fn queue_projection_keeps_post_processing_notes_on_native_and_cloud_roots() {
        let store = TaskStore::open_in_memory().unwrap();
        let native = store
            .create(NewTask::direct("https://example.test/archive.zip"))
            .unwrap();
        let native = store
            .set_post_processing_note(native.id, Some("密码：88in"))
            .unwrap();
        assert_eq!(
            queue_task_payload_from_task(&store, native)
                .unwrap()
                .post_processing_note
                .as_deref(),
            Some("密码：88in")
        );

        let output = tempfile::tempdir().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-root",
                    "合集",
                    output.path(),
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-file",
                    &[] as &[&str],
                    "视频.mp4",
                    Some(42),
                )],
            )
            .unwrap();
        store
            .set_cloud_directory_group_post_processing_note(group.id, Some("完成后合并"))
            .unwrap();
        assert_eq!(
            cloud_directory_group_queue_task(&store, group.id)
                .unwrap()
                .post_processing_note
                .as_deref(),
            Some("完成后合并")
        );
    }

    #[test]
    fn confirmed_share_preflight_precedes_directory_and_file_submission() {
        let body = include_str!("commands.rs")
            .split_once("pub async fn confirm_cloud_share_download(")
            .unwrap()
            .1
            .split_once("fn mark_cloud_share_outcomes_private(")
            .unwrap()
            .0;
        let preflight = body.find("preflight_cloud_share_download").unwrap();
        assert!(
            preflight
                < body
                    .find("submit_cloud_directory_plan_with_group_event")
                    .unwrap()
        );
        assert!(preflight < body.find("submit_cloud_share_batch_with").unwrap());
    }

    #[test]
    fn automatic_share_preflight_precedes_file_submission() {
        let body = include_str!("commands.rs")
            .split_once("pub async fn parse_download(")
            .unwrap()
            .1
            .split_once("let mut outcome = prepare_download")
            .unwrap()
            .0;
        assert!(
            body.find("preflight_cloud_share_download").unwrap()
                < body.find("submit_cloud_share_file").unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn tianyi_local_state_purge_removes_only_tianyi_files_tasks_plans_and_credentials() {
        let root = tempfile::tempdir().unwrap();
        let cloud = CloudState::new(root.path().join("cloud-sessions")).unwrap();
        let tianyi_payload = root.path().join("tianyi.mp4");
        let baidu_payload = root.path().join("baidu.mp4");
        std::fs::write(&tianyi_payload, b"tianyi").unwrap();
        std::fs::write(&baidu_payload, b"baidu").unwrap();
        let profile = root.path().join("web-auth").join("tianyi");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("Cookies"), b"private browser state").unwrap();
        cloud.store.save("tianyi", b"session").unwrap();
        cloud
            .store
            .save("cloud-password-tianyi", b"password")
            .unwrap();

        let tianyi_plan = CloudSharePlan::new(
            CloudProvider::Tianyi,
            "retired-share".into(),
            None,
            vec![CloudShareEntry::new(
                "retired-share",
                "file".into(),
                "tianyi.mp4".into(),
                vec![],
                Some(6),
            )],
            serde_json::json!({}),
        );
        let tianyi_share_source = tianyi_plan.source_for("file");
        cloud.share_plans.upsert(&tianyi_plan).unwrap();

        let store = TaskStore::open_in_memory().unwrap();
        let tianyi = store
            .create(NewTask::direct("cloud://tianyi/tianyi-file"))
            .unwrap();
        store
            .upsert_files(
                tianyi.id,
                &[(0, tianyi_payload.to_string_lossy().into_owned(), 6, 6, true)],
            )
            .unwrap();
        let baidu = store
            .create(NewTask::direct("cloud://baidu/baidu-file"))
            .unwrap();
        store
            .upsert_files(
                baidu.id,
                &[(0, baidu_payload.to_string_lossy().into_owned(), 5, 5, true)],
            )
            .unwrap();

        let directory_payload = root.path().join("tianyi-directory.mp4");
        std::fs::write(&directory_payload, b"tianyi directory").unwrap();
        let directory_group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Tianyi,
                    "tianyi-directory",
                    "Tianyi directory",
                    root.path(),
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "tianyi-directory-file",
                    &[] as &[&str],
                    "tianyi-directory.mp4",
                    Some(16),
                )],
            )
            .unwrap();
        let directory_item = store
            .cloud_directory_group_items(directory_group.id)
            .unwrap()[0]
            .clone();
        let directory_task = store
            .create(NewTask::direct("https://example.test/directory-file"))
            .unwrap();
        store
            .upsert_files(
                directory_task.id,
                &[(
                    0,
                    directory_payload.to_string_lossy().into_owned(),
                    16,
                    16,
                    true,
                )],
            )
            .unwrap();
        store
            .link_cloud_directory_group_item_task(
                directory_group.id,
                directory_item.id,
                directory_task.id,
            )
            .unwrap();

        let report = purge_tianyi_local_state(&store, &cloud).unwrap();

        assert_eq!(report.removed_tasks, 2);
        assert_eq!(report.removed_directory_groups, 1);
        assert_eq!(report.removed_share_plans, 1);
        assert!(!tianyi_payload.exists());
        assert!(!directory_payload.exists());
        assert!(matches!(
            store.find(tianyi.id),
            Err(crate::StoreError::NotFound(_))
        ));
        assert!(matches!(
            store.find(directory_task.id),
            Err(crate::StoreError::NotFound(_))
        ));
        assert!(store
            .cloud_directory_group(directory_group.id)
            .unwrap()
            .is_none());
        assert!(cloud.store.load("tianyi").unwrap().is_none());
        assert!(cloud.store.load("cloud-password-tianyi").unwrap().is_none());
        assert!(cloud
            .share_plans
            .resolve_entry(&tianyi_share_source)
            .is_err());
        assert!(!profile.exists());
        assert!(baidu_payload.exists());
        assert!(store.find(baidu.id).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn tianyi_local_state_purge_keeps_everything_when_its_profile_path_is_unsafe() {
        let root = tempfile::tempdir().unwrap();
        let cloud = CloudState::new(root.path().join("cloud-sessions")).unwrap();
        let profile_parent = root.path().join("web-auth");
        std::fs::create_dir_all(&profile_parent).unwrap();
        std::fs::write(profile_parent.join("tianyi"), b"not a profile directory").unwrap();
        let payload = root.path().join("tianyi.mp4");
        std::fs::write(&payload, b"tianyi").unwrap();
        cloud.store.save("tianyi", b"session").unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud://tianyi/tianyi-file"))
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 6, 6, true)],
            )
            .unwrap();

        assert!(purge_tianyi_local_state(&store, &cloud).is_err());

        assert!(payload.exists());
        assert!(store.find(task.id).is_ok());
        assert!(cloud.store.load("tianyi").unwrap().is_some());
    }

    #[test]
    fn queue_cleanup_projection_reads_the_journal_once_for_nested_completed_files() {
        use std::sync::atomic::Ordering;
        let (plans, credentials) = crate::cloud::share::test_support::counted_plan_store();
        let plan = CloudSharePlan::new(
            CloudProvider::Baidu,
            "startup-batch".into(),
            None,
            vec![CloudShareEntry::new(
                "startup-batch",
                "entry".into(),
                "file.bin".into(),
                vec![],
                Some(1),
            )],
            serde_json::json!({}),
        );
        let source = plan.source_for("entry");
        plans.upsert(&plan).unwrap();
        plans
            .link_local_sources(std::slice::from_ref(&source))
            .unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let mut children = Vec::new();
        for _ in 0..32 {
            let task = store.create(NewTask::direct(&source)).unwrap();
            plans.link_local_task(&source, task.id).unwrap();
            children.push(
                queue_task_payload_from_task(
                    &store,
                    DownloadTask {
                        status: TaskStatus::Completed,
                        progress: 100,
                        ..task
                    },
                )
                .unwrap(),
            );
        }
        plans.record_transfer(&source, "created-id").unwrap();
        plans
            .mark_local_completion(&source, false, |_, _| true)
            .unwrap();
        let root = store
            .create(NewTask::direct("https://example.test/root"))
            .unwrap();
        let mut root = queue_task_payload_from_task(&store, root).unwrap();
        root.children = children;
        let mut queue = vec![root];
        credentials.reads.store(0, Ordering::SeqCst);

        apply_queue_cleanup_projection(&mut queue, &plans).unwrap();

        assert!(!queue[0].cleanup_pending);
        assert!(queue[0].children.iter().all(|task| task.cleanup_pending));
        assert_eq!(
            credentials.reads.load(Ordering::SeqCst),
            1,
            "initial task rendering must not decrypt the entire journal for every completed child"
        );
    }

    #[test]
    fn queue_cleanup_projection_skips_journal_for_empty_or_active_only_lists() {
        use std::sync::atomic::Ordering;
        let (plans, credentials) = crate::cloud::share::test_support::counted_plan_store();
        apply_queue_cleanup_projection(&mut [], &plans).unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud-share://baidu/not-yet-resolved/file"))
            .unwrap();
        let mut queue = vec![queue_task_payload_from_task(&store, task).unwrap()];
        apply_queue_cleanup_projection(&mut queue, &plans).unwrap();
        assert!(!queue[0].cleanup_pending);
        assert_eq!(credentials.reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn engine_submission_mapper_only_labels_structured_link_rejections_as_invalid_links() {
        assert_eq!(
            download_engine_submission_message(&DownloadServiceError::InvalidUriOption("proxy")),
            "下载引擎未能接受下载任务"
        );
        assert_eq!(
            download_engine_submission_message(&DownloadServiceError::Aria2(Aria2Error::Rpc(
                RpcFault {
                    code: -32602,
                    message: "Invalid URI".into()
                }
            ))),
            "下载引擎拒绝该链接：链接格式无效"
        );
    }

    #[test]
    fn private_visibility_requires_the_configured_password_for_this_session() {
        let credentials = MemoryCredentialStore::default();
        let session = crate::privacy::PrivacySession::default();

        crate::privacy::configure_password(&credentials, "correct horse battery staple".into())
            .unwrap();
        assert!(!session.is_unlocked());
        assert!(!session
            .unlock(&credentials, "wrong password".into())
            .unwrap());
        assert!(!session.is_unlocked());
        assert!(session
            .unlock(&credentials, "correct horse battery staple".into())
            .unwrap());
        assert!(session.is_unlocked());

        session.lock();
        assert!(!session.is_unlocked());
    }

    #[test]
    fn privacy_toggle_does_not_require_the_existing_password() {
        assert!(!privacy_change_requires_current_password(true, None));
        assert!(privacy_change_requires_current_password(
            true,
            Some("new password")
        ));
        assert!(!privacy_change_requires_current_password(
            false,
            Some("first password")
        ));
    }

    #[test]
    fn setting_task_privacy_rejects_disabled_private_downloads_and_locked_private_tasks() {
        let store = TaskStore::open_in_memory().unwrap();
        let ordinary = store
            .create(NewTask::direct("https://example.test/ordinary.zip"))
            .unwrap();
        let private = store
            .create(NewTask::direct("https://example.test/private.zip"))
            .unwrap();
        store.set_private(private.id, true).unwrap();

        assert_eq!(
            set_task_private_with_visibility(&store, ordinary.id, true, false, false).unwrap_err(),
            "请先在设置中开启隐私下载"
        );
        assert_eq!(
            set_task_private_with_visibility(&store, private.id, false, true, false).unwrap_err(),
            "隐私任务已锁定，请先解锁"
        );
    }

    #[test]
    fn failed_cloud_authentication_task_projects_a_reauthorization_action() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud://baidu/blocked-file"))
            .unwrap();
        store
            .fail_with_code(
                task.id,
                Some("web_auth_required"),
                "网盘要求完成官方操作验证后重试",
            )
            .unwrap();

        let projected = list_queue_tasks_for_scope(&store, None).unwrap();

        assert_eq!(projected.len(), 1);
        assert_eq!(
            projected[0].failure_code.as_deref(),
            Some("web_auth_required")
        );
        assert!(projected[0].reauthorization_required);
        assert_eq!(
            serde_json::to_value(&projected[0]).unwrap()["reauthorization_required"],
            true
        );
    }

    #[test]
    fn paused_cloud_directory_projects_its_authorization_recovery_state() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Xunlei,
                    "cloud://xunlei/root",
                    "迅雷目录",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "file-1",
                    &["迅雷目录"],
                    "cloud.bin",
                    Some(100),
                )],
            )
            .unwrap();
        let item = store.cloud_directory_group_items(group.id).unwrap()[0].clone();
        let mut task = store
            .create(NewTask::direct("cloud://xunlei/file-1"))
            .unwrap();
        task.status = TaskStatus::Paused;
        task.failure_code = Some("authorization_pending".into());
        task.error = Some("恢复网盘登录后继续下载".into());
        let task = store.apply_transfer_snapshot(task).unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();
        store.set_cloud_directory_paused(group.id, true).unwrap();

        let root = cloud_directory_group_queue_task(&store, group.id).unwrap();

        assert_eq!(root.status, TaskStatus::Paused);
        assert_eq!(root.failure_code.as_deref(), Some("authorization_pending"));
        assert_eq!(root.error.as_deref(), Some("恢复网盘登录后继续下载"));
        assert!(root.reauthorization_required);
    }

    #[test]
    fn locked_queue_projection_excludes_private_tasks_and_private_directory_roots() {
        let store = TaskStore::open_in_memory().unwrap();
        let ordinary = store
            .create(NewTask::direct("https://example.test/ordinary.zip"))
            .unwrap();
        let private = store
            .create(NewTask::direct("https://example.test/private.zip"))
            .unwrap();
        store.set_private(private.id, true).unwrap();
        let private_group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "private-directory",
                    "私密目录",
                    "C:\\Downloads",
                )
                .with_private_download(true),
                &[],
            )
            .unwrap();

        let locked = list_queue_tasks_for_scope_with_privacy(&store, None, false).unwrap();
        assert_eq!(
            locked
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec![ordinary.id.to_string()]
        );
        assert!(!locked
            .iter()
            .any(|task| task.id == private_group.id.to_string()));

        let unlocked = list_queue_tasks_for_scope_with_privacy(&store, None, true).unwrap();
        assert_eq!(unlocked.len(), 3);
        assert!(unlocked
            .iter()
            .any(|task| task.id == private.id.to_string() && task.is_private));
        assert!(unlocked
            .iter()
            .any(|task| task.id == private_group.id.to_string() && task.is_private));
    }

    #[tokio::test]
    async fn background_queue_read_preserves_scopes() {
        let store = Arc::new(TaskStore::open_in_memory().unwrap());
        let active = store
            .create(NewTask::direct("https://example.test/active.zip"))
            .unwrap();
        let completed = store
            .create(NewTask::direct("https://example.test/done.zip"))
            .unwrap();
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                ..completed
            })
            .unwrap();
        let failed = store
            .create(NewTask::direct("https://example.test/failed.zip"))
            .unwrap();
        store.fail(failed.id, "fixture").unwrap();
        let active_tasks =
            list_queue_tasks_in_background(Arc::clone(&store), None, Some("active".into()), true)
                .await
                .unwrap();
        assert_eq!(active_tasks.len(), 1);
        assert_eq!(active_tasks[0].id, active.id.to_string());
        let terminal =
            list_queue_tasks_in_background(Arc::clone(&store), None, Some("terminal".into()), true)
                .await
                .unwrap();
        assert_eq!(terminal.len(), 2);
        assert!(terminal
            .iter()
            .any(|task| task.status == TaskStatus::Completed));
        assert!(terminal
            .iter()
            .any(|task| task.status == TaskStatus::Failed));
    }

    #[tokio::test]
    async fn background_queue_read_yields_while_database_is_locked() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.sqlite");
        let store = Arc::new(TaskStore::open_path(&path).unwrap());
        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let mut read = Box::pin(list_queue_tasks_in_background(store, None, None, true));
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(100), &mut read).await;
        // The caller can keep servicing events even while SQLite is waiting.
        blocker.execute_batch("ROLLBACK").unwrap();
        assert!(
            waiting.is_err(),
            "database waiting must yield to the caller"
        );
        assert!(read.await.unwrap().is_empty());
    }

    #[test]
    fn cloud_directory_group_projection_nests_linked_and_virtual_leaves() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Baidu,
                    "remote-season-1",
                    "Season 1",
                    "C:\\Downloads",
                ),
                &[
                    NewCloudDirectoryGroupItem::new("active", &["Extras"], "active.mkv", Some(100)),
                    NewCloudDirectoryGroupItem::new(
                        "complete",
                        &["Extras"],
                        "complete.mkv",
                        Some(200),
                    ),
                    NewCloudDirectoryGroupItem::new("failed", &["Extras"], "failed.mkv", Some(300)),
                ],
            )
            .unwrap();
        let items = store.cloud_directory_group_items(group.id).unwrap();

        let active = store
            .create(NewTask::direct("https://example.test/active"))
            .unwrap();
        let active = store
            .apply_transfer_snapshot(DownloadTask {
                name: "active.mkv".into(),
                status: TaskStatus::Downloading,
                progress: 25,
                total_length: Some(100),
                completed_length: 25,
                download_speed: 10,
                eta_seconds: Some(75),
                connections: 2,
                ..active
            })
            .unwrap();
        let complete = store
            .create(NewTask::direct("https://example.test/complete"))
            .unwrap();
        let complete = store
            .apply_transfer_snapshot(DownloadTask {
                name: "complete.mkv".into(),
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(200),
                completed_length: 200,
                ..complete
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, items[0].id, active.id)
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, items[1].id, complete.id)
            .unwrap();
        store
            .fail_cloud_directory_group_item(
                group.id,
                items[2].id,
                "submit_failed",
                "不能加入下载队列",
            )
            .unwrap();

        let active_scope =
            project_cloud_directory_groups(&store, store.list().unwrap(), Some("active")).unwrap();

        assert_eq!(active_scope.len(), 1);
        assert_eq!(active_scope[0].kind, "folder");
        assert_eq!(active_scope[0].name, "Season 1");
        assert_eq!(active_scope[0].status, TaskStatus::Downloading);
        assert_eq!(active_scope[0].total_length, Some(600));
        assert_eq!(active_scope[0].completed_length, 225);
        assert_eq!(active_scope[0].download_speed, 10);
        assert_eq!(active_scope[0].eta_seconds, Some(75));
        assert_eq!(active_scope[0].connections, 2);
        assert_eq!(active_scope[0].progress, 37);
        assert_eq!(active_scope[0].children[0].name, "Extras");
        assert_eq!(active_scope[0].children[0].status, TaskStatus::Downloading);
        assert_eq!(active_scope[0].children[0].progress, 37);
        assert_eq!(active_scope[0].children[0].total_length, Some(600));
        assert_eq!(active_scope[0].children[0].completed_length, 225);
        assert_eq!(active_scope[0].children[0].download_speed, 10);
        assert_eq!(active_scope[0].children[0].children.len(), 3);
        assert!(active_scope[0].children[0]
            .children
            .iter()
            .any(|child| child.status == TaskStatus::Failed));

        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(100),
                completed_length: 100,
                download_speed: 0,
                connections: 0,
                ..active
            })
            .unwrap();

        let failed_active_scope =
            project_cloud_directory_groups(&store, store.list().unwrap(), Some("active")).unwrap();
        assert!(failed_active_scope.is_empty());
        let terminal_scope =
            project_cloud_directory_groups(&store, store.list().unwrap(), Some("terminal"))
                .unwrap();
        assert_eq!(terminal_scope.len(), 1);
        assert_eq!(terminal_scope[0].status, TaskStatus::Failed);
    }

    #[test]
    fn cancelled_cloud_directory_group_is_not_projected_as_queued_by_an_active_child() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Xunlei,
                    "cloud://xunlei/directory",
                    "迅雷目录",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "cloud://xunlei/file",
                    &["迅雷目录"],
                    "file.zip",
                    Some(100),
                )],
            )
            .unwrap();
        let item = store.cloud_directory_group_items(group.id).unwrap()[0].clone();
        let task = store
            .create(NewTask::direct("cloud://xunlei/file"))
            .unwrap();
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Queued,
                total_length: Some(100),
                ..task.clone()
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();
        store.cancel_cloud_directory_group(group.id).unwrap();

        let root = cloud_directory_group_queue_task(&store, group.id).unwrap();

        assert_eq!(root.status, TaskStatus::Cancelled);
    }

    #[test]
    fn cloud_directory_resume_keeps_aggregate_state_across_child_submission_phases() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Pikpak,
                    "remote",
                    "Series",
                    "C:\\Downloads",
                ),
                &[
                    NewCloudDirectoryGroupItem::new(
                        "preparing",
                        &["Series"],
                        "next.bin",
                        Some(100),
                    ),
                    NewCloudDirectoryGroupItem::new("active", &["Series"], "active.bin", Some(900)),
                    NewCloudDirectoryGroupItem::new(
                        "pending",
                        &["Series"],
                        "pending.bin",
                        Some(1000),
                    ),
                ],
            )
            .unwrap();
        let items = store.cloud_directory_group_items(group.id).unwrap();
        let preparing = store
            .create(NewTask::direct("cloud://pikpak/preparing"))
            .unwrap();
        let active = store
            .create(NewTask::direct("cloud://pikpak/active"))
            .unwrap();
        for (item, id) in [(&items[0], preparing.id), (&items[1], active.id)] {
            store
                .link_cloud_directory_group_item_task(group.id, item.id, id)
                .unwrap();
        }
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Downloading,
                completed_length: 450,
                total_length: Some(900),
                download_speed: 128,
                ..active.clone()
            })
            .unwrap();
        for phase in [
            TaskStatus::Parsing,
            TaskStatus::Queued,
            TaskStatus::Paused,
            TaskStatus::Downloading,
        ] {
            let updated = store
                .apply_transfer_snapshot(DownloadTask {
                    status: phase,
                    ..preparing.clone()
                })
                .unwrap();
            let TaskUpdatePayload::Queue(root) =
                task_update_payload(&store, None, updated).unwrap()
            else {
                panic!("expected root");
            };
            assert_eq!(root.id, group.id.to_string());
            assert_eq!(
                root.status,
                TaskStatus::Downloading,
                "child phase {phase:?} must not override active siblings"
            );
            assert_eq!(
                (
                    root.total_length,
                    root.completed_length,
                    root.progress,
                    root.download_speed
                ),
                (Some(2000), 450, 22, 128)
            );
        }
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                completed_length: 900,
                total_length: Some(900),
                ..active
            })
            .unwrap();
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Parsing,
                ..preparing
            })
            .unwrap();
        let root = cloud_directory_group_queue_task(&store, group.id).unwrap();
        assert_eq!(
            root.status,
            TaskStatus::Downloading,
            "preparing remaining members is still the same running directory"
        );
        assert_eq!(
            (root.total_length, root.completed_length, root.progress),
            (Some(2000), 900, 45)
        );
    }

    #[test]
    fn cloud_directory_projection_does_not_mix_new_membership_with_older_leaf_snapshot() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Pikpak,
                    "remote",
                    "Series",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "file",
                    &["Series"],
                    "file.bin",
                    Some(100),
                )],
            )
            .unwrap();
        let old_leaves = store.list().unwrap();
        let task = store
            .create(NewTask::direct("cloud://pikpak/file"))
            .unwrap();
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                total_length: Some(100),
                completed_length: 100,
                ..task.clone()
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(
                group.id,
                store.cloud_directory_group_items(group.id).unwrap()[0].id,
                task.id,
            )
            .unwrap();
        let roots = project_cloud_directory_groups(&store, old_leaves, None).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].status, TaskStatus::Completed);
        assert_eq!(roots[0].completed_length, 100);
        assert!(roots[0].children[0].error.is_none());
    }

    #[test]
    fn cloud_directory_group_progress_tracks_every_nested_file() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Baidu,
                    "remote",
                    "Series",
                    "C:\\Downloads",
                ),
                &[
                    NewCloudDirectoryGroupItem::new("small", &["Season"], "small.mp4", Some(100)),
                    NewCloudDirectoryGroupItem::new(
                        "large",
                        &["Season", "Extras"],
                        "large.mp4",
                        Some(900),
                    ),
                ],
            )
            .unwrap();
        let items = store.cloud_directory_group_items(group.id).unwrap();
        let initial = cloud_directory_group_queue_task(&store, group.id).unwrap();
        assert_eq!(initial.children[0].progress, 0);
        assert_eq!(initial.children[0].status, TaskStatus::Queued);
        assert_eq!(initial.children[0].children[1].progress, 0);
        let season_id = initial.children[0].id.clone();
        let extras_id = initial.children[0].children[1].id.clone();

        let small = store
            .create(NewTask::direct("https://example.test/small"))
            .unwrap();
        let large = store
            .create(NewTask::direct("https://example.test/large"))
            .unwrap();
        for (remote, task_id) in [("small", small.id), ("large", large.id)] {
            let item = items
                .iter()
                .find(|item| item.remote_file_id == remote)
                .unwrap();
            store
                .link_cloud_directory_group_item_task(group.id, item.id, task_id)
                .unwrap();
        }
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(100),
                completed_length: 100,
                ..small
            })
            .unwrap();
        for (status, bytes, expected_root, expected_inner) in [
            (TaskStatus::Downloading, 0, 10, 0),
            (TaskStatus::Downloading, 450, 55, 50),
            (TaskStatus::Paused, 450, 55, 50),
            (TaskStatus::Failed, 450, 55, 50),
            (TaskStatus::Completed, 900, 100, 100),
        ] {
            let updated = store
                .apply_transfer_snapshot(DownloadTask {
                    status,
                    total_length: Some(900),
                    completed_length: bytes,
                    progress: expected_inner,
                    ..large.clone()
                })
                .unwrap();
            let TaskUpdatePayload::Queue(root) =
                task_update_payload(&store, None, updated).unwrap()
            else {
                panic!("directory updates must publish the aggregate root");
            };
            let season = &root.children[0];
            let extras = &season.children[1];
            assert_eq!(root.progress, expected_root);
            assert_eq!(season.progress, expected_root);
            assert_eq!(extras.progress, expected_inner);
            assert_eq!(season.status, status);
            assert_eq!(extras.status, status);
            assert_eq!(season.total_length, Some(1000));
            assert_eq!(season.completed_length, 100 + bytes);
            assert_eq!(extras.total_length, Some(900));
            assert_eq!(extras.completed_length, bytes);
            assert_eq!(season.id, season_id);
            assert_eq!(extras.id, extras_id);
            assert_eq!(season.name, "Season");
            assert_eq!(extras.name, "Extras");
            assert_eq!(
                season.completed_at.is_some(),
                status == TaskStatus::Completed
            );
        }
    }

    #[test]
    fn cloud_directory_queue_serializes_transfer_metrics_with_download_task_field_names() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Pikpak,
                    "cloud-share://pikpak/share/root",
                    "分享目录",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "cloud-share://pikpak/share/file",
                    &["分享目录"],
                    "视频.mp4",
                    Some(1_000),
                )],
            )
            .unwrap();
        let item = store.cloud_directory_group_items(group.id).unwrap()[0].clone();
        let task = store
            .create(NewTask::direct("cloud-share://pikpak/share/file"))
            .unwrap();
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Downloading,
                progress: 25,
                total_length: Some(1_000),
                completed_length: 250,
                download_speed: 64,
                eta_seconds: Some(12),
                ..task.clone()
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();

        let json =
            serde_json::to_value(cloud_directory_group_queue_task(&store, group.id).unwrap())
                .unwrap();

        for node in [&json, &json["children"][0]] {
            assert_eq!(node["total_length"], 1_000);
            assert_eq!(node["completed_length"], 250);
            assert_eq!(node["download_speed"], 64);
            assert_eq!(node["eta_seconds"], 12);
            assert!(node.get("totalLength").is_none());
            assert!(node.get("completedLength").is_none());
            assert!(node.get("downloadSpeed").is_none());
            assert!(node.get("etaSeconds").is_none());
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cancelled_directory_can_purge_records_while_retaining_completed_files() {
        for include_completed in [false, true] {
            let output = tempfile::tempdir().unwrap();
            let store = TaskStore::open_in_memory().unwrap();
            let group = store
                .create_cloud_directory_group(
                    NewCloudDirectoryGroup::new(
                        CloudProvider::Quark,
                        "remote",
                        "Series",
                        output.path(),
                    ),
                    &[
                        NewCloudDirectoryGroupItem::new(
                            "first",
                            &["Series"],
                            "first.mp4",
                            Some(100),
                        ),
                        NewCloudDirectoryGroupItem::new(
                            "pending",
                            &["Series"],
                            "pending.mp4",
                            Some(200),
                        ),
                    ],
                )
                .unwrap();
            let file = output.path().join("first.mp4");
            fs::write(&file, b"downloaded file").unwrap();
            if include_completed {
                let task = store
                    .create(NewTask::direct("https://example.test/first"))
                    .unwrap();
                store
                    .apply_transfer_snapshot(DownloadTask {
                        status: TaskStatus::Completed,
                        ..task.clone()
                    })
                    .unwrap();
                store
                    .link_cloud_directory_group_item_task(
                        group.id,
                        store.cloud_directory_group_items(group.id).unwrap()[0].id,
                        task.id,
                    )
                    .unwrap();
            }
            crate::cloud::commands::cancel_cloud_directory_group_with(
                &store,
                group.id,
                |_| async { panic!("no active member") },
            )
            .await
            .unwrap();
            let plans = crate::cloud::share::CloudSharePlanStore::new(
                SystemCredentialStore::new(output.path().to_path_buf()).unwrap(),
            );
            purge_cloud_directory_group(&store, &plans, group.id, false).unwrap();
            assert!(store.cloud_directory_group(group.id).unwrap().is_none());
            assert!(store.list().unwrap().is_empty());
            assert_eq!(fs::read(file).unwrap(), b"downloaded file");
        }
    }

    #[test]
    fn cloud_directory_group_projection_ignores_unknown_size_progress() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Baidu,
                    "remote-directory",
                    "Unknown size",
                    "C:\\Downloads",
                ),
                &[
                    NewCloudDirectoryGroupItem::new(
                        "known",
                        &[] as &[&str],
                        "known.bin",
                        Some(100),
                    ),
                    NewCloudDirectoryGroupItem::new("unknown", &[] as &[&str], "unknown.bin", None),
                ],
            )
            .unwrap();
        let items = store.cloud_directory_group_items(group.id).unwrap();
        let known = store
            .create(NewTask::direct("https://example.test/known"))
            .unwrap();
        let known = store
            .apply_transfer_snapshot(DownloadTask {
                total_length: Some(100),
                completed_length: 25,
                progress: 25,
                status: TaskStatus::Downloading,
                ..known
            })
            .unwrap();
        let unknown = store
            .create(NewTask::direct("https://example.test/unknown"))
            .unwrap();
        let unknown = store
            .apply_transfer_snapshot(DownloadTask {
                total_length: None,
                completed_length: 900,
                progress: 0,
                status: TaskStatus::Downloading,
                ..unknown
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, items[0].id, known.id)
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, items[1].id, unknown.id)
            .unwrap();

        let projected =
            project_cloud_directory_groups(&store, store.list().unwrap(), Some("active")).unwrap();

        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].total_length, Some(100));
        assert_eq!(projected[0].completed_length, 25);
        assert_eq!(projected[0].progress, 25);

        // Known-size bytes reaching 100% must not hide an unfinished unknown-size file.
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                completed_length: 100,
                ..known
            })
            .unwrap();
        for (status, expected_progress) in [
            (TaskStatus::Downloading, 99),
            (TaskStatus::Failed, 99),
            (TaskStatus::Completed, 100),
        ] {
            store
                .apply_transfer_snapshot(DownloadTask {
                    status,
                    ..unknown.clone()
                })
                .unwrap();
            let root = cloud_directory_group_queue_task(&store, group.id).unwrap();
            assert_eq!(root.progress, expected_progress);
            assert_eq!(root.status, status);
        }
    }

    #[test]
    fn cloud_directory_group_uses_aria_file_lengths_when_the_task_total_is_missing() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-directory",
                    "YM-JFSMBYHS",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-file",
                    &["YM-JFSMBYHS"],
                    "episode.mp4",
                    None,
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let task = store
            .create(NewTask::direct("cloud-share://quark/share/episode"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Downloading,
                progress: 25,
                total_length: None,
                completed_length: 250,
                ..task
            })
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(
                    0,
                    "C:\\Downloads\\YM-JFSMBYHS\\episode.mp4".into(),
                    1_000,
                    250,
                    true,
                )],
            )
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();

        let projected =
            project_cloud_directory_groups(&store, store.list().unwrap(), Some("active")).unwrap();

        assert_eq!(projected[0].total_length, Some(1_000));
        assert_eq!(projected[0].completed_length, 250);
        assert_eq!(projected[0].progress, 25);
    }

    #[test]
    fn legacy_share_directory_group_promotes_its_parsed_root_in_the_queue() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-directory",
                    "夸克分享目录",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-file",
                    &["夸克分享目录", "YM-JFSMBYHS"],
                    "episode.mp4",
                    Some(1_000),
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let task = store
            .create(NewTask::direct("cloud-share://quark/share/episode"))
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();

        let projected =
            project_cloud_directory_groups(&store, store.list().unwrap(), None).unwrap();

        assert_eq!(projected[0].name, "YM-JFSMBYHS");
        assert_eq!(projected[0].children.len(), 1);
        assert_eq!(projected[0].children[0].kind, "file");
    }

    #[test]
    fn completed_cloud_directory_member_clears_stale_failure_and_uses_planned_size() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Baidu,
                    "remote-directory",
                    "Season 1",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-episode",
                    &[] as &[&str],
                    "episode.mp4",
                    Some(1_572_864),
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let task = store
            .create(NewTask::direct("cloud-share://baidu/share/episode"))
            .unwrap();
        let completed = store
            .apply_transfer_snapshot(DownloadTask {
                name: "episode.mp4".into(),
                status: TaskStatus::Completed,
                progress: 100,
                error: Some("网盘分享下载失败，请重试或重新解析分享链接".into()),
                total_length: None,
                completed_length: 0,
                ..task
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, completed.id)
            .unwrap();

        let projected =
            project_cloud_directory_groups(&store, store.list().unwrap(), Some("terminal"))
                .unwrap();
        let leaf = &projected[0].children[0];

        assert_eq!(projected[0].status, TaskStatus::Completed);
        assert_eq!(leaf.status, TaskStatus::Completed);
        assert_eq!(leaf.error, None);
        assert_eq!(leaf.total_length, Some(1_572_864));
        assert_eq!(leaf.completed_length, 1_572_864);
    }

    #[test]
    fn completed_cloud_directory_member_uses_its_downloaded_file_size_when_metadata_is_missing() {
        let root = std::env::temp_dir().join(format!("cloud-directory-size-{}", Uuid::new_v4()));
        let payload_path = root.join("Season 1").join("Extras").join("episode.mp4");
        std::fs::create_dir_all(payload_path.parent().unwrap()).unwrap();
        std::fs::write(&payload_path, vec![0_u8; 1_572_864]).unwrap();

        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Pikpak,
                    "remote-directory",
                    "Season 1",
                    &root,
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-episode",
                    &["Season 1", "Extras"],
                    "episode.mp4",
                    None,
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let task = store
            .create(NewTask::direct("cloud-share://pikpak/share/episode"))
            .unwrap();
        let completed = store
            .apply_transfer_snapshot(DownloadTask {
                name: "episode.mp4".into(),
                status: TaskStatus::Completed,
                progress: 100,
                total_length: None,
                completed_length: 0,
                ..task
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, completed.id)
            .unwrap();

        let projected =
            project_cloud_directory_groups(&store, store.list().unwrap(), Some("terminal"))
                .unwrap();
        let leaf = &projected[0].children[0].children[0];

        assert_eq!(leaf.total_length, Some(1_572_864));
        assert_eq!(leaf.completed_length, 1_572_864);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_member_update_emits_its_recursive_group_root() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-directory",
                    "课程",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-file",
                    &["资料"],
                    "讲义.pdf",
                    Some(100),
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let mut new_task = NewTask::direct("cloud://quark/remote-file");
        new_task.name = "讲义.pdf".into();
        let task = store.create(new_task).unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Downloading,
                total_length: Some(100),
                completed_length: 50,
                progress: 50,
                ..task
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();

        let TaskUpdatePayload::Queue(payload) = task_update_payload(&store, None, task).unwrap()
        else {
            panic!("a cloud-directory member update must not emit a top-level leaf");
        };
        assert_eq!(payload.id, group.id.to_string());
        assert_eq!(payload.progress, 50);
        assert_eq!(payload.children[0].name, "资料");
        assert_eq!(payload.children[0].children[0].name, "讲义.pdf");
    }

    #[test]
    fn failed_share_directory_stays_one_root_after_moving_to_recycle() {
        let store = TaskStore::open_in_memory().unwrap();
        let share_hash = "a".repeat(64);
        let first_hash = "b".repeat(64);
        let second_hash = "c".repeat(64);
        let first_source = format!("cloud-share://xunlei/{share_hash}/{first_hash}");
        let second_source = format!("cloud-share://xunlei/{share_hash}/{second_hash}");
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Xunlei,
                    first_source.clone(),
                    "迅雷云盘分享目录",
                    "C:\\Downloads",
                ),
                &[
                    NewCloudDirectoryGroupItem::new(
                        first_source.clone(),
                        &["迅雷云盘分享目录"],
                        "first.bin",
                        Some(10),
                    ),
                    NewCloudDirectoryGroupItem::new(
                        second_source.clone(),
                        &["迅雷云盘分享目录", "nested"],
                        "second.bin",
                        Some(20),
                    ),
                ],
            )
            .unwrap();
        let items = store.cloud_directory_group_items(group.id).unwrap();
        let first = store.create(NewTask::direct(first_source)).unwrap();
        let first = store
            .fail(first.id, "provider rejected the transfer")
            .unwrap();
        let second = store.create(NewTask::direct(second_source)).unwrap();
        let second = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(20),
                completed_length: 20,
                ..second
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, items[0].id, first.id)
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, items[1].id, second.id)
            .unwrap();

        let failed = list_queue_tasks_for_scope(&store, Some("terminal")).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, group.id.to_string());
        assert_eq!(failed[0].status, TaskStatus::Failed);
        assert_eq!(failed[0].children.len(), 2);

        let recycled = recycle_cloud_directory_group(&store, group.id).unwrap();
        assert_eq!(recycled.id, group.id.to_string());
        assert_eq!(recycled.status, TaskStatus::Recycled);
        let recycle_scope = list_queue_tasks_for_scope(&store, Some("terminal")).unwrap();
        assert_eq!(recycle_scope.len(), 1);
        assert_eq!(recycle_scope[0].id, group.id.to_string());
        assert_eq!(recycle_scope[0].status, TaskStatus::Recycled);
        assert_eq!(recycle_scope[0].children.len(), 2);
    }

    #[test]
    fn virtual_failed_directory_group_moves_to_recycle_without_local_members() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-wallpapers",
                    "夸克精选壁纸",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-wallpaper",
                    &["夸克精选壁纸"],
                    "wallpaper.jpg",
                    Some(100),
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        store
            .fail_cloud_directory_group_item(
                group.id,
                item.id,
                "duplicate_active",
                "已存在下载任务，未重复入队",
            )
            .unwrap();

        let recycled = recycle_cloud_directory_group(&store, group.id).unwrap();

        assert_eq!(recycled.status, TaskStatus::Recycled);
        let (_, local_members) = prepare_cloud_directory_group_for_purge(&store, group.id).unwrap();
        assert!(local_members.is_empty());
    }

    #[test]
    fn cancelled_virtual_directory_recycles_without_queued_children() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "root",
                    "Series",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "pending",
                    &["Series", "Season"],
                    "pending.mp4",
                    Some(100),
                )],
            )
            .unwrap();
        store.cancel_cloud_directory_group(group.id).unwrap();
        let recycled = recycle_cloud_directory_group(&store, group.id).unwrap();
        assert_eq!(recycled.status, TaskStatus::Recycled);
        assert_eq!(recycled.children[0].status, TaskStatus::Recycled);
        assert_eq!(
            recycled.children[0].children[0].status,
            TaskStatus::Recycled
        );
    }

    #[test]
    fn local_action_outcome_for_a_directory_member_returns_its_group_root() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-directory",
                    "课程",
                    "C:\\Downloads",
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-file",
                    &[] as &[&str],
                    "讲义.pdf",
                    Some(100),
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let task = store
            .create(NewTask::direct("cloud://quark/remote-file"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Paused,
                total_length: Some(100),
                completed_length: 40,
                progress: 40,
                ..task
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();

        let action = task_action_outcome(&store, task, None).unwrap();
        let root = action
            .queue_task
            .expect("a local directory member action must return its group root");
        assert_eq!(root.id, group.id.to_string());
        assert_eq!(root.progress, 40);
        assert_eq!(root.children[0].name, "remote-file");
    }

    #[cfg(windows)]
    #[test]
    fn cloud_directory_group_recycles_and_purges_its_real_and_virtual_members() {
        let output = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-directory",
                    "Season 1",
                    output.path(),
                ),
                &[
                    NewCloudDirectoryGroupItem::new(
                        "remote-complete",
                        &[] as &[&str],
                        "episode.mkv",
                        Some(100),
                    ),
                    NewCloudDirectoryGroupItem::new(
                        "remote-failed",
                        &[] as &[&str],
                        "missing.mkv",
                        Some(100),
                    ),
                ],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let task = store
            .create(NewTask::direct("cloud://quark/remote-complete"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(100),
                completed_length: 100,
                ..task
            })
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();
        let failed = store.cloud_directory_group_items(group.id).unwrap()[1].id;
        store
            .fail_cloud_directory_group_item(group.id, failed, "provider_error", "link failed")
            .unwrap();

        assert_eq!(
            cloud_directory_group_output_root(&group).unwrap(),
            output.path().join("Season 1")
        );
        let recycled = recycle_cloud_directory_group(&store, group.id).unwrap();
        assert_eq!(recycled.status, TaskStatus::Recycled);
        assert_eq!(
            cloud_directory_group_queue_task(&store, group.id)
                .unwrap()
                .status,
            TaskStatus::Recycled
        );

        let plans = crate::cloud::share::CloudSharePlanStore::new(
            SystemCredentialStore::new(output.path().to_path_buf()).unwrap(),
        );
        purge_cloud_directory_group(&store, &plans, group.id, false).unwrap();
        assert!(store.cloud_directory_group(group.id).unwrap().is_none());
        assert!(matches!(
            store.find(task.id),
            Err(crate::StoreError::NotFound(_))
        ));
    }

    #[test]
    fn unsubmitted_active_task_can_be_force_cancelled_without_engine() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("not-a-download-link"))
            .unwrap();
        assert_eq!(task.status, TaskStatus::Parsing);
        assert!(task.aria_gid.is_none());

        let cancelled = force_cancel_unsubmitted_task(&store, task).unwrap();

        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        assert_eq!(cancelled.download_speed, 0);
        assert_eq!(cancelled.upload_speed, 0);
        assert_eq!(
            store.find(cancelled.id).unwrap().status,
            TaskStatus::Cancelled
        );
    }

    #[test]
    fn permanent_deletion_preparation_recycles_completed_and_failed_tasks() {
        let store = TaskStore::open_in_memory().unwrap();
        let completed = store
            .create(NewTask::direct("https://example.test/completed.zip"))
            .unwrap();
        let completed = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                ..completed
            })
            .unwrap();
        let failed = store
            .create(NewTask::direct("https://example.test/failed.zip"))
            .unwrap();
        let failed = store.fail(failed.id, "fixture").unwrap();

        recycle_local_task_for_permanent_deletion(&store, completed.id).unwrap();
        recycle_local_task_for_permanent_deletion(&store, failed.id).unwrap();

        assert_eq!(
            store.find(completed.id).unwrap().status,
            TaskStatus::Recycled
        );
        assert_eq!(store.find(failed.id).unwrap().status, TaskStatus::Recycled);
    }

    #[cfg(windows)]
    #[test]
    fn permanent_purge_unlinks_each_recycled_cloud_share_task_after_sqlite_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let plans = crate::cloud::share::CloudSharePlanStore::new(
            SystemCredentialStore::new(directory.path().to_path_buf()).unwrap(),
        );
        let plan = CloudSharePlan::new(
            CloudProvider::Baidu,
            "purge".into(),
            None,
            vec![CloudShareEntry::new(
                "purge",
                "entry".into(),
                "file.bin".into(),
                vec![],
                Some(1),
            )],
            serde_json::json!({}),
        );
        let source = plan.source_for("entry");
        plans.upsert(&plan).unwrap();
        plans
            .link_local_sources(std::slice::from_ref(&source))
            .unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store.create(NewTask::direct(&source)).unwrap();
        let task = store.fail(task.id, "fixture").unwrap();
        let task = store.transition(task.id, TaskStatus::Recycled).unwrap();
        plans.link_local_task(&source, task.id).unwrap();

        purge_recycled_task_records(&store, &plans, &[task.id], false).unwrap();

        assert!(matches!(
            store.find(task.id),
            Err(crate::StoreError::NotFound(_))
        ));
        assert!(plans
            .resolve_entry(&source)
            .unwrap()
            .plan
            .linked_local_tasks
            .is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_xunlei_checkpoint_cleanup_uses_only_recorded_payload_paths() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("file.zip");
        let checkpoint = root.path().join(".file.zip.js");
        fs::write(&checkpoint, b"checkpoint").unwrap();
        fs::write(root.path().join(".other.zip.js"), b"unrelated").unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud://xunlei/file-id"))
            .unwrap();
        task.aria_gid = Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:7".into());
        store
            .upsert_files(task.id, &[(0, output.to_string_lossy().into_owned(), 7, 2, true)])
            .unwrap();
        assert_eq!(
            recorded_xunlei_checkpoint_paths(&store, &task).unwrap(),
            vec![checkpoint]
        );
        task.completed_at = Some(1);
        assert!(recorded_xunlei_checkpoint_paths(&store, &task).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn permanent_purge_always_removes_xunlei_checkpoints_but_respects_payload_choice() {
        for delete_files in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let plans = crate::cloud::share::CloudSharePlanStore::new(
                SystemCredentialStore::new(directory.path().join("credentials")).unwrap(),
            );
            let store = TaskStore::open_in_memory().unwrap();
            let output = directory.path().join("file.zip");
            let checkpoint = PathBuf::from(format!("{}.xltd", output.display()));
            let checkpoint_config = PathBuf::from(format!("{}.xltd.cfg", output.display()));
            fs::write(&output, b"payload").unwrap();
            fs::write(&checkpoint, b"checkpoint").unwrap();
            fs::write(&checkpoint_config, b"config").unwrap();

            let mut task = store
                .create(NewTask::direct("cloud://xunlei/file-id"))
                .unwrap();
            task.aria_gid = Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:7".into());
            task = store.apply_transfer_snapshot(task).unwrap();
            store
                .upsert_files(
                    task.id,
                    &[(0, output.to_string_lossy().into_owned(), 7, 7, true)],
                )
                .unwrap();
            let task = store.fail(task.id, "fixture").unwrap();
            let task = store.transition(task.id, TaskStatus::Recycled).unwrap();

            purge_recycled_task_records(&store, &plans, &[task.id], delete_files).unwrap();

            assert_eq!(output.exists(), !delete_files);
            assert!(!checkpoint.exists());
            assert!(!checkpoint_config.exists());
        }
    }
    #[cfg(windows)]
    #[test]
    fn permanent_purge_keeps_a_task_when_its_cloud_share_plan_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let plans = crate::cloud::share::CloudSharePlanStore::new(
            SystemCredentialStore::new(directory.path().to_path_buf()).unwrap(),
        );
        let source = format!("cloud-share://baidu/{}/{}", "a".repeat(64), "b".repeat(64));
        let store = TaskStore::open_in_memory().unwrap();
        let task = store.create(NewTask::direct(&source)).unwrap();
        let task = store.fail(task.id, "fixture").unwrap();
        let task = store.transition(task.id, TaskStatus::Recycled).unwrap();

        assert!(purge_recycled_task_records(&store, &plans, &[task.id], false).is_err());

        assert!(store.find(task.id).is_ok());
    }

    #[test]
    fn background_purge_batch_removes_only_its_supplied_recycled_records() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(TaskStore::open_in_memory().unwrap());
        let cloud = Arc::new(
            crate::cloud::commands::CloudState::new(directory.path().join("cloud")).unwrap(),
        );
        let first = store
            .create(NewTask::direct("https://example.test/first"))
            .unwrap();
        let first = store.fail(first.id, "fixture").unwrap();
        let first = store.transition(first.id, TaskStatus::Recycled).unwrap();
        let second = store
            .create(NewTask::direct("https://example.test/second"))
            .unwrap();
        let second = store.fail(second.id, "fixture").unwrap();
        let second = store.transition(second.id, TaskStatus::Recycled).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(purge_recycled_task_batch(
                Arc::clone(&store),
                Arc::clone(&cloud),
                vec![first.id.to_string()],
                false,
            ))
            .unwrap();

        assert!(matches!(
            store.find(first.id),
            Err(crate::StoreError::NotFound(_))
        ));
        assert!(store.find(second.id).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn background_purge_batch_deletes_legacy_recycled_directory_with_terminal_members() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(TaskStore::open_in_memory().unwrap());
        let cloud = Arc::new(
            crate::cloud::commands::CloudState::new(directory.path().join("cloud")).unwrap(),
        );
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote-directory",
                    "Series",
                    directory.path(),
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-file",
                    &["Series"],
                    "episode.mkv",
                    Some(7),
                )],
            )
            .unwrap();
        let item = store.cloud_directory_group_items(group.id).unwrap()[0].clone();
        let output_root = directory.path().join("Series");
        let payload = output_root.join("episode.mkv");
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::write(&payload, b"payload").unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/episode.mkv"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(7),
                completed_length: 7,
                ..task
            })
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 7, 7, true)],
            )
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();
        // Older builds could persist only the directory root as recycled,
        // leaving completed or failed member rows in their terminal state.
        store.recycle_cloud_directory_group(group.id).unwrap();
        assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Completed);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(purge_recycled_task_batch(
                Arc::clone(&store),
                Arc::clone(&cloud),
                vec![group.id.to_string()],
                true,
            ))
            .unwrap();

        assert!(!payload.exists());
        assert!(!output_root.exists());
        assert!(store.cloud_directory_group(group.id).unwrap().is_none());
        assert!(matches!(
            store.find(task.id),
            Err(crate::StoreError::NotFound(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn background_purge_removes_xunlei_directory_after_native_checkpoints() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(TaskStore::open_in_memory().unwrap());
        let cloud = Arc::new(
            crate::cloud::commands::CloudState::new(directory.path().join("cloud")).unwrap(),
        );
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Xunlei,
                    "remote-directory",
                    "Series",
                    directory.path(),
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "remote-file",
                    &["Series"],
                    "episode.mkv",
                    Some(7),
                )],
            )
            .unwrap();
        let item = store.cloud_directory_group_items(group.id).unwrap()[0].clone();
        let output_root = directory.path().join("Series");
        let payload = output_root.join("episode.mkv");
        let checkpoint = PathBuf::from(format!("{}.xltd", payload.display()));
        let checkpoint_config = PathBuf::from(format!("{}.xltd.cfg", payload.display()));
        fs::create_dir_all(&output_root).unwrap();
        fs::write(&payload, b"payload").unwrap();
        fs::write(&checkpoint, b"checkpoint").unwrap();
        fs::write(&checkpoint_config, b"config").unwrap();
        let task = store
            .create(NewTask::direct("cloud://xunlei/file-id"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                aria_gid: Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:7".into()),
                total_length: Some(7),
                completed_length: 7,
                ..task
            })
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 7, 7, true)],
            )
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();
        store.recycle_cloud_directory_group(group.id).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(purge_recycled_task_batch(
                Arc::clone(&store),
                Arc::clone(&cloud),
                vec![group.id.to_string()],
                true,
            ))
            .unwrap();

        assert!(!payload.exists());
        assert!(!checkpoint.exists());
        assert!(!checkpoint_config.exists());
        assert!(!output_root.exists());
        assert!(store.cloud_directory_group(group.id).unwrap().is_none());
        assert!(matches!(
            store.find(task.id),
            Err(crate::StoreError::NotFound(_))
        ));
    }

    #[test]
    fn disabled_proxy_does_not_leak_process_proxy_settings_into_aria2() {
        assert_eq!(proxy_url(&ProxySettings::default()).unwrap(), None);
    }

    #[test]
    fn automatic_share_submission_preserves_the_existing_duplicate_outcome() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud-share://baidu/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"))
            .unwrap();
        let action = task_action_outcome(
            &store,
            task.clone(),
            Some(DuplicateDownloadState::ExistingActive),
        )
        .unwrap();

        let ParseDownloadOutcome::Task(parsed) = share_action_parse_outcome(action) else {
            panic!("an automatic share must return the ordinary task parse branch")
        };
        assert_eq!(parsed.task.id, task.id);
        assert_eq!(
            parsed.duplicate,
            Some(DuplicateDownloadState::ExistingActive)
        );
        assert!(!parsed.requires_confirmation);
        assert!(parsed.resources.is_empty());
    }

    #[test]
    fn cloud_share_resubmission_reserves_the_original_task_before_engine_or_provider_work() {
        let body = include_str!("commands.rs")
            .split_once("pub(crate) async fn resubmit_task(")
            .expect("resubmit task exists")
            .1
            .split_once("fn task_output_directory")
            .expect("resubmit task has a stable end")
            .0;
        let reserve = body
            .find("reserve_cloud_share_resubmission")
            .expect("cloud-share retry must reserve its original task");
        let engine = body
            .find(".service()")
            .expect("resubmit obtains the engine");
        let dispatch = body
            .find("queue_cloud_dispatch")
            .expect("resubmit reaches the shared cloud dispatch");
        assert!(reserve < engine);
        assert!(reserve < dispatch);

        let dispatch_body = include_str!("cloud/commands/download.rs")
            .split_once("pub async fn queue_cloud_dispatch(")
            .expect("queue cloud dispatch exists")
            .1
            .split_once("fn check_dispatch_owner")
            .expect("queue cloud dispatch has a stable end")
            .0;
        assert!(dispatch_body.contains("resolve_stable_cloud_share_download"));
    }

    #[test]
    fn cloud_resubmission_reapplies_runtime_retry_intent_before_dispatch() {
        let body = include_str!("commands.rs")
            .split_once("pub(crate) async fn resubmit_task(")
            .expect("resubmit task exists")
            .1
            .split_once("fn task_output_directory")
            .expect("resubmit task has a stable end")
            .0;
        let retry_intent = body
            .find("retry_submission_options")
            .expect("cloud retry must restore its runtime duplicate intent");
        let dispatch = body
            .find("queue_cloud_dispatch")
            .expect("cloud retry reaches the shared dispatch");
        assert!(retry_intent < dispatch);

        let queue = include_str!("cloud/commands/download.rs")
            .split_once("pub async fn queue_cloud_dispatch(")
            .expect("queue cloud dispatch exists")
            .1
            .split_once("fn check_dispatch_owner")
            .expect("queue cloud dispatch has a stable end")
            .0;
        assert!(queue.contains("prepare_expected_with_retry_intent"));
    }

    #[test]
    fn cloud_retry_requests_checkpoint_verified_continuation() {
        let store = TaskStore::open_in_memory().unwrap();
        for source in [
            "cloud://quark/file-id",
            "cloud-share://quark/share-id/file-id",
        ] {
            let task = store.create(NewTask::direct(source)).unwrap();
            let options = cloud_retry_submission_options(&task);
            assert!(options.continue_download, "{source}");
            assert!(!options.allow_overwrite, "{source}");
        }

        let task = store
            .create(NewTask::direct("https://example.test/file"))
            .unwrap();
        assert!(!cloud_retry_submission_options(&task).continue_download);
    }

    #[test]
    fn signed_url_failures_bypass_duplicate_confirmation_for_cloud_tasks() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud-share://quark/share/file"))
            .unwrap();
        for code in ["6", "8", "22", "24"] {
            assert!(cloud_download_requires_signed_url_refresh(&DownloadTask {
                aria_error_code: Some(code.into()),
                ..task.clone()
            }));
        }

        assert!(!cloud_download_requires_signed_url_refresh(&DownloadTask {
            source: "https://example.test/file".into(),
            ..task.clone()
        }));
        assert!(!cloud_download_requires_signed_url_refresh(&DownloadTask {
            aria_error_code: Some("9".into()),
            ..task
        }));
    }

    #[tokio::test]
    async fn cloud_share_batch_keeps_successes_before_and_after_a_typed_failure() {
        let store = TaskStore::open_in_memory().unwrap();
        let first = task_action_outcome(
            &store,
            store
                .create(NewTask::direct("https://example.test/first"))
                .unwrap(),
            None,
        )
        .unwrap();
        let last = task_action_outcome(
            &store,
            store
                .create(NewTask::direct("https://example.test/last"))
                .unwrap(),
            None,
        )
        .unwrap();
        let mut submissions = std::collections::VecDeque::from([
            Ok(first),
            Err(crate::cloud::QuarkError::WebAuthorizationRequired),
            Ok(last),
        ]);

        let batch = submit_cloud_share_batch_with(
            vec![
                ("first".into(), "source-1".into()),
                ("failed".into(), "source-2".into()),
                ("last".into(), "source-3".into()),
            ],
            |_| std::future::ready(submissions.pop_front().unwrap()),
        )
        .await;

        assert_eq!(batch.outcomes.len(), 2);
        assert_eq!(batch.failures.len(), 1);
        assert_eq!(batch.failures[0].selected_id, "failed");
        assert_eq!(batch.failures[0].code, "web_auth_required");
        assert_eq!(
            batch.failures[0].message,
            "此登录方式需要网页验证，请点击网页授权完成登录"
        );
    }

    #[cfg(windows)]
    #[test]
    fn cloud_share_resubmission_reservation_is_idempotent_for_the_original_task() {
        let directory = tempfile::tempdir().unwrap();
        let plans = crate::cloud::share::CloudSharePlanStore::new(
            SystemCredentialStore::new(directory.path().to_path_buf()).unwrap(),
        );
        let plan = CloudSharePlan::new(
            CloudProvider::Baidu,
            "retry-reservation".into(),
            None,
            vec![CloudShareEntry::new(
                "retry-reservation",
                "entry".into(),
                "file.bin".into(),
                vec![],
                Some(1),
            )],
            serde_json::json!({}),
        );
        let source = plan.source_for("entry");
        plans.upsert(&plan).unwrap();
        let tasks = TaskStore::open_in_memory().unwrap();
        let task = tasks.create(NewTask::direct(&source)).unwrap();

        reserve_cloud_share_resubmission(&plans, &task).unwrap();
        reserve_cloud_share_resubmission(&plans, &task).unwrap();

        let current = plans.resolve_entry(&source).unwrap().plan;
        let linked = current.linked_local_tasks.values().next().unwrap();
        assert_eq!(linked.len(), 1);
        assert!(linked.contains(&task.id));
    }

    #[cfg(windows)]
    #[test]
    fn task_payload_projects_cleanup_pending_from_the_exact_plan_journal() {
        let directory = tempfile::tempdir().unwrap();
        let plans = crate::cloud::share::CloudSharePlanStore::new(
            SystemCredentialStore::new(directory.path().to_path_buf()).unwrap(),
        );
        let plan = CloudSharePlan::new(
            CloudProvider::Baidu,
            "payload-cleanup".into(),
            None,
            vec![CloudShareEntry::new(
                "payload-cleanup",
                "entry".into(),
                "file.bin".into(),
                vec![],
                Some(1),
            )],
            serde_json::json!({}),
        );
        let source = plan.source_for("entry");
        plans.upsert(&plan).unwrap();
        plans
            .link_local_sources(std::slice::from_ref(&source))
            .unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store.create(NewTask::direct(&source)).unwrap();
        plans.link_local_task(&source, task.id).unwrap();
        plans.record_transfer(&source, "created-id").unwrap();
        plans
            .mark_local_completion(&source, false, |_, _| true)
            .unwrap();
        let completed = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                ..task
            })
            .unwrap();

        let payload = task_payload_with_cleanup(&store, Some(&plans), completed).unwrap();
        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json["cleanup_pending"], true);

        let ordinary = store
            .create(NewTask::direct("https://example.test/file.bin"))
            .unwrap();
        let ordinary = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                ..ordinary
            })
            .unwrap();
        let ordinary = task_payload_with_cleanup(&store, Some(&plans), ordinary).unwrap();
        assert!(!ordinary.cleanup_pending);
    }

    #[cfg(windows)]
    #[test]
    fn task_payload_does_not_clear_cleanup_retry_when_the_plan_state_is_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let plans = crate::cloud::share::CloudSharePlanStore::new(
            SystemCredentialStore::new(directory.path().to_path_buf()).unwrap(),
        );
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct(&format!(
                "cloud-share://baidu/{}/{}",
                "a".repeat(64),
                "b".repeat(64)
            )))
            .unwrap();
        let completed = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                ..task
            })
            .unwrap();

        assert!(task_payload_with_cleanup(&store, Some(&plans), completed).is_err());
    }

    #[test]
    fn authenticated_proxy_is_validated_and_encoded_for_aria2() {
        let proxy = ProxySettings {
            enabled: true,
            protocol: "https".into(),
            host: "127.0.0.1".into(),
            port: 1080,
            username: "user@example.com".into(),
            password: "p@ss word".into(),
        };
        let value = proxy_url(&proxy).unwrap().unwrap();
        assert_eq!(
            value,
            "https://user%40example.com:p%40ss%20word@127.0.0.1:1080/"
        );
    }

    #[test]
    fn unbound_torrent_confirmation_is_rejected_instead_of_becoming_uri_download() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut new_task = NewTask::direct("C:\\Downloads\\game.torrent");
        new_task.kind = "torrent".into();
        let task = store.create(new_task).unwrap();

        assert!(unbound_torrent_confirmation_error(&task)
            .unwrap()
            .contains("尚未完成解析"));
    }

    #[test]
    fn failed_retry_with_payload_without_control_file_requires_overwrite() {
        let root = std::env::temp_dir().join(format!("duplicate-payload-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let payload = root.join("fixture.bin");
        std::fs::write(&payload, [1_u8, 2, 3]).unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/fixture.bin"))
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 3, 1, true)],
            )
            .unwrap();
        let failed = store.fail(task.id, "previous transfer stopped").unwrap();

        assert_eq!(
            retry_duplicate_state(&store, &failed).unwrap(),
            Some(DuplicateDownloadState::OverwriteRequired)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_task_directory_comes_from_the_recorded_output_path() {
        let root = std::env::temp_dir().join(format!("task-directory-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let payload = root.join("fixture.bin");
        std::fs::write(&payload, b"payload").unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/fixture.bin"))
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 7, 7, true)],
            )
            .unwrap();

        assert_eq!(task_output_directory(&store, task.id).unwrap(), root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_only_a_completed_single_existing_task_file_for_associated_application() {
        let root = std::env::temp_dir().join(format!("task-open-file-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let payload = root.join("notes.txt");
        std::fs::write(&payload, b"notes").unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("https://example.com/notes.txt"))
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 5, 5, true)],
            )
            .unwrap();

        assert_eq!(
            task_associated_file_path(&store, task.id).unwrap_err(),
            "仅已完成任务可以打开文件"
        );

        let completed = store
            .apply_transfer_snapshot(DownloadTask {
                name: "notes.txt".into(),
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(5),
                completed_length: 5,
                ..task
            })
            .unwrap();
        assert_eq!(
            task_associated_file_path(&store, completed.id).unwrap(),
            payload
        );

        std::fs::remove_file(&payload).unwrap();
        assert_eq!(
            task_associated_file_path(&store, completed.id).unwrap_err(),
            "下载文件已不存在，无法打开"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opens_first_image_only_when_every_selected_file_is_an_image() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("cover.jpg");
        let second = root.path().join("back.png");
        let note = root.path().join("notes.txt");
        for path in [&first, &second, &note] {
            std::fs::write(path, b"fixture").unwrap();
        }
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/images"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                ..task
            })
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[
                    (0, first.to_string_lossy().into_owned(), 7, 7, true),
                    (1, second.to_string_lossy().into_owned(), 7, 7, true),
                    (2, note.to_string_lossy().into_owned(), 7, 0, false),
                ],
            )
            .unwrap();
        assert_eq!(task_associated_file_path(&store, task.id).unwrap(), first);

        store
            .upsert_files(
                task.id,
                &[
                    (0, first.to_string_lossy().into_owned(), 7, 7, true),
                    (1, note.to_string_lossy().into_owned(), 7, 7, true),
                ],
            )
            .unwrap();
        assert!(task_associated_file_path(&store, task.id).is_err());
    }

    #[test]
    fn opens_first_image_from_a_completed_cloud_directory_group() {
        let root = tempfile::tempdir().unwrap();
        let paths = [root.path().join("cover.jpg"), root.path().join("back.png")];
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(CloudProvider::Quark, "album-id", "album", root.path()),
                &[
                    NewCloudDirectoryGroupItem::new("first", &[] as &[&str], "cover.jpg", Some(7)),
                    NewCloudDirectoryGroupItem::new("second", &["nested"], "back.png", Some(7)),
                ],
            )
            .unwrap();
        for (item, path) in store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .iter()
            .zip(&paths)
        {
            std::fs::write(path, b"fixture").unwrap();
            let task = store
                .create(NewTask::direct(format!(
                    "cloud://quark/{}",
                    item.remote_file_id
                )))
                .unwrap();
            let task = store
                .apply_transfer_snapshot(DownloadTask {
                    status: TaskStatus::Completed,
                    ..task
                })
                .unwrap();
            store
                .upsert_files(
                    task.id,
                    &[(0, path.to_string_lossy().into_owned(), 7, 7, true)],
                )
                .unwrap();
            store
                .link_cloud_directory_group_item_task(group.id, item.id, task.id)
                .unwrap();
        }

        assert_eq!(
            task_associated_file_path(&store, group.id).unwrap(),
            paths[0]
        );
    }

    #[test]
    fn collects_audio_files_from_a_completed_cloud_directory_group_for_playback() {
        let root = tempfile::tempdir().unwrap();
        let paths = [
            root.path().join("track.mp3"),
            root.path().join("bonus.flac"),
        ];
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(CloudProvider::Quark, "music-id", "music", root.path()),
                &[
                    NewCloudDirectoryGroupItem::new("first", &[] as &[&str], "track.mp3", Some(7)),
                    NewCloudDirectoryGroupItem::new("second", &["nested"], "bonus.flac", Some(7)),
                ],
            )
            .unwrap();
        for (item, path) in store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .iter()
            .zip(&paths)
        {
            std::fs::write(path, b"fixture").unwrap();
            let task = store
                .create(NewTask::direct(format!(
                    "cloud://quark/{}",
                    item.remote_file_id
                )))
                .unwrap();
            let task = store
                .apply_transfer_snapshot(DownloadTask {
                    status: TaskStatus::Completed,
                    ..task
                })
                .unwrap();
            store
                .upsert_files(
                    task.id,
                    &[(0, path.to_string_lossy().into_owned(), 7, 7, true)],
                )
                .unwrap();
            store
                .link_cloud_directory_group_item_task(group.id, item.id, task.id)
                .unwrap();
        }

        assert_eq!(task_video_paths(&store, group.id).unwrap(), paths);
    }

    #[test]
    fn associated_application_rejects_relative_and_unsupported_file_paths() {
        let root = std::env::temp_dir().join(format!("task-open-file-policy-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("notes.txt#1.exe");
        std::fs::write(&executable, b"binary").unwrap();
        let store = TaskStore::open_in_memory().unwrap();

        let relative_task = store
            .create(NewTask::direct("https://example.com/notes.txt"))
            .unwrap();
        store
            .upsert_files(relative_task.id, &[(0, "notes.txt".into(), 5, 5, true)])
            .unwrap();
        let relative_task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(5),
                completed_length: 5,
                ..relative_task
            })
            .unwrap();
        assert_eq!(
            task_associated_file_path(&store, relative_task.id).unwrap_err(),
            "下载文件路径不是本地绝对路径，无法打开"
        );

        let executable_task = store
            .create(NewTask::direct("https://example.com/notes.txt#1.exe"))
            .unwrap();
        store
            .upsert_files(
                executable_task.id,
                &[(0, executable.to_string_lossy().into_owned(), 6, 6, true)],
            )
            .unwrap();
        let executable_task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(6),
                completed_length: 6,
                ..executable_task
            })
            .unwrap();
        assert_eq!(
            task_associated_file_path(&store, executable_task.id).unwrap_err(),
            "该文件类型不支持使用系统默认应用打开"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_existing_media_files_are_collected_for_default_playback() {
        let root = std::env::temp_dir().join(format!("task-videos-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let video = root.join("episode.MKV");
        let audio = root.join("commentary.MP3");
        let document = root.join("notes.txt");
        let skipped_video = root.join("trailer.mp4");
        std::fs::write(&video, b"video").unwrap();
        std::fs::write(&audio, b"audio").unwrap();
        std::fs::write(&document, b"notes").unwrap();
        std::fs::write(&skipped_video, b"trailer").unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/episode.MKV"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                ..task
            })
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[
                    (0, video.to_string_lossy().into_owned(), 5, 5, true),
                    (1, audio.to_string_lossy().into_owned(), 5, 5, true),
                    (2, document.to_string_lossy().into_owned(), 5, 5, true),
                    (3, skipped_video.to_string_lossy().into_owned(), 7, 0, false),
                ],
            )
            .unwrap();

        assert_eq!(
            task_video_paths(&store, task.id).unwrap(),
            vec![video, audio]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn permanent_recycle_cleanup_removes_only_recorded_payload_and_sidecar() {
        let root = std::env::temp_dir().join(format!("recycle-cleanup-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let payload = root.join("fixture.bin");
        let sidecar = std::path::PathBuf::from(format!("{}.aria2", payload.display()));
        let neighbor = root.join("keep.bin");
        std::fs::write(&payload, b"payload").unwrap();
        std::fs::write(&sidecar, b"control").unwrap();
        std::fs::write(&neighbor, b"keep").unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/fixture.bin"))
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 7, 7, true)],
            )
            .unwrap();

        remove_task_payload_files(&store, task.id).unwrap();

        assert!(!payload.exists());
        assert!(!sidecar.exists());
        assert!(neighbor.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn magnet_preview_paths_are_persisted_as_absolute_download_paths() {
        let root = Path::new(r"D:\Downloads");

        assert_eq!(
            magnet_preview_file_path(
                root,
                "Black.Myth.Wukong.Digital.Deluxe.Edition-InsaneRamZes",
                "b1.exe",
                true,
            )
            .unwrap(),
            root.join("Black.Myth.Wukong.Digital.Deluxe.Edition-InsaneRamZes")
                .join("b1.exe")
        );
        assert!(magnet_preview_file_path(root, "safe", r"..\outside.exe", true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn magnet_preview_paths_follow_the_confirmed_download_directory() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask {
                source: "magnet:?xt=urn:btih:rebase-preview".into(),
                name: "Collection".into(),
                kind: "magnet".into(),
                is_private: false,
            })
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[
                    (1, r"D:\Old\Collection\Disc 1\a.bin".into(), 1, 0, true),
                    (2, r"D:\Old\Collection\b.bin".into(), 2, 0, true),
                ],
            )
            .unwrap();

        rebase_magnet_preview_files(&store, &task, Path::new(r"E:\Chosen")).unwrap();

        let paths = store
            .files(task.id)
            .unwrap()
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                r"E:\Chosen\Collection\Disc 1\a.bin",
                r"E:\Chosen\Collection\b.bin"
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn magnet_preview_paths_accept_aria2_sanitized_task_directory_names() {
        let task_name = "Collection*";
        let stored = Path::new(r"D:\Old\Collection_\Disc 1\a.bin");

        assert_eq!(
            magnet_preview_relative_path(task_name, stored, true).unwrap(),
            PathBuf::from(r"Disc 1\a.bin")
        );
        assert_eq!(
            magnet_preview_file_path(Path::new(r"E:\Chosen"), task_name, r"Disc 1\a.bin", true,)
                .unwrap(),
            PathBuf::from(r"E:\Chosen\Collection_\Disc 1\a.bin")
        );
    }

    #[cfg(windows)]
    #[test]
    fn unsafe_magnet_preview_paths_fail_before_persisting_any_rebased_files() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask {
                source: "magnet:?xt=urn:btih:unsafe-preview".into(),
                name: "Collection".into(),
                kind: "magnet".into(),
                is_private: false,
            })
            .unwrap();
        let original = r"D:\Old\Collection\..\outside.bin";
        store
            .upsert_files(
                task.id,
                &[
                    (1, original.into(), 1, 0, true),
                    (2, r"D:\Old\Collection\safe.bin".into(), 1, 0, true),
                ],
            )
            .unwrap();

        assert!(rebase_magnet_preview_files(&store, &task, Path::new(r"E:\Chosen")).is_err());
        assert_eq!(store.files(task.id).unwrap()[0].path, original);
    }

    #[cfg(windows)]
    #[test]
    fn permanent_recycle_cleanup_accepts_legacy_zero_byte_metadata_preview_paths() {
        let directory = tempfile::tempdir().unwrap();
        let plans = crate::cloud::share::CloudSharePlanStore::new(
            SystemCredentialStore::new(directory.path().to_path_buf()).unwrap(),
        );
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask {
                source: "magnet:?xt=urn:btih:metadata-preview".into(),
                name: "metadata-preview".into(),
                kind: "magnet".into(),
                is_private: false,
            })
            .unwrap();
        store
            .upsert_files(task.id, &[(1, "b1.exe".into(), 142_100_000_000, 0, true)])
            .unwrap();
        let task = store.fail(task.id, "已取消").unwrap();
        let task = store.transition(task.id, TaskStatus::Recycled).unwrap();

        purge_recycled_task_records(&store, &plans, &[task.id], true).unwrap();

        assert!(matches!(
            store.find(task.id),
            Err(crate::StoreError::NotFound(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn permanent_recycle_cleanup_directory_recovers_missing_file_records() {
        for delete_files in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let plans = crate::cloud::share::CloudSharePlanStore::new(
                SystemCredentialStore::new(directory.path().join("credentials")).unwrap(),
            );
            let store = TaskStore::open_in_memory().unwrap();
            let group = store
                .create_cloud_directory_group(
                    NewCloudDirectoryGroup::new(
                        CloudProvider::Quark,
                        "remote",
                        "Series",
                        directory.path(),
                    ),
                    &[
                        NewCloudDirectoryGroupItem::new(
                            "first",
                            &["Series", "Season"],
                            "first.mp4",
                            Some(7),
                        ),
                        NewCloudDirectoryGroupItem::new(
                            "pending",
                            &["Series", "Season"],
                            "pending.mp4",
                            Some(4),
                        ),
                    ],
                )
                .unwrap();
            let folder = directory.path().join("Series/Season");
            fs::create_dir_all(&folder).unwrap();
            let payload = folder.join("first.mp4");
            let sidecar = folder.join("first.mp4.aria2");
            let neighbor = folder.join("pending.mp4");
            fs::write(&payload, b"payload").unwrap();
            fs::write(&sidecar, b"control").unwrap();
            fs::write(&neighbor, b"keep").unwrap();
            let task = store
                .create(NewTask::direct("https://example.test/first"))
                .unwrap();
            store
                .apply_transfer_snapshot(DownloadTask {
                    status: TaskStatus::Cancelled,
                    completed_length: 7,
                    ..task.clone()
                })
                .unwrap();
            store
                .link_cloud_directory_group_item_task(
                    group.id,
                    store.cloud_directory_group_items(group.id).unwrap()[0].id,
                    task.id,
                )
                .unwrap();
            // No engine file row survived, but the durable member still identifies its payload.
            assert!(store.files(task.id).unwrap().is_empty());
            store.recycle_cloud_directory_group(group.id).unwrap();

            purge_cloud_directory_group(&store, &plans, group.id, delete_files).unwrap();

            assert_eq!(
                payload.exists(),
                !delete_files,
                "directory member payload must follow delete_files"
            );
            assert_eq!(sidecar.exists(), !delete_files);
            assert_eq!(
                fs::read(neighbor).unwrap(),
                b"keep",
                "unsubmitted members are not owned downloads"
            );
            assert!(store.cloud_directory_group(group.id).unwrap().is_none());
            assert!(store.list().unwrap().is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn permanent_recycle_cleanup_directory_protects_files_and_keeps_failed_purges_retryable() {
        for scenario in [
            "renamed",
            "unstarted",
            "submitted_only",
            "blocked",
            "junction",
            "missing",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let plans = crate::cloud::share::CloudSharePlanStore::new(
                SystemCredentialStore::new(directory.path().join("credentials")).unwrap(),
            );
            let store = TaskStore::open_in_memory().unwrap();
            let group = store
                .create_cloud_directory_group(
                    NewCloudDirectoryGroup::new(
                        CloudProvider::Pikpak,
                        "remote",
                        "Series",
                        directory.path(),
                    ),
                    &[NewCloudDirectoryGroupItem::new(
                        "first",
                        &["Series"],
                        "first.mp4",
                        Some(7),
                    )],
                )
                .unwrap();
            let task = store
                .create(NewTask::direct("https://example.test/first"))
                .unwrap();
            store
                .apply_transfer_snapshot(DownloadTask {
                    status: TaskStatus::Cancelled,
                    completed_length: if matches!(scenario, "unstarted" | "submitted_only") {
                        0
                    } else {
                        7
                    },
                    aria_gid: (scenario == "submitted_only")
                        .then(|| "submitted-but-never-started".into()),
                    ..task.clone()
                })
                .unwrap();
            let item = store.cloud_directory_group_items(group.id).unwrap()[0].clone();
            store
                .link_cloud_directory_group_item_task(group.id, item.id, task.id)
                .unwrap();
            store.recycle_cloud_directory_group(group.id).unwrap();
            let folder = directory.path().join("Series");
            let payload = folder.join("first.mp4");
            let actual = directory.path().join("renamed.mp4");
            let outside = tempfile::tempdir().unwrap();
            if scenario == "junction" {
                fs::write(outside.path().join("first.mp4"), b"outside").unwrap();
                let result = std::process::Command::new("cmd.exe")
                    .args(["/d", "/c", "mklink", "/J"])
                    .arg(&folder)
                    .arg(outside.path())
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
            } else if scenario != "missing" {
                fs::create_dir_all(&folder).unwrap();
                if scenario == "blocked" {
                    fs::create_dir(&payload).unwrap();
                } else {
                    fs::write(&payload, b"keep").unwrap();
                }
            }
            if scenario == "renamed" {
                fs::write(&actual, b"payload").unwrap();
                store
                    .upsert_files(
                        task.id,
                        &[(0, actual.to_string_lossy().into_owned(), 7, 7, true)],
                    )
                    .unwrap();
            }
            let result = purge_cloud_directory_group(&store, &plans, group.id, true);
            match scenario {
                "renamed" => {
                    result.unwrap();
                    assert!(!actual.exists());
                    assert_eq!(fs::read(&payload).unwrap(), b"keep");
                }
                "missing" => {
                    result.unwrap();
                }
                _ => {
                    assert!(
                        result.is_err(),
                        "{scenario} must retain records instead of claiming success"
                    );
                    assert!(store.find(task.id).is_ok());
                    assert!(store.cloud_directory_group(group.id).unwrap().is_some());
                    if scenario == "junction" {
                        assert_eq!(
                            fs::read(outside.path().join("first.mp4")).unwrap(),
                            b"outside"
                        );
                        // Remove only this test-created junction, never its target contents.
                        fs::remove_dir(&folder).unwrap();
                    } else if scenario == "blocked" {
                        fs::remove_dir(&payload).unwrap();
                        fs::write(&payload, b"payload").unwrap();
                        purge_cloud_directory_group(&store, &plans, group.id, true).unwrap();
                        assert!(!payload.exists());
                    } else {
                        assert_eq!(fs::read(&payload).unwrap(), b"keep");
                    }
                }
            }
        }
    }

    #[test]
    fn permanent_recycle_cleanup_directory_rejects_unsafe_member_paths() {
        let directory = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "remote",
                    "Series",
                    directory.path(),
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "first",
                    &["Series"],
                    "first.mp4",
                    Some(7),
                )],
            )
            .unwrap();
        let item = store.cloud_directory_group_items(group.id).unwrap()[0].clone();
        for name in [
            "..",
            "../outside.mp4",
            "C:\\outside.mp4",
            "file:stream",
            "NUL",
            "first.mp4.",
        ] {
            let mut unsafe_item = item.clone();
            unsafe_item.output_name = name.to_string();
            assert!(
                directory_payload_fallback_path(&group, &unsafe_item).is_err(),
                "{name}"
            );
        }
        let mut wrong_root = item;
        wrong_root.relative_directory = vec!["Other".into()];
        assert!(directory_payload_fallback_path(&group, &wrong_root).is_err());
        let mut relative_group = group;
        relative_group.output_directory = PathBuf::from("Downloads");
        assert!(directory_payload_fallback_path(&relative_group, &wrong_root).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn permanent_recycle_cleanup_directory_rejects_redirected_output_base() {
        for nested in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let junction = directory.path().join("Downloads");
            let base = if nested {
                junction.join("Nested")
            } else {
                junction.clone()
            };
            let result = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(&junction)
                .arg(outside.path())
                .output()
                .unwrap();
            assert!(result.status.success());
            fs::create_dir_all(base.join("Series")).unwrap();
            let payload = base.join("Series/first.mp4");
            fs::write(&payload, b"outside").unwrap();
            let store = TaskStore::open_in_memory().unwrap();
            let group = store
                .create_cloud_directory_group(
                    NewCloudDirectoryGroup::new(CloudProvider::Quark, "remote", "Series", &base),
                    &[NewCloudDirectoryGroupItem::new(
                        "first",
                        &["Series"],
                        "first.mp4",
                        Some(7),
                    )],
                )
                .unwrap();
            let item = store.cloud_directory_group_items(group.id).unwrap()[0].clone();
            let result = directory_payload_fallback_path(&group, &item);
            assert_eq!(fs::read(&payload).unwrap(), b"outside");
            fs::remove_dir(&junction).unwrap();
            assert!(
                result.is_err(),
                "output root and its ancestors cannot redirect deletion"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn permanent_recycle_cleanup_removes_metadata_only_task_without_a_local_path() {
        let directory = tempfile::tempdir().unwrap();
        let plans = crate::cloud::share::CloudSharePlanStore::new(
            SystemCredentialStore::new(directory.path().to_path_buf()).unwrap(),
        );
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("magnet:?xt=urn:btih:metadata-only"))
            .unwrap();
        store
            .upsert_files(task.id, &[(0, String::new(), 0, 0, true)])
            .unwrap();
        let task = store.fail(task.id, "获取文件信息失败").unwrap();
        let task = store.transition(task.id, TaskStatus::Recycled).unwrap();

        purge_recycled_task_records(&store, &plans, &[task.id], true).unwrap();

        assert!(matches!(
            store.find(task.id),
            Err(crate::StoreError::NotFound(_))
        ));
    }
}
