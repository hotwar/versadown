use super::{
    credentials::{BAIDU_BROWSER_PASSWORD_PREFERENCE_KEY, BAIDU_BROWSER_PROFILE_USERNAME_KEY},
    provider_capabilities, CloudProvider, CredentialStore, PasswordLoginMode, QuarkError,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionWorkKind {
    Read,
    Mutation,
    Logout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloudSessionPolicy {
    pub provider: CloudProvider,
    pub supports_password_fallback: bool,
}

pub fn cloud_session_policy(provider: CloudProvider) -> CloudSessionPolicy {
    CloudSessionPolicy {
        provider,
        supports_password_fallback: provider_capabilities(provider).password_login_mode
            == PasswordLoginMode::RustDirect,
    }
}

pub struct CloudPasswordCredential {
    provider: CloudProvider,
    account_id: String,
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}

impl CloudPasswordCredential {
    pub fn new(
        provider: CloudProvider,
        account_id: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            account_id: account_id.into(),
            username: Zeroizing::new(username.into()),
            password: Zeroizing::new(password.into()),
        }
    }

    pub fn provider(&self) -> CloudProvider {
        self.provider
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn username(&self) -> &str {
        self.username.as_str()
    }

    pub fn password(&self) -> &str {
        self.password.as_str()
    }
}

impl fmt::Debug for CloudPasswordCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloudPasswordCredential")
            .field("provider", &self.provider)
            .field("account_id", &self.account_id)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Serialize)]
struct StoredPasswordCredential<'a> {
    account_id: &'a str,
    username: &'a str,
    password: &'a str,
}

#[derive(Serialize)]
struct StoredPasswordCredentials<'a> {
    version: u8,
    provider: CloudProvider,
    active_account_id: Option<&'a str>,
    credentials: Vec<StoredPasswordCredential<'a>>,
}

#[derive(Deserialize)]
struct PasswordCredentialVersion {
    version: u8,
}

#[derive(Deserialize)]
struct LoadedPasswordCredentialV1 {
    version: u8,
    provider: CloudProvider,
    account_id: String,
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct LoadedPasswordCredential {
    account_id: String,
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct LoadedPasswordCredentials {
    version: u8,
    provider: CloudProvider,
    active_account_id: Option<String>,
    credentials: Vec<LoadedPasswordCredential>,
}

impl Zeroize for LoadedPasswordCredentialV1 {
    fn zeroize(&mut self) {
        self.account_id.zeroize();
        self.username.zeroize();
        self.password.zeroize();
    }
}

impl Zeroize for LoadedPasswordCredential {
    fn zeroize(&mut self) {
        self.account_id.zeroize();
        self.username.zeroize();
        self.password.zeroize();
    }
}

impl Zeroize for LoadedPasswordCredentials {
    fn zeroize(&mut self) {
        if let Some(active_account_id) = self.active_account_id.as_mut() {
            active_account_id.zeroize();
        }
        self.active_account_id = None;
        self.credentials.zeroize();
    }
}

fn password_credential_key(provider: CloudProvider) -> Result<&'static str, QuarkError> {
    if !provider_capabilities(provider).password {
        return Err(QuarkError::InvalidInput(
            "该网盘不支持账号密码自动恢复".to_owned(),
        ));
    }
    match provider {
        CloudProvider::Pikpak => Ok("cloud-password-pikpak"),
        CloudProvider::Baidu => Ok("cloud-password-baidu"),
        CloudProvider::Tianyi => Ok("cloud-password-tianyi"),
        CloudProvider::Xunlei => Ok("cloud-password-xunlei"),
        CloudProvider::Quark
        | CloudProvider::Aliyun
        | CloudProvider::Uc
        | CloudProvider::Mega
        | CloudProvider::Gofile => {
            unreachable!("password providers must have a password credential key")
        }
    }
}

pub fn browser_password_login_preferred(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<bool, QuarkError> {
    if provider != CloudProvider::Baidu {
        return Ok(false);
    }
    Ok(store
        .load(BAIDU_BROWSER_PASSWORD_PREFERENCE_KEY)?
        .is_some_and(|value| value.as_slice() == b"1"))
}

pub fn set_browser_password_login_preferred(
    store: &impl CredentialStore,
    provider: CloudProvider,
    preferred: bool,
) -> Result<(), QuarkError> {
    if provider != CloudProvider::Baidu {
        return Ok(());
    }
    if preferred {
        store.save(BAIDU_BROWSER_PASSWORD_PREFERENCE_KEY, b"1")
    } else {
        store.remove(BAIDU_BROWSER_PASSWORD_PREFERENCE_KEY)
    }
}

pub fn baidu_browser_profile_username(
    store: &impl CredentialStore,
) -> Result<Option<Zeroizing<String>>, QuarkError> {
    let Some(mut bytes) = store.load(BAIDU_BROWSER_PROFILE_USERNAME_KEY)? else {
        return Ok(None);
    };
    let username = std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(|_| QuarkError::Storage);
    bytes.zeroize();
    username.map(|username| Some(Zeroizing::new(username)))
}

pub fn set_baidu_browser_profile_username(
    store: &impl CredentialStore,
    username: Option<&str>,
) -> Result<(), QuarkError> {
    match username
        .map(str::trim)
        .filter(|username| !username.is_empty())
    {
        Some(username) => store.save(BAIDU_BROWSER_PROFILE_USERNAME_KEY, username.as_bytes()),
        None => store.remove(BAIDU_BROWSER_PROFILE_USERNAME_KEY),
    }
}

fn load_password_credentials(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<Option<LoadedPasswordCredentials>, QuarkError> {
    let key = password_credential_key(provider)?;
    let Some(mut bytes) = store.load(key)? else {
        return Ok(None);
    };
    let version = match serde_json::from_slice::<PasswordCredentialVersion>(&bytes) {
        Ok(version) => version,
        Err(_) => {
            bytes.zeroize();
            return Err(QuarkError::Storage);
        }
    };
    let mut credentials = match version.version {
        // Version 1 stored one credential per provider. It remains readable and
        // becomes the active account when next persisted as version 2.
        1 => {
            let mut legacy = match serde_json::from_slice::<LoadedPasswordCredentialV1>(&bytes) {
                Ok(legacy) => legacy,
                Err(_) => {
                    bytes.zeroize();
                    return Err(QuarkError::Storage);
                }
            };
            if legacy.version != 1 || legacy.provider != provider {
                legacy.zeroize();
                bytes.zeroize();
                return Err(QuarkError::Storage);
            }
            let account_id = std::mem::take(&mut legacy.account_id);
            let username = std::mem::take(&mut legacy.username);
            let password = std::mem::take(&mut legacy.password);
            legacy.zeroize();
            LoadedPasswordCredentials {
                version: 2,
                provider,
                active_account_id: Some(account_id.clone()),
                credentials: vec![LoadedPasswordCredential {
                    account_id,
                    username,
                    password,
                }],
            }
        }
        2 => match serde_json::from_slice::<LoadedPasswordCredentials>(&bytes) {
            Ok(credentials) => credentials,
            Err(_) => {
                bytes.zeroize();
                return Err(QuarkError::Storage);
            }
        },
        _ => {
            bytes.zeroize();
            return Err(QuarkError::Storage);
        }
    };
    bytes.zeroize();
    if credentials.provider != provider || credentials.version != 2 {
        credentials.zeroize();
        return Err(QuarkError::Storage);
    }
    Ok(Some(credentials))
}

fn persist_password_credentials(
    store: &impl CredentialStore,
    credentials: &LoadedPasswordCredentials,
) -> Result<(), QuarkError> {
    let key = password_credential_key(credentials.provider)?;
    let stored = StoredPasswordCredentials {
        version: 2,
        provider: credentials.provider,
        active_account_id: credentials.active_account_id.as_deref(),
        credentials: credentials
            .credentials
            .iter()
            .map(|credential| StoredPasswordCredential {
                account_id: &credential.account_id,
                username: &credential.username,
                password: &credential.password,
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec(&stored).map_err(|_| QuarkError::Storage)?;
    let result = store.save(key, &bytes);
    bytes.zeroize();
    result
}

pub fn save_password_credential(
    store: &impl CredentialStore,
    credential: CloudPasswordCredential,
) -> Result<(), QuarkError> {
    let provider = credential.provider();
    let previous = store
        .load(password_credential_key(provider)?)
        .map_err(|error| {
            let _ = store.remove(provider.key());
            error
        })?
        .map(Zeroizing::new);
    let mut credentials = load_password_credentials(store, provider)
        .map_err(|error| {
            let _ = store.remove(provider.key());
            error
        })?
        .unwrap_or(LoadedPasswordCredentials {
            version: 2,
            provider,
            active_account_id: None,
            credentials: Vec::new(),
        });
    let account_id = credential.account_id().to_owned();
    let username = credential.username().to_owned();
    let password = credential.password().to_owned();
    for existing in &mut credentials.credentials {
        existing.zeroize();
    }
    credentials.credentials.clear();
    credentials.credentials.push(LoadedPasswordCredential {
        account_id: account_id.clone(),
        username,
        password,
    });
    if let Some(active_account_id) = credentials.active_account_id.as_mut() {
        active_account_id.zeroize();
    }
    credentials.active_account_id = Some(account_id);
    let result = persist_password_credentials(store, &credentials);
    credentials.zeroize();
    if let Err(error) = result {
        let _ = store.remove(provider.key());
        return Err(error);
    }
    if let Err(error) = store.remove(password_login_draft_key(provider)?) {
        // Do not report a replacement as failed while silently keeping its password.
        match previous.as_deref() {
            Some(bytes) => store.save(password_credential_key(provider)?, bytes)?,
            None => store.remove(password_credential_key(provider)?)?,
        }
        store.remove(provider.key())?;
        return Err(error);
    }
    Ok(())
}

pub fn load_password_credential(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<Option<CloudPasswordCredential>, QuarkError> {
    let Some(mut credentials) = load_password_credentials(store, provider)? else {
        return Ok(None);
    };
    let credential = credentials
        .active_account_id
        .as_deref()
        .and_then(|account_id| {
            credentials
                .credentials
                .iter()
                .find(|credential| credential.account_id == account_id)
        })
        .map(|credential| {
            CloudPasswordCredential::new(
                provider,
                credential.account_id.clone(),
                credential.username.clone(),
                credential.password.clone(),
            )
        });
    credentials.zeroize();
    Ok(credential)
}

pub fn remove_password_credential(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<(), QuarkError> {
    let password_result = store.remove(password_credential_key(provider)?);
    let draft_result = store.remove(password_login_draft_key(provider)?);
    let preference_result = if provider == CloudProvider::Baidu {
        store.remove(BAIDU_BROWSER_PASSWORD_PREFERENCE_KEY)
    } else {
        Ok(())
    };
    let profile_result = if provider == CloudProvider::Baidu {
        store.remove(BAIDU_BROWSER_PROFILE_USERNAME_KEY)
    } else {
        Ok(())
    };
    match (
        password_result,
        draft_result,
        preference_result,
        profile_result,
    ) {
        (Ok(()), Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(error), _, _, _)
        | (_, Err(error), _, _)
        | (_, _, Err(error), _)
        | (_, _, _, Err(error)) => Err(error),
    }
}

/// Records which account owns the current session. Every password-capable
/// provider keeps one credential; selecting another browser account clears the
/// old password and draft instead of retaining an inactive account.
pub fn select_password_credential_account(
    store: &impl CredentialStore,
    provider: CloudProvider,
    account_id: &str,
) -> Result<(), QuarkError> {
    let Some(mut credentials) = load_password_credentials(store, provider)? else {
        return clear_password_account(store, provider);
    };
    let Some(index) = credentials
        .credentials
        .iter()
        .position(|credential| credential.account_id == account_id)
    else {
        credentials.zeroize();
        return clear_password_account(store, provider);
    };
    let selected = credentials.credentials.remove(index);
    for stale in &mut credentials.credentials {
        stale.zeroize();
    }
    credentials.credentials.clear();
    credentials.credentials.push(selected);
    if let Some(active_account_id) = credentials.active_account_id.as_mut() {
        active_account_id.zeroize();
    }
    credentials.active_account_id = Some(account_id.to_owned());
    let result = persist_password_credentials(store, &credentials);
    credentials.zeroize();
    if let Err(error) = result {
        let _ = clear_password_account(store, provider);
        return Err(error);
    }
    if let Err(error) = store.remove(password_login_draft_key(provider)?) {
        let _ = clear_password_account(store, provider);
        return Err(error);
    }
    Ok(())
}

/// Clear every local owner before accepting a different account. Attempt all
/// removals even if one fails, and never allow persistence after an error.
pub fn clear_password_account(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<(), QuarkError> {
    let session = store.remove(provider.key());
    let credential = remove_password_credential(store, provider);
    session.and(credential)
}

pub fn saved_cloud_credential_info(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<Option<super::SavedCloudCredentialInfo>, QuarkError> {
    if !provider_capabilities(provider).password {
        return Ok(None);
    }
    Ok(
        load_saved_password_login(store, provider)?.map(|saved| super::SavedCloudCredentialInfo {
            username: saved.username().to_owned(),
            has_saved_password: !saved.password().is_empty(),
        }),
    )
}

pub fn resolve_password_login(
    store: &impl CredentialStore,
    mut request: super::PasswordLoginRequest,
) -> Result<(Zeroizing<String>, Zeroizing<String>), QuarkError> {
    let raw_username = Zeroizing::new(std::mem::take(&mut request.username));
    let username = Zeroizing::new(raw_username.trim().to_owned());
    let supplied = request
        .password
        .take()
        .map(Zeroizing::new)
        .filter(|password| !password.is_empty());
    if username.is_empty() {
        return Err(QuarkError::InvalidInput("请输入账号和密码".into()));
    }
    let password = match (supplied, request.use_saved_password) {
        (Some(password), false) if !password.is_empty() => password,
        (None, true) => {
            let saved = load_saved_password_login(store, request.provider)?
                .filter(|saved| saved.username() == username.as_str())
                .ok_or_else(|| QuarkError::InvalidInput("请输入密码".into()))?;
            Zeroizing::new(saved.password().to_owned())
        }
        _ => return Err(QuarkError::InvalidInput("请输入密码".into())),
    };
    Ok((username, password))
}

pub(crate) fn uses_password_browser_profile(provider: CloudProvider) -> bool {
    matches!(
        provider,
        CloudProvider::Baidu | CloudProvider::Tianyi | CloudProvider::Xunlei
    )
}

fn password_profile_reset_pending(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<bool, QuarkError> {
    let Some(key) = super::password_profile_reset_key(provider) else {
        return Ok(false);
    };
    match store.load(key)? {
        None => Ok(false),
        Some(value) if value == b"1" => Ok(true),
        Some(_) => Err(QuarkError::Storage),
    }
}

/// A durable marker survives account cleanup and restart. Only a successful
/// browser reset may remove it; a failed reset never permits login to continue.
pub fn reset_password_profile_if_required(
    store: &impl CredentialStore,
    provider: CloudProvider,
    force: bool,
    reset: impl FnOnce() -> Result<(), QuarkError>,
) -> Result<(), QuarkError> {
    if !force && !password_profile_reset_pending(store, provider)? {
        return Ok(());
    }
    let key = super::password_profile_reset_key(provider).ok_or(QuarkError::Storage)?;
    store.save(key, b"1")?;
    reset()?;
    store.remove(key)
}

/// Clear the previous session/profile, but retain the last successful password
/// until the replacement login has actually succeeded.
pub fn prepare_password_account_login(
    store: &impl CredentialStore,
    provider: CloudProvider,
    username: &str,
) -> Result<bool, QuarkError> {
    let pending_reset = password_profile_reset_pending(store, provider)?;
    let credential = load_password_credential(store, provider)?;
    let draft = load_password_login_draft(store, provider)?;
    let owner = if provider == CloudProvider::Baidu {
        baidu_browser_profile_username(store)?
    } else {
        None
    };
    let changed = credential
        .as_ref()
        .is_some_and(|value| value.username() != username)
        || draft
            .as_ref()
            .is_some_and(|value| value.username() != username)
        || owner
            .as_ref()
            .is_some_and(|value| value.as_str() != username)
        || (credential.is_none() && store.load(provider.key())?.is_some());
    if changed {
        if let Some(key) = super::password_profile_reset_key(provider) {
            // Record the obligation before removing the last evidence of the
            // old owner. CredentialStore encrypts and atomically publishes it.
            store.save(key, b"1")?;
        }
        store.remove(provider.key())?;
        if provider == CloudProvider::Baidu {
            set_baidu_browser_profile_username(store, None)?;
            set_browser_password_login_preferred(store, provider, false)?;
        }
    }
    Ok(pending_reset || (changed && uses_password_browser_profile(provider)))
}

pub fn run_with_session_recovery<T>(
    provider: CloudProvider,
    store: &impl CredentialStore,
    kind: SessionWorkKind,
    mut work: impl FnMut() -> Result<T, QuarkError>,
    password_relogin: impl FnOnce(&CloudPasswordCredential) -> Result<(), QuarkError>,
) -> Result<T, QuarkError> {
    if kind != SessionWorkKind::Logout
        && cloud_session_policy(provider).supports_password_fallback
        && super::auth_supervisor::is_manually_disconnected(store, provider)?
    {
        return Err(QuarkError::NotAuthorized);
    }
    let first_result = work();
    let error = match first_result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    if !is_recoverable_authorization_error(&error) || kind == SessionWorkKind::Logout {
        return Err(error);
    }
    if !cloud_session_policy(provider).supports_password_fallback {
        return Err(error);
    }
    let Some(credential) = load_password_credential(store, provider)? else {
        return Err(error);
    };
    password_relogin(&credential)?;
    drop(credential);
    match kind {
        SessionWorkKind::Read => work(),
        SessionWorkKind::Mutation => Err(QuarkError::SessionRenewedRetryOperation),
        SessionWorkKind::Logout => Err(error),
    }
}

fn is_recoverable_authorization_error(error: &QuarkError) -> bool {
    matches!(
        error,
        QuarkError::AuthorizationExpired | QuarkError::NotAuthorized
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
    };

    use super::{
        baidu_browser_profile_username, browser_password_login_preferred, load_password_credential,
        load_password_credentials, load_password_login_draft, load_saved_password_login,
        save_password_credential, save_password_login_draft, select_password_credential_account,
        set_baidu_browser_profile_username, set_browser_password_login_preferred,
        CloudPasswordCredential,
    };
    use crate::cloud::{CloudProvider, CredentialStore, MemoryCredentialStore};

    #[derive(Default)]
    struct FailOnceCredentialStore {
        values: Mutex<BTreeMap<String, Vec<u8>>>,
        fail_next_save: AtomicBool,
        fail_next_draft_remove: AtomicBool,
    }

    impl CredentialStore for FailOnceCredentialStore {
        fn save(&self, key: &str, value: &[u8]) -> Result<(), crate::cloud::QuarkError> {
            if self.fail_next_save.swap(false, Ordering::SeqCst) {
                return Err(crate::cloud::QuarkError::Storage);
            }
            self.values
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_vec());
            Ok(())
        }

        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, crate::cloud::QuarkError> {
            Ok(self.values.lock().unwrap().get(key).cloned())
        }

        fn remove(&self, key: &str) -> Result<(), crate::cloud::QuarkError> {
            if key.contains("password-draft")
                && self.fail_next_draft_remove.swap(false, Ordering::SeqCst)
            {
                return Err(crate::cloud::QuarkError::Storage);
            }
            self.values.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn baidu_password_login_replaces_the_previous_account_instead_of_retaining_it() {
        let store = MemoryCredentialStore::default();
        save_password_credential(
            &store,
            CloudPasswordCredential::new(CloudProvider::Baidu, "account-a", "alice", "secret-a"),
        )
        .unwrap();
        save_password_credential(
            &store,
            CloudPasswordCredential::new(CloudProvider::Baidu, "account-b", "bob", "secret-b"),
        )
        .unwrap();

        let stored = load_password_credentials(&store, CloudProvider::Baidu)
            .unwrap()
            .expect("the replacement credential is stored");
        assert_eq!(stored.credentials.len(), 1);
        assert_eq!(stored.credentials[0].account_id, "account-b");
        let credential = load_password_credential(&store, CloudProvider::Baidu)
            .unwrap()
            .expect("the replacement is active");
        assert_eq!(credential.username(), "bob");
        assert_eq!(credential.password(), "secret-b");

        save_password_login_draft(&store, CloudProvider::Baidu, "bob", "secret-b").unwrap();
        select_password_credential_account(&store, CloudProvider::Baidu, "account-a").unwrap();
        assert!(load_password_credential(&store, CloudProvider::Baidu)
            .unwrap()
            .is_none());
        assert!(load_saved_password_login(&store, CloudProvider::Baidu)
            .unwrap()
            .is_none());
    }

    #[test]
    fn every_password_provider_replaces_the_previous_account() {
        for provider in [CloudProvider::Pikpak, CloudProvider::Baidu] {
            let store = MemoryCredentialStore::default();
            save_password_credential(
                &store,
                CloudPasswordCredential::new(provider, "account-a", "alice", "secret-a"),
            )
            .unwrap();
            save_password_credential(
                &store,
                CloudPasswordCredential::new(provider, "account-b", "bob", "secret-b"),
            )
            .unwrap();

            let stored = load_password_credentials(&store, provider)
                .unwrap()
                .expect("the replacement credential is stored");
            assert_eq!(stored.credentials.len(), 1, "provider: {provider:?}");
            assert_eq!(stored.credentials[0].account_id, "account-b");

            save_password_login_draft(&store, provider, "bob", "secret-b").unwrap();
            select_password_credential_account(&store, provider, "account-a").unwrap();
            assert!(load_password_credential(&store, provider)
                .unwrap()
                .is_none());
            assert!(load_saved_password_login(&store, provider)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn selecting_a_validated_account_discards_an_unbound_login_draft() {
        for provider in [CloudProvider::Pikpak, CloudProvider::Baidu] {
            let store = MemoryCredentialStore::default();
            save_password_credential(
                &store,
                CloudPasswordCredential::new(provider, "account-a", "alice", "secret-a"),
            )
            .unwrap();
            save_password_login_draft(&store, provider, "other-user", "other-secret").unwrap();

            select_password_credential_account(&store, provider, "account-a").unwrap();

            let saved = load_saved_password_login(&store, provider)
                .unwrap()
                .expect("the validated credential remains available");
            assert_eq!(saved.username(), "alice", "provider: {provider:?}");
            assert_eq!(saved.password(), "secret-a", "provider: {provider:?}");
        }
    }

    #[test]
    fn explicit_credential_deletion_removes_the_single_saved_login_for_every_password_provider() {
        for provider in [CloudProvider::Pikpak, CloudProvider::Baidu] {
            let store = MemoryCredentialStore::default();
            save_password_credential(
                &store,
                CloudPasswordCredential::new(provider, "account-a", "alice", "secret-a"),
            )
            .unwrap();
            save_password_login_draft(&store, provider, "alice", "secret-a").unwrap();

            super::remove_password_credential(&store, provider).unwrap();

            assert!(load_saved_password_login(&store, provider)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn failed_account_replacement_preserves_the_old_password() {
        let store = FailOnceCredentialStore::default();
        save_password_credential(
            &store,
            CloudPasswordCredential::new(CloudProvider::Pikpak, "account-a", "alice", "secret-a"),
        )
        .unwrap();
        save_password_login_draft(&store, CloudProvider::Pikpak, "alice", "secret-a").unwrap();
        store.fail_next_save.store(true, Ordering::SeqCst);

        assert!(save_password_credential(
            &store,
            CloudPasswordCredential::new(CloudProvider::Pikpak, "account-b", "bob", "secret-b"),
        )
        .is_err());

        let saved = load_saved_password_login(&store, CloudProvider::Pikpak)
            .unwrap()
            .unwrap();
        assert_eq!(saved.username(), "alice");
        assert_eq!(saved.password(), "secret-a");
    }

    #[test]
    fn failed_draft_cleanup_rolls_back_password_replacement() {
        let store = FailOnceCredentialStore::default();
        let provider = CloudProvider::Pikpak;
        save_password_credential(
            &store,
            CloudPasswordCredential::new(provider, "a", "alice", "secret-a"),
        )
        .unwrap();
        save_password_login_draft(&store, provider, "alice", "secret-a").unwrap();
        store.save(provider.key(), b"new-session").unwrap();
        store.fail_next_draft_remove.store(true, Ordering::SeqCst);
        assert!(save_password_credential(
            &store,
            CloudPasswordCredential::new(provider, "b", "bob", "secret-b")
        )
        .is_err());
        let saved = load_saved_password_login(&store, provider)
            .unwrap()
            .unwrap();
        assert_eq!(saved.username(), "alice");
        assert_eq!(saved.password(), "secret-a");
        assert!(store.load(provider.key()).unwrap().is_none());
    }

    #[test]
    fn failed_draft_cleanup_after_account_selection_removes_the_saved_password_too() {
        let store = FailOnceCredentialStore::default();
        save_password_credential(
            &store,
            CloudPasswordCredential::new(CloudProvider::Pikpak, "account-a", "alice", "secret-a"),
        )
        .unwrap();
        save_password_login_draft(&store, CloudProvider::Pikpak, "other", "other-secret").unwrap();
        store.fail_next_draft_remove.store(true, Ordering::SeqCst);

        assert!(
            select_password_credential_account(&store, CloudProvider::Pikpak, "account-a",)
                .is_err()
        );

        assert!(load_saved_password_login(&store, CloudProvider::Pikpak)
            .unwrap()
            .is_none());
    }

    #[test]
    fn baidu_remembers_to_use_the_stable_browser_after_native_risk_rejection() {
        let store = MemoryCredentialStore::default();
        assert!(!browser_password_login_preferred(&store, CloudProvider::Baidu).unwrap());
        assert!(!browser_password_login_preferred(&store, CloudProvider::Tianyi).unwrap());

        set_browser_password_login_preferred(&store, CloudProvider::Baidu, true).unwrap();
        assert!(browser_password_login_preferred(&store, CloudProvider::Baidu).unwrap());

        set_browser_password_login_preferred(&store, CloudProvider::Baidu, false).unwrap();
        assert!(!browser_password_login_preferred(&store, CloudProvider::Baidu).unwrap());
    }

    #[test]
    fn baidu_browser_profile_records_only_one_username() {
        let store = MemoryCredentialStore::default();
        assert!(baidu_browser_profile_username(&store).unwrap().is_none());

        set_baidu_browser_profile_username(&store, Some(" alice ")).unwrap();
        assert_eq!(
            baidu_browser_profile_username(&store)
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("alice")
        );

        set_baidu_browser_profile_username(&store, Some("bob")).unwrap();
        assert_eq!(
            baidu_browser_profile_username(&store)
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("bob")
        );
    }

    #[test]
    fn baidu_web_account_selection_clears_an_unbound_password_draft() {
        let store = MemoryCredentialStore::default();
        save_password_login_draft(&store, CloudProvider::Baidu, "alice", "secret").unwrap();
        set_baidu_browser_profile_username(&store, Some("alice")).unwrap();

        select_password_credential_account(&store, CloudProvider::Baidu, "account-b").unwrap();

        assert!(load_saved_password_login(&store, CloudProvider::Baidu)
            .unwrap()
            .is_none());
        assert!(baidu_browser_profile_username(&store).unwrap().is_none());
    }

    #[test]
    fn restores_a_legacy_single_account_password_without_discarding_it() {
        let store = MemoryCredentialStore::default();
        store
            .save(
                "cloud-password-baidu",
                br#"{"version":1,"provider":"baidu","account_id":"account-a","username":"alice","password":"secret-a"}"#,
            )
            .unwrap();

        let credential = load_password_credential(&store, CloudProvider::Baidu)
            .unwrap()
            .expect("existing version-one credentials remain usable after the upgrade");
        assert_eq!(credential.account_id(), "account-a");
        assert_eq!(credential.username(), "alice");
        assert_eq!(credential.password(), "secret-a");
    }

    #[test]
    fn remembers_the_latest_password_login_input_even_before_authorization_succeeds() {
        let store = MemoryCredentialStore::default();
        save_password_login_draft(&store, CloudProvider::Pikpak, "first-user", "first-secret")
            .unwrap();
        save_password_login_draft(
            &store,
            CloudProvider::Pikpak,
            "latest-user",
            "latest-secret",
        )
        .unwrap();

        let draft = load_password_login_draft(&store, CloudProvider::Pikpak)
            .unwrap()
            .expect("the latest submitted login remains available to the login form");
        assert_eq!(draft.username(), "latest-user");
        assert_eq!(draft.password(), "latest-secret");
        assert!(load_password_login_draft(&store, CloudProvider::Quark).is_err());
    }

    #[test]
    fn previously_validated_password_is_available_before_a_login_draft_exists() {
        let store = MemoryCredentialStore::default();
        save_password_credential(
            &store,
            CloudPasswordCredential::new(
                CloudProvider::Baidu,
                "account-a",
                "existing-user",
                "existing-secret",
            ),
        )
        .unwrap();

        let saved = load_saved_password_login(&store, CloudProvider::Baidu)
            .unwrap()
            .expect("existing encrypted credentials remain available to the login form");
        assert_eq!(saved.username(), "existing-user");
        assert_eq!(saved.password(), "existing-secret");
    }
}

fn password_login_draft_key(provider: CloudProvider) -> Result<&'static str, QuarkError> {
    if !provider_capabilities(provider).password {
        return Err(QuarkError::InvalidInput(
            "该网盘不支持账号密码登录".to_owned(),
        ));
    }
    match provider {
        CloudProvider::Pikpak => Ok("cloud-password-draft-pikpak"),
        CloudProvider::Baidu => Ok("cloud-password-draft-baidu"),
        CloudProvider::Tianyi => Ok("cloud-password-draft-tianyi"),
        CloudProvider::Xunlei => Ok("cloud-password-draft-xunlei"),
        CloudProvider::Quark
        | CloudProvider::Aliyun
        | CloudProvider::Uc
        | CloudProvider::Mega
        | CloudProvider::Gofile => {
            unreachable!("password providers have a login draft key")
        }
    }
}

pub fn save_password_login_draft(
    store: &impl CredentialStore,
    provider: CloudProvider,
    username: &str,
    password: &str,
) -> Result<(), QuarkError> {
    let username = username.trim();
    if username.is_empty() || password.is_empty() {
        return Err(QuarkError::InvalidInput("请输入账号和密码".into()));
    }
    let mut draft = StoredPasswordLoginDraft {
        version: 1,
        provider,
        username: username.into(),
        password: password.into(),
    };
    let mut bytes = serde_json::to_vec(&draft).map_err(|_| QuarkError::Storage)?;
    let result = store.save(password_login_draft_key(provider)?, &bytes);
    bytes.zeroize();
    draft.zeroize();
    result
}

pub fn load_password_login_draft(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<Option<CloudPasswordLoginDraft>, QuarkError> {
    let Some(mut bytes) = store.load(password_login_draft_key(provider)?)? else {
        return Ok(None);
    };
    let mut stored: StoredPasswordLoginDraft = match serde_json::from_slice(&bytes) {
        Ok(stored) => stored,
        Err(_) => {
            bytes.zeroize();
            return Err(QuarkError::Storage);
        }
    };
    bytes.zeroize();
    if stored.version != 1
        || stored.provider != provider
        || stored.username.trim().is_empty()
        || stored.password.is_empty()
    {
        stored.zeroize();
        return Err(QuarkError::Storage);
    }
    let draft = CloudPasswordLoginDraft {
        provider,
        username: Zeroizing::new(std::mem::take(&mut stored.username)),
        password: Zeroizing::new(std::mem::take(&mut stored.password)),
    };
    stored.zeroize();
    Ok(Some(draft))
}

pub fn load_saved_password_login(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<Option<CloudPasswordLoginDraft>, QuarkError> {
    if let Some(credential) = load_password_credential(store, provider)? {
        return Ok(Some(CloudPasswordLoginDraft {
            provider,
            username: Zeroizing::new(credential.username().to_owned()),
            password: Zeroizing::new(credential.password().to_owned()),
        }));
    }
    load_password_login_draft(store, provider)
}

pub struct CloudPasswordLoginDraft {
    provider: CloudProvider,
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}

impl CloudPasswordLoginDraft {
    pub fn provider(&self) -> CloudProvider {
        self.provider
    }
    pub fn username(&self) -> &str {
        self.username.as_str()
    }
    pub fn password(&self) -> &str {
        self.password.as_str()
    }
}

impl fmt::Debug for CloudPasswordLoginDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloudPasswordLoginDraft")
            .field("provider", &self.provider)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
struct StoredPasswordLoginDraft {
    version: u8,
    provider: CloudProvider,
    username: String,
    password: String,
}

impl Zeroize for StoredPasswordLoginDraft {
    fn zeroize(&mut self) {
        self.username.zeroize();
        self.password.zeroize();
    }
}
