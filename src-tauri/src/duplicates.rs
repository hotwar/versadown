use std::path::Path;

use crate::{DownloadTask, DuplicateDownloadState, StoreError, TaskStatus, TaskStore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentPresence {
    AllPresent,
    Missing,
    Partial,
}

fn content_presence(store: &TaskStore, task: &DownloadTask) -> Result<ContentPresence, StoreError> {
    let files = store.files(task.id)?;
    if files.is_empty() {
        return Ok(ContentPresence::Missing);
    }

    let complete = files
        .iter()
        .map(|file| {
            std::fs::metadata(&file.path)
                .map(|metadata| {
                    metadata.is_file() && (file.length == 0 || metadata.len() == file.length)
                })
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    if complete.iter().all(|present| *present) {
        Ok(ContentPresence::AllPresent)
    } else if files.iter().any(|file| Path::new(&file.path).is_file()) {
        Ok(ContentPresence::Partial)
    } else {
        Ok(ContentPresence::Missing)
    }
}

fn has_existing_payload(store: &TaskStore, task: &DownloadTask) -> Result<bool, StoreError> {
    Ok(store
        .files(task.id)?
        .iter()
        .any(|file| Path::new(&file.path).is_file()))
}

fn can_resume(store: &TaskStore, task: &DownloadTask) -> Result<bool, StoreError> {
    if task
        .aria_gid
        .as_deref()
        .is_some_and(|gid| crate::aria2::native_resume::checkpoint(store, task, gid).is_some())
    {
        return Ok(true);
    }
    let files = store.files(task.id)?;
    let has_resumable_progress = task.completed_length > 0
        && task
            .total_length
            .map(|total_length| task.completed_length < total_length)
            .unwrap_or(true);
    Ok(has_resumable_progress
        && task.aria_gid.is_some()
        && !files.is_empty()
        && files.iter().any(|file| {
            let payload = Path::new(&file.path);
            let mut control = payload.as_os_str().to_os_string();
            control.push(".aria2");
            payload.is_file() && Path::new(&control).is_file()
        }))
}

/// Classifies an existing task without contacting aria2. The caller must show
/// the corresponding user choice before changing any files on disk.
pub fn duplicate_state(
    store: &TaskStore,
    task: &DownloadTask,
) -> Result<DuplicateDownloadState, StoreError> {
    match task.status {
        TaskStatus::Draft
        | TaskStatus::Parsing
        | TaskStatus::AwaitingUser
        | TaskStatus::Queued
        | TaskStatus::Downloading
        | TaskStatus::Processing => Ok(DuplicateDownloadState::ExistingActive),
        TaskStatus::Completed => Ok(match content_presence(store, task)? {
            ContentPresence::AllPresent => DuplicateDownloadState::CompletedFilesPresent,
            ContentPresence::Missing => DuplicateDownloadState::CompletedFilesMissing,
            ContentPresence::Partial => DuplicateDownloadState::OverwriteRequired,
        }),
        TaskStatus::Paused | TaskStatus::Failed | TaskStatus::Cancelled => {
            if can_resume(store, task)? {
                Ok(DuplicateDownloadState::ResumeAvailable)
            } else {
                Ok(DuplicateDownloadState::OverwriteRequired)
            }
        }
        // Recycled records are kept only for user recovery. Callers avoid treating
        // them as active duplicates, and this defensive fallback never resumes one.
        TaskStatus::Recycled => Ok(DuplicateDownloadState::OverwriteRequired),
    }
}

/// Returns a decision only if retrying might overwrite a payload or can resume
/// it. If every prior output is gone, the caller may submit a new transfer.
pub fn retry_duplicate_state(
    store: &TaskStore,
    task: &DownloadTask,
) -> Result<Option<DuplicateDownloadState>, StoreError> {
    #[cfg(target_os = "macos")]
    if task.failure_code.as_deref()
        == Some(crate::cloud::XunleiNativeDownloadFailure::UnsupportedCheckpoint.code())
        && crate::xunlei_native::is_native_gid(task.aria_gid.as_deref())
    {
        // The version check ran before a new transfer. Retry the preserved
        // native checkpoint, even when the partial payload already exists.
        return Ok(None);
    }
    let state = duplicate_state(store, task)?;
    match state {
        DuplicateDownloadState::ResumeAvailable => Ok(Some(state)),
        DuplicateDownloadState::OverwriteRequired if has_existing_payload(store, task)? => {
            Ok(Some(state))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::{duplicate_state, retry_duplicate_state};
    use crate::{DownloadTask, DuplicateDownloadState, NewTask, TaskStatus, TaskStore};

    fn task_with_status(store: &TaskStore, source: &str, status: TaskStatus) -> DownloadTask {
        let task = store.create(NewTask::direct(source)).unwrap();
        store
            .apply_transfer_snapshot(DownloadTask { status, ..task })
            .unwrap()
    }

    #[test]
    fn distinguishes_completed_present_missing_partial_and_resumable_outputs() {
        let root = std::env::temp_dir().join(format!("duplicate-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = TaskStore::open_in_memory().unwrap();

        let completed = task_with_status(
            &store,
            "https://example.test/complete",
            TaskStatus::Completed,
        );
        let complete_file = root.join("complete.bin");
        std::fs::write(&complete_file, b"abc").unwrap();
        store
            .upsert_files(
                completed.id,
                &[(0, complete_file.to_string_lossy().into_owned(), 3, 3, true)],
            )
            .unwrap();
        assert_eq!(
            duplicate_state(&store, &completed).unwrap(),
            DuplicateDownloadState::CompletedFilesPresent
        );
        std::fs::remove_file(&complete_file).unwrap();
        assert_eq!(
            duplicate_state(&store, &completed).unwrap(),
            DuplicateDownloadState::CompletedFilesMissing
        );

        let partial = task_with_status(
            &store,
            "https://example.test/partial",
            TaskStatus::Completed,
        );
        let partial_file = root.join("partial.bin");
        std::fs::write(&partial_file, b"a").unwrap();
        store
            .upsert_files(
                partial.id,
                &[(0, partial_file.to_string_lossy().into_owned(), 3, 1, true)],
            )
            .unwrap();
        assert_eq!(
            duplicate_state(&store, &partial).unwrap(),
            DuplicateDownloadState::OverwriteRequired
        );

        let resumable = task_with_status(&store, "https://example.test/resume", TaskStatus::Failed);
        let resumable = store
            .apply_transfer_snapshot(DownloadTask {
                total_length: Some(3),
                completed_length: 1,
                ..resumable
            })
            .unwrap();
        let resumable = store.bind_gid(resumable.id, "native-resume-gid").unwrap();
        let resumable_file = root.join("resume.bin");
        std::fs::write(&resumable_file, b"a").unwrap();
        std::fs::write(format!("{}.aria2", resumable_file.display()), b"control").unwrap();
        store
            .upsert_files(
                resumable.id,
                &[(0, resumable_file.to_string_lossy().into_owned(), 3, 1, true)],
            )
            .unwrap();
        assert_eq!(
            duplicate_state(&store, &resumable).unwrap(),
            DuplicateDownloadState::ResumeAvailable
        );
        assert_eq!(
            retry_duplicate_state(&store, &resumable).unwrap(),
            Some(DuplicateDownloadState::ResumeAvailable)
        );

        let recycled = task_with_status(
            &store,
            "https://example.test/recycled",
            TaskStatus::Recycled,
        );
        assert_eq!(
            duplicate_state(&store, &recycled).unwrap(),
            DuplicateDownloadState::OverwriteRequired,
            "a recycled task must never be resumed implicitly"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_preallocated_file_with_native_checkpoint_is_resumable() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::open_path(&root.path().join("tasks.sqlite")).unwrap();
        let failed = task_with_status(
            &store,
            "https://example.test/native.bin",
            TaskStatus::Failed,
        );
        let failed = store.bind_gid(failed.id, "0123456789abcdef").unwrap();
        let failed = store
            .apply_transfer_snapshot(DownloadTask {
                total_length: Some(128),
                completed_length: 7,
                ..failed
            })
            .unwrap();
        let payload = root.path().join("native.bin");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        store
            .upsert_files(
                failed.id,
                &[(0, payload.to_string_lossy().into_owned(), 128, 7, true)],
            )
            .unwrap();
        let state_directory = root.path().join("aria2-next-state/stream");
        std::fs::create_dir_all(&state_directory).unwrap();
        let checkpoint = rusqlite::Connection::open(state_directory.join("state.db")).unwrap();
        checkpoint.execute_batch("CREATE TABLE downloads (gid TEXT PRIMARY KEY, uri TEXT, path TEXT, total_length INTEGER, completed_length INTEGER, completed_ranges TEXT);").unwrap();
        checkpoint
            .execute(
                "INSERT INTO downloads VALUES (?1, ?2, ?3, 128, 7, '0-7')",
                rusqlite::params![
                    "0123456789abcdef",
                    "https://example.test/native.bin",
                    payload.to_string_lossy()
                ],
            )
            .unwrap();
        assert_eq!(
            retry_duplicate_state(&store, &failed).unwrap(),
            Some(DuplicateDownloadState::ResumeAvailable)
        );
        for invalid_ranges in ["0-128", "0-7,5-8", "0-129", "broken"] {
            checkpoint
                .execute("UPDATE downloads SET completed_ranges=?1", [invalid_ranges])
                .unwrap();
            assert_eq!(
                retry_duplicate_state(&store, &failed).unwrap(),
                Some(DuplicateDownloadState::OverwriteRequired)
            );
        }
    }

    #[test]
    fn failed_preallocated_file_without_aria2_control_file_requires_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let failed = task_with_status(
            &store,
            "https://example.test/preallocated.bin",
            TaskStatus::Failed,
        );
        let failed = store.bind_gid(failed.id, "failed-gid").unwrap();
        let payload = root.path().join("preallocated.bin");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        store
            .upsert_files(
                failed.id,
                &[(0, payload.to_string_lossy().into_owned(), 128, 7, true)],
            )
            .unwrap();

        assert_eq!(
            duplicate_state(&store, &failed).unwrap(),
            DuplicateDownloadState::OverwriteRequired,
            "a preallocated payload alone cannot prove aria2 knows which bytes are complete"
        );
    }

    #[test]
    fn failed_zero_progress_with_an_aria2_control_file_requires_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let failed = task_with_status(
            &store,
            "https://example.test/zero-progress.bin",
            TaskStatus::Failed,
        );
        let failed = store.bind_gid(failed.id, "failed-gid").unwrap();
        let payload = root.path().join("zero-progress.bin");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        std::fs::write(format!("{}.aria2", payload.display()), b"control").unwrap();
        store
            .upsert_files(
                failed.id,
                &[(0, payload.to_string_lossy().into_owned(), 128, 0, true)],
            )
            .unwrap();

        assert_eq!(
            duplicate_state(&store, &failed).unwrap(),
            DuplicateDownloadState::OverwriteRequired,
            "a control file cannot make an unstarted preallocated payload resumable"
        );
    }

    #[test]
    fn paused_zero_progress_with_an_aria2_control_file_requires_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let paused = task_with_status(
            &store,
            "https://example.test/paused-zero-progress.bin",
            TaskStatus::Paused,
        );
        let paused = store
            .apply_transfer_snapshot(DownloadTask {
                total_length: Some(128),
                completed_length: 0,
                ..paused
            })
            .unwrap();
        let paused = store.bind_gid(paused.id, "paused-zero-gid").unwrap();
        let payload = root.path().join("paused-zero-progress.bin");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        std::fs::write(format!("{}.aria2", payload.display()), b"control").unwrap();
        store
            .upsert_files(
                paused.id,
                &[(0, payload.to_string_lossy().into_owned(), 128, 0, true)],
            )
            .unwrap();

        assert_eq!(
            duplicate_state(&store, &paused).unwrap(),
            DuplicateDownloadState::OverwriteRequired,
            "a paused task still needs positive progress before it is resumable"
        );
    }

    #[test]
    fn paused_fully_progressed_with_an_aria2_control_file_requires_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let paused = task_with_status(
            &store,
            "https://example.test/paused-fully-progressed.bin",
            TaskStatus::Paused,
        );
        let paused = store
            .apply_transfer_snapshot(DownloadTask {
                total_length: Some(128),
                completed_length: 128,
                ..paused
            })
            .unwrap();
        let paused = store.bind_gid(paused.id, "paused-complete-gid").unwrap();
        let payload = root.path().join("paused-fully-progressed.bin");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        std::fs::write(format!("{}.aria2", payload.display()), b"control").unwrap();
        store
            .upsert_files(
                paused.id,
                &[(0, payload.to_string_lossy().into_owned(), 128, 128, true)],
            )
            .unwrap();

        assert_eq!(
            duplicate_state(&store, &paused).unwrap(),
            DuplicateDownloadState::OverwriteRequired,
            "a paused task at its known total length is not an incomplete resume"
        );
    }
}
