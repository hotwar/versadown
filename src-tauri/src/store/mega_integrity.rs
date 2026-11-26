//! MEGA's decrypted bytes are not a successful download until their MAC passes.
//! Reuse Processing and an attempt marker; no engine receipt can certify a file.
use super::*;

const PENDING_PREFIX: &str = "mega_integrity_pending:";

fn is_mega_source(source: &str) -> bool {
    source.starts_with("cloud://mega/") || source.starts_with("cloud-share://mega/")
}

pub(crate) fn is_mega_integrity_pending(task: &DownloadTask) -> bool {
    task.status == TaskStatus::Processing
        && task.failure_code.as_deref().is_some_and(|code| code.starts_with(PENDING_PREFIX))
        && is_mega_source(&task.source)
}

pub(crate) fn is_mega_integrity_failure(task: &DownloadTask) -> bool {
    task.status == TaskStatus::Failed
        && matches!(task.failure_code.as_deref(),
            Some("mega_integrity_failed" | "mega_integrity_unavailable"))
        && is_mega_source(&task.source)
}

/// Called while the same transaction owns the current row, before any
/// Completed state (or completed_at) can become visible to another reader.
/// True preserves the entire current record, including the file list that
/// the verifier opened; a late engine reply must not swap the delivered path.
pub(super) fn hold_mega_completion(snapshot: &mut DownloadTask, current: &DownloadTask) -> bool {
    if !is_mega_source(&current.source) {
        return false;
    }
    let same_attempt = snapshot.aria_gid == current.aria_gid;
    if same_attempt && is_mega_integrity_pending(current)
        && !matches!(snapshot.status, TaskStatus::Cancelled | TaskStatus::Recycled)
    {
        return true;
    }
    if snapshot.status != TaskStatus::Completed {
        return false;
    }
    if same_attempt && (is_mega_integrity_failure(current)
        || matches!(current.status, TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Recycled))
    {
        return true;
    }
    snapshot.status = TaskStatus::Processing;
    snapshot.progress = 99;
    snapshot.download_speed = 0;
    snapshot.upload_speed = 0;
    snapshot.eta_seconds = None;
    snapshot.error = None;
    snapshot.failure_code = Some(format!("{PENDING_PREFIX}{}", Uuid::new_v4()));
    snapshot.completed_at = None;
    false
}

impl TaskStore {
    /// Only the worker for this exact verification attempt may publish its
    /// result. Cancellation, retry and a newer GID invalidate an old worker.
    pub(crate) fn finish_mega_integrity(
        &self,
        pending: &DownloadTask,
        result: Result<(), crate::cloud::mega::MegaFailure>,
    ) -> Result<Option<DownloadTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let Some(current) = find_with_transaction(&transaction, pending.id)? else {
            return Ok(None);
        };
        if !is_mega_integrity_pending(&current)
            || current.aria_gid != pending.aria_gid
            || current.failure_code != pending.failure_code
        {
            return Ok(None);
        }
        let (status, progress, error, failure_code, completed_at) = match result {
            Ok(()) => (TaskStatus::Completed, 100, None, None, Some(current_timestamp())),
            Err(failure) => (
                TaskStatus::Failed, current.progress, Some(failure.message()),
                Some(failure.code()), None,
            ),
        };
        transaction.execute(
            "UPDATE tasks SET status=?1, progress=?2, error=?3, failure_code=?4,
             completed_at=?5, updated_at=?6, download_speed=0, upload_speed=0,
             eta_seconds=NULL WHERE id=?7",
            params![status.as_db(), progress, error, failure_code, completed_at,
                current_timestamp(), pending.id.to_string()],
        )?;
        transaction.commit()?;
        Ok(Some(find_with_connection(&connection, pending.id)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::mega::MegaFailure;

    fn engine_complete(store: &TaskStore, source: &str) -> DownloadTask {
        let mut task = store.create(NewTask {
            source: source.into(), name: "payload.bin".into(), kind: "cloud".into(), is_private: false,
        }).unwrap();
        task.aria_gid = Some("0011223344556677".into());
        task.status = TaskStatus::Completed;
        task.progress = 100;
        task.total_length = Some(10);
        task.completed_length = 10;
        store.apply_transfer_snapshot_with_files(task, &[(1, "C:/payload.bin".into(), 10, 10, true)]).unwrap()
    }

    #[test]
    fn mega_engine_completion_is_atomically_processing_until_verification_succeeds() {
        for source in ["cloud://mega/FILEAAAA", "cloud-share://mega/plan/file"] {
            let store = TaskStore::open_in_memory().unwrap();
            let pending = engine_complete(&store, source);
            assert!(is_mega_integrity_pending(&pending));
            assert_eq!(store.find(pending.id).unwrap().status, TaskStatus::Processing);
            assert_eq!(pending.completed_at, None);
            assert_eq!(store.files(pending.id).unwrap().len(), 1);
            let mut receipt = pending.clone();
            receipt.status = TaskStatus::Completed;
            let stale_files = [(1, "C:/different.bin".into(), 20, 20, true)];
            let again = store.apply_transfer_snapshot_with_files(receipt.clone(), &stale_files).unwrap();
            assert_eq!(store.files(pending.id).unwrap()[0].path, "C:/payload.bin");
            assert_eq!(again.failure_code, pending.failure_code);
            let done = store.finish_mega_integrity(&pending, Ok(())).unwrap().unwrap();
            assert_eq!(done.status, TaskStatus::Completed);
            assert_eq!(done.progress, 100);
            assert!(done.failure_code.is_none() && done.completed_at.is_some());
            let repeated = store.apply_transfer_snapshot_with_files(receipt, &stale_files).unwrap();
            assert_eq!(store.files(pending.id).unwrap()[0].path, "C:/payload.bin");
            assert_eq!(repeated.status, TaskStatus::Completed);
            assert_eq!(repeated.completed_at, done.completed_at);
            assert!(store.finish_mega_integrity(&pending, Ok(())).unwrap().is_none());
        }
    }

    #[test]
    fn integrity_failures_are_terminal_and_engine_receipts_cannot_overwrite_them() {
        for failure in [MegaFailure::IntegrityFailed, MegaFailure::IntegrityUnavailable] {
            let store = TaskStore::open_in_memory().unwrap();
            let pending = engine_complete(&store, "cloud://mega/FILEAAAA");
            let failed = store.finish_mega_integrity(&pending, Err(failure)).unwrap().unwrap();
            assert!(is_mega_integrity_failure(&failed));
            assert_eq!(failed.completed_at, None);
            let mut stale = pending;
            stale.status = TaskStatus::Completed;
            let after = store.apply_transfer_snapshot(stale).unwrap();
            assert_eq!(after.status, TaskStatus::Failed);
            assert_eq!(after.failure_code, failed.failure_code);
        }
    }

    #[test]
    fn cancellation_and_new_verification_attempts_reject_old_worker_results() {
        let store = TaskStore::open_in_memory().unwrap();
        let pending = engine_complete(&store, "cloud://mega/FILEAAAA");
        let mut cancelled = pending.clone();
        cancelled.status = TaskStatus::Cancelled;
        store.apply_transfer_snapshot(cancelled.clone()).unwrap();
        assert!(store.finish_mega_integrity(&pending, Ok(())).unwrap().is_none());
        // Retrying can reuse a native GID; the marker still distinguishes workers.
        cancelled.status = TaskStatus::Queued;
        cancelled.failure_code = None;
        let mut retry = store.apply_transfer_snapshot(cancelled).unwrap();
        retry.status = TaskStatus::Completed;
        let newer = store.apply_transfer_snapshot(retry).unwrap();
        assert!(is_mega_integrity_pending(&newer));
        assert_ne!(newer.failure_code, pending.failure_code);
        assert!(store.finish_mega_integrity(&pending, Ok(())).unwrap().is_none());
        assert!(store.finish_mega_integrity(&newer, Ok(())).unwrap().is_some());
    }

    #[test]
    fn engine_pause_or_failure_during_verification_does_not_replace_processing() {
        let store = TaskStore::open_in_memory().unwrap();
        let pending = engine_complete(&store, "cloud://mega/FILEAAAA");
        for status in [TaskStatus::Paused, TaskStatus::Downloading, TaskStatus::Failed] {
            let mut stale = pending.clone();
            stale.status = status;
            stale.failure_code = None;
            let actual = store.apply_transfer_snapshot(stale).unwrap();
            assert!(is_mega_integrity_pending(&actual));
            assert_eq!(actual.failure_code, pending.failure_code);
        }
    }


    #[test]
    fn directory_and_agent_projections_wait_for_the_same_integrity_result() {
        for result in [Ok(()), Err(MegaFailure::IntegrityFailed), Err(MegaFailure::IntegrityUnavailable)] {
            let store = TaskStore::open_in_memory().unwrap();
            let pending = engine_complete(&store, "cloud://mega/FILEAAAA");
            let group = store.create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Mega, "FOLDERAA", "bundle", std::env::temp_dir(),
                ),
                &[NewCloudDirectoryGroupItem::new(
                    "FILEAAAA", &[] as &[&str], "payload.bin", Some(10),
                )],
            ).unwrap();
            let item = store.cloud_directory_group_items(group.id).unwrap().remove(0);
            store.link_cloud_directory_group_item_task(group.id, item.id, pending.id).unwrap();
            let root = crate::commands::cloud_directory_group_queue_task(&store, group.id).unwrap();
            assert_eq!(root.status, TaskStatus::Processing);
            assert_eq!(root.progress, 99);
            assert_eq!(crate::cli_service::public_task(&root).status, "processing");
            store.finish_mega_integrity(&pending, result).unwrap().unwrap();
            let root = crate::commands::cloud_directory_group_queue_task(&store, group.id).unwrap();
            let expected = if result.is_ok() { TaskStatus::Completed } else { TaskStatus::Failed };
            assert_eq!(root.status, expected);
            assert_eq!(crate::cli_service::public_task(&root).status, expected.as_db());
        }
    }

    #[test]
    fn ordinary_download_completion_is_unchanged() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = engine_complete(&store, "cloud://quark/FILEAAAA");
        assert_eq!(task.status, TaskStatus::Completed);
        assert!(task.completed_at.is_some());
    }
}
