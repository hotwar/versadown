use super::QuarkError;
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use keyring::{Entry, Error as KeyringError};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};
use zeroize::Zeroize;

#[cfg(target_os = "macos")]
#[path = "credentials_macos.rs"]
mod macos;

#[cfg(any(windows, test))]
#[path = "credentials_windows.rs"]
mod windows;

const SYSTEM_CREDENTIAL_SERVICE_PREFIX: &str = "com.versadown.desktop";
const SHARE_PROTECTOR_SERVICE: &str = "com.versadown.desktop.share-storage";
const SHARE_PROTECTOR_KEY: &str = "share-storage-key-v1";
const PROTECTED_BLOB_MAGIC: &[u8] = b"ADSC1";
pub trait CredentialStore: Send + Sync {
    /// Snapshot consumers delegate renewal to AuthSupervisor.
    fn automatic_session_recovery(&self) -> bool {
        true
    }
    fn save(&self, key: &str, value: &[u8]) -> Result<(), QuarkError>;
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError>;
    fn remove(&self, key: &str) -> Result<(), QuarkError>;
}
#[derive(Default)]
pub struct MemoryCredentialStore(Mutex<BTreeMap<String, Vec<u8>>>);
impl CredentialStore for MemoryCredentialStore {
    fn save(&self, k: &str, v: &[u8]) -> Result<(), QuarkError> {
        self.0
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .insert(k.into(), v.into());
        Ok(())
    }
    fn load(&self, k: &str) -> Result<Option<Vec<u8>>, QuarkError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .get(k)
            .cloned())
    }
    fn remove(&self, k: &str) -> Result<(), QuarkError> {
        self.0.lock().map_err(|_| QuarkError::Storage)?.remove(k);
        Ok(())
    }
}
#[derive(Clone)]
pub struct SystemCredentialStore {
    root: PathBuf,
    service: String,
    #[cfg(all(target_os = "macos", test))]
    test_key: Option<[u8; 32]>,
}

// Native UI regression binaries must not access the developer's real
// credential storage. This adapter is excluded from shipped application builds and
// enabled only by the isolated native-smoke harness.
#[cfg(feature = "native-smoke")]
fn native_smoke_credentials() -> Option<&'static MemoryCredentialStore> {
    if std::env::var("VERSADOWN_NATIVE_SMOKE").as_deref() != Ok("1")
        || std::env::var("VERSADOWN_BACKGROUND_UI_TEST").as_deref() != Ok("1")
        || std::env::var_os("VERSADOWN_TEST_DATA_DIR").is_none()
    {
        return None;
    }
    static STORE: std::sync::LazyLock<MemoryCredentialStore> =
        std::sync::LazyLock::new(MemoryCredentialStore::default);
    Some(&STORE)
}

#[cfg(not(target_os = "macos"))]
fn credential_error(
    _operation: &str,
    _key: &str,
    _error: KeyringError,
    _len: Option<usize>,
) -> QuarkError {
    #[cfg(windows)]
    eprintln!(
        "{}",
        credential_error_diagnostic(_operation, _key, &_error, _len)
    );
    QuarkError::Storage
}

#[cfg(any(windows, test))]
fn credential_error_diagnostic(
    operation: &str,
    key: &str,
    error: &KeyringError,
    len: Option<usize>,
) -> String {
    let operation = match operation {
        "open" | "save" | "read" | "remove" => operation,
        _ => "unknown",
    };
    let key = if credential_key_is_supported(key) || key == windows::MASTER_KEY_NAME {
        key
    } else {
        "unknown"
    };
    // Several keyring errors carry raw secret bytes. Never format the error itself.
    let kind = match error {
        KeyringError::TooLong(..) => "too_long",
        KeyringError::NoEntry => "missing",
        KeyringError::NoStorageAccess(..) => "access",
        KeyringError::PlatformFailure(..) => "platform",
        KeyringError::BadEncoding(..) | KeyringError::BadDataFormat(..) => "encoding",
        KeyringError::Invalid(..) => "invalid",
        _ => "other",
    };
    let limit = match error {
        KeyringError::TooLong(_, limit) => Some(*limit),
        _ => None,
    };
    format!(
        "[credentials] operation={operation} key={key} error_kind={kind} len={} limit={}",
        len.map_or_else(|| "unknown".into(), |value| value.to_string()),
        limit.map_or_else(|| "unknown".into(), |value| value.to_string()),
    )
}

pub(crate) const BAIDU_BROWSER_PASSWORD_PREFERENCE_KEY: &str =
    "cloud-password-baidu-browser-preferred";
pub(crate) const BAIDU_BROWSER_PROFILE_USERNAME_KEY: &str =
    "cloud-password-baidu-browser-profile-username";

pub(crate) fn password_profile_reset_key(provider: super::CloudProvider) -> Option<&'static str> {
    match provider {
        super::CloudProvider::Baidu => Some("cloud-password-profile-reset-baidu"),
        super::CloudProvider::Tianyi => Some("cloud-password-profile-reset-tianyi"),
        super::CloudProvider::Xunlei => Some("cloud-password-profile-reset-xunlei"),
        _ => None,
    }
}

pub fn credential_key_is_supported(key: &str) -> bool {
    matches!(
        key,
        "quark"
            | "pikpak"
            | "baidu"
            | "tianyi"
            | "aliyun"
            | "xunlei"
            | "uc"
            | "ai-api-key"
            | "privacy-download-password-v1"
            | "cloud-share-plans-v1"
            | "cloud-password-pikpak"
            | "cloud-password-baidu"
            | "cloud-password-tianyi"
            | "cloud-password-xunlei"
            | "cloud-password-draft-pikpak"
            | "cloud-password-draft-baidu"
            | "cloud-password-draft-tianyi"
            | "cloud-password-draft-xunlei"
            | BAIDU_BROWSER_PASSWORD_PREFERENCE_KEY
            | BAIDU_BROWSER_PROFILE_USERNAME_KEY
            | "cloud-password-profile-reset-baidu"
            | "cloud-password-profile-reset-tianyi"
            | "cloud-password-profile-reset-xunlei"
            | "cloud-auth-maintenance-quark"
            | "cloud-auth-maintenance-pikpak"
            | "cloud-auth-maintenance-baidu"
            | "cloud-auth-maintenance-tianyi"
            | "cloud-auth-maintenance-aliyun"
            | "cloud-auth-maintenance-xunlei"
            | "cloud-auth-maintenance-uc"
    )
}

impl SystemCredentialStore {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    pub fn new(root: PathBuf) -> Result<Self, QuarkError> {
        std::fs::create_dir_all(&root).map_err(|_| QuarkError::Storage)?;
        Ok(Self::unprepared(root))
    }
    /// Startup never aborts on a broken credential directory: the store is
    /// returned unusable and the flag tells the caller to report degradation.
    pub(crate) fn new_or_degraded(root: PathBuf) -> (Self, bool) {
        match Self::new(root.clone()) {
            Ok(store) => (store, false),
            Err(_) => (Self::unprepared(root), true),
        }
    }
    fn unprepared(root: PathBuf) -> Self {
        let root_id = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
        Self {
            #[cfg(all(target_os = "macos", test))]
            test_key: None,
            root,
            service: format!("{SYSTEM_CREDENTIAL_SERVICE_PREFIX}.{}", &root_id[..24]),
        }
    }
    #[cfg(target_os = "macos")]
    fn master_key(&self, create: bool) -> Result<[u8; 32], QuarkError> {
        #[cfg(test)]
        if let Some(key) = self.test_key {
            return Ok(key);
        }
        share_protection_key(create)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn contains_local(&self, key: &str) -> Result<bool, QuarkError> {
        Ok(macos::payload(&self.root, key)?.is_some())
    }
    #[cfg(not(target_os = "macos"))]
    fn entry(&self, key: &str) -> Result<Entry, QuarkError> {
        if !credential_key_is_supported(key) {
            return Err(QuarkError::Storage);
        }
        Entry::new(&self.service, key).map_err(|error| credential_error("open", key, error, None))
    }
}
impl CredentialStore for SystemCredentialStore {
    fn save(&self, k: &str, v: &[u8]) -> Result<(), QuarkError> {
        #[cfg(feature = "native-smoke")]
        if let Some(store) = native_smoke_credentials() {
            return store.save(&format!("{}:{k}", self.service), v);
        }
        #[cfg(windows)]
        if windows::handles(k) {
            return windows::save(
                &self.root,
                &self.service,
                k,
                v,
                &windows::SystemMasterKey(&self.service),
            );
        }
        #[cfg(target_os = "macos")]
        {
            if !credential_key_is_supported(k) {
                return Err(QuarkError::Storage);
            }
            let key = self.master_key(true)?;
            macos::purge_undecryptable(&self.root, &self.service, key)?;
            return macos::save(&self.root, &self.service, k, v, key);
        }
        #[cfg(not(target_os = "macos"))]
        self.entry(k)?
            .set_secret(v)
            .map_err(|error| credential_error("save", k, error, Some(v.len())))
    }
    fn load(&self, k: &str) -> Result<Option<Vec<u8>>, QuarkError> {
        #[cfg(feature = "native-smoke")]
        if let Some(store) = native_smoke_credentials() {
            return store.load(&format!("{}:{k}", self.service));
        }
        #[cfg(windows)]
        if windows::handles(k) {
            return windows::load(
                &self.root,
                &self.service,
                k,
                &windows::SystemMasterKey(&self.service),
            );
        }
        #[cfg(target_os = "macos")]
        {
            if !credential_key_is_supported(k) {
                return Err(QuarkError::Storage);
            }
            if let Some(payload) = macos::payload(&self.root, k)? {
                return macos::decrypt(&self.service, k, &payload, self.master_key(false)?)
                    .map(Some);
            }
            return Ok(None);
        }
        #[cfg(not(target_os = "macos"))]
        match self.entry(k)?.get_secret() {
            Ok(value) => Ok(Some(value)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(credential_error("read", k, error, None)),
        }
    }
    fn remove(&self, k: &str) -> Result<(), QuarkError> {
        #[cfg(feature = "native-smoke")]
        if let Some(store) = native_smoke_credentials() {
            return store.remove(&format!("{}:{k}", self.service));
        }
        #[cfg(windows)]
        if windows::handles(k) {
            return windows::remove(&self.root, k);
        }
        #[cfg(target_os = "macos")]
        {
            if !credential_key_is_supported(k) {
                return Err(QuarkError::Storage);
            }
            return macos::remove(&self.root, k);
        }
        #[cfg(not(target_os = "macos"))]
        match self.entry(k)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => Err(credential_error("remove", k, error, None)),
        }
    }
}

fn share_protector_entry() -> Result<Entry, QuarkError> {
    Entry::new(SHARE_PROTECTOR_SERVICE, SHARE_PROTECTOR_KEY).map_err(|_| QuarkError::Storage)
}

fn share_protection_key(create: bool) -> Result<[u8; 32], QuarkError> {
    #[cfg(feature = "native-smoke")]
    if native_smoke_credentials().is_some() {
        static KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        return Ok(*KEY.get_or_init(|| {
            let mut key = [0; 32];
            OsRng.fill_bytes(&mut key);
            key
        }));
    }
    #[cfg(target_os = "macos")]
    {
        return macos::MASTER_KEY.get(|| read_share_protection_key(create));
    }
    #[cfg(not(target_os = "macos"))]
    read_share_protection_key(create)
}

fn read_share_protection_key(_create: bool) -> Result<[u8; 32], QuarkError> {
    let entry = share_protector_entry()?;
    let mut key = match entry.get_secret() {
        Ok(key) if key.len() == 32 => key,
        Ok(_) => return Err(QuarkError::Storage),
        Err(KeyringError::NoEntry) => {
            #[cfg(target_os = "macos")]
            // A lost key may be replaced only by an explicit create request;
            // callers then purge the rows the old key protected.
            if !_create {
                return Err(QuarkError::Storage);
            }
            let mut key = vec![0_u8; 32];
            OsRng.fill_bytes(&mut key);
            entry.set_secret(&key).map_err(|_| QuarkError::Storage)?;
            key
        }
        Err(_) => return Err(QuarkError::Storage),
    };
    let mut result = [0_u8; 32];
    result.copy_from_slice(&key);
    key.zeroize();
    Ok(result)
}

/// Encrypts the SQLite share-plan payloads with a key kept only in the system
/// credential store. This replaces the platform-specific adapter while leaving the transactional plan
/// repository intact on Windows, Linux and macOS.
pub(crate) fn protect(input: &[u8], encrypt: bool) -> Result<Vec<u8>, QuarkError> {
    protect_with_key(input, encrypt, share_protection_key(encrypt)?)
}

pub(super) fn protect_with_key(
    input: &[u8],
    encrypt: bool,
    mut key: [u8; 32],
) -> Result<Vec<u8>, QuarkError> {
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|_| QuarkError::Storage)?;
    key.zeroize();
    if encrypt {
        let mut nonce = XNonce::default();
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(&nonce, input)
            .map_err(|_| QuarkError::Storage)?;
        let mut protected =
            Vec::with_capacity(PROTECTED_BLOB_MAGIC.len() + nonce.len() + ciphertext.len());
        protected.extend_from_slice(PROTECTED_BLOB_MAGIC);
        protected.extend_from_slice(&nonce);
        protected.extend_from_slice(&ciphertext);
        return Ok(protected);
    }
    let nonce_end = PROTECTED_BLOB_MAGIC.len() + XNonce::default().len();
    if input.len() <= nonce_end || !input.starts_with(PROTECTED_BLOB_MAGIC) {
        return Err(QuarkError::Storage);
    }
    let nonce = XNonce::try_from(&input[PROTECTED_BLOB_MAGIC.len()..nonce_end])
        .map_err(|_| QuarkError::Storage)?;
    cipher
        .decrypt(&nonce, &input[nonce_end..])
        .map_err(|_| QuarkError::Storage)
}

#[cfg(test)]
mod system_store_tests {
    use super::{protect, CredentialStore, SystemCredentialStore};

    #[test]
    fn system_credential_store_round_trips_binary_secrets_and_treats_missing_entries_as_absent() {
        let directory = tempfile::tempdir().unwrap();
        let store = SystemCredentialStore::new(directory.path().into()).unwrap();

        assert_eq!(store.load("privacy-download-password-v1").unwrap(), None);
        store
            .save(
                "privacy-download-password-v1",
                b"correct horse battery staple",
            )
            .unwrap();
        assert_eq!(
            store.load("privacy-download-password-v1").unwrap(),
            Some(b"correct horse battery staple".to_vec())
        );
        store.remove("privacy-download-password-v1").unwrap();
        assert_eq!(store.load("privacy-download-password-v1").unwrap(), None);
    }

    #[test]
    fn share_plan_protector_never_persists_its_plaintext_payload() {
        let plaintext = b"cloud-share-plan-secret";
        let protected = protect(plaintext, true).unwrap();

        assert_ne!(protected, plaintext);
        assert!(!protected
            .windows(plaintext.len())
            .any(|bytes| bytes == plaintext));
        assert_eq!(protect(&protected, false).unwrap(), plaintext);
    }
}
