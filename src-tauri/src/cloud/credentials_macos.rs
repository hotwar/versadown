//! macOS uses one Keychain key, with authenticated credential rows on disk.
//! No legacy cloud entry is read: users authorize their providers afresh.
use super::*;
use chacha20poly1305::aead::Payload;
use rusqlite::{params, Connection, OptionalExtension};
use zeroize::Zeroizing;

#[derive(Default)]
pub(super) struct MasterKeyCache(Mutex<Option<Result<Zeroizing<[u8; 32]>, ()>>>);
impl MasterKeyCache {
    pub(super) fn get(
        &self,
        read: impl FnOnce() -> Result<[u8; 32], QuarkError>,
    ) -> Result<[u8; 32], QuarkError> {
        let mut state = self.0.lock().map_err(|_| QuarkError::Storage)?;
        if state.is_none() {
            *state = Some(read().map(Zeroizing::new).map_err(|_| ()));
        }
        match state.as_ref().unwrap() {
            Ok(key) => Ok(**key),
            Err(()) => Err(QuarkError::Storage),
        }
    }
}
pub(super) static MASTER_KEY: MasterKeyCache = MasterKeyCache(Mutex::new(None));

// If encrypted data already exists, a missing Keychain key is data loss,
// never permission to generate a replacement and overwrite credentials.
pub(super) static KEY_REQUIRED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(super) fn register_existing_data(root: &Path) -> Result<(), QuarkError> {
    for (file, table) in [
        ("credentials-v2.sqlite", "credentials"),
        ("cloud-share-plans.sqlite", "share_plans"),
    ] {
        let path = root.join(file);
        if !path.exists() {
            continue;
        }
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| QuarkError::Storage)?;
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |r| r.get(0),
            )
            .map_err(|_| QuarkError::Storage)?;
        if exists {
            let has_rows: bool = db
                .query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
                    r.get(0)
                })
                .map_err(|_| QuarkError::Storage)?;
            if has_rows {
                KEY_REQUIRED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
    Ok(())
}

fn open(root: &Path) -> Result<Connection, QuarkError> {
    let db =
        Connection::open(root.join("credentials-v2.sqlite")).map_err(|_| QuarkError::Storage)?;
    db.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| QuarkError::Storage)?;
    db.execute_batch("PRAGMA secure_delete=ON; CREATE TABLE IF NOT EXISTS credentials (name TEXT PRIMARY KEY, payload BLOB NOT NULL);").map_err(|_| QuarkError::Storage)?;
    Ok(db)
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
    fn concurrent_consumers_unlock_once_and_denial_is_not_retried() {
        for succeeds in [true, false] {
            let cache = Arc::new(MasterKeyCache::default());
            let calls = Arc::new(AtomicUsize::new(0));
            std::thread::scope(|scope| {
                for _ in 0..24 {
                    let cache = cache.clone();
                    let calls = calls.clone();
                    scope.spawn(move || {
                        assert_eq!(
                            cache
                                .get(|| {
                                    calls.fetch_add(1, Ordering::SeqCst);
                                    if succeeds {
                                        Ok([7; 32])
                                    } else {
                                        Err(QuarkError::Storage)
                                    }
                                })
                                .is_ok(),
                            succeeds
                        );
                    });
                }
            });
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
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
