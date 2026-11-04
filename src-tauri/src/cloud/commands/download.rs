use super::*;
use uuid::Uuid;

pub(crate) const MAX_DIRECTORY_DOWNLOAD_DIRECTORIES: usize = 1_000;

pub(crate) const MAX_DIRECTORY_DOWNLOAD_FILES: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedCloudFile {
    pub file_id: String,
    pub relative_directory: Vec<String>,
    pub output_name: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudDirectoryDownloadPlan {
    pub files: Vec<PlannedCloudFile>,
}

/// Builds a bounded, path-safe file plan before any signed URL is resolved or
/// task is submitted. A failed plan therefore cannot leave a partial queue.
pub fn plan_cloud_directory_download(
    provider: CloudProvider,
    root_id: &str,
    root_name: &str,
    mut list_directory: impl FnMut(&str) -> Result<CloudDirectory, QuarkError>,
) -> Result<CloudDirectoryDownloadPlan, QuarkError> {
    if root_id.trim().is_empty() {
        return Err(QuarkError::InvalidInput("目录 ID 无效".into()));
    }
    if !ProviderRegistry::global()
        .get(provider)
        .recycle_directory_download
        && root_id
            == ProviderRegistry::global()
                .get(provider)
                .recycle_bin_directory_id
    {
        return Err(QuarkError::InvalidInput("PikPak 回收站不能下载目录".into()));
    }

    let mut files = Vec::new();
    let mut visited_directories = HashSet::new();
    let mut visited_files = HashSet::new();
    let mut pending = vec![(
        root_id.to_owned(),
        vec![safe_directory_component(root_name)],
    )];

    while let Some((directory_id, relative_directory)) = pending.pop() {
        if visited_directories.len() >= MAX_DIRECTORY_DOWNLOAD_DIRECTORIES {
            return Err(QuarkError::InvalidInput("目录下载包含过多子目录".into()));
        }
        if !visited_directories.insert(directory_id.clone()) {
            return Err(QuarkError::InvalidInput(
                "目录下载检测到循环或重复目录".into(),
            ));
        }

        let directory = list_directory(&directory_id)?;
        if directory.id != directory_id {
            return Err(QuarkError::InvalidResponse);
        }
        for item in directory.items.into_iter().rev() {
            match item.kind.as_str() {
                "file" => {
                    if files.len() >= MAX_DIRECTORY_DOWNLOAD_FILES {
                        return Err(QuarkError::InvalidInput("目录下载包含过多文件".into()));
                    }
                    if item.id.trim().is_empty() || !visited_files.insert(item.id.clone()) {
                        return Err(QuarkError::InvalidInput("目录下载包含重复文件".into()));
                    }
                    files.push(PlannedCloudFile {
                        file_id: item.id,
                        relative_directory: relative_directory.clone(),
                        output_name: safe_filename(&item.name),
                        size: item.size,
                    });
                }
                "folder" => {
                    if item.id.trim().is_empty() {
                        return Err(QuarkError::InvalidResponse);
                    }
                    if !ProviderRegistry::global()
                        .get(provider)
                        .recycle_directory_download
                        && item.id
                            == ProviderRegistry::global()
                                .get(provider)
                                .recycle_bin_directory_id
                    {
                        return Err(QuarkError::InvalidInput("PikPak 回收站不能下载目录".into()));
                    }
                    let mut child_path = relative_directory.clone();
                    child_path.push(safe_directory_component(&item.name));
                    pending.push((item.id, child_path));
                }
                _ => return Err(QuarkError::InvalidResponse),
            }
        }
    }

    if files.is_empty() {
        return Err(QuarkError::InvalidInput("空目录不能下载".into()));
    }
    if has_windows_output_path_collision(&files) {
        return Err(QuarkError::InvalidInput("目录下载的输出路径冲突".into()));
    }
    Ok(CloudDirectoryDownloadPlan { files })
}

/// Converts an already-inspected share selection into the same durable plan
/// used by personal-cloud directories. Stable `cloud-share://` sources become
/// group item identities; provider credentials and signed URLs remain only in
/// the encrypted share-plan store.
pub fn plan_cloud_share_directory_download(
    selection: &share::CloudShareDownloadSelection,
) -> Result<CloudDirectoryDownloadPlan, QuarkError> {
    let root_name = selection
        .directory_name
        .as_deref()
        .ok_or_else(|| QuarkError::InvalidInput("该分享选择不是目录下载".into()))?;
    if selection.entries.is_empty() {
        return Err(QuarkError::InvalidInput("空目录不能下载".into()));
    }
    if selection.entries.len() > MAX_DIRECTORY_DOWNLOAD_FILES {
        return Err(QuarkError::InvalidInput("目录下载包含过多文件".into()));
    }
    let root = safe_directory_component(root_name);
    let strip_promoted_root = selection.entries.iter().all(|entry| {
        entry
            .relative_directory
            .first()
            .is_some_and(|component| component == root_name)
    });
    let mut sources = HashSet::new();
    let files = selection
        .entries
        .iter()
        .map(|entry| {
            share::parse_stable_cloud_share_source(&entry.source)?;
            if !sources.insert(entry.source.clone()) {
                return Err(QuarkError::InvalidInput("目录下载包含重复文件".into()));
            }
            let mut relative_directory = Vec::with_capacity(entry.relative_directory.len() + 1);
            relative_directory.push(root.clone());
            relative_directory.extend(
                entry
                    .relative_directory
                    .iter()
                    .skip(usize::from(strip_promoted_root))
                    .map(|component| safe_directory_component(component)),
            );
            Ok(PlannedCloudFile {
                file_id: entry.source.clone(),
                relative_directory,
                output_name: safe_filename(&entry.name),
                size: entry.size,
            })
        })
        .collect::<Result<Vec<_>, QuarkError>>()?;
    if has_windows_output_path_collision(&files) {
        return Err(QuarkError::InvalidInput("目录下载的输出路径冲突".into()));
    }
    Ok(CloudDirectoryDownloadPlan { files })
}

pub(crate) fn has_windows_output_path_collision(files: &[PlannedCloudFile]) -> bool {
    let mut file_targets = HashSet::new();
    let mut directories = HashSet::new();
    files.iter().any(|file| {
        let target = file
            .relative_directory
            .iter()
            .chain(std::iter::once(&file.output_name))
            .map(|component| component.to_lowercase())
            .collect::<Vec<_>>()
            .join("\\");
        if file_targets.contains(&target) || directories.contains(&target) {
            return true;
        }
        for (index, _) in target.match_indices('\\') {
            let ancestor = &target[..index];
            if file_targets.contains(ancestor) {
                return true;
            }
            directories.insert(ancestor.to_owned());
        }
        !file_targets.insert(target)
    })
}

pub(crate) fn safe_directory_component(name: &str) -> String {
    let value = safe_filename(name);
    if matches!(value.as_str(), "." | "..") {
        "folder".into()
    } else {
        value
    }
}

pub fn enqueue_cloud_download(
    store: &TaskStore,
    url: &str,
    name: &str,
    _directory: &Path,
    enqueue: impl FnOnce(&str) -> Result<String, String>,
) -> Result<DownloadTask, QuarkError> {
    let task = store
        .create(NewTask {
            source: url.into(),
            name: name.into(),
            kind: "cloud".into(),
            is_private: false,
        })
        .map_err(|_| QuarkError::Storage)?;
    // Keep parsing until engine acceptance so a failure can be recorded with a valid transition.
    let gid = match enqueue(url) {
        Ok(g) => g,
        Err(message) => {
            let _ = store.fail(task.id, &message);
            return Err(QuarkError::Transport(message));
        }
    };
    store
        .bind_gid(task.id, &gid)
        .map_err(|_| QuarkError::Storage)?;
    store
        .transition(task.id, TaskStatus::Queued)
        .and_then(|_| store.transition(task.id, TaskStatus::Downloading))
        .map_err(|_| QuarkError::Storage)
}

pub fn create_cloud_task(
    store: &TaskStore,
    provider: CloudProvider,
    file_id: &str,
    name: &str,
) -> Result<DownloadTask, QuarkError> {
    if file_id.trim().is_empty() {
        return Err(QuarkError::InvalidInput("网盘文件 ID 无效".into()));
    }
    store
        .create(NewTask {
            source: format!(
                "cloud://{}/{}",
                provider.key(),
                crate::cloud::types::encoded(file_id)
            ),
            name: safe_filename(name),
            kind: "cloud".into(),
            is_private: false,
        })
        .map_err(|_| QuarkError::Storage)
}

/// Decodes the durable cloud task source. Its restrictive shape prevents a
/// signed URL or arbitrary endpoint from being substituted during retry.
pub fn parse_stable_cloud_source(source: &str) -> Result<(CloudProvider, String), QuarkError> {
    let invalid = || QuarkError::InvalidInput("网盘任务来源无效，请从网盘重新创建下载".into());
    let (provider, encoded) = source
        .strip_prefix("cloud://")
        .and_then(|value| value.split_once('/'))
        .ok_or_else(invalid)?;
    let provider = crate::cloud::ProviderRegistry::global()
        .by_key(provider)
        .ok_or_else(invalid)?
        .provider;
    if encoded.is_empty() || encoded.contains(['/', '?', '#', '&', '=']) || !encoded.is_ascii() {
        return Err(invalid());
    }
    for (index, byte) in encoded.bytes().enumerate() {
        if byte == b'%'
            && !encoded
                .as_bytes()
                .get(index + 1..index + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
        {
            return Err(invalid());
        }
    }
    let id = url::form_urlencoded::parse(format!("id={encoded}").as_bytes())
        .next()
        .ok_or_else(invalid)?
        .1
        .into_owned();
    if id.trim().is_empty()
        || id
            .chars()
            .any(|character| character.is_control() || character == '\u{fffd}')
    {
        return Err(invalid());
    }
    Ok((provider, id))
}

/// Retrieves a fresh signed URL and request headers only for immediate aria2
/// submission. Neither value is stored in `TaskStore`.
pub async fn resolve_stable_cloud_download(
    app: AppHandle,
    source: &str,
) -> Result<ResolvedCloudDownload, QuarkError> {
    let (provider, file_id) = parse_stable_cloud_source(source)?;
    run_session_work(app, provider, SessionWorkKind::Read, move |state, http| {
        resolve_download(provider, &file_id, &state.store, http)
    })
    .await
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudDirectoryDownloadFailure {
    pub relative_path: String,
    pub code: &'static str,
    pub message: &'static str,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudDirectoryDownloadOutcome {
    pub task: TaskActionOutcome,
    pub queued_count: usize,
    pub failures: Vec<CloudDirectoryDownloadFailure>,
}

/// The only two durable outcomes of submitting one planned directory leaf.
/// The resolver and aria2 details stay behind the submission boundary.
pub enum DirectoryLeafSubmission {
    Queued(uuid::Uuid),
    Duplicate,
}

#[derive(Clone, Copy, Debug)]
pub struct DirectoryLeafContext {
    pub group_id: uuid::Uuid,
    pub item_id: uuid::Uuid,
}

/// Persists the complete safe directory plan before any leaf receives a
/// signed URL. The journal contains only stable provider IDs and local output
/// components, never a URL or session material.
pub fn create_cloud_directory_group_for_download(
    store: &TaskStore,
    provider: CloudProvider,
    directory_id: &str,
    plan: &CloudDirectoryDownloadPlan,
    output_directory: &Path,
    private_download: bool,
) -> Result<CloudDirectoryGroup, QuarkError> {
    let relative_directory = plan
        .files
        .first()
        .and_then(|file| file.relative_directory.first())
        .ok_or_else(|| QuarkError::InvalidInput("空目录不能下载".into()))?
        .clone();
    let items = plan
        .files
        .iter()
        .map(|file| {
            NewCloudDirectoryGroupItem::new(
                file.file_id.clone(),
                &file.relative_directory,
                file.output_name.clone(),
                file.size,
            )
        })
        .collect::<Vec<_>>();
    store
        .create_cloud_directory_group(
            NewCloudDirectoryGroup::new(
                provider,
                directory_id,
                relative_directory,
                output_directory,
            )
            .with_private_download(private_download),
            &items,
        )
        .map_err(|_| QuarkError::Storage)
}

pub(crate) enum CloudFileSubmission {
    Queued(TaskActionOutcome),
    Duplicate(TaskActionOutcome),
}

pub(crate) fn source_duplicate_blocks_submission(
    directory_context: Option<DirectoryLeafContext>,
) -> bool {
    directory_context.is_none()
}

pub(crate) fn validate_download_account(
    current: Option<&str>,
    expected: &str,
) -> Result<(), QuarkError> {
    let current = current
        .filter(|account_id| !account_id.trim().is_empty())
        .ok_or(QuarkError::NotAuthorized)?;
    if current != expected {
        return Err(QuarkError::AccountMismatch);
    }
    Ok(())
}

pub(crate) fn require_task_download_owner(owner: Option<String>) -> Result<String, QuarkError> {
    owner
        .filter(|account_id| !account_id.trim().is_empty())
        .ok_or(QuarkError::AccountMismatch)
}

pub(crate) fn require_download_account(
    app: &AppHandle,
    provider: CloudProvider,
    expected: &str,
) -> Result<(), QuarkError> {
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    let record = cloud.auth_supervisor.record(&cloud.store, provider)?;
    validate_download_account(record.account_id.as_deref(), expected)
}

fn download_account(app: &AppHandle, provider: CloudProvider) -> Result<String, QuarkError> {
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    cloud
        .auth_supervisor
        .record(&cloud.store, provider)?
        .account_id
        .filter(|account_id| !account_id.trim().is_empty())
        .ok_or(QuarkError::NotAuthorized)
}

/// Public-share downloads may not have an authenticated account. When one is
/// already ready, persist it with the local task so a later authorization
/// recovery remains scoped to the account that created the download.
pub(crate) fn ready_cloud_download_account(
    app: &AppHandle,
    provider: CloudProvider,
) -> Result<Option<String>, QuarkError> {
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    let record = cloud.auth_supervisor.record(&cloud.store, provider)?;
    Ok((record.health == auth_supervisor::AuthHealth::Ready)
        .then_some(record.account_id)
        .flatten()
        .filter(|account| !account.trim().is_empty()))
}

pub(crate) async fn submit_cloud_file(
    app: AppHandle,
    state: &AppState,
    provider: CloudProvider,
    file_id: String,
    configured: UriSubmissionOptions,
    private_download: bool,
    directory_context: Option<DirectoryLeafContext>,
) -> Result<CloudFileSubmission, QuarkError> {
    let stable_source = format!(
        "cloud://{}/{}",
        provider.key(),
        crate::cloud::types::encoded(&file_id)
    );
    let _source_submission = state.cloud_download_sources.acquire(&stable_source).await?;
    // A directory leaf has a different concrete target from an ordinary task
    // for the same cloud file: the directory root and its nested components
    // are part of its output path. Do not let a prior standalone download
    // suppress that leaf merely because their stable cloud sources match.
    // The group's item journal still prevents that leaf itself from queuing
    // twice, while standalone downloads retain source-level de-duplication.
    if source_duplicate_blocks_submission(directory_context) {
        if let Some(existing) = state
            .store
            .find_by_source(&stable_source)
            .map_err(|_| QuarkError::Storage)?
            .filter(|task| task.status != TaskStatus::Recycled)
        {
            ensure_private_task_access(state, &existing).map_err(QuarkError::InvalidInput)?;
            let duplicate =
                duplicate_state(&state.store, &existing).map_err(|_| QuarkError::Storage)?;
            return Ok(CloudFileSubmission::Duplicate(
                task_action_outcome(&state.store, existing, Some(duplicate))
                    .map_err(QuarkError::Transport)?,
            ));
        }
    }

    let account_id = download_account(&app, provider)?;
    if let Some(context) = directory_context {
        state
            .store
            .bind_cloud_download_account(context.group_id, provider, &account_id)
            .map_err(|_| QuarkError::NotAuthorized)?;
    }
    require_download_account(&app, provider, &account_id)?;
    let task_name = configured
        .output_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&file_id);
    let task = if let Some(context) = directory_context {
        state
            .store
            .claim_cloud_directory_leaf_task(
                context.group_id,
                context.item_id,
                provider,
                &account_id,
                NewTask {
                    source: stable_source,
                    name: safe_filename(task_name),
                    kind: "cloud".into(),
                    is_private: private_download,
                },
            )
            .map_err(|_| QuarkError::Storage)?
            .ok_or_else(|| QuarkError::InvalidInput("目录已回收或文件已入队".into()))?
    } else {
        let task = create_cloud_task(&state.store, provider, &file_id, task_name)?;
        let task = state
            .store
            .set_private(task.id, private_download)
            .map_err(|_| QuarkError::Storage)?;
        state
            .store
            .bind_cloud_download_account(task.id, provider, &account_id)
            .map_err(|_| QuarkError::Storage)?;
        task
    };
    let task = queue_cloud_dispatch(app, state, task, configured).await?;
    Ok(CloudFileSubmission::Queued(
        task_action_outcome(&state.store, task, None).map_err(QuarkError::Transport)?,
    ))
}

/// Captures only ephemeral options; the task source and existing group/share
/// metadata remain the authority for a later user-initiated resume or retry.
pub async fn queue_cloud_dispatch(
    app: AppHandle,
    state: &AppState,
    task: DownloadTask,
    mut configured: UriSubmissionOptions,
) -> Result<DownloadTask, QuarkError> {
    let share = task.source.starts_with("cloud-share://");
    let provider = if share {
        share::parse_stable_cloud_share_source(&task.source)?.0
    } else {
        parse_stable_cloud_source(&task.source)?.0
    };
    configured.output_name.get_or_insert(task.name.clone());
    configured
        .clone()
        .into_aria2_options()
        .map_err(|_| QuarkError::InvalidInput("下载路径或文件名无效".into()))?;
    let (queued, version) = state
        .cloud_download_dispatch
        .prepare_expected_with_retry_intent(
            &state.store,
            &task,
            configured.allow_overwrite,
            configured.continue_download,
        )
        .await?;
    configured = state
        .cloud_download_dispatch
        .retry_submission_options(task.id, configured);
    let directory_id = state
        .store
        .cloud_directory_group_for_task(task.id)
        .map_err(|_| QuarkError::Storage)?
        .map(|group| group.id);
    if let Some(directory_id) = directory_id {
        let max_active_members = state
            .settings
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .aria2
            .max_connections_per_server;
        state.cloud_download_dispatch.configure_directory(
            task.id,
            directory_id,
            max_active_members,
        );
    }
    emit_cloud_dispatch_update(&app, state, &queued);
    let id = queued.id;
    let source = queued.source.clone();
    let replacing_gid = (queued.failure_code.as_deref()
        == Some(crate::cloud::download_dispatch::CLOUD_URL_REFRESHING))
    .then(|| queued.aria_gid.clone())
    .flatten();
    let directory_member = directory_id.is_some();
    let dispatch = state.cloud_download_dispatch.clone();
    let store = state.store.clone();
    tauri::async_runtime::spawn(async move {
        let worker_app = app.clone();
        let submit_app = app.clone();
        let result = dispatch
            .run(
                &store,
                provider,
                id,
                version,
                || async {
                    if !share {
                        check_dispatch_owner(&worker_app, &store, provider, id)?;
                    }
                    if share {
                        resolve_stable_cloud_share_download(worker_app, &source).await
                    } else {
                        resolve_stable_cloud_download(worker_app, &source).await
                    }
                },
                |resolved| async move {
                    let state = submit_app.state::<AppState>();
                    if !share {
                        check_dispatch_owner(&submit_app, &state.store, provider, id)?;
                    }
                    // Refresh user settings at the last possible moment. Keep the
                    // configured proxy intact, including authentication and routing.
                    let current = crate::commands::download_submission_options(&state)
                        .map_err(|_| QuarkError::Storage)?;
                    let configured_connections = state
                        .settings
                        .lock()
                        .map_err(|_| QuarkError::Storage)?
                        .aria2
                        .max_connections_per_server;
                    configured.proxy = current.proxy;
                    let configured = if provider == CloudProvider::Quark {
                        let connections = crate::cloud::quark_initial_connection_count(
                            &state.store,
                            id,
                            configured_connections,
                        )
                        .map_err(|_| QuarkError::Storage)?;
                        configured.max_connections_per_server = Some(connections);
                        configured
                    } else if directory_member {
                        apply_cloud_directory_member_policy(
                            provider,
                            configured_connections,
                            configured,
                        )
                    } else {
                        apply_cloud_download_policy(provider, configured_connections, configured)
                    };
                    let mut xunlei_native_plugin_update_required = false;
                    if provider == CloudProvider::Xunlei {
                        match state.xunlei_native_backend.submit(
                            &submit_app,
                            &state.store,
                            id,
                            &resolved,
                            &configured,
                        ) {
                            Ok(Some(task)) => return Ok(task),
                            Ok(None) => {
                                xunlei_native_plugin_update_required =
                                    xunlei_native_fallback_allowed(
                                        configured.native_resume_gid.as_deref(),
                                        false,
                                    )?;
                            }
                            Err(crate::xunlei_native::NativeSubmitError::Incompatible) => {
                                xunlei_native_plugin_update_required =
                                    xunlei_native_fallback_allowed(
                                        configured.native_resume_gid.as_deref(),
                                        true,
                                    )?;
                            }
                            Err(error) => {
                                return Err(QuarkError::Transport(error.user_message()));
                            }
                        }
                    }
                    let service = state
                        .runtime
                        .service()
                        .await
                        .map_err(|_| QuarkError::DownloadEngineUnavailable)?;
                    let (signed_url, options) =
                        share::share_submission_options(resolved, configured)?;
                    let mut gateway_registration = None;
                    let (url, options) = if requires_stable_range_gateway(provider) {
                        let gateway = state
                            .baidu_range_gateway
                            .get_or_try_init(|| async {
                                crate::cloud::baidu_range_gateway::BaiduRangeGateway::start()
                                    .await
                                    .map(Arc::new)
                            })
                            .await
                            .map_err(|_| QuarkError::DownloadSubmissionFailed)?;
                        let (url, options, registration) = register_stable_range_gateway_download(
                            gateway,
                            id,
                            signed_url,
                            options,
                            replacing_gid.is_some(),
                        )?;
                        gateway_registration = Some((Arc::clone(gateway), registration));
                        (url, options)
                    } else {
                        (signed_url, options)
                    };
                    let submitted = if let Some(expected_gid) = replacing_gid.as_deref() {
                        // The old GID is a native checkpoint only.  Replace it
                        // atomically after aria2 has accepted this freshly
                        // resolved provider URL; generic recovery can never
                        // reactivate the saved URL in between.
                        service
                            .replace_cloud_uri_with_options(id, expected_gid, vec![url], options)
                            .await
                    } else {
                        service
                            .submit_uri_with_options(id, vec![url], options)
                            .await
                    };
                    if let Some((gateway, mut registration)) = gateway_registration {
                        if submitted.is_ok() {
                            gateway.retire(id, registration.token());
                            registration.commit();
                        }
                    }
                    let submitted = submitted.map_err(cloud_download_submission_error)?;
                    let submitted = if xunlei_native_plugin_update_required {
                        let mut submitted = submitted;
                        submitted.status_message =
                            Some(crate::xunlei_native::PLUGIN_UPDATE_MESSAGE.into());
                        state
                            .store
                            .apply_transfer_snapshot(submitted)
                            .map_err(|_| QuarkError::Storage)?
                    } else {
                        submitted
                    };
                    if provider == CloudProvider::Quark {
                        crate::cloud::activate_quark_download_scheduler(submit_app.clone());
                    }
                    Ok(submitted)
                },
            )
            .await;
        if let Some(task) = result {
            let state = app.state::<AppState>();
            if matches!(
                task.status,
                TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
                    | TaskStatus::Recycled
            ) {
                let directory_finished = directory_id.is_some_and(|group_id| {
                    crate::commands::cloud_directory_group_queue_task(&state.store, group_id)
                        .is_ok_and(|root| {
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
            emit_cloud_dispatch_update(&app, &state, &task);
            schedule_cloud_share_cleanup(app.clone(), &task);
        }
    });
    Ok(queued)
}

fn check_dispatch_owner(
    app: &AppHandle,
    store: &TaskStore,
    provider: CloudProvider,
    id: uuid::Uuid,
) -> Result<(), QuarkError> {
    let owner = require_task_download_owner(
        store
            .cloud_download_account(id)
            .map_err(|_| QuarkError::Storage)?,
    )?;
    require_download_account(app, provider, &owner)
}

pub(crate) fn emit_cloud_dispatch_update(app: &AppHandle, state: &AppState, task: &DownloadTask) {
    let _ = crate::commands::emit_task_update_for_task(app, &state.store, None, task.clone());
}

pub(crate) fn register_stable_range_gateway_download(
    gateway: &crate::cloud::baidu_range_gateway::BaiduRangeGateway,
    task_id: Uuid,
    signed_url: String,
    mut options: UriSubmissionOptions,
    refreshing: bool,
) -> Result<
    (
        String,
        UriSubmissionOptions,
        crate::cloud::baidu_range_gateway::GatewayRegistration,
    ),
    QuarkError,
> {
    let registration = if refreshing {
        gateway.register_refreshing(
            task_id,
            signed_url,
            options.headers.clone(),
            options.proxy.clone(),
        )
    } else {
        gateway.register(
            task_id,
            signed_url,
            options.headers.clone(),
            options.proxy.clone(),
        )
    }
    .map_err(|_| QuarkError::DownloadSubmissionFailed)?;
    options.headers.clear();
    options.proxy = None;
    options.direct_loopback = true;
    Ok((registration.url().to_owned(), options, registration))
}

/// Signed CDN URLs that must be refreshed keep one loopback URI in aria2 so
/// native range checkpoints never retain an expired provider URL as a mirror.
pub(crate) fn requires_stable_range_gateway(provider: CloudProvider) -> bool {
    matches!(provider, CloudProvider::Baidu | CloudProvider::Pikpak)
}

pub fn apply_cloud_download_policy(
    provider: CloudProvider,
    configured_connections: u8,
    mut options: UriSubmissionOptions,
) -> UriSubmissionOptions {
    options.max_connections_per_server = Some(
        configured_connections.min(
            ProviderRegistry::global()
                .get(provider)
                .download_concurrency
                .max_connections_per_file,
        ),
    );
    if provider == CloudProvider::Xunlei {
        // Empty CDN replies are transient. Keep recovery in the engine so it
        // retains its verified ranges and remains interruptible by pause/cancel.
        options.max_tries = Some(5);
        options.retry_wait_seconds = Some(3);
    }
    options
}

/// aria2 can resume its own checkpoint, but it cannot validate the private
/// range map saved by Thunder. A native retry must therefore remain on the
/// native backend when the retry intent contains a native task identity.
fn xunlei_native_fallback_allowed(
    native_resume_gid: Option<&str>,
    plugin_update_required: bool,
) -> Result<bool, QuarkError> {
    if crate::xunlei_native::is_native_gid(native_resume_gid) {
        let message = if plugin_update_required {
            crate::xunlei_native::PLUGIN_UPDATE_MESSAGE
        } else {
            crate::xunlei_native::NATIVE_RESUME_BACKEND_UNAVAILABLE_MESSAGE
        };
        return Err(QuarkError::Transport(message.into()));
    }
    Ok(plugin_update_required)
}

/// Baidu's range gateway can safely split each file into concurrent 4 MiB
/// requests. Other providers retain directory-level parallelism by using one
/// connection per member.
pub fn apply_cloud_directory_member_policy(
    provider: CloudProvider,
    configured_connections: u8,
    mut options: UriSubmissionOptions,
) -> UriSubmissionOptions {
    if provider == CloudProvider::Baidu {
        return apply_cloud_download_policy(provider, configured_connections, options);
    }
    options.max_connections_per_server = Some(1);
    options
}

pub(crate) fn planned_relative_path(file: &PlannedCloudFile) -> String {
    file.relative_directory
        .iter()
        .chain(std::iter::once(&file.output_name))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn directory_download_output_directory(
    base: &Path,
    components: &[String],
) -> Result<PathBuf, QuarkError> {
    if !base.is_absolute() {
        return Err(QuarkError::InvalidInput("下载目录必须是绝对路径".into()));
    }
    let mut target = base.to_path_buf();
    for component in components {
        target.push(provider_directory_component(component)?);
    }
    // Check lexical containment before touching the filesystem, even if the
    // component normalization above changes in the future.
    if !target.strip_prefix(base).is_ok_and(|relative| {
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    }) {
        return Err(QuarkError::InvalidResponse);
    }
    std::fs::create_dir_all(&target)
        .map_err(|_| QuarkError::Transport("无法创建目录下载目标文件夹".into()))?;
    Ok(target)
}

/// Restores the exact target of a linked directory member before a retry.
/// This is needed when resolution or engine submission failed before aria2
/// could persist a file path for the local task.
pub fn cloud_directory_member_retry_options(
    store: &TaskStore,
    task_id: uuid::Uuid,
    mut configured: UriSubmissionOptions,
) -> Result<UriSubmissionOptions, QuarkError> {
    let Some(group) = store
        .cloud_directory_group_for_task(task_id)
        .map_err(|_| QuarkError::Storage)?
    else {
        return Ok(configured);
    };
    let item = store
        .cloud_directory_group_items(group.id)
        .map_err(|_| QuarkError::Storage)?
        .into_iter()
        .find(|item| item.task_id == Some(task_id))
        .ok_or(QuarkError::Storage)?;
    configured.directory = Some(directory_download_output_directory(
        &group.output_directory,
        &item.relative_directory,
    )?);
    configured.output_name = Some(item.output_name);
    Ok(configured)
}

pub(crate) fn provider_directory_component(component: &str) -> Result<String, QuarkError> {
    let mut path_components = Path::new(component).components();
    // Reject drive prefixes, ADS and device names before safe_filename could
    // hide the unsafe provider input. Validate all components before mkdir.
    if component.contains(['/', '\\', ':'])
        || is_windows_device_name(component)
        || component.chars().any(char::is_control)
        || !matches!(path_components.next(), Some(Component::Normal(_)))
        || path_components.next().is_some()
    {
        return Err(QuarkError::InvalidResponse);
    }
    // Confirmation renders these names with the same filename normalization.
    Ok(safe_filename(component))
}

pub(crate) fn directory_download_failure(
    file: &PlannedCloudFile,
    error: &QuarkError,
) -> CloudDirectoryDownloadFailure {
    let (code, message) = directory_download_failure_fields(error);
    CloudDirectoryDownloadFailure {
        relative_path: planned_relative_path(file),
        code,
        message,
    }
}

pub(crate) fn directory_download_failure_fields(
    error: &QuarkError,
) -> (&'static str, &'static str) {
    match error {
        QuarkError::XunleiVerificationUnavailable(reason) => ("xunlei_verification_unavailable", reason),
        QuarkError::AuthorizationExpired | QuarkError::NotAuthorized => {
            ("credentials_expired", "网盘会话已过期")
        }
        QuarkError::AccountMismatch => (
            "account_mismatch",
            "当前登录的网盘账号与下载任务所属账号不一致，请切换账号或取消该任务",
        ),
        QuarkError::SessionRenewedRetryOperation => (
            "session_renewed_retry_operation",
            "登录已自动恢复，请重新执行本次操作",
        ),
        QuarkError::WebAuthorizationRequired | QuarkError::CaptchaRequired => {
            ("web_auth_required", "网盘需要网页验证")
        }
        QuarkError::PikpakOperationVerificationRequired(_) => (
            "pikpak_operation_verification_required",
            "PikPak 需要官方操作验证，请在我的网盘中完成验证后重试",
        ),
        QuarkError::BaiduOperationVerificationRequired => (
            "baidu_operation_verification_required",
            "baidu_operation_verification_required",
        ),
        QuarkError::SourceFileMissing => (
            "source_file_missing",
            "网盘中的源文件已不存在，请刷新网盘列表后重新创建下载",
        ),
        QuarkError::DownloadCheckpointInvalid => ("download_checkpoint_invalid", "本地下载文件或续传断点已缺失或失效，无法继续下载；请重新创建下载任务从头下载（不会自动覆盖已有文件）"),
        QuarkError::DownloadEngineUnavailable => ("download_engine_unavailable", "本地下载引擎不可用，请重启应用后重试"),
        QuarkError::DownloadSubmissionFailed => ("download_submission_failed", "本地下载引擎未能接受任务，请重试；若持续失败请检查下载目录和引擎日志"),
        QuarkError::Transport(_) => ("network_error", "获取网盘下载地址时网络请求失败，请检查网络后重试"),
        QuarkError::Provider(_) => ("provider_error", "网盘拒绝下载请求"),
        QuarkError::InvalidResponse => ("invalid_response", "网盘返回无效下载数据"),
        QuarkError::Storage => ("credential_storage_error", "本机凭据或任务存储不可用"),
        QuarkError::InvalidInput(_) => ("invalid_input", "下载参数无效"),
        QuarkError::AuthorizationPending => ("authorization_pending", "网页授权尚未完成"),
        QuarkError::UploadUnavailable | QuarkError::ProviderUnsupported => {
            ("provider_unsupported", "当前网盘不支持该下载")
        }
        QuarkError::PikpakRegionalMembershipRestricted => (
            "pikpak_regional_membership_restricted",
            "PikPak 当前地区下载受限",
        ),
    }
}

// Separate from source/plan locks: always acquired before the per-source lock.
// Cancellation records durable intent first, then waits for the current leaf
// to finish linking, so it cannot miss a task created during URL resolution.
pub(crate) async fn lock_directory_submission(
    group_id: uuid::Uuid,
) -> Result<tokio::sync::OwnedMutexGuard<()>, QuarkError> {
    static GATES: std::sync::LazyLock<CloudDownloadSourceGate> =
        std::sync::LazyLock::new(CloudDownloadSourceGate::default);
    GATES.acquire(&group_id.to_string()).await
}

fn directory_submission_stopped(
    store: &TaskStore,
    group_id: uuid::Uuid,
) -> Result<bool, QuarkError> {
    Ok(store
        .cloud_directory_group(group_id)
        .map_err(|_| QuarkError::Storage)?
        .is_none_or(|group| {
            matches!(
                group.status,
                TaskStatus::Paused | TaskStatus::Cancelled | TaskStatus::Recycled
            )
        }))
}

/// Shared by user retries and background URL/authorization refreshes.
pub async fn lock_directory_member_submission(
    store: &TaskStore,
    task_id: uuid::Uuid,
) -> Result<Option<tokio::sync::OwnedMutexGuard<()>>, QuarkError> {
    let Some(group) = store
        .cloud_directory_group_for_task(task_id)
        .map_err(|_| QuarkError::Storage)?
    else {
        return Ok(None);
    };
    let guard = lock_directory_submission(group.id).await?;
    if directory_submission_stopped(store, group.id)? {
        return Err(QuarkError::InvalidInput(
            "目录下载任务已暂停、取消或移入回收站".into(),
        ));
    }
    Ok(Some(guard))
}

/// Called only with the directory/source submission gates held. A live
/// resolver must finish first; after restart there is no resolver to wait for.
pub fn recover_interrupted_directory_preparation(
    store: &TaskStore,
    task_id: uuid::Uuid,
) -> Result<DownloadTask, QuarkError> {
    let mut task = store.find(task_id).map_err(|_| QuarkError::Storage)?;
    if task.aria_gid.is_none() && task.status == TaskStatus::Parsing {
        task.status = TaskStatus::Paused;
        task.download_speed = 0;
        task.upload_speed = 0;
        task = store
            .apply_transfer_snapshot(task)
            .map_err(|_| QuarkError::Storage)?;
    }
    Ok(task)
}

pub async fn cancel_cloud_directory_group_with<F, Fut>(
    store: &TaskStore,
    group_id: uuid::Uuid,
    mut cancel: F,
) -> Result<TaskActionOutcome, String>
where
    F: FnMut(uuid::Uuid) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    store
        .cancel_cloud_directory_group(group_id)
        .map_err(|error| error.to_string())?;
    let _submission = lock_directory_submission(group_id)
        .await
        .map_err(|error| error.to_string())?;
    let group = store
        .cloud_directory_group(group_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "网盘目录下载任务不存在".to_string())?;
    let mut first_error = None;
    for item in store
        .cloud_directory_group_items(group.id)
        .map_err(|error| error.to_string())?
    {
        let Some(id) = item.task_id else {
            continue;
        };
        let task = match store.find(id) {
            Ok(task) => task,
            Err(error) => {
                first_error.get_or_insert_with(|| error.to_string());
                continue;
            }
        };
        if !matches!(
            task.status,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Recycled
        ) {
            if let Err(error) = cancel(id).await {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        return Err(format!("部分目录文件未能取消，请重试：{error}"));
    }
    cloud_directory_group_task_outcome(store, &group).map_err(|error| error.to_string())
}

pub(crate) fn cloud_directory_group_task_outcome(
    store: &TaskStore,
    group: &CloudDirectoryGroup,
) -> Result<TaskActionOutcome, QuarkError> {
    let queue_task =
        cloud_directory_group_queue_task(store, group.id).map_err(QuarkError::Transport)?;
    Ok(TaskActionOutcome {
        payload: crate::commands::DownloadTaskPayload {
            task: DownloadTask {
                id: group.id,
                source: format!("cloud-directory://{}/{}", group.provider.key(), group.id),
                name: group.relative_directory.clone(),
                kind: "folder".into(),
                status: queue_task.status,
                progress: queue_task.progress,
                error: queue_task.error.clone(),
                status_message: queue_task.status_message.clone(),
                failure_code: None,
                aria_gid: None,
                followed_by_gid: None,
                metadata_gid: None,
                total_length: queue_task.total_length,
                completed_length: queue_task.completed_length,
                download_speed: queue_task.download_speed,
                upload_speed: queue_task.upload_speed,
                connections: queue_task.connections,
                discovered_nodes: queue_task.discovered_nodes,
                eta_seconds: queue_task.eta_seconds,
                aria_error_code: None,
                created_at: group.created_at,
                updated_at: queue_task.updated_at,
                completed_at: queue_task.completed_at,
                p2p_sharing: false,
                is_private: group.is_private,
                post_processing_note: None,
            },
            files: Vec::new(),
            cleanup_pending: false,
            reauthorization_required: false,
        },
        duplicate: None,
        queue_task: Some(queue_task),
    })
}

/// Finds a prior personal-cloud directory download that owns the same remote
/// directory and local root. Directory leaves intentionally bypass source
/// de-duplication because their nested target paths are distinct; the durable
/// group is therefore the duplicate identity at this boundary.
pub(crate) fn existing_cloud_directory_duplicate(
    store: &TaskStore,
    provider: CloudProvider,
    account: &str,
    directory_id: &str,
    output_directory: &Path,
) -> Result<Option<TaskActionOutcome>, QuarkError> {
    let mut completed = None;
    for group in store
        .cloud_directory_groups()
        .map_err(|_| QuarkError::Storage)?
        .into_iter()
        .rev()
    {
        if group.provider != provider
            || group.remote_directory_id != directory_id
            || group.status == TaskStatus::Recycled
            || !group
                .output_directory
                .to_string_lossy()
                .eq_ignore_ascii_case(&output_directory.to_string_lossy())
            || store
                .cloud_download_account(group.id)
                .map_err(|_| QuarkError::Storage)?
                .as_deref()
                != Some(account)
        {
            continue;
        }
        let root =
            cloud_directory_group_queue_task(store, group.id).map_err(QuarkError::Transport)?;
        match root.status {
            TaskStatus::Draft
            | TaskStatus::Parsing
            | TaskStatus::AwaitingUser
            | TaskStatus::Queued
            | TaskStatus::Downloading
            | TaskStatus::Processing
            | TaskStatus::Paused => {
                let mut outcome = cloud_directory_group_task_outcome(store, &group)?;
                outcome.duplicate = Some(crate::DuplicateDownloadState::ExistingActive);
                return Ok(Some(outcome));
            }
            TaskStatus::Completed if cloud_directory_group_files_present(store, group.id)? => {
                let mut outcome = cloud_directory_group_task_outcome(store, &group)?;
                outcome.duplicate = Some(crate::DuplicateDownloadState::CompletedFilesPresent);
                completed.get_or_insert(outcome);
            }
            TaskStatus::Completed
            | TaskStatus::Failed
            | TaskStatus::Cancelled
            | TaskStatus::Recycled => {}
        }
    }
    Ok(completed)
}

fn cloud_directory_group_files_present(
    store: &TaskStore,
    group_id: uuid::Uuid,
) -> Result<bool, QuarkError> {
    let items = store
        .cloud_directory_group_items(group_id)
        .map_err(|_| QuarkError::Storage)?;
    if items.is_empty() {
        return Ok(false);
    }
    for item in items {
        let Some(task_id) = item.task_id else {
            return Ok(false);
        };
        let task = store.find(task_id).map_err(|_| QuarkError::Storage)?;
        if duplicate_state(store, &task).map_err(|_| QuarkError::Storage)?
            != crate::DuplicateDownloadState::CompletedFilesPresent
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Executes a previously validated plan through a narrow, injectable leaf
/// submission boundary. This is shared by the Tauri command and deterministic
/// connector tests, so persistence/linking semantics cannot drift from the
/// real directory-download flow.
pub async fn submit_cloud_directory_plan<F, Fut>(
    store: &TaskStore,
    provider: CloudProvider,
    directory_id: &str,
    plan: CloudDirectoryDownloadPlan,
    configured: UriSubmissionOptions,
    mut submit: F,
) -> Result<CloudDirectoryDownloadOutcome, QuarkError>
where
    F: FnMut(CloudProvider, String, UriSubmissionOptions) -> Fut,
    Fut: Future<Output = Result<DirectoryLeafSubmission, QuarkError>>,
{
    submit_cloud_directory_plan_with_group_event(
        store,
        provider,
        directory_id,
        plan,
        configured,
        false,
        |_| Ok(()),
        move |provider, file_id, options, _| submit(provider, file_id, options),
    )
    .await
}

/// Publishes the durable directory root before starting any leaf submission.
/// The leaf context lets the real downloader bind each local task to that root
/// before aria2 can publish its first status event.
pub async fn submit_cloud_directory_plan_with_group_event<F, Fut, G>(
    store: &TaskStore,
    provider: CloudProvider,
    directory_id: &str,
    plan: CloudDirectoryDownloadPlan,
    configured: UriSubmissionOptions,
    private_download: bool,
    mut group_created: G,
    mut submit: F,
) -> Result<CloudDirectoryDownloadOutcome, QuarkError>
where
    F: FnMut(CloudProvider, String, UriSubmissionOptions, DirectoryLeafContext) -> Fut,
    Fut: Future<Output = Result<DirectoryLeafSubmission, QuarkError>>,
    G: FnMut(QueueTaskPayload) -> Result<(), QuarkError>,
{
    let base_directory = configured
        .directory
        .clone()
        .ok_or_else(|| QuarkError::InvalidInput("未配置下载目录".into()))?;
    let group = create_cloud_directory_group_for_download(
        store,
        provider,
        directory_id,
        &plan,
        &base_directory,
        private_download,
    )?;
    group_created(
        cloud_directory_group_queue_task(store, group.id).map_err(QuarkError::Transport)?,
    )?;
    let items = store
        .cloud_directory_group_items(group.id)
        .map_err(|_| QuarkError::Storage)?;
    if items.len() != plan.files.len() {
        return Err(QuarkError::Storage);
    }
    let mut failures = Vec::new();
    let mut queued_count = 0;
    for (file, item) in plan.files.into_iter().zip(items) {
        let _submission = lock_directory_submission(group.id).await?;
        if directory_submission_stopped(store, group.id)? {
            break;
        }
        // A resume may have submitted this item while the original loop waited.
        if store
            .cloud_directory_group_item_is_linked(group.id, item.id)
            .map_err(|_| QuarkError::Storage)?
        {
            continue;
        }
        let relative_path = planned_relative_path(&file);
        let output_directory =
            match directory_download_output_directory(&base_directory, &file.relative_directory) {
                Ok(directory) => directory,
                Err(error) => {
                    persist_directory_failure(
                        store,
                        group.id,
                        item.id,
                        &file,
                        &error,
                        &mut failures,
                    )?;
                    continue;
                }
            };
        let mut options = configured.clone();
        options.directory = Some(output_directory);
        options.output_name = Some(file.output_name.clone());
        let context = DirectoryLeafContext {
            group_id: group.id,
            item_id: item.id,
        };
        match submit(provider, file.file_id.clone(), options, context).await {
            Ok(DirectoryLeafSubmission::Queued(task_id)) => {
                store
                    .link_cloud_directory_group_item_task(group.id, item.id, task_id)
                    .map_err(|_| QuarkError::Storage)?;
                queued_count += 1;
            }
            Ok(DirectoryLeafSubmission::Duplicate) => {
                let failure = CloudDirectoryDownloadFailure {
                    relative_path,
                    code: "duplicate",
                    message: "已存在下载任务，未重复入队",
                };
                store
                    .fail_cloud_directory_group_item(
                        group.id,
                        item.id,
                        failure.code,
                        failure.message,
                    )
                    .map_err(|_| QuarkError::Storage)?;
                failures.push(failure);
            }
            Err(error) => {
                persist_directory_failure(store, group.id, item.id, &file, &error, &mut failures)?;
            }
        }
    }
    Ok(CloudDirectoryDownloadOutcome {
        task: cloud_directory_group_task_outcome(store, &group)?,
        queued_count,
        failures,
    })
}

pub(crate) fn persist_directory_failure(
    store: &TaskStore,
    group_id: uuid::Uuid,
    item_id: uuid::Uuid,
    file: &PlannedCloudFile,
    error: &QuarkError,
    failures: &mut Vec<CloudDirectoryDownloadFailure>,
) -> Result<(), QuarkError> {
    let failure = directory_download_failure(file, error);
    store
        .fail_cloud_directory_group_item(group_id, item_id, failure.code, failure.message)
        .map_err(|_| QuarkError::Storage)?;
    failures.push(failure);
    Ok(())
}

#[tauri::command]
pub async fn cloud_download_url(
    app: AppHandle,
    provider: CloudProvider,
    file_id: String,
    file_name: String,
    private_download: Option<bool>,
) -> Result<TaskActionOutcome, QuarkError> {
    let app_handle = app.clone();
    let state = app_handle.state::<AppState>();
    let mut configured =
        crate::commands::download_submission_options(&state).map_err(QuarkError::Transport)?;
    if !file_name.trim().is_empty() {
        configured.output_name = Some(safe_filename(&file_name));
    }
    let private_download = private_download.unwrap_or(false);
    ensure_private_download_enabled(&state, private_download).map_err(QuarkError::InvalidInput)?;
    match submit_cloud_file(
        app,
        &state,
        provider,
        file_id,
        configured,
        private_download,
        None,
    )
    .await?
    {
        CloudFileSubmission::Queued(outcome) | CloudFileSubmission::Duplicate(outcome) => {
            Ok(outcome)
        }
    }
}

/// Queues every real file below one selected cloud folder. The plan is built in
/// full before any download URL is resolved so cycles, duplicate nodes, empty
/// folders, and the traversal limits fail without enqueuing a partial tree.
#[tauri::command]
pub async fn cloud_download_directory(
    app: AppHandle,
    provider: CloudProvider,
    directory_id: String,
    directory_name: String,
    private_download: Option<bool>,
) -> Result<CloudDirectoryDownloadOutcome, QuarkError> {
    let account_id = download_account(&app, provider)?;
    let planned_directory_id = directory_id.clone();
    let plan = run_session_work(
        app.clone(),
        provider,
        SessionWorkKind::Read,
        move |state, http| {
            plan_cloud_directory_download(provider, &planned_directory_id, &directory_name, |id| {
                list_cloud_directory(provider, &state.store, http, id)
            })
        },
    )
    .await?;
    require_download_account(&app, provider, &account_id)?;

    let state = app.state::<AppState>();
    let configured =
        crate::commands::download_submission_options(&state).map_err(QuarkError::Transport)?;
    let private_download = private_download.unwrap_or(false);
    ensure_private_download_enabled(&state, private_download).map_err(QuarkError::InvalidInput)?;
    if let Some(outcome) = existing_cloud_directory_duplicate(
        &state.store,
        provider,
        &account_id,
        &directory_id,
        configured
            .directory
            .as_deref()
            .ok_or_else(|| QuarkError::InvalidInput("未配置下载目录".into()))?,
    )? {
        let group = state
            .store
            .cloud_directory_group(outcome.payload.task.id)
            .map_err(|_| QuarkError::Storage)?
            .ok_or(QuarkError::Storage)?;
        ensure_private_group_access(&state, &group).map_err(QuarkError::InvalidInput)?;
        return Ok(CloudDirectoryDownloadOutcome {
            task: outcome,
            queued_count: 0,
            failures: Vec::new(),
        });
    }
    let store = Arc::clone(&state.store);
    let submit_app = app.clone();
    let root_event_app = app.clone();
    let owner_store = Arc::clone(&store);
    submit_cloud_directory_plan_with_group_event(
        &store,
        provider,
        &directory_id,
        plan,
        configured,
        private_download,
        move |root| {
            // Binding precedes every leaf submission; failures are checked again by submit_cloud_file.
            let id = uuid::Uuid::parse_str(&root.id).map_err(|_| QuarkError::Storage)?;
            owner_store
                .bind_cloud_download_account(id, provider, &account_id)
                .map_err(|_| QuarkError::Storage)?;
            crate::commands::emit_task_update(
                &root_event_app,
                crate::commands::TaskUpdatePayload::Queue(root),
            );
            Ok(())
        },
        move |provider, file_id, options, context| {
            let app = submit_app.clone();
            async move {
                let state = app.state::<AppState>();
                match submit_cloud_file(
                    app.clone(),
                    &state,
                    provider,
                    file_id,
                    options,
                    private_download,
                    Some(context),
                )
                .await?
                {
                    CloudFileSubmission::Queued(outcome) => {
                        Ok(DirectoryLeafSubmission::Queued(outcome.payload.task.id))
                    }
                    CloudFileSubmission::Duplicate(_) => Ok(DirectoryLeafSubmission::Duplicate),
                }
            }
        },
    )
    .await
}

/// Resubmits exactly one previously failed, unlinked directory leaf. The
/// durable item is the authority for provider ID and output target; retry never
/// scans by filename and never replays the rest of the directory plan.
pub async fn retry_cloud_directory_group_item(
    app: AppHandle,
    group_id: uuid::Uuid,
    item_id: uuid::Uuid,
) -> Result<TaskActionOutcome, QuarkError> {
    let state = app.state::<AppState>();
    let configured =
        crate::commands::download_submission_options(&state).map_err(QuarkError::Transport)?;
    let private_download = state
        .store
        .cloud_directory_group(group_id)
        .map_err(|_| QuarkError::Storage)?
        .ok_or_else(|| QuarkError::InvalidInput("网盘目录下载任务不存在".into()))?
        .is_private;
    let store = Arc::clone(&state.store);
    let submit_app = app.clone();
    retry_cloud_directory_group_item_with(
        &store,
        group_id,
        item_id,
        configured,
        move |provider, file_id, options, context| {
            let app = submit_app.clone();
            async move {
                let state = app.state::<AppState>();
                if file_id.starts_with("cloud-share://") {
                    return submit_cloud_share_directory_leaf(
                        app.clone(),
                        &state,
                        file_id,
                        options,
                        context,
                        private_download,
                    )
                    .await;
                }
                match submit_cloud_file(
                    app.clone(),
                    &state,
                    provider,
                    file_id,
                    options,
                    private_download,
                    Some(context),
                )
                .await?
                {
                    CloudFileSubmission::Queued(outcome) => {
                        Ok(DirectoryLeafSubmission::Queued(outcome.payload.task.id))
                    }
                    CloudFileSubmission::Duplicate(_) => Ok(DirectoryLeafSubmission::Duplicate),
                }
            }
        },
    )
    .await
}

/// Resubmits every unlinked failed leaf in one durable directory group. Linked
/// leaves are deliberately excluded: their existing local task remains the
/// authority and must never be submitted a second time.
pub async fn retry_cloud_directory_group_with<F, Fut>(
    store: &TaskStore,
    group_id: uuid::Uuid,
    configured: UriSubmissionOptions,
    mut submit: F,
) -> Result<TaskActionOutcome, QuarkError>
where
    F: FnMut(CloudProvider, String, UriSubmissionOptions, DirectoryLeafContext) -> Fut,
    Fut: Future<Output = Result<DirectoryLeafSubmission, QuarkError>>,
{
    let group = store
        .cloud_directory_group(group_id)
        .map_err(|_| QuarkError::Storage)?
        .ok_or_else(|| QuarkError::InvalidInput("网盘目录下载任务不存在".into()))?;
    let pending = store
        .cloud_directory_group_items(group.id)
        .map_err(|_| QuarkError::Storage)?
        .into_iter()
        .filter(|item| item.task_id.is_none())
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Err(QuarkError::InvalidInput(
            "目录内没有可重新提交的网盘文件".into(),
        ));
    }
    for item in pending {
        // A failed provider attempt is recorded by the single-item path. Keep
        // trying sibling leaves so one transient failure cannot starve the
        // remainder of a directory retry.
        let _ = retry_cloud_directory_group_item_with(
            store,
            group.id,
            item.id,
            configured.clone(),
            |provider, file_id, options, context| submit(provider, file_id, options, context),
        )
        .await;
    }
    cloud_directory_group_task_outcome(store, &group)
}

pub async fn retry_cloud_directory_group(
    app: AppHandle,
    group_id: uuid::Uuid,
) -> Result<TaskActionOutcome, QuarkError> {
    let state = app.state::<AppState>();
    let configured =
        crate::commands::download_submission_options(&state).map_err(QuarkError::Transport)?;
    let store = Arc::clone(&state.store);
    let private_download = store
        .cloud_directory_group(group_id)
        .map_err(|_| QuarkError::Storage)?
        .ok_or_else(|| QuarkError::InvalidInput("网盘目录下载任务不存在".into()))?
        .is_private;
    let submit_app = app.clone();
    retry_cloud_directory_group_with(
        &store,
        group_id,
        configured,
        move |provider, file_id, options, context| {
            let app = submit_app.clone();
            async move {
                let state = app.state::<AppState>();
                if file_id.starts_with("cloud-share://") {
                    return submit_cloud_share_directory_leaf(
                        app.clone(),
                        &state,
                        file_id,
                        options,
                        context,
                        private_download,
                    )
                    .await;
                }
                match submit_cloud_file(
                    app.clone(),
                    &state,
                    provider,
                    file_id,
                    options,
                    private_download,
                    Some(context),
                )
                .await?
                {
                    CloudFileSubmission::Queued(outcome) => {
                        Ok(DirectoryLeafSubmission::Queued(outcome.payload.task.id))
                    }
                    CloudFileSubmission::Duplicate(_) => Ok(DirectoryLeafSubmission::Duplicate),
                }
            }
        },
    )
    .await
}

pub async fn retry_cloud_directory_group_item_with<F, Fut>(
    store: &TaskStore,
    group_id: uuid::Uuid,
    item_id: uuid::Uuid,
    configured: UriSubmissionOptions,
    mut submit: F,
) -> Result<TaskActionOutcome, QuarkError>
where
    F: FnMut(CloudProvider, String, UriSubmissionOptions, DirectoryLeafContext) -> Fut,
    Fut: Future<Output = Result<DirectoryLeafSubmission, QuarkError>>,
{
    let _leaf = store
        .lock_cloud_directory_leaf(item_id)
        .await
        .map_err(|_| QuarkError::Storage)?;
    let group = store
        .cloud_directory_group(group_id)
        .map_err(|_| QuarkError::Storage)?
        .ok_or_else(|| QuarkError::InvalidInput("网盘目录下载任务不存在".into()))?;
    let _submission = lock_directory_submission(group_id).await?;
    if directory_submission_stopped(store, group_id)? {
        return Err(QuarkError::InvalidInput(
            "目录下载任务已取消或移入回收站".into(),
        ));
    }
    if matches!(
        group.status,
        TaskStatus::Recycled | TaskStatus::Cancelled | TaskStatus::Completed
    ) {
        return Err(QuarkError::InvalidInput("网盘目录下载任务已结束".into()));
    }
    let item = store
        .cloud_directory_group_items(group_id)
        .map_err(|_| QuarkError::Storage)?
        .into_iter()
        .find(|item| item.id == item_id)
        .ok_or_else(|| QuarkError::InvalidInput("网盘目录文件不存在".into()))?;
    if item.task_id.is_some() {
        return Err(QuarkError::InvalidInput(
            "网盘目录文件已经入队，不能重复提交".into(),
        ));
    }
    let output_directory =
        directory_download_output_directory(&group.output_directory, &item.relative_directory)?;
    let mut options = configured;
    options.directory = Some(output_directory);
    options.output_name = Some(item.output_name.clone());
    let context = DirectoryLeafContext {
        group_id: group.id,
        item_id: item.id,
    };
    match submit(
        group.provider,
        item.remote_file_id.clone(),
        options,
        context,
    )
    .await
    {
        Ok(DirectoryLeafSubmission::Queued(task_id)) => {
            store
                .link_cloud_directory_group_item_task(group.id, item.id, task_id)
                .map_err(|_| QuarkError::Storage)?;
            let mut outcome = task_action_outcome(
                store,
                store.find(task_id).map_err(|_| QuarkError::Storage)?,
                None,
            )
            .map_err(QuarkError::Transport)?;
            outcome.queue_task = Some(
                cloud_directory_group_queue_task(store, group.id).map_err(QuarkError::Transport)?,
            );
            Ok(outcome)
        }
        Ok(DirectoryLeafSubmission::Duplicate) => {
            store
                .fail_cloud_directory_group_item(
                    group.id,
                    item.id,
                    "duplicate",
                    "已存在下载任务，未重复入队",
                )
                .map_err(|_| QuarkError::Storage)?;
            Err(QuarkError::InvalidInput(
                "已存在下载任务，未重复入队".into(),
            ))
        }
        Err(error) => {
            let (code, message) = directory_download_failure_fields(&error);
            store
                .fail_cloud_directory_group_item(group.id, item.id, code, message)
                .map_err(|_| QuarkError::Storage)?;
            Err(error)
        }
    }
}

impl ResolvedCloudDownload {
    pub fn into_uri_submission_options(self) -> UriSubmissionOptions {
        UriSubmissionOptions {
            output_name: Some(safe_filename(&self.name)),
            // This marker carries an opaque native credential only while the
            // resolver hands the result to the backend selector. Never expose
            // it to aria2, its RPC log, or the CDN request.
            headers: self
                .headers
                .into_iter()
                .filter(|(name, _)| {
                    !name.eq_ignore_ascii_case(
                        crate::xunlei_native::ACCELERATION_TOKEN_MARKER_HEADER,
                    ) && !name
                        .eq_ignore_ascii_case(crate::xunlei_native::ACCESS_TOKEN_MARKER_HEADER)
                        && !name.eq_ignore_ascii_case(crate::xunlei_native::USER_ID_MARKER_HEADER)
                        && !name.eq_ignore_ascii_case(crate::xunlei_native::PEER_ID_MARKER_HEADER)
                        && !name.eq_ignore_ascii_case(
                            crate::xunlei_native::CONTENT_LENGTH_MARKER_HEADER,
                        )
                        && !name.eq_ignore_ascii_case(crate::xunlei_native::CID_MARKER_HEADER)
                        && !name.eq_ignore_ascii_case(crate::xunlei_native::GCID_MARKER_HEADER)
                })
                .collect(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xunlei_native_unavailability_only_falls_back_without_a_native_checkpoint() {
        assert!(!xunlei_native_fallback_allowed(None, false).unwrap());
        assert!(xunlei_native_fallback_allowed(None, true).unwrap());

        assert_eq!(
            super::share_resolution_failure_message(
                &xunlei_native_fallback_allowed(
                    Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:42"),
                    false,
                )
                .unwrap_err(),
            ),
            crate::xunlei_native::NATIVE_RESUME_BACKEND_UNAVAILABLE_MESSAGE,
        );
        assert_eq!(
            super::share_resolution_failure_message(
                &xunlei_native_fallback_allowed(
                    Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:42"),
                    true,
                )
                .unwrap_err(),
            ),
            crate::xunlei_native::PLUGIN_UPDATE_MESSAGE,
        );
    }

    #[test]
    fn directory_leaf_source_matches_do_not_block_distinct_output_paths() {
        assert!(source_duplicate_blocks_submission(None));
        assert!(!source_duplicate_blocks_submission(Some(
            DirectoryLeafContext {
                group_id: uuid::Uuid::new_v4(),
                item_id: uuid::Uuid::new_v4(),
            }
        )));
    }

    fn completed_directory_group(
        store: &TaskStore,
        output: &std::path::Path,
        provider: CloudProvider,
        directory_id: &str,
        account: &str,
    ) -> CloudDirectoryGroup {
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(provider, directory_id, "Archive", output),
                &[NewCloudDirectoryGroupItem::new(
                    "file-id",
                    &["Archive"],
                    "complete.bin",
                    Some(3),
                )],
            )
            .unwrap();
        store
            .bind_cloud_download_account(group.id, provider, account)
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let task = store
            .claim_cloud_directory_leaf_task(
                group.id,
                item.id,
                provider,
                account,
                NewTask {
                    source: format!("cloud://{}/file-id", provider.key()),
                    name: "complete.bin".into(),
                    kind: "cloud".into(),
                    is_private: false,
                },
            )
            .unwrap()
            .unwrap();
        let payload = output.join("Archive").join("complete.bin");
        std::fs::create_dir_all(payload.parent().unwrap()).unwrap();
        std::fs::write(&payload, b"yes").unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 3, 3, true)],
            )
            .unwrap();
        store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Completed,
                progress: 100,
                total_length: Some(3),
                completed_length: 3,
                ..task
            })
            .unwrap();
        group
    }

    #[test]
    fn completed_directory_reuses_its_existing_group_for_the_same_account_and_destination() {
        let output = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let group = completed_directory_group(
            &store,
            output.path(),
            CloudProvider::Xunlei,
            "folder-id",
            "alice",
        );

        let duplicate = existing_cloud_directory_duplicate(
            &store,
            CloudProvider::Xunlei,
            "alice",
            "folder-id",
            output.path(),
        )
        .unwrap()
        .expect("completed directory must be reused");

        assert_eq!(
            duplicate.duplicate,
            Some(crate::DuplicateDownloadState::CompletedFilesPresent)
        );
        assert_eq!(duplicate.payload.task.id, group.id);
        assert_eq!(duplicate.payload.task.status, TaskStatus::Completed);
        assert_eq!(duplicate.queue_task.unwrap().id, group.id.to_string());
    }

    #[test]
    fn completed_directory_does_not_match_a_different_account_or_destination() {
        let output = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        completed_directory_group(
            &store,
            output.path(),
            CloudProvider::Xunlei,
            "folder-id",
            "alice",
        );

        assert!(existing_cloud_directory_duplicate(
            &store,
            CloudProvider::Xunlei,
            "bob",
            "folder-id",
            output.path(),
        )
        .unwrap()
        .is_none());
        assert!(existing_cloud_directory_duplicate(
            &store,
            CloudProvider::Xunlei,
            "alice",
            "folder-id",
            elsewhere.path(),
        )
        .unwrap()
        .is_none());
    }
}

pub fn resolve_download(
    provider: CloudProvider,
    file_id: &str,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
) -> Result<ResolvedCloudDownload, QuarkError> {
    ProviderRegistry::global()
        .driver(provider, store, http)
        .resolve_download(file_id)
}
