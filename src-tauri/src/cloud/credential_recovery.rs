//! Platform-independent recovery policy for the encrypted credential store:
//! master-key caching, undecryptable-row selection and corrupt-file backup.
#![cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
use super::QuarkError;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};
use zeroize::Zeroizing;

/// Caches only a successfully read key. Denials, missing entries and other
/// failures are retried on the next call, and a `create=false` probe can never
/// poison a later `create=true` request.
#[derive(Default)]
pub(super) struct MasterKeyCache(Mutex<Option<Zeroizing<[u8; 32]>>>);
impl MasterKeyCache {
    pub(super) const fn new() -> Self {
        Self(Mutex::new(None))
    }
    pub(super) fn get(
        &self,
        read: impl FnOnce() -> Result<[u8; 32], QuarkError>,
    ) -> Result<[u8; 32], QuarkError> {
        let mut state = self.0.lock().map_err(|_| QuarkError::Storage)?;
        if let Some(key) = state.as_ref() {
            return Ok(**key);
        }
        let key = Zeroizing::new(read()?);
        *state = Some(Zeroizing::new(*key));
        Ok(*key)
    }
}

/// Names of rows that the current key cannot authenticate. These are the
/// only rows removed when a lost key is replaced.
pub(super) fn undecryptable_names(
    rows: &[(String, Vec<u8>)],
    decrypts: impl Fn(&str, &[u8]) -> bool,
) -> Vec<String> {
    rows.iter()
        .filter(|(name, payload)| !decrypts(name, payload))
        .map(|(name, _)| name.clone())
        .collect()
}

/// True for errors that mean the file itself is damaged, as opposed to
/// transient ones (locked, busy, permissions) which must never trigger a
/// backup-and-rebuild.
pub(super) fn is_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            )
    )
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Renames a damaged database (and its WAL/SHM side files) to a unique
/// `.corrupt-N` backup so a fresh file can be created in its place.
pub(super) fn quarantine_database(path: &Path) -> Result<PathBuf, QuarkError> {
    let backup = (0..1000)
        .map(|n| suffixed(path, &format!(".corrupt-{n}")))
        .find(|candidate| !candidate.exists())
        .ok_or(QuarkError::Storage)?;
    std::fs::rename(path, &backup).map_err(|_| QuarkError::Storage)?;
    for side in ["-wal", "-shm"] {
        let from = suffixed(path, side);
        if from.exists() {
            std::fs::rename(&from, suffixed(&backup, side)).map_err(|_| QuarkError::Storage)?;
        }
    }
    Ok(backup)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn failures_are_not_cached_and_success_is_cached() {
        let cache = MasterKeyCache::default();
        assert!(cache.get(|| Err(QuarkError::Storage)).is_err());
        assert_eq!(cache.get(|| Ok([1; 32])).unwrap(), [1; 32]);
        let calls = AtomicUsize::new(0);
        let again = cache.get(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok([2; 32])
        });
        assert_eq!(again.unwrap(), [1; 32]);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn earlier_probe_failure_does_not_block_a_later_create() {
        let cache = MasterKeyCache::default();
        // create=false probe sees NoEntry.
        assert!(cache.get(|| Err(QuarkError::Storage)).is_err());
        // create=true then reads/creates the key.
        assert_eq!(cache.get(|| Ok([5; 32])).unwrap(), [5; 32]);
    }

    #[test]
    fn selects_only_rows_the_key_cannot_decrypt() {
        let rows = vec![
            ("quark".to_string(), vec![1]),
            ("baidu".to_string(), vec![2]),
        ];
        let bad = undecryptable_names(&rows, |name, _| name == "quark");
        assert_eq!(bad, vec!["baidu".to_string()]);
    }

    #[test]
    fn corrupt_database_is_backed_up_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials-v2.sqlite");
        std::fs::write(&path, b"not a sqlite file at all, garbage bytes").unwrap();
        let backup = quarantine_database(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            b"not a sqlite file at all, garbage bytes"
        );
        std::fs::write(&path, b"x").unwrap();
        let second = quarantine_database(&path).unwrap();
        assert_ne!(backup, second);
        assert!(backup.exists() && second.exists());
    }

    #[test]
    fn classifies_corruption_but_not_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.sqlite");
        std::fs::write(&path, vec![b'z'; 4096]).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        let error = db
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_err();
        assert!(is_corruption(&error));
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert!(!is_corruption(&busy));
    }
}
