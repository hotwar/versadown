//! macOS uses one Keychain key, with authenticated credential rows on disk.
//! No legacy cloud entry is read: users authorize their providers afresh.
use super::*;
use chacha20poly1305::aead::Payload;
use rusqlite::{params, Connection, OptionalExtension};
use zeroize::Zeroizing;

use crate::cloud::credential_recovery::{
    is_corruption, quarantine_database, undecryptable_names, MasterKeyCache,
};
pub(super) static MASTER_KEY: MasterKeyCache = MasterKeyCache::new();

fn open_once(root: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open(root.join("credentials-v2.sqlite"))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("PRAGMA secure_delete=ON; CREATE TABLE IF NOT EXISTS credentials (name TEXT PRIMARY KEY, payload BLOB NOT NULL);")?;
    Ok(db)
}
// A damaged file is backed up and rebuilt; locked/busy files are left alone.
fn open(root: &Path) -> Result<Connection, QuarkError> {
    match open_once(root) {
        Ok(db) => Ok(db),
        Err(error) if is_corruption(&error) => {
            quarantine_database(&root.join("credentials-v2.sqlite"))?;
            open_once(root).map_err(|_| QuarkError::Storage)
        }
        Err(_) => Err(QuarkError::Storage),
    }
}
// After a lost Keychain key is replaced, rows the new key cannot
// authenticate are unrecoverable; removing them restores the store.
pub(super) fn purge_undecryptable(
    root: &Path,
    service: &str,
    key: [u8; 32],
) -> Result<(), QuarkError> {
    let db = open(root)?;
    let mut statement = db
        .prepare("SELECT name, payload FROM credentials")
        .map_err(|_| QuarkError::Storage)?;
    let rows = statement
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        .map_err(|_| QuarkError::Storage)?;
    drop(statement);
    for name in undecryptable_names(&rows, |name, payload| {
        decrypt(service, name, payload, key).is_ok()
    }) {
        db.execute("DELETE FROM credentials WHERE name=?1", [name])
            .map_err(|_| QuarkError::Storage)?;
    }
    Ok(())
}
pub(super) fn payload(root: &Path, name: &str) -> Result<Option<Vec<u8>>, QuarkError> {
    open(root)?
        .query_row(
            "SELECT payload FROM credentials WHERE name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| QuarkError::Storage)
}
pub(super) fn save(
    root: &Path,
    service: &str,
    name: &str,
    value: &[u8],
    key: [u8; 32],
) -> Result<(), QuarkError> {
    let key = Zeroizing::new(key);
    let cipher =
        XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| QuarkError::Storage)?;
    let mut nonce = XNonce::default();
    OsRng.fill_bytes(&mut nonce);
    let aad = format!("{service}:{name}");
    let encrypted = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: value,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| QuarkError::Storage)?;
    let mut payload = nonce.to_vec();
    payload.extend_from_slice(&encrypted);
    open(root)?.execute("INSERT INTO credentials(name,payload) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET payload=excluded.payload", params![name, payload]).map_err(|_| QuarkError::Storage)?;
    Ok(())
}
pub(super) fn decrypt(
    service: &str,
    name: &str,
    payload: &[u8],
    key: [u8; 32],
) -> Result<Vec<u8>, QuarkError> {
    let key = Zeroizing::new(key);
    if payload.len() < 24 + 16 {
        return Err(QuarkError::Storage);
    }
    let cipher =
        XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| QuarkError::Storage)?;
    let aad = format!("{service}:{name}");
    cipher
        .decrypt(
            XNonce::from_slice(&payload[..24]),
            Payload {
                msg: &payload[24..],
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| QuarkError::Storage)
}
pub(super) fn remove(root: &Path, name: &str) -> Result<(), QuarkError> {
    open(root)?
        .execute("DELETE FROM credentials WHERE name=?1", [name])
        .map_err(|_| QuarkError::Storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    #[test]
    fn providers_and_non_cloud_secrets_reopen_without_legacy_entries() {
        let root = tempfile::tempdir().unwrap();
        let mut store = SystemCredentialStore::new(root.path().into()).unwrap();
        store.test_key = Some([9; 32]);
        let names = [
            "quark",
            "baidu",
            "xunlei",
            "uc",
            "pikpak",
            "tianyi",
            "aliyun",
            "ai-api-key",
            "privacy-download-password-v1",
        ];
        for name in names {
            assert!(store.load(name).unwrap().is_none());
            store.save(name, name.as_bytes()).unwrap();
        }
        drop(store);
        let mut reopened = SystemCredentialStore::new(root.path().into()).unwrap();
        reopened.test_key = Some([9; 32]);
        for name in names {
            assert_eq!(reopened.load(name).unwrap().unwrap(), name.as_bytes());
        }
        reopened.remove("baidu").unwrap();
        assert!(reopened.load("baidu").unwrap().is_none());
        assert!(reopened.load("xunlei").unwrap().is_some());
        assert!(reopened.save("unknown", b"secret").is_err());
        assert!(reopened.load("unknown").is_err());
    }
    #[test]
    fn concurrent_consumers_unlock_once_and_failures_are_retried() {
        for succeeds in [true, false] {
            let cache = Arc::new(MasterKeyCache::default());
            let calls = Arc::new(AtomicUsize::new(0));
            std::thread::scope(|scope| {
                for _ in 0..24 {
                    let cache = cache.clone();
                    let calls = calls.clone();
                    scope.spawn(move || {
                        let read = || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            if succeeds {
                                Ok([7; 32])
                            } else {
                                Err(QuarkError::Storage)
                            }
                        };
                        assert_eq!(cache.get(read).is_ok(), succeeds);
                    });
                }
            });
            assert_eq!(calls.load(Ordering::SeqCst), if succeeds { 1 } else { 24 });
        }
    }
    #[test]
    fn replacing_a_lost_key_purges_only_undecryptable_rows() {
        let root = tempfile::tempdir().unwrap();
        save(root.path(), "cloud", "quark", b"old", [1; 32]).unwrap();
        save(root.path(), "cloud", "baidu", b"new", [2; 32]).unwrap();
        purge_undecryptable(root.path(), "cloud", [2; 32]).unwrap();
        assert!(payload(root.path(), "quark").unwrap().is_none());
        assert!(payload(root.path(), "baidu").unwrap().is_some());
    }
    #[test]
    fn corrupt_credential_file_is_backed_up_and_rebuilt() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("credentials-v2.sqlite"), vec![b'z'; 4096]).unwrap();
        save(root.path(), "cloud", "quark", b"v", [3; 32]).unwrap();
        assert!(payload(root.path(), "quark").unwrap().is_some());
        assert!(root.path().join("credentials-v2.sqlite.corrupt-0").exists());
    }
    #[test]
    fn encrypted_rows_reopen_bind_to_namespace_and_detect_tampering() {
        let root = tempfile::tempdir().unwrap();
        assert!(payload(root.path(), "quark").unwrap().is_none());
        save(root.path(), "cloud", "quark", b"secret-cookie", [7; 32]).unwrap();
        let bytes = payload(root.path(), "quark").unwrap().unwrap();
        assert!(!bytes.windows(13).any(|v| v == b"secret-cookie"));
        assert_eq!(
            decrypt("cloud", "quark", &bytes, [7; 32]).unwrap(),
            b"secret-cookie"
        );
        assert!(decrypt("ai", "quark", &bytes, [7; 32]).is_err());
        assert!(decrypt("cloud", "baidu", &bytes, [7; 32]).is_err());
        assert!(decrypt("cloud", "quark", &bytes, [8; 32]).is_err());
        let mut corrupt = bytes.clone();
        corrupt[24] ^= 1;
        assert!(decrypt("cloud", "quark", &corrupt, [7; 32]).is_err());
        assert_eq!(payload(root.path(), "quark").unwrap().unwrap(), bytes);
        remove(root.path(), "quark").unwrap();
        assert!(payload(root.path(), "quark").unwrap().is_none());
    }
}
