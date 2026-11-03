//! Read-only adapter for the bundled Next engine's persisted HTTP ranges.
//! A preallocated payload is never evidence of downloaded data.
use crate::{DownloadTask, TaskStore};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

pub(crate) struct NativeCheckpoint {
    pub uri: String,
    pub path: std::path::PathBuf,
}

pub(crate) fn has_legacy_control(store: &TaskStore, task: &DownloadTask) -> bool {
    store.files(task.id).is_ok_and(|files| {
        files.iter().any(|file| {
            let path = Path::new(&file.path);
            let mut control = path.as_os_str().to_os_string();
            control.push(".aria2");
            path.is_file() && Path::new(&control).is_file()
        })
    })
}

/// A connection can fail before aria2 creates a payload or any byte ranges.
/// Its GID alone is not a checkpoint. Permit a new submission only when both
/// the durable task and its reserved output confirm that no data can be lost.
pub(crate) fn can_restart_unwritten(store: &TaskStore, task: &DownloadTask, gid: &str) -> bool {
    unwritten_download(store, task, gid).unwrap_or(false)
}

fn unwritten_download(store: &TaskStore, task: &DownloadTask, gid: &str) -> Option<bool> {
    if task.completed_length != 0
        || gid.len() != 16
        || !gid.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Some(false);
    }
    let files = store.files(task.id).ok()?;
    if files.len() > 1 || files.iter().any(|file| file.completed_length != 0) {
        return Some(false);
    }
    let mut paths = files
        .iter()
        .map(|file| std::path::PathBuf::from(&file.path))
        .collect::<Vec<_>>();
    paths.extend(store.reserved_output_path(task.id).ok()?);
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Some(false);
    }
    for path in &paths {
        if !path.is_absolute() {
            return Some(false);
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Some(false),
        }
        for suffix in [".aria2", ".xltd", ".xltd.cfg"] {
            let mut control = path.as_os_str().to_os_string();
            control.push(suffix);
            match std::fs::symlink_metadata(control) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Some(false),
            }
        }
    }
    let database = store.native_stream_state_path()?;
    match std::fs::symlink_metadata(&database) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(true),
        Ok(metadata) if metadata.is_file() => {}
        _ => return Some(false),
    }
    let connection =
        Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    // Also inspect the output identity: a stale GID must not hide ranges that
    // the engine has already transferred to another GID.
    let mut statement = connection
        .prepare("SELECT gid,uri,path,total_length,completed_length,completed_ranges FROM downloads WHERE gid=?1 OR path=?2 OR path=?3")
        .ok()?;
    let mut rows = statement
        .query(rusqlite::params![
            gid,
            paths[0].to_string_lossy(),
            paths.last()?.to_string_lossy(),
        ])
        .ok()?;
    while let Some(row) = rows.next().ok()? {
        let (record_gid, uri, path, total, completed, ranges): (
            String,
            String,
            String,
            u64,
            u64,
            String,
        ) = (
            row.get(0).ok()?,
            row.get(1).ok()?,
            row.get(2).ok()?,
            row.get(3).ok()?,
            row.get(4).ok()?,
            row.get(5).ok()?,
        );
        let uri = url::Url::parse(&uri).ok()?;
        if record_gid != gid
            || !matches!(uri.scheme(), "http" | "https")
            || uri.host_str().is_none()
            || !paths.iter().any(|output| output == Path::new(&path))
            || (total != 0 && task.total_length.is_some_and(|known| known != total))
            || completed != 0
            || !ranges.is_empty()
        {
            return Some(false);
        }
    }
    Some(true)
}

pub(crate) fn checkpoint(
    store: &TaskStore,
    task: &DownloadTask,
    gid: &str,
) -> Option<NativeCheckpoint> {
    if gid.len() != 16 || !gid.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (payload, file_length) = task_payload(store, task)?;
    let connection = Connection::open_with_flags(
        store.native_stream_state_path()?,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let (uri, path, total, completed, ranges): (String, String, u64, u64, String) = connection.query_row(
        "SELECT uri,path,total_length,completed_length,completed_ranges FROM downloads WHERE gid=?1",
        [gid], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).ok()?;
    checkpoint_from_record(
        task,
        &payload,
        file_length,
        uri,
        path,
        total,
        completed,
        ranges,
    )
}

/// Find one trusted native checkpoint when an interrupted engine recovery has
/// already detached the original GID from the durable task.  The output path,
/// declared length, exact completed ranges, and URL are all validated before
/// the checkpoint may be reused.  Ambiguous matches fail closed.
pub(crate) fn checkpoint_for_task(
    store: &TaskStore,
    task: &DownloadTask,
) -> Option<NativeCheckpoint> {
    let (payload, file_length) = task_payload(store, task)?;
    let connection = Connection::open_with_flags(
        store.native_stream_state_path()?,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let mut statement = connection
        .prepare(
            "SELECT gid,uri,path,total_length,completed_length,completed_ranges FROM downloads",
        )
        .ok()?;
    let mut rows = statement.query([]).ok()?;
    let mut found = None;
    while let Some(row) = rows.next().ok()? {
        let (gid, uri, path, total, completed, ranges): (String, String, String, u64, u64, String) = (
            row.get(0).ok()?,
            row.get(1).ok()?,
            row.get(2).ok()?,
            row.get(3).ok()?,
            row.get(4).ok()?,
            row.get(5).ok()?,
        );
        if gid.len() != 16 || !gid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let Some(checkpoint) = checkpoint_from_record(
            task,
            &payload,
            file_length,
            uri,
            path,
            total,
            completed,
            ranges,
        ) else {
            continue;
        };
        if found.is_some() {
            return None;
        }
        found = Some(checkpoint);
    }
    found
}

/// Moves a fully validated, stopped checkpoint to a replacement local URI.
/// The payload path and completed ranges stay owned by aria2-next; only the
/// identity it uses to find that record is changed before a replacement GID is
/// submitted.
pub(crate) fn rebind_verified_checkpoint_uri(
    store: &TaskStore,
    task: &DownloadTask,
    gid: &str,
    replacement_uri: &str,
) -> Option<String> {
    let checkpoint = checkpoint(store, task, gid)?;
    let parsed = url::Url::parse(replacement_uri).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    if checkpoint.uri == replacement_uri {
        return Some(checkpoint.uri);
    }
    let connection = Connection::open_with_flags(
        store.native_stream_state_path()?,
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .ok()?;
    (connection
        .execute(
            "UPDATE downloads SET uri=?1 WHERE gid=?2 AND uri=?3 AND path=?4",
            rusqlite::params![
                replacement_uri,
                gid,
                checkpoint.uri,
                checkpoint.path.to_string_lossy(),
            ],
        )
        .ok()?
        == 1)
        .then_some(checkpoint.uri)
}

fn task_payload(store: &TaskStore, task: &DownloadTask) -> Option<(std::path::PathBuf, u64)> {
    let files = store.files(task.id).ok()?;
    if files.len() != 1 {
        return None;
    }
    let payload = std::path::PathBuf::from(&files[0].path);
    payload
        .metadata()
        .ok()?
        .is_file()
        .then_some((payload, files[0].length))
}

fn checkpoint_from_record(
    task: &DownloadTask,
    payload: &Path,
    file_length: u64,
    uri: String,
    path: String,
    total: u64,
    completed: u64,
    ranges: String,
) -> Option<NativeCheckpoint> {
    let metadata = payload.metadata().ok()?;
    let parsed = url::Url::parse(&uri).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || Path::new(&path).canonicalize().ok()? != payload.canonicalize().ok()?
        || total == 0
        || completed == 0
        || completed >= total
        || task.total_length.is_some_and(|known| known != total)
        || (file_length != 0 && file_length != total)
        || ranges.is_empty()
        || ranges.len() > 1_048_576
    {
        return None;
    }
    let mut previous_end = 0;
    let mut counted = 0u64;
    for range in ranges.split(',') {
        let (start, end) = range.split_once('-')?;
        let start = start.parse::<u64>().ok()?;
        let end = end.parse::<u64>().ok()?;
        if start < previous_end || start >= end || end > total || end > metadata.len() {
            return None;
        }
        counted = counted.checked_add(end - start)?;
        previous_end = end;
    }
    (counted == completed).then_some(NativeCheckpoint {
        uri,
        path: path.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewTask;

    #[test]
    fn finds_a_native_checkpoint_for_a_detached_task_by_its_output_path() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::open_path(&root.path().join("tasks.sqlite")).unwrap();
        let task = store
            .create(NewTask::direct("https://fresh.example.test/archive.zip"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                total_length: Some(128),
                completed_length: 7,
                ..task
            })
            .unwrap();
        let payload = root.path().join("archive.zip");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 128, 7, true)],
            )
            .unwrap();
        let state_directory = root.path().join("aria2-next-state/stream");
        std::fs::create_dir_all(&state_directory).unwrap();
        let checkpoint_db = rusqlite::Connection::open(state_directory.join("state.db")).unwrap();
        checkpoint_db
            .execute_batch("CREATE TABLE downloads (gid TEXT PRIMARY KEY, uri TEXT, path TEXT, total_length INTEGER, completed_length INTEGER, completed_ranges TEXT);")
            .unwrap();
        checkpoint_db
            .execute(
                "INSERT INTO downloads VALUES (?1, ?2, ?3, 128, 7, '0-7')",
                rusqlite::params![
                    "0123456789abcdef",
                    "https://previous.example.test/archive.zip",
                    payload.to_string_lossy(),
                ],
            )
            .unwrap();

        let recovered = checkpoint_for_task(&store, &task).expect("checkpoint should be found");

        assert_eq!(recovered.path, payload);
        assert_eq!(recovered.uri, "https://previous.example.test/archive.zip");
    }

    #[test]
    fn rebinds_a_verified_checkpoint_to_a_stable_gateway_uri() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::open_path(&root.path().join("tasks.sqlite")).unwrap();
        let task = store
            .create(NewTask::direct("https://fresh.example.test/archive.zip"))
            .unwrap();
        let task = store
            .apply_transfer_snapshot(DownloadTask {
                total_length: Some(128),
                completed_length: 7,
                ..task
            })
            .unwrap();
        let payload = root.path().join("archive.zip");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        store
            .upsert_files(
                task.id,
                &[(0, payload.to_string_lossy().into_owned(), 128, 7, true)],
            )
            .unwrap();
        let state_directory = root.path().join("aria2-next-state/stream");
        std::fs::create_dir_all(&state_directory).unwrap();
        let checkpoint_db = rusqlite::Connection::open(state_directory.join("state.db")).unwrap();
        checkpoint_db
            .execute_batch("CREATE TABLE downloads (gid TEXT PRIMARY KEY, uri TEXT, path TEXT, total_length INTEGER, completed_length INTEGER, completed_ranges TEXT);")
            .unwrap();
        checkpoint_db
            .execute(
                "INSERT INTO downloads VALUES (?1, ?2, ?3, 128, 7, '0-7')",
                rusqlite::params![
                    "0123456789abcdef",
                    "https://previous.example.test/archive.zip",
                    payload.to_string_lossy(),
                ],
            )
            .unwrap();

        let old = rebind_verified_checkpoint_uri(
            &store,
            &task,
            "0123456789abcdef",
            "http://127.0.0.1:49152/stable-token",
        )
        .expect("verified checkpoint should be rebound");

        assert_eq!(old, "https://previous.example.test/archive.zip");
        assert_eq!(
            checkpoint(&store, &task, "0123456789abcdef")
                .expect("rebound checkpoint should stay valid")
                .uri,
            "http://127.0.0.1:49152/stable-token"
        );
    }
}
