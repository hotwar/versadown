//! Runtime-only cloud admission and cancellation coordination. No request data
//! or credentials are retained here; jobs own their ephemeral options.
use super::{
    download_scheduler::{AdmissionOutcome, CloudDownloadScheduler},
    CloudProvider, QuarkError,
};
use crate::{DownloadAction, DownloadTask, TaskStatus, TaskStore, UriSubmissionOptions};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard};
use uuid::Uuid;

#[derive(Default)]
struct TaskDispatch {
    version: AtomicU64,
    submission: Arc<AsyncMutex<()>>,
    job: AsyncMutex<()>,
    changed: Notify,
    retry_intent: Mutex<RetryIntent>,
    directory: Mutex<Option<DirectoryAdmission>>,
}

#[derive(Clone, Copy, Debug)]
struct DirectoryAdmission {
    id: Uuid,
    max_active_members: usize,
}

#[derive(Clone, Debug, Default)]
struct RetryIntent {
    original_gid: Option<String>,
    native_resume_gid: Option<String>,
    allow_overwrite: bool,
    continue_download: bool,
}

/// A provider-issued URL is an engine implementation detail, never the
/// durable identity of a cloud download.  While this marker is set, the GID
/// remains available solely for the native checkpoint lookup; recovery and
/// synchronization must not project its (possibly expired) URL back onto the
/// stable cloud task.
pub(crate) const CLOUD_URL_REFRESHING: &str = "cloud_url_refreshing";
pub(crate) const CLOUD_URL_REFRESH_FAILED: &str = "cloud_url_refresh_failed";

pub(crate) fn is_cloud_download_source(source: &str) -> bool {
    source.starts_with("cloud://") || source.starts_with("cloud-share://")
}

pub(crate) fn cloud_task_requires_provider_dispatch(task: &DownloadTask) -> bool {
    is_cloud_download_source(&task.source)
        && matches!(
            task.failure_code.as_deref(),
            Some(
                "authorization_checking"
                    | "authorization_recovering"
                    | "authorization_pending"
                    | CLOUD_URL_REFRESHING
                    | CLOUD_URL_REFRESH_FAILED
            )
        )
}

fn signed_cloud_url_requires_refresh(task: &DownloadTask) -> bool {
    task.status == TaskStatus::Failed
        && is_cloud_download_source(&task.source)
        && matches!(
            task.aria_error_code.as_deref(),
            Some("6" | "8" | "22" | "24")
        )
}

#[derive(Clone, Default)]
pub struct CloudDownloadDispatch {
    pub scheduler: CloudDownloadScheduler,
    tasks: Arc<Mutex<HashMap<Uuid, Arc<TaskDispatch>>>>,
}

struct Slot<'a> {
    scheduler: CloudDownloadScheduler,
    store: &'a TaskStore,
    id: Uuid,
    directory: Option<DirectoryAdmission>,
    accepted: bool,
}

/// A cancelled, recycled or deleted directory will never be released by a
/// terminal-member event, because it has no member left to emit one.
fn directory_is_closed(store: &TaskStore, directory: Uuid) -> bool {
    match store.cloud_directory_group(directory) {
        Ok(Some(group)) => matches!(group.status, TaskStatus::Cancelled | TaskStatus::Recycled),
        Ok(None) => true,
        Err(_) => false,
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if !self.accepted {
            if let Some(directory) = self.directory {
                // A queued member can be admitted after its directory was
                // cancelled and the owner was released. That admission creates
                // the owner again, so give it back together with the member.
                self.scheduler.release_directory_member(
                    directory.id,
                    self.id,
                    directory_is_closed(self.store, directory.id),
                );
            } else {
                self.scheduler.release(self.id);
            }
        }
    }
}

impl CloudDownloadDispatch {
    fn task(&self, id: Uuid) -> Arc<TaskDispatch> {
        self.tasks.lock().unwrap().entry(id).or_default().clone()
    }

    pub async fn submission_lock(&self, id: Uuid) -> OwnedMutexGuard<()> {
        self.task(id).submission.clone().lock_owned().await
    }

    /// A directory root is one logical cloud task; this only controls how many
    /// of its independently-addressed files may be admitted concurrently.
    pub fn configure_directory(&self, id: Uuid, directory: Uuid, max_active_members: u8) {
        self.task(id)
            .directory
            .lock()
            .unwrap()
            .replace(DirectoryAdmission {
                id: directory,
                max_active_members: usize::from(max_active_members.max(1)),
            });
    }

    /// Terminal children free their file admission. The directory's logical
    /// task admission survives until the projected directory root is terminal.
    pub fn release_terminal(&self, id: Uuid, directory_finished: bool) {
        let directory = *self.task(id).directory.lock().unwrap();
        if let Some(directory) = directory {
            self.scheduler
                .release_directory_member(directory.id, id, directory_finished);
        } else {
            self.scheduler.release(id);
        }
        self.scheduler.cancel(id);
    }

    /// Invalidate before changing local intent. The worker drops a pending
    /// resolver and its slot; an accepted aria2 submission retains ownership.
    pub fn invalidate(&self, id: Uuid) {
        self.task(id).version.fetch_add(1, Ordering::SeqCst);
        self.scheduler.cancel(id);
        self.task(id).changed.notify_waiters();
    }

    /// A fresh cloud submission replaces the engine submission that previously
    /// owned this task. Its old provider admission must not make the new
    /// resolver look like a duplicate of itself.
    fn release_replaced_admission(&self, id: Uuid) {
        let directory = *self.task(id).directory.lock().unwrap();
        if let Some(directory) = directory {
            self.scheduler
                .release_directory_member(directory.id, id, false);
        } else {
            self.scheduler.release(id);
        }
    }

    pub async fn prepare(
        &self,
        store: &TaskStore,
        id: Uuid,
    ) -> Result<(DownloadTask, u64), QuarkError> {
        let expected = store.find(id).map_err(|_| QuarkError::Storage)?;
        self.prepare_expected(store, &expected).await
    }

    pub async fn prepare_expected(
        &self,
        store: &TaskStore,
        expected: &DownloadTask,
    ) -> Result<(DownloadTask, u64), QuarkError> {
        self.prepare_expected_inner(store, expected, None).await
    }

    pub async fn prepare_expected_with_retry_intent(
        &self,
        store: &TaskStore,
        expected: &DownloadTask,
        allow_overwrite: bool,
        continue_download: bool,
    ) -> Result<(DownloadTask, u64), QuarkError> {
        self.prepare_expected_inner(store, expected, Some((allow_overwrite, continue_download)))
            .await
    }

    async fn prepare_expected_inner(
        &self,
        store: &TaskStore,
        expected: &DownloadTask,
        retry_options: Option<(bool, bool)>,
    ) -> Result<(DownloadTask, u64), QuarkError> {
        let id = expected.id;
        let state = self.task(id);
        let _guard = state.submission.clone().lock_owned().await;
        let mut task = store.find(id).map_err(|_| QuarkError::Storage)?;
        if task.status != expected.status
            || task.aria_gid != expected.aria_gid
            || task.updated_at != expected.updated_at
        {
            return Err(QuarkError::InvalidInput(
                "任务状态已变化，本次提交已跳过".into(),
            ));
        }
        let signed_url_refresh = signed_cloud_url_requires_refresh(&task);
        let retry_options = retry_options.map(|(allow_overwrite, continue_download)| {
            // Error 8 describes the provider URL that aria2 just used, not
            // the integrity of the persisted byte ranges. A fresh signed URL
            // must receive the verified checkpoint unless the user explicitly
            // chose overwrite.
            (
                allow_overwrite,
                (continue_download || signed_url_refresh) && !allow_overwrite,
            )
        });
        let native_resume = retry_options.is_some_and(|(_, continue_download)| continue_download)
            && task.status == TaskStatus::Paused
            && crate::xunlei_native::is_native_gid(task.aria_gid.as_deref());
        // Cloud GIDs point at short-lived provider URLs.  Retain the old GID
        // only as the native checkpoint identity until the fresh URL is
        // accepted; it must not be treated as the active engine submission.
        let replacing_cloud_url = is_cloud_download_source(&task.source)
            && task.aria_gid.is_some()
            && !crate::xunlei_native::is_native_gid(task.aria_gid.as_deref());
        if task.status == TaskStatus::Recycled
            || (!native_resume
                && !replacing_cloud_url
                && task.aria_gid.is_some()
                && matches!(
                    task.status,
                    TaskStatus::Queued | TaskStatus::Downloading | TaskStatus::Paused
                ))
        {
            return Err(QuarkError::InvalidInput("任务正在下载或已回收".into()));
        }
        {
            let mut intent = state.retry_intent.lock().unwrap();
            if task.aria_gid.is_some() {
                intent.original_gid.clone_from(&task.aria_gid);
            }
            if let Some((allow_overwrite, continue_download)) = retry_options {
                intent.allow_overwrite = allow_overwrite;
                intent.continue_download = continue_download && !allow_overwrite;
                if !intent.continue_download {
                    intent.native_resume_gid = None;
                } else if let Some(gid) = task.aria_gid.as_deref() {
                    // Retain identity even if the native database is momentarily
                    // unavailable; submission must fail closed, never guess length.
                    intent.native_resume_gid =
                        (!crate::aria2::native_resume::has_legacy_control(store, &task))
                            .then(|| gid.to_owned());
                }
            }
        }
        self.invalidate(id);
        self.release_replaced_admission(id);
        task.status = TaskStatus::Queued;
        task.error = None;
        task.download_speed = 0;
        task.upload_speed = 0;
        if replacing_cloud_url {
            task.failure_code = Some(CLOUD_URL_REFRESHING.into());
            task.status_message = Some("正在刷新网盘下载地址".into());
        } else {
            task.aria_gid = None;
            task.status_message = None;
            task.failure_code = None;
            task.aria_error_code = None;
            task.followed_by_gid = None;
        }
        let task = store
            .apply_transfer_snapshot(task)
            .map_err(|_| QuarkError::Storage)?;
        Ok((task, self.task(id).version.load(Ordering::SeqCst)))
    }

    pub fn retry_submission_options(
        &self,
        id: Uuid,
        mut options: UriSubmissionOptions,
    ) -> UriSubmissionOptions {
        let intent = self.task(id).retry_intent.lock().unwrap().clone();
        if intent.allow_overwrite {
            options.allow_overwrite = true;
            options.continue_download = false;
        } else if intent.continue_download {
            options.continue_download = true;
            options.native_resume_gid = intent.native_resume_gid;
        }
        options
    }

    /// Returns None once aria2 owns the task; callers then use normal engine control.
    pub async fn control_unsubmitted(
        &self,
        store: &TaskStore,
        id: Uuid,
        action: DownloadAction,
    ) -> Result<Option<DownloadTask>, QuarkError> {
        // A native submission may already be inside a foreign SDK call. Let
        // cancellation invalidate that worker immediately; Xunlei's submit
        // path rechecks the durable state before publishing its result.
        let skip_submission_wait = action == DownloadAction::Cancel
            && store.find(id).is_ok_and(|task| {
                task.source.starts_with("cloud://xunlei/")
                    || task.source.starts_with("cloud-share://xunlei/")
            });
        let _guard = if skip_submission_wait {
            None
        } else {
            Some(self.submission_lock(id).await)
        };
        let mut task = store.find(id).map_err(|_| QuarkError::Storage)?;
        let refreshing_cloud_url = cloud_task_requires_provider_dispatch(&task)
            && task.failure_code.as_deref() == Some(CLOUD_URL_REFRESHING);
        if task.aria_gid.is_some() && !refreshing_cloud_url {
            return Ok(None);
        }
        let status = match (action, task.status) {
            (DownloadAction::Pause, TaskStatus::Queued | TaskStatus::Parsing) => TaskStatus::Paused,
            (
                DownloadAction::Cancel,
                TaskStatus::Queued | TaskStatus::Parsing | TaskStatus::Paused,
            ) => TaskStatus::Cancelled,
            _ => return Err(QuarkError::InvalidInput("当前任务不能执行此操作".into())),
        };
        self.invalidate(id);
        task.status = status;
        if action == DownloadAction::Cancel {
            if refreshing_cloud_url {
                // The resolver has not submitted a fresh URL.  Detach the
                // stale session so a later engine restart can dispose of it
                // without ever reviving this cancelled task.
                task.aria_gid = None;
                task.followed_by_gid = None;
                task.failure_code = None;
                task.status_message = None;
                *self.task(id).retry_intent.lock().unwrap() = RetryIntent::default();
            } else {
                task.aria_gid = self
                    .task(id)
                    .retry_intent
                    .lock()
                    .unwrap()
                    .original_gid
                    .clone();
            }
        }
        task.download_speed = 0;
        task.upload_speed = 0;
        store
            .apply_transfer_snapshot(task)
            .map(Some)
            .map_err(|_| QuarkError::Storage)
    }

    fn runnable(&self, store: &TaskStore, id: Uuid, version: u64) -> bool {
        self.task(id).version.load(Ordering::SeqCst) == version
            && store.find(id).is_ok_and(|task| {
                task.status == TaskStatus::Queued
                    && (task.aria_gid.is_none()
                        || (task.failure_code.as_deref() == Some(CLOUD_URL_REFRESHING)
                            && cloud_task_requires_provider_dispatch(&task)))
            })
            && store.cloud_directory_group_for_task(id).is_ok_and(|group| {
                group.is_none_or(|group| {
                    !matches!(
                        group.status,
                        TaskStatus::Paused | TaskStatus::Cancelled | TaskStatus::Recycled
                    )
                })
            })
    }

    /// Resolve only after admission. A per-task job lock prevents an old
    /// resolver's slot guard from releasing a newer retry's slot.
    pub async fn run<R, RF, S, SF, T>(
        &self,
        store: &TaskStore,
        provider: CloudProvider,
        id: Uuid,
        version: u64,
        resolve: R,
        submit: S,
    ) -> Option<DownloadTask>
    where
        R: FnOnce() -> RF,
        RF: Future<Output = Result<T, QuarkError>>,
        S: FnOnce(T) -> SF,
        SF: Future<Output = Result<DownloadTask, QuarkError>>,
    {
        let state = self.task(id);
        let _job = state.job.lock().await;
        let changed = state.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !self.runnable(store, id, version) {
            return None;
        }
        let directory = *state.directory.lock().unwrap();
        let admission = tokio::select! {
            admission = async {
                if let Some(directory) = directory {
                    self.scheduler
                        .enqueue_directory(provider, directory.id, id, directory.max_active_members)
                        .await
                } else {
                    self.scheduler.enqueue(provider, id).await
                }
            } => admission,
            _ = &mut changed => return None,
        };
        if admission != AdmissionOutcome::Granted {
            return None;
        }
        let mut slot = Slot {
            scheduler: self.scheduler.clone(),
            store,
            id,
            directory,
            accepted: false,
        };
        if !self.runnable(store, id, version) {
            return None;
        }
        let resolved = tokio::select! {
            result = resolve() => result,
            _ = &mut changed => return None,
        };
        let _submission = state.submission.lock().await;
        if !self.runnable(store, id, version) {
            return None;
        }
        let result = match resolved {
            Ok(resolved) => submit(resolved).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(task) => {
                slot.accepted = !matches!(
                    task.status,
                    TaskStatus::Completed
                        | TaskStatus::Failed
                        | TaskStatus::Cancelled
                        | TaskStatus::Recycled
                );
                *state.retry_intent.lock().unwrap() = RetryIntent::default();
                Some(task)
            }
            Err(error) => {
                let original_gid = state.retry_intent.lock().unwrap().original_gid.clone();
                if let Ok(current) = store.find(id) {
                    // `addUri` may have succeeded and only the immediate
                    // status projection failed.  A distinct replacement GID
                    // is already authoritative; do not turn it back into the
                    // expired checkpoint.
                    if current.aria_gid.is_some()
                        && current.aria_gid != original_gid
                        && !cloud_task_requires_provider_dispatch(&current)
                    {
                        slot.accepted = !matches!(
                            current.status,
                            TaskStatus::Completed
                                | TaskStatus::Failed
                                | TaskStatus::Cancelled
                                | TaskStatus::Recycled
                        );
                        *state.retry_intent.lock().unwrap() = RetryIntent::default();
                        return Some(current);
                    }
                }
                let message = super::commands::share_resolution_failure_message(&error);
                // A replacement retains the expired GID as its local resume
                // checkpoint, so it needs the guarded failure transition
                // above. A first-time cloud submission has no checkpoint:
                // applying that guard to it would reject the update and leave
                // the visible task queued forever.
                if store
                    .find(id)
                    .is_ok_and(|task| task.failure_code.as_deref() == Some(CLOUD_URL_REFRESHING))
                {
                    store.fail_cloud_url_refresh(id, message).ok()
                } else {
                    store.fail(id, message).ok()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewTask;

    /// A directory with one admitted file and one queued file is cancelled.
    /// Releasing the admitted file frees the directory's provider slot, which
    /// immediately admits the queued file as a brand-new owner. That file is no
    /// longer runnable, so its dropped slot must also give the owner back;
    /// otherwise the provider's only slot stays taken by a deleted directory.
    #[tokio::test]
    async fn cancelled_directory_does_not_keep_the_provider_slot_through_a_late_admission() {
        use crate::models::{NewCloudDirectoryGroup, NewCloudDirectoryGroupItem};

        let store = Arc::new(TaskStore::open_in_memory().unwrap());
        // Tianyi admits a single provider task, so a leaked slot is visible.
        let provider = CloudProvider::Tianyi;
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    provider,
                    "cloud-share://tianyi/share",
                    "Share",
                    std::path::Path::new("/downloads"),
                ),
                &[
                    NewCloudDirectoryGroupItem::new(
                        "cloud-share://tianyi/share/a",
                        &["Share"],
                        "a.bin",
                        Some(1),
                    ),
                    NewCloudDirectoryGroupItem::new(
                        "cloud-share://tianyi/share/b",
                        &["Share"],
                        "b.bin",
                        Some(1),
                    ),
                ],
            )
            .unwrap();
        let mut members = Vec::new();
        for item in store.cloud_directory_group_items(group.id).unwrap() {
            members.push(
                store
                    .prepare_cloud_share_directory_leaf_task(
                        group.id,
                        item.id,
                        NewTask {
                            source: item.remote_file_id.clone(),
                            name: item.output_name.clone(),
                            kind: "cloud".into(),
                            is_private: false,
                        },
                        None,
                    )
                    .unwrap(),
            );
        }
        for member in &members {
            store.transition(member.id, TaskStatus::Queued).unwrap();
        }
        let (admitted, queued) = (members[0].clone(), members[1].clone());
        let dispatch = CloudDownloadDispatch::default();
        for member in &members {
            dispatch.configure_directory(member.id, group.id, 1);
        }
        assert_eq!(
            dispatch
                .scheduler
                .enqueue_directory(provider, group.id, admitted.id, 1)
                .await,
            AdmissionOutcome::Granted
        );
        let version = dispatch.task(queued.id).version.load(Ordering::SeqCst);
        let worker = {
            let (dispatch, store, id) = (dispatch.clone(), Arc::clone(&store), queued.id);
            tokio::spawn(async move {
                dispatch
                    .run(
                        &store,
                        provider,
                        id,
                        version,
                        || async { Ok::<(), QuarkError>(()) },
                        |()| async { unreachable!("a cancelled directory never submits") },
                    )
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !dispatch.scheduler.pending(provider).contains(&queued.id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the queued member should wait for the directory's single member slot");

        store.cancel_cloud_directory_group(group.id).unwrap();
        dispatch.release_terminal(admitted.id, true);
        assert!(worker.await.unwrap().is_none());

        let unrelated = Uuid::new_v4();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            dispatch.scheduler.enqueue(provider, unrelated),
        )
        .await;
        assert_eq!(outcome.ok(), Some(AdmissionOutcome::Granted));
    }

    #[tokio::test]
    async fn startup_recovered_partial_download_reenters_dispatch_without_creating_a_task() {
        for provider in super::super::ProviderRegistry::global().catalog() {
            for scheme in ["cloud", "cloud-share"] {
                let store = TaskStore::open_in_memory().unwrap();
                let mut task = store
                    .create(NewTask::direct(format!(
                        "{scheme}://{}/fixture",
                        provider.provider.key()
                    )))
                    .unwrap();
                task.status = TaskStatus::Queued;
                task.completed_length = 40;
                task.total_length = Some(100);
                let task = store.apply_transfer_snapshot(task).unwrap();
                store.defer_startup_cloud_downloads().unwrap();
                let paused = store.find(task.id).unwrap();
                assert_eq!(paused.status, TaskStatus::Paused);
                let dispatch = CloudDownloadDispatch::default();
                let (_, version) = dispatch
                    .prepare_expected_with_retry_intent(&store, &paused, false, true)
                    .await
                    .unwrap();
                let options =
                    dispatch.retry_submission_options(task.id, UriSubmissionOptions::default());
                assert!(options.continue_download);
                assert!(!options.allow_overwrite);
                let result = dispatch
                    .run(
                        &store,
                        provider.provider,
                        task.id,
                        version,
                        || async { Ok(()) },
                        |_| async {
                            let mut resumed = store.find(task.id).unwrap();
                            resumed.aria_gid = Some("replacement-gid".into());
                            resumed.status = TaskStatus::Downloading;
                            store
                                .apply_transfer_snapshot(resumed)
                                .map_err(|_| QuarkError::Storage)
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(result.id, task.id);
                assert_eq!(result.status, TaskStatus::Downloading);
                assert_eq!(result.completed_length, 40);
                assert_eq!(result.total_length, Some(100));
                assert_eq!(store.list().unwrap().len(), 1);
            }
        }
    }

    #[tokio::test]
    async fn paused_xunlei_native_task_can_reenter_dispatch_for_fresh_authorization() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud://xunlei/file-id"))
            .unwrap();
        task.status = TaskStatus::Paused;
        task.aria_gid = Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:42".into());
        let task = store.apply_transfer_snapshot(task).unwrap();
        let dispatch = CloudDownloadDispatch::default();

        let (queued, _) = dispatch
            .prepare_expected_with_retry_intent(&store, &task, false, true)
            .await
            .unwrap();

        assert_eq!(queued.status, TaskStatus::Queued);
        assert!(queued.aria_gid.is_none());
        assert!(queued.failure_code.is_none());
        let options = dispatch.retry_submission_options(task.id, UriSubmissionOptions::default());
        assert!(options.continue_download);
        assert_eq!(
            options.native_resume_gid.as_deref(),
            Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:42")
        );
    }

    #[tokio::test]
    async fn queued_xunlei_cancel_does_not_wait_for_a_blocked_native_submission() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud://xunlei/file-id"))
            .unwrap();
        let dispatch = CloudDownloadDispatch::default();
        dispatch.prepare_expected(&store, &task).await.unwrap();
        let _blocked_submission = dispatch.submission_lock(task.id).await;

        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            dispatch.control_unsubmitted(&store, task.id, DownloadAction::Cancel),
        )
        .await
        .expect("Xunlei cancellation must be persisted before native submission returns")
        .unwrap()
        .unwrap();

        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn resuming_a_native_xunlei_directory_member_releases_its_previous_admission() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud://xunlei/file-id"))
            .unwrap();
        task.status = TaskStatus::Paused;
        task.aria_gid = Some("xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:42".into());
        let task = store.apply_transfer_snapshot(task).unwrap();
        let dispatch = CloudDownloadDispatch::default();
        let directory_id = Uuid::new_v4();
        dispatch.configure_directory(task.id, directory_id, 2);

        assert_eq!(
            dispatch
                .scheduler
                .enqueue_directory(CloudProvider::Xunlei, directory_id, task.id, 2)
                .await,
            AdmissionOutcome::Granted
        );

        dispatch
            .prepare_expected_with_retry_intent(&store, &task, false, true)
            .await
            .unwrap();

        assert_eq!(
            dispatch
                .scheduler
                .enqueue_directory(CloudProvider::Xunlei, directory_id, task.id, 2)
                .await,
            AdmissionOutcome::Granted
        );
    }

    #[tokio::test]
    async fn paused_direct_aria_task_cannot_reenter_dispatch_while_it_owns_a_gid() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("https://example.test/file"))
            .unwrap();
        task.status = TaskStatus::Paused;
        task.aria_gid = Some("0123456789abcdef".into());
        let task = store.apply_transfer_snapshot(task).unwrap();

        assert!(CloudDownloadDispatch::default()
            .prepare_expected_with_retry_intent(&store, &task, false, true)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn paused_cloud_aria_task_reenters_dispatch_to_refresh_its_url() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud://aliyun/file-id"))
            .unwrap();
        task.status = TaskStatus::Paused;
        task.aria_gid = Some("0123456789abcdef".into());
        let task = store.apply_transfer_snapshot(task).unwrap();
        let dispatch = CloudDownloadDispatch::default();

        let (queued, _) = dispatch
            .prepare_expected_with_retry_intent(&store, &task, false, true)
            .await
            .unwrap();

        assert_eq!(queued.status, TaskStatus::Queued);
        assert_eq!(queued.aria_gid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(queued.failure_code.as_deref(), Some(CLOUD_URL_REFRESHING));
        assert!(
            dispatch
                .retry_submission_options(task.id, UriSubmissionOptions::default())
                .continue_download
        );
    }

    #[tokio::test]
    async fn failed_cloud_aria_task_keeps_its_checkpoint_after_range_rejection() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud-share://pikpak/share-id/file-id"))
            .unwrap();
        task.status = TaskStatus::Failed;
        task.aria_gid = Some("0123456789abcdef".into());
        task.aria_error_code = Some("8".into());
        let task = store.apply_transfer_snapshot(task).unwrap();
        let dispatch = CloudDownloadDispatch::default();

        let (queued, _) = dispatch
            .prepare_expected_with_retry_intent(&store, &task, false, true)
            .await
            .unwrap();

        assert_eq!(queued.status, TaskStatus::Queued);
        assert_eq!(queued.aria_gid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(queued.failure_code.as_deref(), Some(CLOUD_URL_REFRESHING));
        let options = dispatch.retry_submission_options(task.id, UriSubmissionOptions::default());
        assert!(!options.allow_overwrite);
        assert!(options.continue_download);
        assert_eq!(
            options.native_resume_gid.as_deref(),
            Some("0123456789abcdef")
        );
    }

    #[tokio::test]
    async fn failed_cloud_aria_task_refreshes_its_signed_url_with_its_checkpoint() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud://aliyun/file-id"))
            .unwrap();
        task.status = TaskStatus::Failed;
        task.aria_gid = Some("0123456789abcdef".into());
        task.aria_error_code = Some("24".into());
        let task = store.apply_transfer_snapshot(task).unwrap();
        let dispatch = CloudDownloadDispatch::default();

        let (queued, _) = dispatch
            .prepare_expected_with_retry_intent(&store, &task, false, false)
            .await
            .unwrap();

        assert_eq!(queued.status, TaskStatus::Queued);
        assert_eq!(queued.aria_gid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(queued.failure_code.as_deref(), Some(CLOUD_URL_REFRESHING));
        let options = dispatch.retry_submission_options(task.id, UriSubmissionOptions::default());
        assert!(options.continue_download);
        assert_eq!(
            options.native_resume_gid.as_deref(),
            Some("0123456789abcdef")
        );
    }

    #[tokio::test]
    async fn failed_cloud_url_refresh_keeps_its_checkpoint_out_of_engine_recovery() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud-share://aliyun/share/file"))
            .unwrap();
        task.status = TaskStatus::Failed;
        task.aria_gid = Some("0123456789abcdef".into());
        task.aria_error_code = Some("24".into());
        let task = store.apply_transfer_snapshot(task).unwrap();
        let dispatch = CloudDownloadDispatch::default();
        let (queued, version) = dispatch
            .prepare_expected_with_retry_intent(&store, &task, false, false)
            .await
            .unwrap();

        let result = dispatch
            .run(
                &store,
                CloudProvider::Aliyun,
                task.id,
                version,
                || async { Err::<(), _>(QuarkError::Transport("provider unavailable".into())) },
                |_: ()| async { unreachable!() },
            )
            .await
            .unwrap();

        assert_eq!(queued.aria_gid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(result.status, TaskStatus::Failed);
        assert_eq!(result.aria_gid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(
            result.failure_code.as_deref(),
            Some(CLOUD_URL_REFRESH_FAILED)
        );
        assert!(cloud_task_requires_provider_dispatch(&result));
        assert_eq!(result.aria_error_code.as_deref(), Some("24"));
    }

    #[tokio::test]
    async fn failed_initial_cloud_submission_does_not_remain_queued() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud://aliyun/file-id"))
            .unwrap();
        let dispatch = CloudDownloadDispatch::default();
        let (queued, version) = dispatch.prepare_expected(&store, &task).await.unwrap();

        let result = dispatch
            .run(
                &store,
                CloudProvider::Aliyun,
                task.id,
                version,
                || async { Err::<(), _>(QuarkError::Transport("provider unavailable".into())) },
                |_: ()| async { unreachable!() },
            )
            .await
            .unwrap();

        assert_eq!(queued.status, TaskStatus::Queued);
        assert!(queued.aria_gid.is_none());
        assert!(queued.failure_code.is_none());
        assert_eq!(result.status, TaskStatus::Failed);
        assert!(result.aria_gid.is_none());
        assert!(result.failure_code.is_none());
        assert_eq!(
            result.error.as_deref(),
            Some("获取网盘下载地址时网络请求失败，请检查网络后重试")
        );
    }
}
