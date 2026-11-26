use super::*;

#[cfg(test)]
#[path = "share/atomic_membership_tests.rs"]
mod atomic_membership_tests;

#[path = "share/mega_integrity.rs"]
mod mega_integrity;
pub(crate) use mega_integrity::resume_pending_mega_integrity_checks;

struct CancelShareOnDrop(Arc<share::ShareWorkControl>);
impl Drop for CancelShareOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Whether public MEGA links should be downloaded with the signed-in account:
/// the account's session is ready. The adapter then carries the session only
/// for paid accounts; any other state downloads anonymously, exactly as
/// without an account.
fn share_adapter_options(
    state: &CloudState,
    provider: CloudProvider,
) -> share::ShareAdapterOptions {
    if provider != CloudProvider::Mega {
        return share::ShareAdapterOptions::default();
    }
    let ready = state
        .auth_supervisor
        .record(&state.store, provider)
        .is_ok_and(|record| record.health == auth_supervisor::AuthHealth::Ready);
    share::ShareAdapterOptions {
        mega_account_quota: ready,
    }
}

pub async fn inspect_cloud_share(
    app: AppHandle,
    candidate: crate::parser::CloudShareCandidate,
) -> Result<share::ShareConfirmationOutcome, QuarkError> {
    // Only genuine authentication expiry uses the shared recovery order. Share
    // access codes, CAPTCHA and risk checks retain their direct user flows.
    run_share_session_read_work(app, candidate.provider, move |state, http| {
        let retrying_http = share::RetryingShareReadTransport::new(http);
        let adapter =
            share::provider_share_adapter(candidate.provider, &state.store, &retrying_http)?;
        share::inspect_share_with_adapter(&state.share_plans, adapter.as_ref(), &candidate)
    })
    .await
}

pub async fn preflight_cloud_share_download(
    app: AppHandle,
    selection: &share::CloudShareDownloadSelection,
) -> Result<(), QuarkError> {
    let provider = selection.provider;
    let selection = selection.clone();
    let result = run_share_session_read_work(app, provider, move |state, http| {
        let retrying_http = share::RetryingShareReadTransport::new(http);
        // The preflight requests a download URL like the real resolution does,
        // so it must be issued against the same quota.
        let adapter = share::provider_share_adapter_with(
            provider,
            &state.store,
            &retrying_http,
            share_adapter_options(state, provider),
        )?;
        share::preflight_selected_share_download(&state.share_plans, adapter.as_ref(), &selection)
    })
    .await;
    result.map_err(share_preflight_user_error)
}

fn share_preflight_user_error(error: QuarkError) -> QuarkError {
    if error.to_string().contains("未建立下载任务") {
        error
    } else {
        QuarkError::InvalidInput(format!("{error}，未建立下载任务。"))
    }
}

pub(crate) async fn resolve_agent_share_read_only(
    app: AppHandle,
    source: &str,
) -> Result<ResolvedCloudDownload, QuarkError> {
    let (provider, _, _) = share::parse_stable_cloud_share_source(source)?;
    let source = source.to_owned();
    run_share_session_read_work(app, provider, move |state, http| {
        let adapter = share::provider_share_adapter(provider, &state.store, http)?;
        share::resolve_agent_share_read_only_with(&state.share_plans, adapter.as_ref(), &source)
    }).await
}

pub async fn resolve_stable_cloud_share_download(
    app: AppHandle,
    source: &str,
) -> Result<ResolvedCloudDownload, QuarkError> {
    // This check also covers URL refresh and later GUI/Agent resume calls.
    let direct_only = app.try_state::<AppState>().ok_or(QuarkError::Storage)?
        .store.is_agent_direct_share_source(source).map_err(|_| QuarkError::Storage)?;
    if direct_only {
        return resolve_agent_share_read_only(app, source).await;
    }
    let (provider, _, _) = share::parse_stable_cloud_share_source(source)?;
    let source = source.to_owned();
    let control = Arc::new(share::ShareWorkControl::new(
        std::time::Duration::from_secs(300),
    ));
    let _cancel_on_drop = CancelShareOnDrop(Arc::clone(&control));
    let completion_control = Arc::clone(&control);
    let (download, transferred) = run_share_work_checked(
        app.clone(),
        provider,
        // Any provider, including Aliyun, can fall back to a personal-drive
        // transfer when the share service denies a direct URL. Never replay a
        // mutation after session recovery; the caller can safely retry.
        SessionWorkKind::Mutation,
        move |state, http| {
            let before = state
                .share_plans
                .resolve_entry(&source)?
                .plan
                .transfers
                .len();
            let download = share::resolve_current_share_download_cancellable_with(
                &state.share_plans,
                &state.store,
                http,
                &source,
                &control,
                share_adapter_options(state, provider),
            )?;
            let after = state
                .share_plans
                .resolve_entry(&source)?
                .plan
                .transfers
                .len();
            Ok((download, after > before))
        },
        move || completion_control.check(),
    )
    .await?;
    if transferred {
        schedule_cloud_capacity_invalidation(app, provider, CloudCapacityChangeReason::Transfer);
    }
    Ok(download)
}

pub async fn submit_cloud_share_file(
    app: AppHandle,
    state: &AppState,
    source: String,
    configured: UriSubmissionOptions,
    private_download: bool,
    on_created: Option<tauri::ipc::Channel<QueueTaskPayload>>,
) -> Result<TaskActionOutcome, QuarkError> {
    let submission = submit_cloud_share_file_internal(
        app.clone(),
        state,
        source,
        configured,
        None,
        true,
        private_download,
        on_created,
    )
    .await?;
    let outcome = match submission {
        CloudShareFileSubmission::Queued(outcome)
        | CloudShareFileSubmission::Duplicate(outcome) => outcome,
    };
    schedule_cloud_share_cleanup(app, &outcome.payload.task);
    Ok(outcome)
}

pub(crate) enum CloudShareFileSubmission {
    Queued(TaskActionOutcome),
    Duplicate(TaskActionOutcome),
}

pub(crate) async fn submit_cloud_share_file_internal(
    app: AppHandle,
    state: &AppState,
    source: String,
    configured: UriSubmissionOptions,
    directory_context: Option<DirectoryLeafContext>,
    append_entry_directory: bool,
    private_download: bool,
    on_created: Option<tauri::ipc::Channel<QueueTaskPayload>>,
) -> Result<CloudShareFileSubmission, QuarkError> {
    let (provider, _, _) = share::parse_stable_cloud_share_source(&source)?;
    let _source_submission = state.cloud_download_sources.acquire(&source).await?;
    // Directory leaves have their own persisted output path below the chosen
    // directory root. They must not be de-duplicated against a prior
    // standalone selection of the same shared file.
    if super::download::source_duplicate_blocks_submission(directory_context) {
        if let Some(existing) = state
            .store
            .find_by_source(&source)
            .map_err(|_| QuarkError::Storage)?
            .filter(|task| task.status != TaskStatus::Recycled)
        {
            ensure_private_task_access(state, &existing).map_err(QuarkError::InvalidInput)?;
            if !is_retryable_cloud_share_prepare(&existing) {
                let duplicate =
                    duplicate_state(&state.store, &existing).map_err(|_| QuarkError::Storage)?;
                return Ok(CloudShareFileSubmission::Duplicate(
                    task_action_outcome(&state.store, existing, Some(duplicate))
                        .map_err(QuarkError::Transport)?,
                ));
            }
        }
    }
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    let mut configured = configured;
    let entry = cloud.share_plans.resolve_entry(&source)?;
    if append_entry_directory {
        if let Some(directory) = configured.directory.as_ref() {
            configured.directory = Some(directory_download_output_directory(
                directory,
                &entry.entry.relative_directory,
            )?);
        } else if !entry.entry.relative_directory.is_empty() {
            return Err(QuarkError::InvalidInput(
                "分享目录下载需要设置绝对下载目录".into(),
            ));
        }
    }
    configured
        .output_name
        .get_or_insert_with(|| safe_filename(&entry.entry.name));
    configured
        .clone()
        .into_aria2_options()
        .map_err(|_| QuarkError::InvalidInput("下载路径或文件名无效".into()))?;
    let owner = ready_cloud_download_account(&app, provider)?;
    if let (Some(account), Some(context)) = (owner.as_deref(), directory_context) {
        // Verify the existing group before a new leaf is created. A session
        // switch cannot leave an orphaned parsing task behind.
        bind_reauthorized_cloud_task_owner(&state.store, context.group_id, provider, account)?;
    }
    let retry = retryable_cloud_share_task_for_submission(
        &state.store,
        &source,
        directory_context.as_ref(),
    )?;
    let new_task = NewTask {
        source: source.clone(),
        name: configured.output_name.clone().unwrap(),
        kind: "cloud".into(),
        is_private: private_download,
    };
    let prepared = if let Some(context) = directory_context {
        state
            .store
            .prepare_cloud_share_directory_leaf_task(
                context.group_id,
                context.item_id,
                new_task,
                retry.map(|task| task.id),
            )
            .map_err(|_| QuarkError::Storage)?
    } else {
        let task = match retry {
            Some(task) => task,
            None => state
                .store
                .create(new_task)
                .map_err(|_| QuarkError::Storage)?,
        };
        state
            .store
            .set_private(task.id, private_download)
            .map_err(|_| QuarkError::Storage)?
    };
    if let Some(account) = owner.as_deref() {
        bind_reauthorized_cloud_task_owner(&state.store, prepared.id, provider, &account)?;
    }
    if cloud
        .share_plans
        .reserve_local_task(&source, prepared.id)
        .is_err()
    {
        return Ok(CloudShareFileSubmission::Queued(
            cloud_share_failure_outcome(&state.store, prepared, None, CLOUD_SHARE_PREPARE_FAILURE),
        ));
    }
    let queued = queue_cloud_dispatch(app, state, prepared, configured).await?;
    if let Some(channel) = on_created {
        if let Ok(root) =
            crate::commands::queue_task_payload_from_task(&state.store, queued.clone())
        {
            let _ = channel.send(root);
        }
    }
    let outcome = cloud_share_task_outcome(&state.store, queued);
    Ok(CloudShareFileSubmission::Queued(outcome))
}

pub async fn submit_cloud_share_directory_leaf(
    app: AppHandle,
    state: &AppState,
    source: String,
    configured: UriSubmissionOptions,
    context: DirectoryLeafContext,
    private_download: bool,
) -> Result<DirectoryLeafSubmission, QuarkError> {
    match submit_cloud_share_file_internal(
        app.clone(),
        state,
        source,
        configured,
        Some(context),
        false,
        private_download,
        None,
    )
    .await?
    {
        CloudShareFileSubmission::Queued(outcome) => {
            schedule_cloud_share_cleanup(app, &outcome.payload.task);
            Ok(DirectoryLeafSubmission::Queued(outcome.payload.task.id))
        }
        CloudShareFileSubmission::Duplicate(_) => Ok(DirectoryLeafSubmission::Duplicate),
    }
}

#[cfg(test)]
pub(crate) async fn submit_new_cloud_share_file_with<S, SFut, R, RFut>(
    tasks: Arc<TaskStore>,
    plans: &share::CloudSharePlanStore,
    source: String,
    configured: UriSubmissionOptions,
    service: S,
    resolve: R,
) -> Result<TaskActionOutcome, QuarkError>
where
    S: FnOnce() -> SFut,
    SFut: std::future::Future<Output = Result<Arc<crate::DownloadService>, QuarkError>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<ResolvedCloudDownload, QuarkError>>,
{
    submit_new_cloud_share_file_scoped_with(
        tasks, plans, source, configured, None, true, false, None, service, resolve,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn submit_new_cloud_share_file_scoped_with<S, SFut, R, RFut>(
    tasks: Arc<TaskStore>,
    plans: &share::CloudSharePlanStore,
    source: String,
    mut configured: UriSubmissionOptions,
    directory_context: Option<DirectoryLeafContext>,
    append_entry_directory: bool,
    private_download: bool,
    on_created: Option<Arc<dyn Fn(&DownloadTask) + Send + Sync>>,
    service: S,
    resolve: R,
) -> Result<TaskActionOutcome, QuarkError>
where
    S: FnOnce() -> SFut,
    SFut: std::future::Future<Output = Result<Arc<crate::DownloadService>, QuarkError>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<ResolvedCloudDownload, QuarkError>>,
{
    share::parse_stable_cloud_share_source(&source)?;
    let entry = plans.resolve_entry(&source)?;
    if append_entry_directory {
        if let Some(directory) = configured.directory.as_ref() {
            configured.directory = Some(directory_download_output_directory(
                directory,
                &entry.entry.relative_directory,
            )?);
        } else if !entry.entry.relative_directory.is_empty() {
            return Err(QuarkError::InvalidInput(
                "分享目录下载需要设置绝对下载目录".into(),
            ));
        }
    }
    let output_name = configured
        .output_name
        .clone()
        .unwrap_or_else(|| safe_filename(&entry.entry.name));
    configured.output_name = Some(output_name.clone());
    configured
        .clone()
        .into_aria2_options()
        .map_err(|_| QuarkError::InvalidInput("下载路径或文件名无效".into()))?;

    // Preserve the single-file contract: an unavailable engine must not leave
    // a task behind. A directory leaf is different because the durable group
    // must retain a failed child for Failed/Recycle projections.
    let mut service_factory = Some(service);
    let eager_service = if directory_context.is_none() {
        Some(
            service_factory
                .take()
                .expect("service factory is available")()
            .await?,
        )
    } else {
        None
    };

    let retry =
        retryable_cloud_share_task_for_submission(&tasks, &source, directory_context.as_ref())?;
    let retrying_directory_leaf = directory_context.is_some() && retry.is_some();
    let new_task = crate::NewTask {
        source: source.clone(),
        name: output_name,
        kind: "cloud".into(),
        is_private: private_download,
    };
    let prepared = if let Some(context) = directory_context.as_ref() {
        tasks
            .prepare_cloud_share_directory_leaf_task(
                context.group_id,
                context.item_id,
                new_task,
                retry.map(|task| task.id),
            )
            .map_err(|_| QuarkError::Storage)?
    } else {
        let prepared = match retry {
            Some(mut prepared) => {
                prepared.status = TaskStatus::Parsing;
                prepared.error = None;
                prepared.failure_code = None;
                tasks
                    .apply_transfer_snapshot(prepared)
                    .map_err(|_| QuarkError::Storage)?
            }
            None => tasks.create(new_task).map_err(|_| QuarkError::Storage)?,
        };
        tasks
            .set_private(prepared.id, private_download)
            .map_err(|_| QuarkError::Storage)?
    };
    if let Err(error) = plans.reserve_local_task(&source, prepared.id) {
        if retrying_directory_leaf {
            return Ok(cloud_share_failure_outcome(
                &tasks,
                prepared,
                None,
                CLOUD_SHARE_PREPARE_FAILURE,
            ));
        }
        return match tasks.remove_task_record(prepared.id) {
            Ok(()) | Err(crate::StoreError::NotFound(_)) => Err(error),
            // The task may still be durable, so report the conservative
            // recoverable state instead of claiming creation was rolled back.
            Err(_) => Ok(cloud_share_failure_outcome(
                &tasks,
                prepared,
                None,
                CLOUD_SHARE_PREPARE_FAILURE,
            )),
        };
    }
    if let Some(on_created) = on_created {
        on_created(&prepared);
    }
    let service = match eager_service {
        Some(service) => service,
        None => match service_factory
            .take()
            .expect("directory service factory is available")()
        .await
        {
            Ok(service) => service,
            Err(error) => {
                return Ok(cloud_share_failure_outcome(
                    &tasks,
                    prepared,
                    Some(cloud_failure_code(&error)),
                    share_resolution_failure_message(&error),
                ))
            }
        },
    };

    let resolved = match resolve().await {
        Ok(resolved) => resolved,
        Err(error) => {
            return Ok(cloud_share_failure_outcome(
                &tasks,
                prepared,
                Some(cloud_failure_code(&error)),
                share_resolution_failure_message(&error),
            ))
        }
    };
    let task = match share::submit_current_share_download(&service, &prepared, resolved, configured)
        .await
    {
        Ok(task) => task,
        Err(_) => {
            return Ok(cloud_share_failure_outcome(
                &tasks,
                prepared,
                None,
                "aria2 未能接受网盘分享下载任务",
            ))
        }
    };
    Ok(cloud_share_task_outcome(&tasks, task))
}

pub(crate) fn cloud_share_failure_outcome(
    tasks: &TaskStore,
    prepared: DownloadTask,
    failure_code: Option<&str>,
    message: impl AsRef<str>,
) -> TaskActionOutcome {
    let task = tasks
        .fail_with_code(prepared.id, failure_code, message.as_ref())
        .or_else(|_| tasks.find(prepared.id))
        .unwrap_or(prepared);
    cloud_share_task_outcome(tasks, task)
}

pub(crate) fn is_retryable_cloud_share_prepare(task: &DownloadTask) -> bool {
    task.kind == "cloud"
        && task.aria_gid.is_none()
        && matches!(task.status, TaskStatus::Parsing | TaskStatus::Failed)
}

pub(crate) fn retryable_cloud_share_task_for_submission(
    tasks: &TaskStore,
    source: &str,
    directory_context: Option<&DirectoryLeafContext>,
) -> Result<Option<DownloadTask>, QuarkError> {
    if let Some(context) = directory_context {
        return tasks
            .cloud_directory_group_item_task(context.group_id, context.item_id)
            .map_err(|_| QuarkError::Storage)
            .map(|task| {
                task.filter(|task| task.source == source)
                    .filter(is_retryable_cloud_share_prepare)
            });
    }
    let retry = tasks
        .find_by_source(source)
        .map_err(|_| QuarkError::Storage)?
        .filter(is_retryable_cloud_share_prepare);
    Ok(retry)
}

pub(crate) fn cloud_share_task_outcome(tasks: &TaskStore, task: DownloadTask) -> TaskActionOutcome {
    // Directory submission consumes only the leaf ID. Projecting its root here
    // expands every group item, which turns a large share into a quadratic
    // amount of SQLite work before later leaves can be scheduled.
    if tasks
        .cloud_directory_group_for_task(task.id)
        .ok()
        .flatten()
        .is_some()
    {
        return cloud_share_leaf_outcome(tasks, task);
    }
    if let Ok(outcome) = task_action_outcome(tasks, task.clone(), None) {
        return outcome;
    }
    cloud_share_leaf_outcome(tasks, task)
}

fn cloud_share_leaf_outcome(tasks: &TaskStore, task: DownloadTask) -> TaskActionOutcome {
    let files = tasks.files(task.id).unwrap_or_default();
    let reauthorization_required =
        requires_cloud_reauthorization_failure(task.failure_code.as_deref());
    TaskActionOutcome {
        payload: crate::commands::DownloadTaskPayload {
            task,
            files,
            cleanup_pending: false,
            reauthorization_required,
        },
        duplicate: None,
        queue_task: None,
    }
}

pub(crate) fn schedule_cloud_share_cleanup(app: AppHandle, task: &DownloadTask) {
    mega_integrity::schedule_mega_integrity_check(app.clone(), task);
    if task.status != TaskStatus::Completed || !task.source.starts_with("cloud-share://") {
        return;
    }
    if app.try_state::<AppState>().map(|state| state.store.is_agent_resource_task(task.id).unwrap_or(true)).unwrap_or(true) {
        return;
    }
    let id = task.id;
    tauri::async_runtime::spawn(async move {
        let _ = cleanup_cloud_share_task(app, id, false).await;
    });
}

/// Direct-share downloads never create a personal cloud file. Only a transfer
/// journal entry for this exact leaf may require a personal identity or a
/// provider delete call after the local task completes.
pub(super) fn share_entry_needs_cleanup(entry: &share::CloudShareResolvedEntry) -> bool {
    entry.plan.transfers.iter().any(|transfer| {
        transfer.entry_fingerprint == entry.entry.entry_fingerprint
            && !matches!(
                transfer.cleanup_state,
                share::CloudShareCleanupState::Succeeded
            )
    })
}

/// Explicit retry uses the same exact-object journal, and never resolves a URL
/// or resubmits a local download. It also works when aria2 is unavailable.
pub(crate) async fn cleanup_cloud_share_task(
    app: AppHandle,
    id: uuid::Uuid,
    retry: bool,
) -> Result<DownloadTask, QuarkError> {
    let state = app.try_state::<AppState>().ok_or(QuarkError::Storage)?;
    let task = state.store.find(id).map_err(|_| QuarkError::Storage)?;
    if state.store.is_agent_resource_task(id).map_err(|_| QuarkError::Storage)? {
        return Ok(task);
    }
    share::parse_stable_cloud_share_source(&task.source)?;
    if task.status != TaskStatus::Completed {
        return Err(QuarkError::InvalidInput(
            "只有已完成的分享任务可以重试清理".into(),
        ));
    }
    if !state
        .settings
        .lock()
        .map_err(|_| QuarkError::Storage)?
        .delete_transferred_share_files_after_download
    {
        return Ok(task);
    }
    match cleanup_cloud_share_task_inner(app.clone(), id, retry).await {
        Ok(task) => Ok(task),
        Err(_) => {
            let task = state
                .store
                .set_completed_cleanup_message(id, true)
                .map_err(|_| QuarkError::Storage)?;
            let cloud = app.try_state::<Arc<CloudState>>();
            let _ = crate::commands::emit_task_update_for_task(
                &app,
                &state.store,
                cloud.as_ref().map(|cloud| &cloud.share_plans),
                task.clone(),
            );
            Ok(task)
        }
    }
}

pub(crate) async fn cleanup_cloud_share_task_inner(
    app: AppHandle,
    id: uuid::Uuid,
    retry: bool,
) -> Result<DownloadTask, QuarkError> {
    let state = app.try_state::<AppState>().ok_or(QuarkError::Storage)?;
    let task = state.store.find(id).map_err(|_| QuarkError::Storage)?;
    if state.store.is_agent_resource_task(id).map_err(|_| QuarkError::Storage)? {
        return Ok(task);
    }
    if task.status != TaskStatus::Completed {
        return Err(QuarkError::InvalidInput(
            "只有已完成的分享任务可以重试清理".into(),
        ));
    }
    let (provider, _, _) = share::parse_stable_cloud_share_source(&task.source)?;
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    // Acquire the stable plan lock before reading its current linked tasks.
    // New entries share this lock even if an earlier plan snapshot omitted them.
    let _plan_guard = state.cloud_download_sources.acquire(&task.source).await?;
    cloud.share_plans.link_local_task(&task.source, id)?;
    let tasks = Arc::clone(&state.store);
    let worker_app = app.clone();
    // No automatic session-recovery replay around a destructive action.
    let updated = run_session_work(
        app.clone(),
        provider,
        SessionWorkKind::Mutation,
        move |cloud, http| {
            let state = worker_app
                .try_state::<AppState>()
                .ok_or(QuarkError::Storage)?;
            let enabled = state
                .settings
                .lock()
                .map_err(|_| QuarkError::Storage)?
                .delete_transferred_share_files_after_download;
            let entry = cloud.share_plans.resolve_entry(&task.source)?;
            if !share_entry_needs_cleanup(&entry) {
                return Ok(None);
            }
            let adapter = share::provider_share_adapter(provider, &cloud.store, http)?;
            let owner = adapter.transfer_owner_fingerprint(&entry)?;
            crate::engine_runtime::cleanup_completed_share_for_owner(
                &cloud.share_plans,
                &tasks,
                &task,
                enabled,
                retry,
                owner.as_deref(),
                |provider, ids| {
                    ProviderRegistry::global()
                        .driver(provider, &cloud.store, http)
                        .trash_transferred_files(ids)
                },
            )
        },
    )
    .await?;
    if let Some(updated) = updated {
        let _ = crate::commands::emit_task_update_for_task(
            &app,
            &state.store,
            Some(&cloud.share_plans),
            updated.clone(),
        );
        Ok(updated)
    } else {
        state.store.find(id).map_err(|_| QuarkError::Storage)
    }
}
