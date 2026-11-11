use rusqlite::{Connection, OpenFlags};
use std::path::Path;

const ERROR: &str = "video.error.checkpoint";

/// Next can fall back to a partial file's length when its range record is absent.
/// A multi-connection file may already have its full length, so require the real
/// checkpoint before handing an existing HTTP track back to the downloader.
pub(super) fn validate(folder: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(folder).map_err(|_| ERROR)? {
        let entry = entry.map_err(|_| ERROR)?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("payload.") || !name.ends_with(".part") {
            continue;
        }
        let metadata = entry.metadata().map_err(|_| ERROR)?;
        if !metadata.is_file() || metadata.len() == 0 {
            continue;
        }
        // HLS/DASH keep yt-dlp's native fragment downloader. Its checkpoint is
        // named after the final track, including for unfinished -FragN files.
        let track = name
            .split_once(".part-Frag")
            .map_or(name, |(track, _)| track);
        let track = track.strip_suffix(".part").unwrap_or(track);
        if folder.join(format!("{track}.ytdl")).is_file() {
            continue;
        }
        if checkpoint(folder, &path, metadata.len()).is_none() {
            return Err(ERROR.into());
        }
    }
    Ok(())
}

/// A pause reply precedes the stream worker's final checkpoint write. Confirm
/// that its saved ranges cover the paused RPC snapshot before stopping it.
pub(super) fn saved(folder: &Path, status: &crate::aria2::Aria2Status) -> bool {
    let [file] = status.files.as_slice() else {
        return false;
    };
    let Ok(expected) = status.completed_length.parse::<u64>() else {
        return false;
    };
    let payload = folder.join(&file.path);
    let length = match payload.metadata() {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return expected == 0,
        _ => return false,
    };
    if expected == 0 && length == 0 {
        return true;
    }
    checkpoint(folder, &payload, length)
        .is_some_and(|(gid, completed)| gid == status.gid && completed >= expected)
}

fn checkpoint(folder: &Path, payload: &Path, file_length: u64) -> Option<(String, u64)> {
    let payload = payload.canonicalize().ok()?;
    let connection = Connection::open_with_flags(
        folder.join("aria2-state/stream/state.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let mut statement = connection
        .prepare(
            "SELECT path,uri,total_length,completed_length,completed_ranges,gid FROM downloads",
        )
        .ok()?;
    let mut rows = statement.query([]).ok()?;
    let mut found = None;
    while let Some(row) = rows.next().ok()? {
        let path: String = row.get(0).ok()?;
        if folder.join(path).canonicalize().ok().as_ref() != Some(&payload) {
            continue;
        }
        let uri: String = row.get(1).ok()?;
        let uri = url::Url::parse(&uri).ok()?;
        let total: u64 = row.get(2).ok()?;
        let completed: u64 = row.get(3).ok()?;
        let ranges: String = row.get(4).ok()?;
        let gid: String = row.get(5).ok()?;
        if found.is_some()
            || !matches!(uri.scheme(), "http" | "https")
            || uri.host_str().is_none()
            || total == 0
            || completed == 0
            || completed > total
            || file_length > total
            || !valid_ranges(&ranges, completed, file_length)
        {
            return None;
        }
        found = Some((gid, completed));
    }
    found
}

fn valid_ranges(ranges: &str, completed: u64, file_length: u64) -> bool {
    if ranges.is_empty() || ranges.len() > 1_048_576 {
        return false;
    }
    let mut previous_end = 0;
    let mut counted = 0u64;
    for range in ranges.split(',') {
        let Some((start, end)) = range.split_once('-') else {
            return false;
        };
        let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
            return false;
        };
        if start < previous_end || start >= end || end > file_length {
            return false;
        }
        let Some(sum) = counted.checked_add(end - start) else {
            return false;
        };
        counted = sum;
        previous_end = end;
    }
    counted == completed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(folder: &Path, payload: &Path, completed: u64, ranges: &str) -> Connection {
        let directory = folder.join("aria2-state/stream");
        std::fs::create_dir_all(&directory).unwrap();
        let connection = Connection::open(directory.join("state.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE downloads(path TEXT,uri TEXT,total_length INTEGER,completed_length INTEGER,completed_ranges TEXT,gid TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO downloads VALUES (?1,'https://old.example/video?token=expired',128,?2,?3,'0123456789abcdef')",
                rusqlite::params![payload.to_string_lossy(), completed, ranges],
            )
            .unwrap();
        connection
    }

    #[test]
    fn preallocated_partial_without_checkpoint_is_kept_and_rejected() {
        let folder = tempfile::tempdir().unwrap();
        let payload = folder.path().join("payload.f401.mp4.part");
        std::fs::write(&payload, b"downloaded bytes").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        let before = std::fs::read(&payload).unwrap();
        assert_eq!(validate(folder.path()).unwrap_err(), ERROR);
        assert_eq!(std::fs::read(&payload).unwrap(), before);
        assert!(!folder.path().join("aria2-state").exists());
    }

    #[test]
    fn sparse_ranges_match_by_output_and_must_describe_existing_bytes() {
        let folder = tempfile::tempdir().unwrap();
        let payload = folder.path().join("payload.f401.mp4.part");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        let connection = checkpoint(folder.path(), &payload, 24, "0-8,64-80");
        // Expiring source URLs do not determine the identity of the saved track.
        assert!(validate(folder.path()).is_ok());

        // A successful pause RPC alone does not guarantee that Next has flushed
        // its in-flight ranges; preallocation must not stand in for that record.
        connection
            .execute(
                "UPDATE downloads SET completed_length=0, completed_ranges=''",
                [],
            )
            .unwrap();
        assert_eq!(validate(folder.path()).unwrap_err(), ERROR);
        connection
            .execute(
                "UPDATE downloads SET completed_length=128, completed_ranges='0-8,64-80'",
                [],
            )
            .unwrap();
        assert_eq!(validate(folder.path()).unwrap_err(), ERROR);
        connection
            .execute(
                "UPDATE downloads SET completed_length=24, completed_ranges='0-8,4-20'",
                [],
            )
            .unwrap();
        assert_eq!(validate(folder.path()).unwrap_err(), ERROR);
        connection
            .execute(
                "UPDATE downloads SET completed_length=128, completed_ranges='0-128'",
                [],
            )
            .unwrap();
        assert!(validate(folder.path()).is_ok());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&payload)
            .unwrap()
            .set_len(64)
            .unwrap();
        assert_eq!(validate(folder.path()).unwrap_err(), ERROR);
    }

    #[test]
    fn another_tracks_checkpoint_does_not_validate_partial_data() {
        let folder = tempfile::tempdir().unwrap();
        let payload = folder.path().join("payload.f401.mp4.part");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        let other = folder.path().join("payload.f140.m4a");
        std::fs::write(&other, b"complete").unwrap();
        let _connection = checkpoint(folder.path(), &other, 8, "0-8");
        assert_eq!(validate(folder.path()).unwrap_err(), ERROR);
    }

    #[test]
    fn native_fragments_and_empty_tracks_do_not_require_aria2_state() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("payload.mp4.part"), b"fragment").unwrap();
        std::fs::write(
            folder.path().join("payload.mp4.ytdl"),
            br#"{"downloader":{"current_fragment":{"index":1}}}"#,
        )
        .unwrap();
        std::fs::write(
            folder.path().join("payload.mp4.part-Frag2.part"),
            b"next fragment",
        )
        .unwrap();
        std::fs::File::create(folder.path().join("payload.f140.m4a.part")).unwrap();
        assert!(validate(folder.path()).is_ok());
        assert!(!folder.path().join("aria2-state").exists());
    }

    fn paused_status(completed: u64) -> crate::aria2::Aria2Status {
        serde_json::from_value(serde_json::json!({
            "gid":"0123456789abcdef", "status":"paused", "totalLength":"128",
            "completedLength":completed.to_string(), "downloadSpeed":"0",
            "uploadSpeed":"0", "connections":"0",
            "files":[{"index":"1", "path":"././payload.mp4.part", "length":"128",
                "completedLength":completed.to_string(), "selected":"true"}]
        }))
        .unwrap()
    }

    #[test]
    fn saved_ranges_must_cover_the_paused_snapshot_before_process_exit() {
        let folder = tempfile::tempdir().unwrap();
        let payload = folder.path().join("payload.mp4.part");
        std::fs::File::create(&payload)
            .unwrap()
            .set_len(128)
            .unwrap();
        let connection = checkpoint(folder.path(), Path::new("././payload.mp4.part"), 8, "0-8");
        let mut status = paused_status(24);
        assert!(!saved(folder.path(), &status));
        connection
            .execute(
                "UPDATE downloads SET completed_length=24, completed_ranges='0-8,64-80'",
                [],
            )
            .unwrap();
        assert!(saved(folder.path(), &status));
        status.gid = "fedcba9876543210".into();
        assert!(!saved(folder.path(), &status));
    }

    #[test]
    fn zero_progress_is_safe_without_state_only_when_no_data_file_exists() {
        let folder = tempfile::tempdir().unwrap();
        let status = paused_status(0);
        assert!(saved(folder.path(), &status));
        let payload = folder.path().join("payload.mp4.part");
        let file = std::fs::File::create(&payload).unwrap();
        assert!(saved(folder.path(), &status));
        file.set_len(128).unwrap();
        assert!(!saved(folder.path(), &status));
    }
}
