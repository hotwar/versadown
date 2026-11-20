use super::*;
use crate::cloud::upload::{report_upload_progress, UploadProgressTracker};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::ipc::Channel;

async fn run_upload_session<F, Fut>(
    provider: CloudProvider,
    upload_id: String,
    on_progress: Channel<CloudUploadProgress>,
    operation: F,
) -> Result<CloudUploadOutcome, QuarkError>
where
    F: FnOnce(UploadProgressTracker, UploadControl) -> Fut,
    Fut: std::future::Future<Output = Result<CloudUploadOutcome, QuarkError>>,
{
    run_upload_session_with_sink(provider, upload_id, move |event| { let _ = on_progress.send(event); }, operation).await
}

async fn run_upload_session_with_sink<F, Fut>(
    provider: CloudProvider,
    upload_id: String,
    on_progress: impl Fn(CloudUploadProgress),
    operation: F,
) -> Result<CloudUploadOutcome, QuarkError>
where
    F: FnOnce(UploadProgressTracker, UploadControl) -> Fut,
    Fut: std::future::Future<Output = Result<CloudUploadOutcome, QuarkError>>,
{
    let control = UploadControl::default();
    let _registration =
        crate::cloud::upload_registry().register(provider, &upload_id, control.clone())?;
    let tracker = UploadProgressTracker::new(provider);
    tracker.bind_session(upload_id, control.clone());
    report_upload_progress(
        tracker.clone(),
        on_progress,
        async {
            let result = operation(tracker.clone(), control.clone()).await;
            let result = match result {
                Ok(mut outcome) => {
                    outcome.cancelled |= control.is_cancelled();
                    Ok(outcome)
                }
                Err(error) if crate::cloud::is_upload_cancelled(&error) => Ok(CloudUploadOutcome {
                    uploaded: Vec::new(),
                    failures: Vec::new(),
                    cancelled: true,
                }),
                Err(error) => Err(error),
            };
            let failed_items = match &result {
                Ok(outcome) => outcome
                    .failures
                    .iter()
                    .filter(|failure| failure.code != "upload_cancelled")
                    .count(),
                Err(_) => 1,
            };
            if result.as_ref().is_ok_and(|outcome| outcome.cancelled) {
                tracker.finish_cancelled(failed_items);
            } else {
                tracker.finish(failed_items);
            }
            result
        },
    )
    .await
}

#[tauri::command]
pub fn cloud_control_upload(
    provider: CloudProvider,
    upload_id: String,
    action: String,
) -> Result<(), QuarkError> {
    crate::cloud::upload_registry().apply(provider, &upload_id, &action)
}

fn agent_upload_access(
    app: &AppHandle, provider: CloudProvider, account_id: &str, permission_scope: &str, path: &Path,
) -> Result<PathBuf, QuarkError> {
    let state = app.state::<AppState>();
    crate::agent_access::require_cloud_account(&state, provider.key(), account_id)
        .map_err(|_| QuarkError::InvalidInput("agent.error.forbidden".into()))?;
    // Provider session work runs on a blocking thread without Tokio task-local
    // context. The durable cloud plan is the authority for this upload only.
    let source = crate::agent_permissions::authorize_upload_path_in_scope(&state, permission_scope, path)
        .map_err(|_| QuarkError::InvalidInput("agent.error.forbidden".into()))?;
    if source.is_dir() { return Err(QuarkError::ProviderUnsupported); }
    crate::agent_permissions::authorize_upload_file_in_scope(&state, permission_scope, &source)
        .map_err(|_| QuarkError::InvalidInput("agent.error.forbidden".into()))
}

fn ensure_agent_upload_name_available(listing: &CloudDirectory, path: &Path) -> Result<(), QuarkError> {
    let name = path.file_name().and_then(|name| name.to_str())
        .ok_or_else(|| QuarkError::InvalidInput("agent.error.invalidArgument".into()))?;
    if listing.items.iter().any(|file| file.name.eq_ignore_ascii_case(name)) {
        return Err(QuarkError::InvalidInput("agent.error.fileConflict".into()));
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum AgentUploadError {
    Provider(QuarkError),
    ResultUncertain,
    SourceChanged,
}

impl From<QuarkError> for AgentUploadError {
    fn from(error: QuarkError) -> Self {
        if matches!(&error, QuarkError::InvalidInput(reason) if reason == "agent.error.sourceChanged") {
            Self::SourceChanged
        } else {
            Self::Provider(error)
        }
    }
}

fn dispatch_agent_upload(
    expected: &crate::cli_cloud::FileSnapshot,
    approved: &Path,
    entered_mutation: &AtomicBool,
    upload: impl FnOnce() -> Result<CloudFile, QuarkError>,
) -> Result<CloudFile, QuarkError> {
    let changed = || QuarkError::InvalidInput("agent.error.sourceChanged".into());
    let metadata = std::fs::symlink_metadata(approved).map_err(|_| changed())?;
    let modified = metadata.modified().map_err(|_| changed())?
        .duration_since(std::time::UNIX_EPOCH).map_err(|_| changed())?.as_nanos();
    if approved != expected.path || !metadata.is_file() || metadata.file_type().is_symlink()
        || metadata.len() != expected.size || modified != expected.modified
    {
        return Err(changed());
    }
    // From here cancellation cannot prove that the provider created no file.
    // The provider's UploadFileSnapshot guards hashing and transfer itself.
    entered_mutation.store(true, Ordering::Release);
    upload()
}

fn agent_upload_result(
    result: Result<CloudUploadOutcome, QuarkError>,
    entered_mutation: bool,
) -> Result<CloudFile, AgentUploadError> {
    let outcome = match result {
        Err(error) if entered_mutation && crate::cloud::is_upload_cancelled(&error) => {
            return Err(AgentUploadError::ResultUncertain);
        }
        Err(error) => return Err(error.into()),
        Ok(outcome) => outcome,
    };
    if let Some(file) = outcome.uploaded.into_iter().next() {
        // A late cancellation does not undo an authoritative provider result.
        Ok(file)
    } else if outcome.cancelled {
        if entered_mutation { Err(AgentUploadError::ResultUncertain) }
        else { Err(AgentUploadError::Provider(crate::cloud::upload_cancelled())) }
    } else {
        Err(AgentUploadError::Provider(QuarkError::InvalidResponse))
    }
}

/// Single-file Agent uploads share progress/control, bandwidth and provider
/// implementations with the desktop. Account validation and mutation use one
/// credential snapshot, so an account change cannot redirect the write.
pub(crate) async fn upload_agent_file(
    app: AppHandle,
    provider: CloudProvider,
    account_id: String,
    directory_chain: Vec<String>,
    parent_id: String,
    expected_source: crate::cli_cloud::FileSnapshot,
    upload_id: String,
    permission_scope: String,
    on_progress: Arc<dyn Fn(CloudUploadProgress) + Send + Sync>,
) -> Result<CloudFile, AgentUploadError> {
    let source = agent_upload_access(&app, provider, &account_id, &permission_scope, &expected_source.path)?;
    let capacity_app = app.clone();
    let entered_mutation = Arc::new(AtomicBool::new(false));
    let worker_entered_mutation = Arc::clone(&entered_mutation);
    let result = run_upload_session_with_sink(provider, upload_id, move |event| on_progress(event),
        |tracker, control| async move {
            let runtime = app.state::<AppState>().runtime.clone();
            let lease = runtime.begin_cloud_upload().await;
            let upload_control = control.with_progress(tracker.clone(), 0).with_rate_limiter(lease.limiter());
            let check_control = upload_control.clone();
            let access_app = app.clone();
            let result = run_session_work_checked(app, provider, SessionWorkKind::Mutation, move |state, _| with_upload_http(|transport| {
                upload_control.wait_ready_blocking()?;
                let http = crate::cloud::http::ControlledUploadHttp { inner: transport, control: upload_control.clone() };
                let approved = agent_upload_access(&access_app, provider, &account_id, &permission_scope, &source)?;
                let prepared = prepare_upload_input(provider, &approved, &upload_control, &tracker, 0)?;
                let PreparedUpload::File(path) = prepared else { return Err(QuarkError::ProviderUnsupported); };
                tracker.start_uploading();
                upload_control.wait_ready_blocking()?;
                upload_control.start_file(&path);
                let file = {
                    // All destination checks occur after any pause, within the
                    // same snapshot used for the subsequent provider mutation.
                    validate_agent_cloud_path(provider, &state.store, &http, &account_id, &directory_chain, &parent_id, true)?;
                    let listing = list_cloud_directory(provider, &state.store, &http, &parent_id)?;
                    ensure_agent_upload_name_available(&listing, &path)?;
                    let approved = agent_upload_access(&access_app, provider, &account_id, &permission_scope, &path)?;
                    dispatch_agent_upload(&expected_source, &approved, &worker_entered_mutation, || {
                        ProviderRegistry::global().driver(provider, &state.store, &http).upload_no_replace(&http, &parent_id, &approved)
                    })?
                };
                upload_control.complete_file(&path);
                Ok(CloudUploadOutcome { uploaded: vec![file], failures: Vec::new(), cancelled: false })
            }), move || check_control.check()).await;
            lease.release().await;
            result
        }).await;
    let file = agent_upload_result(result, entered_mutation.load(Ordering::Acquire))?;
    schedule_cloud_capacity_invalidation(capacity_app, provider, CloudCapacityChangeReason::Upload);
    Ok(file)
}

/// Token acquisition and the following writes own one HTTP session. Background
/// account/directory requests must not rotate its cookies between those steps.
fn with_upload_http<T>(
    work: impl FnOnce(&ReqwestTransport) -> Result<T, QuarkError>,
) -> Result<T, QuarkError> {
    let transport = ReqwestTransport::new()?;
    work(&transport)
}

enum PreparedUpload {
    File(PathBuf),
    Directory(DirectoryUploadPlan),
}

fn prepare_upload_input(
    provider: CloudProvider,
    path: &Path,
    control: &crate::cloud::UploadControl,
    tracker: &UploadProgressTracker,
    input: usize,
) -> Result<PreparedUpload, QuarkError> {
    control.check()?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| QuarkError::InvalidInput("无法读取选中的上传项目".into()))?;
    if metadata.file_type().is_symlink() {
        return Err(QuarkError::InvalidInput(
            "不能上传符号链接或目录联接".into(),
        ));
    }
    if metadata.is_file() {
        validate_upload_file(provider, path, &metadata)?;
        tracker.add_file(input, path, metadata.len());
        Ok(PreparedUpload::File(path.into()))
    } else if metadata.is_dir() {
        let plan = build_directory_upload_plan_with_limits(
            provider,
            path,
            control,
            DIRECTORY_UPLOAD_LIMITS,
        )?;
        for entry in &plan.entries {
            if let DirectoryUploadEntry::File { source, size, .. } = entry {
                tracker.add_file(input, source, *size);
            }
        }
        Ok(PreparedUpload::Directory(plan))
    } else {
        Err(QuarkError::InvalidInput("只能上传普通文件或目录".into()))
    }
}

fn upload_file_with_progress(
    control: &crate::cloud::UploadControl,
    path: &Path,
    upload: impl FnOnce() -> Result<CloudFile, QuarkError>,
) -> Result<CloudFile, QuarkError> {
    control.wait_ready_blocking()?;
    control.start_file(path);
    let file = upload().map_err(|error| {
        if control.is_cancelled() {
            crate::cloud::upload_cancelled()
        } else {
            error
        }
    })?;
    control.complete_file(path);
    Ok(file)
}

#[cfg(test)]
async fn run_with_upload_channel<F, Fut>(
    provider: CloudProvider,
    on_progress: Channel<CloudUploadProgress>,
    operation: F,
) -> Result<CloudUploadOutcome, QuarkError>
where
    F: FnOnce(UploadProgressTracker) -> Fut,
    Fut: std::future::Future<Output = Result<CloudUploadOutcome, QuarkError>>,
{
    let tracker = UploadProgressTracker::new(provider);
    report_upload_progress(
        tracker.clone(),
        move |event| {
            // Closing the view or losing the callback never cancels the upload.
            let _ = on_progress.send(event);
        },
        async {
            let result = operation(tracker.clone()).await;
            tracker.finish(match &result {
                Ok(outcome) => outcome.failures.len(),
                Err(_) => 1,
            });
            result
        },
    )
    .await
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DirectoryUploadEntry {
    Directory {
        relative_path: PathBuf,
    },
    File {
        relative_path: PathBuf,
        source: PathBuf,
        size: u64,
    },
}

#[derive(Clone, Copy)]
struct DirectoryUploadLimits {
    directories: usize,
    files: usize,
    depth: usize,
}

const DIRECTORY_UPLOAD_LIMITS: DirectoryUploadLimits = DirectoryUploadLimits {
    directories: 1_000,
    files: 10_000,
    depth: 128,
};

struct DirectoryUploadPlan {
    root_name: String,
    entries: Vec<DirectoryUploadEntry>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudUploadFailure {
    pub input_index: usize,
    pub item_name: String,
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudUploadOutcome {
    pub uploaded: Vec<CloudFile>,
    pub failures: Vec<CloudUploadFailure>,
    #[serde(skip_serializing_if = "is_false")]
    pub cancelled: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl DirectoryUploadEntry {
    fn relative_path(&self) -> &Path {
        match self {
            Self::Directory { relative_path } | Self::File { relative_path, .. } => relative_path,
        }
    }

    #[cfg(test)]
    fn is_directory(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }
}

fn build_directory_upload_plan_with_limits(
    provider: CloudProvider,
    root: &Path,
    control: &crate::cloud::UploadControl,
    limits: DirectoryUploadLimits,
) -> Result<DirectoryUploadPlan, QuarkError> {
    control.check()?;
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|_| QuarkError::InvalidInput("无法读取选中的目录".into()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(QuarkError::InvalidInput("请选择可访问的普通目录".into()));
    }
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| QuarkError::InvalidInput("无法识别上传目录名称".into()))?;
    valid_name(root_name)?;

    let mut entries = Vec::new();
    let mut directories = 1_usize;
    let mut files = 0_usize;
    let mut pending = vec![(root.to_path_buf(), PathBuf::new(), false)];
    while let Some((path, relative_path, emit_directory)) = pending.pop() {
        control.check()?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| QuarkError::InvalidInput("无法读取目录中的项目".into()))?;
        if metadata.file_type().is_symlink() {
            return Err(QuarkError::InvalidInput(
                "上传目录不能包含符号链接或目录联接".into(),
            ));
        }
        let depth = relative_path.components().count();
        if depth > limits.depth {
            return Err(QuarkError::InvalidInput("上传目录层级过深".into()));
        }
        if metadata.is_file() {
            files = files.saturating_add(1);
            if files > limits.files {
                return Err(QuarkError::InvalidInput("上传目录包含过多文件".into()));
            }
            validate_upload_file(provider, &path, &metadata)?;
            entries.push(DirectoryUploadEntry::File {
                relative_path,
                source: path,
                size: metadata.len(),
            });
            continue;
        }
        if !metadata.is_dir() {
            return Err(QuarkError::InvalidInput(
                "上传目录只能包含普通文件和目录".into(),
            ));
        }
        if emit_directory {
            directories = directories.saturating_add(1);
            if directories > limits.directories {
                return Err(QuarkError::InvalidInput("上传目录包含过多子目录".into()));
            }
            let name = relative_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| QuarkError::InvalidInput("目录名称无效".into()))?;
            valid_name(name)?;
            entries.push(DirectoryUploadEntry::Directory {
                relative_path: relative_path.clone(),
            });
        }
        let mut children = std::fs::read_dir(&path)
            .map_err(|_| QuarkError::InvalidInput("无法读取选中的目录".into()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| QuarkError::InvalidInput("无法读取选中的目录".into()))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children.into_iter().rev() {
            control.check()?;
            let child_path = child.path();
            let child_relative = child_path
                .strip_prefix(root)
                .map_err(|_| QuarkError::InvalidInput("目录路径无效".into()))?
                .to_path_buf();
            pending.push((child_path, child_relative, true));
        }
    }
    Ok(DirectoryUploadPlan {
        root_name: root_name.to_owned(),
        entries,
    })
}

#[cfg(test)]
fn collect_directory_upload_entries(root: &Path) -> Result<Vec<DirectoryUploadEntry>, QuarkError> {
    Ok(build_directory_upload_plan_with_limits(
        CloudProvider::Xunlei,
        root,
        &crate::cloud::UploadControl::default(),
        DIRECTORY_UPLOAD_LIMITS,
    )?
    .entries)
}

fn validate_upload_file(
    provider: CloudProvider,
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), QuarkError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| QuarkError::InvalidInput("文件名无效".into()))?;
    valid_name(name)?;
    if provider == CloudProvider::Baidu && metadata.len() == 0 {
        return Err(QuarkError::InvalidInput(
            "百度网盘暂不支持上传空文件".into(),
        ));
    }
    Ok(())
}

fn ensure_upload_root_available(
    destination: &CloudDirectory,
    root_name: &str,
) -> Result<(), QuarkError> {
    if destination.items.iter().any(|item| item.name == root_name) {
        return Err(QuarkError::InvalidInput(
            "目标目录已存在同名项目，请先重命名本地目录或远端项目".into(),
        ));
    }
    Ok(())
}

pub(crate) fn select_cloud_upload_paths(
    descriptor: &ProviderDescriptor,
    paths: Option<Vec<String>>,
    pick_files: impl FnOnce(String) -> Result<Vec<PathBuf>, QuarkError>,
) -> Result<Vec<PathBuf>, QuarkError> {
    match paths {
        Some(paths) => Ok(paths.into_iter().map(PathBuf::from).collect()),
        None => pick_files(format!(
            "选择上传到 {} 的文件",
            descriptor.upload_display_name
        )),
    }
}

fn upload_directory_tree(
    state: &CloudState,
    http: &(impl CloudHttpTransport + CloudUploadTransport),
    provider: CloudProvider,
    parent_id: &str,
    plan: DirectoryUploadPlan,
    control: &crate::cloud::UploadControl,
) -> Result<Vec<CloudFile>, QuarkError> {
    control.wait_ready_blocking()?;
    let destination = state.with_upload_session(provider, http, |store| {
        ProviderRegistry::global()
            .driver(provider, store, http)
            .list_directory(parent_id)
    })?;
    ensure_upload_root_available(&destination, &plan.root_name)?;
    let create_directory = |parent_id: &str, name: &str| {
        state
            .with_upload_session(provider, http, |store| {
                ProviderRegistry::global()
                    .driver(provider, store, http)
                    .create_directory(parent_id, name)
            })
            .map_err(|error| {
                if control.is_cancelled() {
                    crate::cloud::upload_cancelled()
                } else {
                    error
                }
            })
    };
    execute_directory_upload_plan(
        parent_id,
        plan,
        || control.wait_ready_blocking(),
        create_directory,
        |remote_parent, source| {
            upload_file_with_progress(control, source, || {
                state.upload_file(provider, http, remote_parent, source)
            })
        },
        |root_id| {
            control.check()?;
            let ids = vec![root_id.to_owned()];
            state.with_upload_session(provider, http, |store| {
                ProviderRegistry::global()
                    .driver(provider, store, http)
                    .trash(&ids)
            })
        },
    )
}

fn execute_directory_upload_plan(
    parent_id: &str,
    plan: DirectoryUploadPlan,
    mut check_control: impl FnMut() -> Result<(), QuarkError>,
    mut create_directory: impl FnMut(&str, &str) -> Result<CloudFile, QuarkError>,
    mut upload_file: impl FnMut(&str, &Path) -> Result<CloudFile, QuarkError>,
    mut trash_root: impl FnMut(&str) -> Result<(), QuarkError>,
) -> Result<Vec<CloudFile>, QuarkError> {
    check_control()?;
    let uploaded_root = create_directory(parent_id, &plan.root_name)?;
    let mut results = vec![uploaded_root.clone()];
    let root_id = uploaded_root.id.clone();
    let operation = (|| {
        let mut remote_directories = HashMap::from([(PathBuf::new(), root_id.clone())]);
        for entry in plan.entries {
            check_control()?;
            let relative_parent = entry
                .relative_path()
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let remote_parent = remote_directories
                .get(relative_parent)
                .cloned()
                .ok_or(QuarkError::InvalidResponse)?;
            match entry {
                DirectoryUploadEntry::Directory { relative_path } => {
                    let name = relative_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| QuarkError::InvalidInput("目录名称无效".into()))?;
                    let created = create_directory(&remote_parent, name)?;
                    remote_directories.insert(relative_path, created.id.clone());
                    results.push(created);
                }
                DirectoryUploadEntry::File { source, .. } => {
                    results.push(upload_file(&remote_parent, &source)?);
                }
            }
        }
        Ok(())
    })();
    match operation {
        Ok(()) => Ok(results),
        Err(error) if crate::cloud::is_upload_cancelled(&error) => Ok(results),
        // The final write may already exist. Keep the directory so the user
        // can reconcile its contents before choosing to retry or remove it.
        Err(error @ QuarkError::BaiduUploadAuthorizationRequired(BaiduUploadAuthStage::Commit)) => Err(error),
        Err(error) => match trash_root(&root_id) {
            Ok(()) => Err(error),
            Err(error) if crate::cloud::is_upload_cancelled(&error) => Ok(results),
            Err(_) => Err(QuarkError::Provider(
                "目录上传失败，且无法自动回收本次新建的远端目录；请刷新后手动处理".into(),
            )),
        },
    }
}

fn upload_selected_path(
    state: &CloudState,
    provider: CloudProvider,
    parent_id: &str,
    prepared: PreparedUpload,
    expected_account_id: Option<&str>,
    control: crate::cloud::UploadControl,
) -> Result<Vec<CloudFile>, QuarkError> {
    control.wait_ready_blocking()?;
    with_upload_http(|transport| {
        let http = crate::cloud::http::ControlledUploadHttp {
            inner: transport,
            control: control.clone(),
        };
        if let Some(expected) = expected_account_id {
            let PreparedUpload::File(path) = prepared else { return Err(QuarkError::SourceFileMissing); };
            if expected.is_empty() || provider != CloudProvider::Baidu { return Err(QuarkError::AccountMismatch); }
            return upload_file_with_progress(&control, &path, || {
                state.with_upload_session(provider, &http, |store| {
                    let client = baidu::BaiduClient::new(load_provider_session(provider, store)?, &http);
                    if client.account()?.id != expected { return Err(QuarkError::AccountMismatch); }
                    client.upload_file(parent_id, &path, &http)
                })
            }).map(|file| vec![file]);
        }
        match prepared {
            PreparedUpload::Directory(plan) => {
                upload_directory_tree(state, &http, provider, parent_id, plan, &control).map_err(|error| {
                    // A directory may already contain successful writes. Only single files can continue after authorization.
                    if matches!(error, QuarkError::BaiduUploadWebSessionRequired) {
                        QuarkError::BaiduUploadPrecreateRejected { status: 200, errno: Some(-6) }
                    } else { error }
                })
            }
            PreparedUpload::File(path) => Ok(vec![upload_file_with_progress(&control, &path, || {
                state.upload_file(provider, &http, parent_id, &path)
            })?]),
        }
    })
}

fn upload_error_code(error: &QuarkError) -> &'static str {
    if crate::cloud::is_upload_cancelled(error) {
        return "upload_cancelled";
    }
    match error {
        QuarkError::XunleiVerificationUnavailable(_) => "xunlei_verification_unavailable",
        QuarkError::NotAuthorized => "not_authorized",
        QuarkError::AccountMismatch => "account_mismatch",
        QuarkError::AuthorizationExpired => "credentials_expired",
        QuarkError::SessionRenewedRetryOperation => "session_renewed_retry_operation",
        QuarkError::PikpakOperationVerificationRequired(_) => {
            "pikpak_operation_verification_required"
        }
        QuarkError::BaiduDriveVerificationRequired => "baidu_drive_verification_required",
        QuarkError::BaiduOperationVerificationRequired => "baidu_operation_verification_required",
        QuarkError::BaiduUploadAuthorizationRequired(stage) => stage.code(),
        QuarkError::BaiduUploadWebSessionRequired => "baidu_upload_web_session_required",
        QuarkError::BaiduUploadPrecreateRejected { .. } => "baidu_upload_precreate_rejected",
        QuarkError::WebAuthorizationRequired | QuarkError::CaptchaRequired => "web_auth_required",
        QuarkError::PikpakRegionalMembershipRestricted => "pikpak_regional_membership_restricted",
        QuarkError::AuthorizationPending => "authorization_pending",
        QuarkError::SourceFileMissing => "source_file_missing",
        QuarkError::DownloadCheckpointInvalid => "download_checkpoint_invalid",
        QuarkError::DownloadEngineUnavailable => "download_engine_unavailable",
        QuarkError::DownloadSubmissionFailed => "download_submission_failed",
        QuarkError::XunleiNativeDownload(failure) => failure.code(),
        QuarkError::BaiduTransferPending => "baidu_transfer_pending",
        QuarkError::ShareTransferDownloadUnavailable => "share_transfer_download_unavailable",
        QuarkError::InvalidResponse => "invalid_response",
        QuarkError::Storage => "credential_storage_error",
        QuarkError::Transport(_) => "network_error",
        QuarkError::Provider(_) => "provider_error",
        QuarkError::InvalidInput(_) => "invalid_input",
        QuarkError::UploadUnavailable | QuarkError::ProviderUnsupported => "provider_unsupported",
    }
}

fn summarize_upload_batches(
    batches: Vec<(usize, String, Result<Vec<CloudFile>, QuarkError>)>,
) -> CloudUploadOutcome {
    let mut outcome = CloudUploadOutcome {
        uploaded: Vec::new(),
        failures: Vec::new(),
        cancelled: false,
    };
    for (input_index, item_name, batch) in batches {
        match batch {
            Ok(files) => outcome.uploaded.extend(files),
            Err(error) => {
                outcome.cancelled |= crate::cloud::is_upload_cancelled(&error);
                outcome.failures.push(CloudUploadFailure {
                    input_index,
                    item_name,
                    code: upload_error_code(&error),
                    message: error.to_string(),
                });
            }
        }
    }
    outcome
}

async fn upload_selected_paths(
    state: Arc<CloudState>,
    descriptor: &'static ProviderDescriptor,
    provider: CloudProvider,
    parent_id: String,
    paths: Vec<PathBuf>,
    expected_account_id: Option<String>,
    tracker: UploadProgressTracker,
    control: UploadControl,
    rate_limiter: Arc<crate::bandwidth::BandwidthLimiter>,
) -> Result<CloudUploadOutcome, QuarkError> {
    let scan_control = control.clone();
    let scan_tracker = tracker.clone();
    // Build each bounded directory plan once, before any remote mutation. The
    // same plan provides totals and later drives uploads; failures stay indexed.
    let inputs = tauri::async_runtime::spawn_blocking(move || {
        paths
            .into_iter()
            .enumerate()
            .map(|(input_index, path)| {
                let item_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "上传项目".into());
                let prepared = prepare_upload_input(
                    provider,
                    &path,
                    &scan_control,
                    &scan_tracker,
                    input_index,
                );
                (input_index, item_name, prepared)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|_| QuarkError::InvalidInput("上传预扫描中断".into()))?;
    tracker.start_uploading();
    let batches = crate::cloud::schedule_uploads(
        inputs,
        descriptor.max_upload_concurrency,
        control,
        move |(input_index, item_name, prepared), control| {
            let control = control
                .with_progress(tracker.clone(), input_index)
                .with_rate_limiter(rate_limiter.clone());
            let result = prepared
                .and_then(|prepared| {
                    upload_selected_path(&state, provider, &parent_id, prepared, expected_account_id.as_deref(), control.clone())
                })
                .map_err(|error| {
                    if control.is_cancelled() {
                        crate::cloud::upload_cancelled()
                    } else {
                        error
                    }
                });
            if result
                .as_ref()
                .is_err_and(|error| !crate::cloud::is_upload_cancelled(error))
            {
                tracker.fail_input(input_index);
            }
            Ok((item_name, result))
        },
    )
    .await;
    let batches = batches
        .into_iter()
        .enumerate()
        .map(|(input_index, batch)| {
            let (item_name, result) = batch.unwrap_or_else(|error| ("上传项目".into(), Err(error)));
            (input_index, item_name, result)
        })
        .collect();
    Ok(summarize_upload_batches(batches))
}

#[tauri::command]
pub async fn cloud_upload_files(
    app: AppHandle,
    provider: CloudProvider,
    parent_id: String,
    paths: Option<Vec<String>>,
    expected_account_id: Option<String>,
    on_progress: Channel<CloudUploadProgress>,
    upload_id: String,
) -> Result<CloudUploadOutcome, QuarkError> {
    let capacity_event_app = app.clone();
    let outcome = run_upload_session(
        provider,
        upload_id,
        on_progress,
        |tracker, control| async move {
            let picker = app.clone();
            let descriptor = ProviderRegistry::global().get(provider);
            // The native picker may stay open indefinitely; it must hold no session lock.
            let paths = tauri::async_runtime::spawn_blocking(move || {
                select_cloud_upload_paths(descriptor, paths, |title| {
                    picker
                        .dialog()
                        .file()
                        .set_title(title)
                        .blocking_pick_files()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|path| {
                            path.into_path()
                                .map_err(|_| QuarkError::InvalidInput("无法访问选中的文件".into()))
                        })
                        .collect()
                })
            })
            .await
            .map_err(|_| QuarkError::InvalidInput("文件选择中断".into()))??;
            if paths.is_empty() {
                return Ok(CloudUploadOutcome {
                    uploaded: Vec::new(),
                    failures: Vec::new(),
                    cancelled: false,
                });
            }
            let runtime = app.state::<crate::commands::AppState>().runtime.clone();
            let lease = runtime.begin_cloud_upload().await;
            let result = upload_selected_paths(
                app.state::<Arc<CloudState>>().inner().clone(),
                descriptor,
                provider,
                parent_id,
                paths,
                expected_account_id,
                tracker,
                control,
                lease.limiter(),
            )
            .await;
            lease.release().await;
            result
        },
    )
    .await?;
    if !outcome.uploaded.is_empty() {
        schedule_cloud_capacity_invalidation(
            capacity_event_app,
            provider,
            CloudCapacityChangeReason::Upload,
        );
    }
    Ok(outcome)
}

#[tauri::command]
pub async fn cloud_upload_directory(
    app: AppHandle,
    provider: CloudProvider,
    parent_id: String,
    path: Option<String>,
    on_progress: Channel<CloudUploadProgress>,
    upload_id: String,
) -> Result<CloudUploadOutcome, QuarkError> {
    let capacity_event_app = app.clone();
    let outcome = run_upload_session(
        provider,
        upload_id,
        on_progress,
        |tracker, control| async move {
            let descriptor = ProviderRegistry::global().get(provider);
            let selected = match path {
                Some(path) => Some(PathBuf::from(path)),
                None => {
                    let picker = app.clone();
                    let title = format!("选择上传到 {} 的目录", descriptor.upload_display_name);
                    tauri::async_runtime::spawn_blocking(move || {
                        picker
                            .dialog()
                            .file()
                            .set_title(title)
                            .blocking_pick_folder()
                            .map(|path| {
                                path.into_path().map_err(|_| {
                                    QuarkError::InvalidInput("无法访问选中的目录".into())
                                })
                            })
                            .transpose()
                    })
                    .await
                    .map_err(|_| QuarkError::InvalidInput("目录选择中断".into()))??
                }
            };
            let Some(path) = selected else {
                return Ok(CloudUploadOutcome {
                    uploaded: Vec::new(),
                    failures: Vec::new(),
                    cancelled: false,
                });
            };
            let runtime = app.state::<crate::commands::AppState>().runtime.clone();
            let lease = runtime.begin_cloud_upload().await;
            let result = upload_selected_paths(
                app.state::<Arc<CloudState>>().inner().clone(),
                descriptor,
                provider,
                parent_id,
                vec![path],
                None,
                tracker,
                control,
                lease.limiter(),
            )
            .await;
            lease.release().await;
            result
        },
    )
    .await?;
    if !outcome.uploaded.is_empty() {
        schedule_cloud_capacity_invalidation(
            capacity_event_app,
            provider,
            CloudCapacityChangeReason::Upload,
        );
    }
    Ok(outcome)
}

impl CloudState {
    /// Own the mutation session once, then bind the driver to its snapshot.
    pub fn upload_file(
        &self,
        provider: CloudProvider,
        http: &(impl CloudHttpTransport + CloudUploadTransport),
        parent: &str,
        path: &Path,
    ) -> Result<CloudFile, QuarkError> {
        self.with_upload_session(provider, http, |store| {
            ProviderRegistry::global().driver(provider, store, http).upload(http, parent, path)
        })
    }

    pub(crate) fn with_upload_session<T>(
        &self,
        provider: CloudProvider,
        http: &impl CloudHttpTransport,
        mut work: impl FnMut(&crate::cloud::driver::StoreRef<'_>) -> Result<T, CloudError>,
    ) -> Result<T, CloudError> {
        self.auth_supervisor.run_checked(
            &self.store,
            provider,
            SessionWorkKind::Mutation,
            |snapshot| work(&crate::cloud::driver::StoreRef(snapshot)),
            |genuine| {
                if let Some(app) = self.application.get() {
                    return recover_authorization(app, self, provider, http, genuine);
                }
                auth_supervisor::recover_in_order(
                    provider,
                    &self.store,
                    ProviderRegistry::global()
                        .driver(provider, &self.store, http)
                        .auth(),
                    genuine,
                    || Ok(false),
                )
            },
            || http.check_transfer(),
        )
    }
    pub fn upload_pikpak_file(
        &self,
        http: &(impl CloudHttpTransport + CloudUploadTransport),
        parent: &str,
        path: &Path,
    ) -> Result<CloudFile, QuarkError> {
        self.upload_file(CloudProvider::Pikpak, http, parent, path)
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    fn agent_source(path: &Path) -> crate::cli_cloud::FileSnapshot {
        let metadata = std::fs::metadata(path).unwrap();
        crate::cli_cloud::FileSnapshot {
            path: path.into(),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            size: metadata.len(),
            modified: metadata.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        }
    }

    #[test]
    fn agent_upload_late_cancel_preserves_a_known_remote_file() {
        let uploaded = file("remote-created", "parent", "report.txt", CloudFileKind::File);
        let result = agent_upload_result(Ok(CloudUploadOutcome {
            uploaded: vec![uploaded], failures: Vec::new(), cancelled: true,
        }), true).unwrap();
        assert_eq!(result.id, "remote-created");
    }

    #[test]
    fn agent_upload_cancel_after_mutation_is_uncertain_but_before_mutation_is_plain_cancel() {
        let cancelled = || CloudUploadOutcome {
            uploaded: Vec::new(), failures: Vec::new(), cancelled: true,
        };
        assert!(matches!(agent_upload_result(Ok(cancelled()), true), Err(AgentUploadError::ResultUncertain)));
        assert!(matches!(agent_upload_result(Err(crate::cloud::upload_cancelled()), true), Err(AgentUploadError::ResultUncertain)));
        assert!(matches!(agent_upload_result(Ok(cancelled()), false), Err(AgentUploadError::Provider(error)) if crate::cloud::is_upload_cancelled(&error)));
    }

    #[test]
    fn agent_upload_does_not_mask_lost_commit_responses_as_cancellation() {
        let result = agent_upload_result(Err(QuarkError::Transport("commit response lost".into())), true);
        assert!(matches!(result, Err(AgentUploadError::Provider(QuarkError::Transport(message))) if message == "commit response lost"));
    }

    #[test]
    fn agent_upload_rechecks_the_original_source_after_preparing_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.txt");
        std::fs::write(&path, b"approved contents").unwrap();
        let expected = agent_source(&path);
        let control = UploadControl::default();
        let tracker = UploadProgressTracker::new(CloudProvider::Quark);
        let prepared = prepare_upload_input(CloudProvider::Quark, &path, &control, &tracker, 0).unwrap();
        let PreparedUpload::File(approved) = prepared else { panic!("expected a file"); };
        // Simulate a local edit while awaiting provider metadata/authentication.
        std::fs::write(&path, b"contents changed during the wait").unwrap();
        let entered = AtomicBool::new(false);
        let result = dispatch_agent_upload(&expected, &approved, &entered, || {
            panic!("changed source must never enter provider mutation")
        });
        assert!(matches!(result.map_err(AgentUploadError::from), Err(AgentUploadError::SourceChanged)));
        assert!(!entered.load(Ordering::Acquire));
    }

    #[test]
    fn agent_upload_rechecks_canonical_identity_and_modified_time_even_when_length_matches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.txt");
        let other = directory.path().join("replacement.txt");
        std::fs::write(&path, b"same length").unwrap();
        std::fs::write(&other, b"same length").unwrap();
        let expected = agent_source(&path);
        let entered = AtomicBool::new(false);
        let result = dispatch_agent_upload(&expected, &other, &entered, || panic!("wrong canonical path"));
        assert!(matches!(result.map_err(AgentUploadError::from), Err(AgentUploadError::SourceChanged)));
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap() + std::time::Duration::from_secs(2);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
        let result = dispatch_agent_upload(&expected, &path, &entered, || panic!("modified source"));
        assert!(matches!(result.map_err(AgentUploadError::from), Err(AgentUploadError::SourceChanged)));
        assert!(!entered.load(Ordering::Acquire));
    }

    #[test]
    fn agent_upload_unchanged_source_enters_provider_once_and_preserves_its_result() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.txt");
        std::fs::write(&path, b"approved").unwrap();
        let expected = agent_source(&path);
        let entered = AtomicBool::new(false);
        let result = dispatch_agent_upload(&expected, &path, &entered, || {
            assert!(entered.load(Ordering::Acquire));
            Ok(file("remote-created", "parent", "report.txt", CloudFileKind::File))
        }).unwrap();
        assert_eq!(result.id, "remote-created");
    }

    #[tokio::test]
    async fn agent_upload_sink_uses_the_shared_control_registration_and_terminal_progress() {
        let id = uuid::Uuid::new_v4().to_string();
        let callback_id = id.clone();
        let events = Arc::new(Mutex::new(Vec::<CloudUploadProgress>::new()));
        let received = events.clone();
        let outcome = run_upload_session_with_sink(CloudProvider::Quark, id.clone(), move |event| {
            if event.phase == "preparing" {
                crate::cloud::upload_registry().apply(CloudProvider::Quark, &callback_id, "cancel").unwrap();
            }
            received.lock().unwrap().push(event);
        }, |_, control| async move {
            control.check()?;
            panic!("cancelled uploads must not reach a provider mutation");
        }).await.unwrap();
        assert!(outcome.cancelled);
        assert!(outcome.uploaded.is_empty());
        let events = events.lock().unwrap();
        assert_eq!(events.last().unwrap().phase, "cancelled");
        assert!(events.iter().all(|event| event.upload_id == id));
        assert!(crate::cloud::upload_registry().apply(CloudProvider::Quark, &id, "resume").is_err());
    }

    #[test]
    fn agent_upload_rejects_existing_file_or_folder_names_before_mutation() {
        for kind in [CloudFileKind::File, CloudFileKind::Folder] {
            let listing = CloudDirectory { id: "approved".into(), items: vec![CloudFile {
                id: "existing".into(), parent_id: "approved".into(), name: "REPORT.txt".into(),
                kind, size: None, modified_at: None,
            }] };
            assert!(matches!(ensure_agent_upload_name_available(&listing, Path::new("report.txt")),
                Err(QuarkError::InvalidInput(message)) if message == "agent.error.fileConflict"));
            assert!(ensure_agent_upload_name_available(&listing, Path::new("new.txt")).is_ok());
        }
    }

    #[tokio::test]
    async fn upload_control_native_channel_returns_cancelled_with_successes_and_releases_id() {
        let upload_id = uuid::Uuid::new_v4().to_string();
        let event_id = upload_id.clone();
        let events = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let received = events.clone();
        let control_result = Arc::new(Mutex::new(None));
        let applied = control_result.clone();
        let channel = Channel::new(move |body| {
            let event = body.deserialize::<serde_json::Value>()?;
            if event["phase"] == "preparing" {
                *applied.lock().unwrap() = Some(crate::cloud::upload_registry().apply(
                    CloudProvider::Xunlei,
                    &event_id,
                    "cancel",
                ));
            }
            received.lock().unwrap().push(event);
            Ok(())
        });
        let outcome = run_upload_session(
            CloudProvider::Xunlei,
            upload_id.clone(),
            channel,
            |_, _| async {
                Ok(summarize_upload_batches(vec![(
                    0,
                    "finished.txt".into(),
                    Ok(vec![file(
                        "finished",
                        "parent",
                        "finished.txt",
                        CloudFileKind::File,
                    )]),
                )]))
            },
        )
        .await
        .unwrap();
        assert!(control_result.lock().unwrap().as_ref().unwrap().is_ok());
        assert_eq!(outcome.uploaded[0].id, "finished");
        assert_eq!(serde_json::to_value(outcome).unwrap()["cancelled"], true);
        let events = events.lock().unwrap();
        assert_eq!(events.last().unwrap()["uploadId"], upload_id);
        assert_eq!(events.last().unwrap()["phase"], "cancelled");
        assert_eq!(events.last().unwrap()["failedItems"], 0);
        assert!(crate::cloud::upload_registry()
            .apply(CloudProvider::Xunlei, &upload_id, "pause")
            .is_err());
    }

    #[test]
    fn upload_control_cancelled_directory_preserves_confirmed_children_without_trash() {
        let plan = DirectoryUploadPlan {
            root_name: "fixture".into(),
            entries: vec![
                DirectoryUploadEntry::File {
                    relative_path: "first.txt".into(),
                    source: "first.txt".into(),
                    size: 4,
                },
                DirectoryUploadEntry::File {
                    relative_path: "second.txt".into(),
                    source: "second.txt".into(),
                    size: 6,
                },
            ],
        };
        let control = UploadControl::default();
        let mut trashed = Vec::new();
        let result = execute_directory_upload_plan(
            "parent",
            plan,
            || control.check(),
            |parent, name| Ok(file("new-root", parent, name, CloudFileKind::Folder)),
            |parent, path| {
                assert_eq!(path, Path::new("first.txt"));
                control.cancel();
                Ok(file("first", parent, "first.txt", CloudFileKind::File))
            },
            |id| {
                trashed.push(id.to_owned());
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert!(trashed.is_empty());
    }

    #[test]
    fn upload_progress_preflight_counts_files_and_reuses_the_directory_plan() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("empty")).unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        std::fs::write(temp.path().join("top.txt"), b"top").unwrap();
        std::fs::write(temp.path().join("nested/leaf.txt"), b"12345").unwrap();
        let tracker = UploadProgressTracker::new(CloudProvider::Xunlei);
        let prepared = prepare_upload_input(
            CloudProvider::Xunlei,
            temp.path(),
            &UploadControl::default(),
            &tracker,
            0,
        )
        .unwrap();
        let event = tracker.sample(std::time::Duration::ZERO);
        assert_eq!(event.total_files, 2);
        assert_eq!(event.total_bytes, 8);
        assert_eq!(event.uploaded_bytes, 0);
        std::fs::write(temp.path().join("late.txt"), b"not in the plan").unwrap();
        let PreparedUpload::Directory(plan) = prepared else {
            panic!("expected directory");
        };
        let mut uploaded_names = Vec::new();
        execute_directory_upload_plan(
            "parent",
            plan,
            || Ok(()),
            |parent, name| Ok(file(name, parent, name, CloudFileKind::Folder)),
            |parent, path| {
                let name = path.file_name().unwrap().to_str().unwrap();
                uploaded_names.push(name.to_owned());
                Ok(file(name, parent, name, CloudFileKind::File))
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(uploaded_names, ["leaf.txt", "top.txt"]);
    }

    #[test]
    fn upload_progress_only_confirms_successful_provider_results_and_keeps_input_index() {
        let tracker = UploadProgressTracker::new(CloudProvider::Xunlei);
        let first = Path::new("first.txt");
        let second = Path::new("second.txt");
        tracker.add_file(0, first, 3);
        tracker.add_file(1, second, 4);
        let control = UploadControl::default().with_progress(tracker.clone(), 0);
        let success = upload_file_with_progress(&control, first, || {
            let pending = tracker.sample(std::time::Duration::ZERO);
            assert_eq!(pending.completed_files, 0);
            assert_eq!(pending.current_files, ["first.txt"]);
            Ok(file("first", "parent", "first.txt", CloudFileKind::File))
        });
        let failure =
            upload_file_with_progress(&control.with_progress(tracker.clone(), 1), second, || {
                Err(QuarkError::Transport("offline".into()))
            });
        tracker.fail_input(1);
        let outcome = summarize_upload_batches(vec![
            (0, "first.txt".into(), success.map(|file| vec![file])),
            (1, "second.txt".into(), failure.map(|file| vec![file])),
        ]);
        assert_eq!(outcome.failures[0].input_index, 1);
        assert_eq!(outcome.uploaded[0].id, "first");
        let event = tracker.sample(std::time::Duration::ZERO);
        assert_eq!(event.completed_files, 1);
        assert_eq!(event.uploaded_bytes, 3);
        assert_eq!(event.failed_items, 1);
    }

    #[tokio::test]
    async fn upload_progress_channel_emits_camel_case_terminal_failure_before_returning_outcome() {
        let events = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let received = events.clone();
        let channel = Channel::new(move |body| {
            received
                .lock()
                .unwrap()
                .push(body.deserialize::<serde_json::Value>()?);
            Ok(())
        });
        let outcome = run_with_upload_channel(CloudProvider::Xunlei, channel, |_| async {
            Ok(summarize_upload_batches(vec![(
                2,
                "bad.txt".into(),
                Err(QuarkError::Transport("offline".into())),
            )]))
        })
        .await
        .unwrap();
        assert_eq!(outcome.failures[0].input_index, 2);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["phase"], "preparing");
        assert_eq!(
            events[1],
            serde_json::json!({
                "provider": "xunlei", "phase": "failed", "totalBytes": 0,
                "uploadId": "",
                "uploadedBytes": 0, "speedBytesPerSecond": 0.0, "totalFiles": 0,
                "completedFiles": 0, "failedItems": 1, "currentFiles": [], "progress": 0.0,
            })
        );
    }

    #[tokio::test]
    async fn upload_progress_channel_send_failure_does_not_abort_the_operation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let sent = attempts.clone();
        let channel = Channel::new(move |_| {
            sent.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("closed progress channel").into())
        });
        let outcome = run_with_upload_channel(CloudProvider::Xunlei, channel, |_| async {
            Ok(summarize_upload_batches(vec![(
                0,
                "ok.txt".into(),
                Ok(vec![file("ok", "parent", "ok.txt", CloudFileKind::File)]),
            )]))
        })
        .await
        .unwrap();
        assert_eq!(outcome.uploaded[0].id, "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    fn file(id: &str, parent_id: &str, name: &str, kind: CloudFileKind) -> CloudFile {
        CloudFile {
            id: id.into(),
            parent_id: parent_id.into(),
            name: name.into(),
            kind,
            size: None,
            modified_at: None,
        }
    }

    #[test]
    fn directory_upload_plan_preserves_empty_and_nested_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("empty")).unwrap();
        std::fs::create_dir_all(root.path().join("nested").join("child")).unwrap();
        std::fs::write(root.path().join("top.txt"), b"top").unwrap();
        std::fs::write(
            root.path().join("nested").join("child").join("leaf.txt"),
            b"leaf",
        )
        .unwrap();

        let plan = collect_directory_upload_entries(root.path()).unwrap();
        let relative = plan
            .iter()
            .map(|entry| entry.relative_path().to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();

        assert_eq!(
            relative,
            [
                "empty",
                "nested",
                "nested/child",
                "nested/child/leaf.txt",
                "top.txt"
            ]
        );
        assert!(plan[0].is_directory());
        assert!(plan[2].is_directory());
        assert!(!plan[3].is_directory());
    }

    #[test]
    fn directory_upload_plan_checks_cancellation_and_bounds_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), b"a").unwrap();
        std::fs::write(root.path().join("b.txt"), b"b").unwrap();
        let control = crate::cloud::UploadControl::default();

        assert!(matches!(
            build_directory_upload_plan_with_limits(
                CloudProvider::Xunlei,
                root.path(),
                &control,
                DirectoryUploadLimits { directories: 10, files: 1, depth: 10 },
            ),
            Err(QuarkError::InvalidInput(message)) if message == "上传目录包含过多文件"
        ));

        control.cancel();
        assert!(matches!(
            build_directory_upload_plan_with_limits(
                CloudProvider::Xunlei,
                root.path(),
                &control,
                DirectoryUploadLimits { directories: 10, files: 10, depth: 10 },
            ),
            Err(QuarkError::InvalidInput(message)) if message == "上传已取消"
        ));
    }

    #[test]
    fn baidu_empty_file_is_rejected_during_preflight() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("empty.txt"), []).unwrap();

        assert!(matches!(
            build_directory_upload_plan_with_limits(
                CloudProvider::Baidu,
                root.path(),
                &crate::cloud::UploadControl::default(),
                DirectoryUploadLimits { directories: 10, files: 10, depth: 10 },
            ),
            Err(QuarkError::InvalidInput(message)) if message == "百度网盘暂不支持上传空文件"
        ));
    }

    #[test]
    fn baidu_directory_upload_keeps_root_for_uncertain_commit() {
        let plan = DirectoryUploadPlan {
            root_name: "fixture".into(),
            entries: vec![DirectoryUploadEntry::File {
                relative_path: "leaf".into(), source: "leaf".into(), size: 1,
            }],
        };
        let result = execute_directory_upload_plan("parent", plan, || Ok(()),
            |parent, name| Ok(file("root", parent, name, CloudFileKind::Folder)),
            |_, _| Err(QuarkError::BaiduUploadAuthorizationRequired(BaiduUploadAuthStage::Commit)),
            |_| panic!("must preserve possibly committed files"),
        );
        assert!(matches!(result, Err(QuarkError::BaiduUploadAuthorizationRequired(BaiduUploadAuthStage::Commit))));
    }

    #[test]
    fn directory_upload_failure_trashes_only_the_new_root() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("leaf.txt");
        std::fs::write(&source, b"leaf").unwrap();
        let plan = DirectoryUploadPlan {
            root_name: "fixture".into(),
            entries: vec![DirectoryUploadEntry::File {
                relative_path: "leaf.txt".into(),
                source,
                size: 4,
            }],
        };
        let mut trashed = Vec::new();

        let result = execute_directory_upload_plan(
            "parent",
            plan,
            || Ok(()),
            |parent, name| Ok(file("new-root", parent, name, CloudFileKind::Folder)),
            |_, _| Err(QuarkError::Transport("offline".into())),
            |root_id| {
                trashed.push(root_id.to_owned());
                Ok(())
            },
        );

        assert!(matches!(result, Err(QuarkError::Transport(_))));
        assert_eq!(trashed, ["new-root"]);
    }

    #[test]
    fn directory_upload_cancellation_stops_before_the_next_mutation_and_retains_root() {
        let plan = DirectoryUploadPlan {
            root_name: "fixture".into(),
            entries: vec![DirectoryUploadEntry::Directory {
                relative_path: "empty".into(),
            }],
        };
        let mut checks = 0;
        let mut creates = Vec::new();
        let mut trashed = Vec::new();

        let result = execute_directory_upload_plan(
            "parent",
            plan,
            || {
                checks += 1;
                if checks == 2 {
                    Err(QuarkError::InvalidInput("上传已取消".into()))
                } else {
                    Ok(())
                }
            },
            |parent, name| {
                creates.push((parent.to_owned(), name.to_owned()));
                Ok(file("new-root", parent, name, CloudFileKind::Folder))
            },
            |_, _| unreachable!(),
            |root_id| {
                trashed.push(root_id.to_owned());
                Ok(())
            },
        );

        assert_eq!(result.unwrap()[0].id, "new-root");
        assert_eq!(creates, [("parent".into(), "fixture".into())]);
        assert!(trashed.is_empty());
    }

    #[test]
    fn directory_upload_rejects_a_conflicting_remote_name() {
        let destination = CloudDirectory {
            id: "parent".into(),
            items: vec![file("existing", "parent", "fixture", CloudFileKind::Folder)],
        };

        assert!(matches!(
            ensure_upload_root_available(&destination, "fixture"),
            Err(QuarkError::InvalidInput(message)) if message.contains("已存在同名项目")
        ));
    }

    #[test]
    fn upload_batch_outcome_keeps_successes_and_reports_failures() {
        let outcome = summarize_upload_batches(vec![
            (
                0,
                "a.txt".into(),
                Ok(vec![file("a", "root", "a.txt", CloudFileKind::File)]),
            ),
            (
                1,
                "b.txt".into(),
                Err(QuarkError::Transport("offline".into())),
            ),
        ]);

        assert_eq!(outcome.uploaded.len(), 1);
        assert_eq!(outcome.uploaded[0].id, "a");
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].input_index, 1);
        assert_eq!(outcome.failures[0].item_name, "b.txt");
        assert_eq!(outcome.failures[0].code, "network_error");
    }
}

#[cfg(test)]
#[path = "live_baidu_upload_test.rs"]
mod live_baidu_upload_test;

#[cfg(test)]
mod upload_session_isolation_tests {
    use super::*;
    use std::{collections::BTreeMap, io::{Read, Write}, net::TcpListener, time::Duration};

    #[test]
    fn upload_token_cookie_survives_a_concurrent_account_cookie_update() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", server.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let mut upload_cookie_matched = false;
            for stage in 0..3 {
                let (mut stream, _) = server.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let header = String::from_utf8(header).unwrap();
                let cookie = header.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("Cookie").then(|| value.trim())
                }).unwrap();
                let (status, rotation, body) = match stage {
                    0 => {
                        assert!(header.starts_with("GET /token "));
                        assert_eq!(cookie, "fixture=initial");
                        ("200 OK", "Set-Cookie: fixture=upload-state; Path=/\r\n", "token-for-upload-state")
                    }
                    1 => {
                        assert!(header.starts_with("GET /background-account "));
                        ("200 OK", "Set-Cookie: fixture=background-state; Path=/\r\n", "{}")
                    }
                    _ => {
                        assert!(header.starts_with("POST /precreate?token=token-for-upload-state "));
                        upload_cookie_matched = cookie == "fixture=upload-state";
                        (if upload_cookie_matched { "200 OK" } else { "409 Conflict" }, "", "{}")
                    }
                };
                write!(stream, "HTTP/1.1 {status}\r\n{rotation}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            upload_cookie_matched
        });
        let shared = ReqwestTransport::new().unwrap();
        let request = |method, path: &str| CloudHttpRequest {
            method, url: format!("{origin}{path}"),
            headers: BTreeMap::from([("Cookie".into(), "fixture=initial".into())]),
            body: None,
        };
        let response = with_upload_http(|http| {
            let token = http.send(request("GET", "/token"))?;
            assert_eq!(token.status, 200);
            // A background account response arrives between token and precreate.
            shared.send(request("GET", "/background-account"))?;
            http.send(request("POST", &format!("/precreate?token={}", token.body)))
        }).unwrap();
        let matched = worker.join().unwrap();
        assert_eq!(response.status, 200, "upload state was changed by an unrelated request");
        assert!(matched);
    }
}
