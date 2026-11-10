use super::*;
use crate::cloud::upload::{report_upload_progress, UploadProgressTracker};
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
    let control = UploadControl::default();
    let _registration =
        crate::cloud::upload_registry().register(provider, &upload_id, control.clone())?;
    let tracker = UploadProgressTracker::new(provider);
    tracker.bind_session(upload_id, control.clone());
    report_upload_progress(
        tracker.clone(),
        move |event| {
            let _ = on_progress.send(event);
        },
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
                ProviderRegistry::global()
                    .driver(provider, &state.store, http)
                    .upload(state, http, remote_parent, source)
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
    control: crate::cloud::UploadControl,
) -> Result<Vec<CloudFile>, QuarkError> {
    control.wait_ready_blocking()?;
    let transport = ReqwestTransport::new()?;
    let http = crate::cloud::http::ControlledUploadHttp {
        inner: &transport,
        control: control.clone(),
    };
    match prepared {
        PreparedUpload::Directory(plan) => {
            upload_directory_tree(state, &http, provider, parent_id, plan, &control)
        }
        PreparedUpload::File(path) => Ok(vec![upload_file_with_progress(&control, &path, || {
            ProviderRegistry::global()
                .driver(provider, &state.store, &http)
                .upload(state, &http, parent_id, &path)
        })?]),
    }
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
        QuarkError::BaiduOperationVerificationRequired => "baidu_operation_verification_required",
        QuarkError::WebAuthorizationRequired | QuarkError::CaptchaRequired => "web_auth_required",
        QuarkError::PikpakRegionalMembershipRestricted => "pikpak_regional_membership_restricted",
        QuarkError::AuthorizationPending => "authorization_pending",
        QuarkError::SourceFileMissing => "source_file_missing",
        QuarkError::DownloadCheckpointInvalid => "download_checkpoint_invalid",
        QuarkError::DownloadEngineUnavailable => "download_engine_unavailable",
        QuarkError::DownloadSubmissionFailed => "download_submission_failed",
        QuarkError::XunleiNativeDownload(failure) => failure.code(),
        QuarkError::BaiduTransferPending => "baidu_transfer_pending",
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
                    upload_selected_path(&state, provider, &parent_id, prepared, control.clone())
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
        self.with_upload_session(CloudProvider::Pikpak, http, |store| {
            PikPakClient::new(http, store).upload_file(parent, path)
        })
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;

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
