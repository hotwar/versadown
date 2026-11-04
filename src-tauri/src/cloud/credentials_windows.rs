//! Windows Credential Manager holds a 32-byte key, not the full cloud session.
//! Provider rows are authenticated ciphertext; SQLite commits replacements atomically.
//! Legacy provider entries are neither read nor removed. Providers require a new login.
use super::*;
use chacha20poly1305::aead::Payload;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use zeroize::Zeroizing;

pub(super) const MASTER_KEY_NAME: &str = "cloud-session-key-v1";
const DATABASE: &str = "cloud-sessions-v1.sqlite";

pub(super) fn handles(key: &str) -> bool {
    matches!(
        key,
        "quark" | "pikpak" | "baidu" | "tianyi" | "aliyun" | "xunlei" | "uc"
    )
}

pub(super) trait MasterKeyStore {
    fn read(&self) -> Result<Option<Vec<u8>>, QuarkError>;
    fn write(&self, key: &[u8]) -> Result<(), QuarkError>;
}

#[cfg(windows)]
pub(super) struct SystemMasterKey<'a>(pub(super) &'a str);

#[cfg(windows)]
impl SystemMasterKey<'_> {
    fn entry(&self) -> Result<Entry, QuarkError> {
        Entry::new(self.0, MASTER_KEY_NAME)
            .map_err(|error| credential_error("open", MASTER_KEY_NAME, error, None))
    }
}

#[cfg(windows)]
impl MasterKeyStore for SystemMasterKey<'_> {
    fn read(&self) -> Result<Option<Vec<u8>>, QuarkError> {
        match self.entry()?.get_secret() {
            Ok(key) => Ok(Some(key)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(credential_error("read", MASTER_KEY_NAME, error, None)),
        }
    }

    fn write(&self, key: &[u8]) -> Result<(), QuarkError> {
        self.entry()?
            .set_secret(key)
            .map_err(|error| credential_error("save", MASTER_KEY_NAME, error, Some(key.len())))
    }
}

fn open(root: &Path, write: bool) -> Result<Connection, QuarkError> {
    let flags = if write {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let db =
        Connection::open_with_flags(root.join(DATABASE), flags).map_err(|_| QuarkError::Storage)?;
    db.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| QuarkError::Storage)?;
    if write {
        db.execute_batch("PRAGMA secure_delete=ON; CREATE TABLE IF NOT EXISTS sessions (provider TEXT PRIMARY KEY, payload BLOB NOT NULL);")
            .map_err(|_| QuarkError::Storage)?;
    }
    Ok(db)
}

fn read_key(keys: &impl MasterKeyStore) -> Result<Option<Zeroizing<Vec<u8>>>, QuarkError> {
    let key = keys.read()?.map(Zeroizing::new);
    if key.as_ref().is_some_and(|key| key.len() != 32) {
        return Err(QuarkError::Storage);
    }
    Ok(key)
}

fn aad(service: &str, provider: &str) -> String {
    format!("cloud-sessions-v1:{service}:{provider}")
}

pub(super) fn save(
    root: &Path,
    service: &str,
    provider: &str,
    value: &[u8],
    keys: &impl MasterKeyStore,
) -> Result<(), QuarkError> {
    if !handles(provider) {
        return Err(QuarkError::Storage);
    }
    let mut db = open(root, true)?;
    // Serialize the first-key creation together with the first row, including
    // across processes. Existing ciphertext must never receive a replacement key.
    let transaction = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| QuarkError::Storage)?;
    let key = match read_key(keys)? {
        Some(key) => key,
        None => {
            let has_rows: bool = transaction
                .query_row("SELECT EXISTS(SELECT 1 FROM sessions)", [], |row| {
                    row.get(0)
                })
                .map_err(|_| QuarkError::Storage)?;
            if has_rows {
                return Err(QuarkError::Storage);
            }
            let mut key = Zeroizing::new(vec![0; 32]);
            OsRng.fill_bytes(&mut key);
            keys.write(&key)?;
            key
        }
    };
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|_| QuarkError::Storage)?;
    let mut nonce = XNonce::default();
    OsRng.fill_bytes(&mut nonce);
    let encrypted = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: value,
                aad: aad(service, provider).as_bytes(),
            },
        )
        .map_err(|_| QuarkError::Storage)?;
    let mut payload = nonce.to_vec();
    payload.extend_from_slice(&encrypted);
    transaction.execute(
        "INSERT INTO sessions(provider,payload) VALUES(?1,?2) ON CONFLICT(provider) DO UPDATE SET payload=excluded.payload",
        params![provider, payload],
    ).map_err(|_| QuarkError::Storage)?;
    transaction.commit().map_err(|_| QuarkError::Storage)
}

pub(super) fn load(
    root: &Path,
    service: &str,
    provider: &str,
    keys: &impl MasterKeyStore,
) -> Result<Option<Vec<u8>>, QuarkError> {
    if !handles(provider) {
        return Err(QuarkError::Storage);
    }
    if !root
        .join(DATABASE)
        .try_exists()
        .map_err(|_| QuarkError::Storage)?
    {
        return Ok(None);
    }
    let payload: Option<Vec<u8>> = open(root, false)?
        .query_row(
            "SELECT payload FROM sessions WHERE provider=?1",
            [provider],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| QuarkError::Storage)?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let key = read_key(keys)?.ok_or(QuarkError::Storage)?;
    if payload.len() < 24 + 16 {
        return Err(QuarkError::Storage);
    }
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|_| QuarkError::Storage)?;
    let nonce = XNonce::try_from(&payload[..24]).map_err(|_| QuarkError::Storage)?;
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &payload[24..],
                aad: aad(service, provider).as_bytes(),
            },
        )
        .map(Some)
        .map_err(|_| QuarkError::Storage)
}

pub(super) fn remove(root: &Path, provider: &str) -> Result<(), QuarkError> {
    if !handles(provider) {
        return Err(QuarkError::Storage);
    }
    if !root
        .join(DATABASE)
        .try_exists()
        .map_err(|_| QuarkError::Storage)?
    {
        return Ok(());
    }
    open(root, true)?
        .execute("DELETE FROM sessions WHERE provider=?1", [provider])
        .map_err(|_| QuarkError::Storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeKeyring {
        key: Mutex<Option<Vec<u8>>>,
        reads: AtomicUsize,
        writes: AtomicUsize,
    }
    impl MasterKeyStore for FakeKeyring {
        fn read(&self) -> Result<Option<Vec<u8>>, QuarkError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.key.lock().unwrap().clone())
        }
        fn write(&self, key: &[u8]) -> Result<(), QuarkError> {
            // Match Credential Manager's actual per-entry size contract.
            if key.len() > 2560 {
                return Err(QuarkError::Storage);
            }
            self.writes.fetch_add(1, Ordering::SeqCst);
            *self.key.lock().unwrap() = Some(key.to_vec());
            Ok(())
        }
    }

    #[test]
    fn large_cloud_sessions_round_trip_with_only_a_small_key_in_keyring() {
        let root = tempfile::tempdir().unwrap();
        let keys = FakeKeyring::default();
        let session: crate::cloud::xunlei::XunleiSession = serde_json::from_value(serde_json::json!({
            "access_token": "synthetic-access".repeat(128),
            "refresh_token": "synthetic-refresh".repeat(128),
            "action_tickets": {"POST:/drive/v1/files": {"token": "synthetic-captcha".repeat(512), "expires_at": 1800000000}},
        })).unwrap();
        let session = serde_json::to_vec(&session).unwrap();
        assert!(session.len() > 2560);
        assert!(keys.write(&session).is_err());
        for provider in [
            "quark", "pikpak", "baidu", "tianyi", "aliyun", "xunlei", "uc",
        ] {
            save(root.path(), "service", provider, &session, &keys).unwrap();
            assert_eq!(
                load(root.path(), "service", provider, &keys).unwrap(),
                Some(session.clone())
            );
        }
        assert_eq!(keys.writes.load(Ordering::SeqCst), 1);
        assert_eq!(keys.key.lock().unwrap().as_ref().unwrap().len(), 32);
        let bytes = std::fs::read(root.path().join(DATABASE)).unwrap();
        assert!(!bytes.windows(17).any(|bytes| bytes == b"synthetic-captcha"));
        assert!(!handles("privacy-download-password-v1"));
        assert!(!handles("cloud-password-xunlei"));
    }

    #[test]
    fn missing_and_deleted_sessions_do_not_access_the_master_key() {
        let root = tempfile::tempdir().unwrap();
        let keys = FakeKeyring::default();
        assert_eq!(load(root.path(), "service", "xunlei", &keys).unwrap(), None);
        remove(root.path(), "xunlei").unwrap();
        assert!(!root.path().join(DATABASE).exists());
        assert_eq!(keys.reads.load(Ordering::SeqCst), 0);
        assert_eq!(keys.writes.load(Ordering::SeqCst), 0);
        save(root.path(), "service", "xunlei", b"value", &keys).unwrap();
        remove(root.path(), "xunlei").unwrap();
        let reads = keys.reads.load(Ordering::SeqCst);
        assert_eq!(load(root.path(), "service", "xunlei", &keys).unwrap(), None);
        remove(root.path(), "xunlei").unwrap();
        assert_eq!(keys.reads.load(Ordering::SeqCst), reads);
    }

    #[test]
    fn failed_atomic_replacement_leaves_the_previous_session_intact() {
        let root = tempfile::tempdir().unwrap();
        let keys = FakeKeyring::default();
        save(root.path(), "service", "xunlei", b"previous-session", &keys).unwrap();
        let db = open(root.path(), true).unwrap();
        db.execute_batch("CREATE TRIGGER fail_replace BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'test write failure'); END;").unwrap();
        assert!(save(
            root.path(),
            "service",
            "xunlei",
            b"replacement-session",
            &keys
        )
        .is_err());
        assert_eq!(
            load(root.path(), "service", "xunlei", &keys)
                .unwrap()
                .unwrap(),
            b"previous-session"
        );
        db.execute_batch("DROP TRIGGER fail_replace;").unwrap();
        save(
            root.path(),
            "service",
            "xunlei",
            b"replacement-session",
            &keys,
        )
        .unwrap();
        assert_eq!(
            load(root.path(), "service", "xunlei", &keys)
                .unwrap()
                .unwrap(),
            b"replacement-session"
        );
    }

    #[test]
    fn ciphertext_authentication_binds_the_provider_namespace_and_key() {
        let root = tempfile::tempdir().unwrap();
        let keys = FakeKeyring::default();
        save(root.path(), "service", "xunlei", b"secret-value", &keys).unwrap();
        assert!(load(root.path(), "other-service", "xunlei", &keys).is_err());
        let wrong_keys = FakeKeyring::default();
        wrong_keys.write(&[5; 32]).unwrap();
        assert!(load(root.path(), "service", "xunlei", &wrong_keys).is_err());
        let db = open(root.path(), true).unwrap();
        db.execute(
            "INSERT INTO sessions SELECT 'baidu', payload FROM sessions WHERE provider='xunlei'",
            [],
        )
        .unwrap();
        assert!(load(root.path(), "service", "baidu", &keys).is_err());
        let mut payload: Vec<u8> = db
            .query_row(
                "SELECT payload FROM sessions WHERE provider='xunlei'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        payload[24] ^= 1;
        db.execute(
            "UPDATE sessions SET payload=?1 WHERE provider='xunlei'",
            [payload],
        )
        .unwrap();
        assert!(load(root.path(), "service", "xunlei", &keys).is_err());
    }

    #[test]
    fn a_missing_master_key_never_replaces_existing_encrypted_sessions() {
        let root = tempfile::tempdir().unwrap();
        let keys = FakeKeyring::default();
        save(root.path(), "service", "xunlei", b"previous-session", &keys).unwrap();
        let previous_key = keys.key.lock().unwrap().take();
        assert!(load(root.path(), "service", "xunlei", &keys).is_err());
        assert!(save(root.path(), "service", "baidu", b"new-session", &keys).is_err());
        assert_eq!(keys.writes.load(Ordering::SeqCst), 1);
        *keys.key.lock().unwrap() = previous_key;
        assert_eq!(
            load(root.path(), "service", "xunlei", &keys)
                .unwrap()
                .unwrap(),
            b"previous-session"
        );
    }

    #[test]
    fn diagnostics_never_format_secret_bearing_errors_or_untrusted_labels() {
        let secret = "private-credential-value";
        let errors = [
            KeyringError::BadEncoding(secret.as_bytes().to_vec()),
            KeyringError::Invalid(secret.into(), secret.into()),
            KeyringError::TooLong(secret.into(), 2560),
        ];
        for error in errors {
            let text = credential_error_diagnostic(secret, secret, &error, Some(6749));
            assert!(!text.contains(secret));
            assert!(text.contains("operation=unknown key=unknown"));
        }
        let text = credential_error_diagnostic(
            "save",
            "xunlei",
            &KeyringError::TooLong("secret".into(), 2560),
            Some(6749),
        );
        assert_eq!(
            text,
            "[credentials] operation=save key=xunlei error_kind=too_long len=6749 limit=2560"
        );
    }
}
