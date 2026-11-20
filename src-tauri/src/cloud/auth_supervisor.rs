//! Durable authorization policy. Session secrets remain in the credential store.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthHealth {
    Disconnected,
    Restoring,
    Ready,
    Refreshing,
    NeedsSilentRelogin,
    NeedsInteractiveAuth,
    RiskBlocked,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthMaintenanceRecord {
    pub schema_version: u32,
    pub provider: CloudProvider,
    pub account_id: Option<String>,
    pub auth_method: Option<CloudAuthMethod>,
    pub session_revision: u64,
    pub expires_at: Option<i64>,
    pub next_refresh_at: Option<i64>,
    pub consecutive_failures: u32,
    pub next_allowed_attempt_at: Option<i64>,
    pub health: AuthHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deferred_failure: Option<DeferredMaintenanceFailure>,
}

// Persist only a category. Provider and transport messages can contain private details.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum DeferredMaintenanceFailure {
    Network,
    Store,
    InvalidResponse,
    Service,
    XunleiMetadataExpired,
    ProfilePending,
}

impl DeferredMaintenanceFailure {
    fn from_error(error: &CloudError) -> Option<Self> {
        match error {
            CloudError::Transport(_) => Some(Self::Network),
            CloudError::Storage => Some(Self::Store),
            CloudError::XunleiVerificationUnavailable("metadata_expired") => Some(Self::XunleiMetadataExpired),
            CloudError::InvalidResponse => Some(Self::InvalidResponse),
            CloudError::Provider(_) | CloudError::InvalidInput(_) => Some(Self::Service),
            _ => None,
        }
    }

    fn as_error(self) -> CloudError {
        match self {
            Self::Network => CloudError::Transport("连接失败".into()),
            Self::Store => CloudError::Storage,
            Self::XunleiMetadataExpired => CloudError::XunleiVerificationUnavailable("metadata_expired"),
            Self::InvalidResponse => CloudError::InvalidResponse,
            Self::Service | Self::ProfilePending => CloudError::Provider("cloud.session.maintenanceDeferred".into()),
        }
    }
}

/// Only an explicitly persisted disconnect blocks automatic recovery. A missing
/// record is a legacy session, not a user request to log out.
pub fn is_manually_disconnected(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<bool, CloudError> {
    let Some(bytes) = store.load(&format!("cloud-auth-maintenance-{}", provider.key()))? else {
        return Ok(false);
    };
    let record: AuthMaintenanceRecord =
        serde_json::from_slice(&bytes).map_err(|_| CloudError::Storage)?;
    if record.schema_version != 1 || record.provider != provider {
        return Err(CloudError::Storage);
    }
    Ok(record.health == AuthHealth::Disconnected)
}

pub struct AuthSupervisor {
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
    jitter: Box<dyn Fn() -> i64 + Send + Sync>,
    locks: HashMap<CloudProvider, tokio::sync::Mutex<()>>,
    // Only complete credential revisions are visible to ordinary operations.
    // This short lock is never held while contacting a provider.
    published: std::sync::Mutex<HashMap<CloudProvider, PublishedSession>>,
    persistence_warning: std::sync::OnceLock<Box<dyn Fn(SessionPersistenceWarning) + Send + Sync>>,
    // Upload-token recovery is independent of the account's read health.
    baidu_upload_retry: std::sync::Mutex<Option<BaiduUploadRecovery>>,
}

/// A local persistence failure must not change the outcome of completed work.
/// This event carries no credentials or raw storage error.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPersistenceWarning {
    pub provider: CloudProvider,
    pub operation_succeeded: bool,
}

struct BaiduUploadRecovery {
    binding: uuid::Uuid,
    next_attempt_at: i64,
    verified_revision: Option<u64>,
}

#[derive(Clone)]
struct PublishedSession {
    revision: u64,
    account_id: Option<String>,
    binding: uuid::Uuid,
    bytes: Option<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RecoveryPhase {
    Proactive,
    AuthFailure,
    ProfileOnly,
    OperationVerification,
    BaiduUploadToken,
}
impl From<bool> for RecoveryPhase {
    fn from(genuine: bool) -> Self {
        if genuine {
            Self::AuthFailure
        } else {
            Self::Proactive
        }
    }
}

// A read snapshots the unconsumed profile check before doing any provider work.
struct ProfileContinuation {
    revision: u64,
    account_id: Option<String>,
    failures: u32,
    next_allowed: Option<i64>,
    binding: uuid::Uuid,
}
impl Default for AuthSupervisor {
    fn default() -> Self {
        Self::new(
            || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            },
            || 30 + (uuid::Uuid::new_v4().as_u128() % 91) as i64,
        )
    }
}
impl AuthSupervisor {
    pub fn new(
        clock: impl Fn() -> i64 + Send + Sync + 'static,
        jitter: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            published: Default::default(),
            persistence_warning: Default::default(),
            baidu_upload_retry: Default::default(),
            clock: Box::new(clock),
            jitter: Box::new(jitter),
            locks: ProviderRegistry::global()
                .all()
                .iter()
                .map(|d| (d.provider, tokio::sync::Mutex::new(())))
                .collect(),
        }
    }
    pub fn on_persistence_warning(
        &self,
        notify: impl Fn(SessionPersistenceWarning) + Send + Sync + 'static,
    ) {
        let _ = self.persistence_warning.set(Box::new(notify));
    }

    fn warn_persistence_failure(&self, provider: CloudProvider, operation_succeeded: bool) {
        eprintln!(
            "[cloud-session] persistence_failed provider={} operation_succeeded={operation_succeeded}",
            provider.key()
        );
        if let Some(notify) = self.persistence_warning.get() {
            notify(SessionPersistenceWarning { provider, operation_succeeded });
        }
    }
    pub fn with_provider<T>(
        &self,
        provider: CloudProvider,
        work: impl FnOnce() -> Result<T, CloudError>,
    ) -> Result<T, CloudError> {
        let _guard = self
            .locks
            .get(&provider)
            .ok_or(CloudError::ProviderUnsupported)?
            .blocking_lock();
        work()
    }
    pub fn record(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
    ) -> Result<AuthMaintenanceRecord, CloudError> {
        if let Some(bytes) = store.load(&format!("cloud-auth-maintenance-{}", provider.key()))? {
            let record: AuthMaintenanceRecord =
                serde_json::from_slice(&bytes).map_err(|_| CloudError::Storage)?;
            if record.schema_version != 1 || record.provider != provider {
                return Err(CloudError::Storage);
            }
            return Ok(record);
        }
        Ok(AuthMaintenanceRecord {
            schema_version: 1,
            provider,
            account_id: None,
            auth_method: None,
            session_revision: 0,
            expires_at: None,
            next_refresh_at: None,
            consecutive_failures: 0,
            next_allowed_attempt_at: None,
            health: AuthHealth::Disconnected,
            deferred_failure: None,
        })
    }

    /// Removes an in-memory snapshot after its provider's durable credentials
    /// have been deleted. This prevents a decommissioned provider from
    /// retaining a session until the next application restart.
    pub fn forget(&self, provider: CloudProvider) -> Result<(), CloudError> {
        self.published
            .lock()
            .map_err(|_| CloudError::Storage)?
            .remove(&provider);
        Ok(())
    }
    fn publish_locked(
        &self,
        store: &impl CredentialStore,
        record: &AuthMaintenanceRecord,
        explicit_authorization: bool,
    ) -> Result<(), CloudError> {
        let bytes = store.load(record.provider.key())?;
        let mut published = self.published.lock().map_err(|_| CloudError::Storage)?;
        let binding = published
            .get(&record.provider)
            .filter(|previous| {
                !explicit_authorization
                    || (record.account_id.is_some() && previous.account_id == record.account_id)
            })
            .map(|previous| previous.binding)
            .unwrap_or_else(uuid::Uuid::new_v4);
        published.insert(
            record.provider,
            PublishedSession {
                revision: record.session_revision,
                account_id: record.account_id.clone(),
                binding,
                bytes,
            },
        );
        Ok(())
    }
    fn seed_locked(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
    ) -> Result<(), CloudError> {
        if !self
            .published
            .lock()
            .map_err(|_| CloudError::Storage)?
            .contains_key(&provider)
        {
            self.publish_locked(store, &self.record(store, provider)?, false)?;
        }
        Ok(())
    }
    fn snapshot(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
    ) -> Result<SessionSnapshot, CloudError> {
        // Legacy native entry points may update the credential store without a
        // maintenance record. Reconcile only when no writer owns the provider;
        // an unversioned replacement starts a fresh binding (never a retry grant).
        if let Ok(_guard) = self
            .locks
            .get(&provider)
            .ok_or(CloudError::ProviderUnsupported)?
            .try_lock()
        {
            let record = self.record(store, provider)?;
            let bytes = store.load(provider.key())?;
            let mut published = self.published.lock().map_err(|_| CloudError::Storage)?;
            if published.get(&provider).is_none_or(|previous| {
                previous.revision != record.session_revision
                    || previous.account_id != record.account_id
                    || previous.bytes != bytes
            }) {
                published.insert(
                    provider,
                    PublishedSession {
                        revision: record.session_revision,
                        account_id: record.account_id,
                        binding: uuid::Uuid::new_v4(),
                        bytes,
                    },
                );
            }
        }
        let published = self
            .published
            .lock()
            .map_err(|_| CloudError::Storage)?
            .get(&provider)
            .cloned();
        let published = match published {
            Some(value) => value,
            None => self.with_provider(provider, || {
                self.seed_locked(store, provider)?;
                self.published
                    .lock()
                    .map_err(|_| CloudError::Storage)?
                    .get(&provider)
                    .cloned()
                    .ok_or(CloudError::Storage)
            })?,
        };
        SessionSnapshot::from_published(provider, published)
    }
    fn save(
        &self,
        store: &impl CredentialStore,
        record: &AuthMaintenanceRecord,
    ) -> Result<(), CloudError> {
        store.save(
            &format!("cloud-auth-maintenance-{}", record.provider.key()),
            &serde_json::to_vec(record).map_err(|_| CloudError::Storage)?,
        )
    }
    fn mark_ready(&self, record: &mut AuthMaintenanceRecord, expires: Option<i64>) {
        let now = (self.clock)();
        record.session_revision = record.session_revision.saturating_add(1);
        record.expires_at = expires;
        record.next_refresh_at = if matches!(
            record.provider,
            CloudProvider::Quark | CloudProvider::Baidu | CloudProvider::Uc
        ) {
            None
        } else {
            expires.map(|expiry| {
                let ttl = expiry.saturating_sub(now).max(0);
                now.saturating_add(ttl.saturating_mul(4) / 5)
                    .min(expiry.saturating_sub(300))
                    .saturating_sub((self.jitter)().clamp(30, 120))
                    .max(now)
            })
        };
        record.consecutive_failures = 0;
        record.next_allowed_attempt_at = None;
        record.deferred_failure = None;
        record.health = AuthHealth::Ready;
    }
    pub fn ready(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
        account: Option<String>,
        method: Option<CloudAuthMethod>,
        expires: Option<i64>,
    ) -> Result<(), CloudError> {
        self.with_provider(provider, || {
            self.ready_locked(store, provider, account, method, expires)
        })
    }
    /// Interactive commands already own the provider guard while replacing credentials.
    pub(crate) fn ready_locked(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
        account: Option<String>,
        method: Option<CloudAuthMethod>,
        expires: Option<i64>,
    ) -> Result<(), CloudError> {
        let mut record = self.record(store, provider)?;
        record.account_id = account;
        record.auth_method = method;
        self.mark_ready(&mut record, expires);
        self.save(store, &record)?;
        self.publish_locked(store, &record, true)
    }
    /// Publish a read-side owner observation only for the session that produced it.
    /// In particular, ordinary account reads do not renew the recovery budget.
    pub(crate) fn observe_account(
        &self,
        store: &impl CredentialStore,
        observed: &SessionSnapshot,
        account: &CloudAccount,
    ) -> Result<(), CloudError> {
        let provider = observed.provider;
        self.with_provider(provider, || {
            let current = self.snapshot(store, provider)?;
            if current.initial.is_none() || is_manually_disconnected(store, provider)? {
                return Err(CloudError::NotAuthorized);
            }
            if current.binding != observed.binding || account.provider != provider {
                return Err(CloudError::AccountMismatch);
            }
            if (current.revision != observed.revision || store.load(provider.key())? != observed.initial)
                && current.account_id.as_ref() != Some(&account.id)
            {
                return Err(CloudError::AccountMismatch);
            }
            self.record_verified_account_locked(store, account)
        })
    }

    pub(crate) fn record_verified_account_locked(
        &self,
        store: &impl CredentialStore,
        account: &CloudAccount,
    ) -> Result<(), CloudError> {
        let provider = account.provider;
        let mut record = self.record(store, provider)?;
        if record.health == AuthHealth::RiskBlocked {
            // An identity lookup can succeed while the provider still requires
            // the user to complete a challenge. Only explicit authorization clears it.
            if record.account_id.as_ref().is_some_and(|owner| owner != &account.id) {
                return Err(CloudError::AccountMismatch);
            }
            return Ok(());
        }
        if record.account_id.as_ref() == Some(&account.id)
            && record.auth_method == Some(account.auth_method)
        {
            // The account is usable again. Reads do not grant a new credential
            // revision or reset the attempt budget for an unrelated operation.
            if record.health != AuthHealth::Ready {
                record.health = AuthHealth::Ready;
                self.save(store, &record)?;
            }
            return Ok(());
        }
        let replaces_owner = record.account_id.is_some() && record.account_id.as_ref() != Some(&account.id);
        record.account_id = Some(account.id.clone());
        record.auth_method = Some(account.auth_method);
        self.mark_ready(&mut record, session_expiry(store, provider)?);
        self.save(store, &record)?;
        self.publish_locked(store, &record, replaces_owner)
    }

    pub fn disconnect(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
    ) -> Result<(), CloudError> {
        self.with_provider(provider, || self.disconnect_locked(store, provider))
    }
    pub(crate) fn disconnect_locked(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
    ) -> Result<(), CloudError> {
        let mut record = self.record(store, provider)?;
        self.mark_ready(&mut record, None);
        record.account_id = None;
        record.auth_method = None;
        record.health = AuthHealth::Disconnected;
        self.save(store, &record)?;
        self.publish_locked(store, &record, true)
    }
    pub fn restore(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
    ) -> Result<(), CloudError> {
        if store
            .load(&format!("cloud-auth-maintenance-{}", provider.key()))?
            .is_some()
        {
            return self.with_provider(provider, || self.seed_locked(store, provider));
        }
        if store.load(provider.key())?.is_none() {
            return self.with_provider(provider, || self.seed_locked(store, provider));
        }
        let mut record = self.record(store, provider)?;
        record.health = AuthHealth::Restoring;
        self.save(store, &record)?;
        self.ready(
            store,
            provider,
            None,
            None,
            session_expiry(store, provider)?,
        )
    }
    pub fn is_due(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
    ) -> Result<bool, CloudError> {
        let record = self.record(store, provider)?;
        Ok(!matches!(
            record.health,
            AuthHealth::Disconnected | AuthHealth::RiskBlocked | AuthHealth::NeedsInteractiveAuth
        ) && record.consecutive_failures < 3
            && record
                .next_allowed_attempt_at
                .is_none_or(|next| next <= (self.clock)())
            && record
                .next_refresh_at
                .is_some_and(|next| next <= (self.clock)()))
    }
    pub fn maintain(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
        revision: Option<u64>,
        auth_error: bool,
        work: impl FnOnce() -> Result<(), CloudError>,
    ) -> Result<(), CloudError> {
        self.with_provider(provider, || {
            self.seed_locked(store, provider)?;
            if is_manually_disconnected(store, provider)? {
                return Err(CloudError::NotAuthorized);
            }
            let mut record = self.record(store, provider)?;
            if revision.is_some_and(|revision| revision != record.session_revision) {
                return Ok(());
            }
            if record.consecutive_failures >= 3 || record.health == AuthHealth::RiskBlocked {
                return Err(CloudError::WebAuthorizationRequired);
            }
            if record.next_allowed_attempt_at.is_some_and(|next| next > (self.clock)()) {
                return Err(record.deferred_failure.map_or(
                    CloudError::WebAuthorizationRequired,
                    DeferredMaintenanceFailure::as_error,
                ));
            }
            if !auth_error && !self.is_due(store, provider)? {
                return Ok(());
            }
            let previous_health = record.health;
            record.health = if auth_error {
                AuthHealth::NeedsSilentRelogin
            } else {
                AuthHealth::Refreshing
            };
            self.reserve_attempt(store, &mut record)?;
            match work() {
                Ok(()) => {
                    let expires = session_expiry(store, provider)?;
                    self.mark_ready(&mut record, expires);
                    self.save(store, &record)?;
                    self.publish_locked(store, &record, false)
                }
                Err(error) => {
                    if !auth_error && matches!(error, CloudError::WebAuthorizationRequired) {
                        // Scheduled refresh cannot inspect a browser profile. Let the
                        // next real authentication failure complete this attempt.
                        record.deferred_failure = Some(DeferredMaintenanceFailure::ProfilePending);
                        record.next_refresh_at = None;
                        record.health = previous_health;
                        self.save(store, &record)?;
                    } else {
                        self.record_failure(store, &mut record, previous_health, &error)?;
                    }
                    Err(error)
                }
            }
        })
    }
    fn reserve_attempt(
        &self,
        store: &impl CredentialStore,
        record: &mut AuthMaintenanceRecord,
    ) -> Result<(), CloudError> {
        let delay = match record.consecutive_failures {
            0 => 300,
            1 => 1800,
            _ => 7200,
        };
        // A process exit mid-request establishes a cooldown, never a rejection.
        record.deferred_failure = Some(DeferredMaintenanceFailure::Service);
        record.next_allowed_attempt_at = Some((self.clock)().saturating_add(delay));
        self.save(store, record)
    }

    fn record_failure(
        &self,
        store: &impl CredentialStore,
        record: &mut AuthMaintenanceRecord,
        previous_health: AuthHealth,
        error: &CloudError,
    ) -> Result<(), CloudError> {
        record.deferred_failure = DeferredMaintenanceFailure::from_error(error);
        if record.deferred_failure.is_none() {
            record.consecutive_failures = record.consecutive_failures.saturating_add(1);
        }
        record.health = if matches!(error, CloudError::XunleiVerificationUnavailable("metadata_expired")) {
            previous_health
        } else if matches!(error, CloudError::CaptchaRequired) {
            AuthHealth::RiskBlocked
        } else if record.consecutive_failures >= 3 {
            AuthHealth::NeedsInteractiveAuth
        } else {
            AuthHealth::NeedsSilentRelogin
        };
        self.save(store, record)
    }

    pub fn run<T>(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
        kind: SessionWorkKind,
        work: impl FnMut(&dyn CredentialStore) -> Result<T, CloudError>,
        recover: impl FnMut(RecoveryPhase) -> Result<(), CloudError>,
    ) -> Result<T, CloudError> {
        self.run_checked(store, provider, kind, work, recover, || Ok(()))
    }

    /// A cancelled or timed-out worker cannot persist private credentials or
    /// enter recovery. Ordinary provider errors may still carry valid rotation.
    pub fn run_checked<T>(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
        kind: SessionWorkKind,
        mut work: impl FnMut(&dyn CredentialStore) -> Result<T, CloudError>,
        recover: impl FnMut(RecoveryPhase) -> Result<(), CloudError>,
        check: impl Fn() -> Result<(), CloudError>,
    ) -> Result<T, CloudError> {
        self.run_with_snapshot(store, provider, kind, |credentials, _| work(credentials), recover, check)
    }

    pub(crate) fn run_with_snapshot<T>(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
        kind: SessionWorkKind,
        mut work: impl FnMut(&dyn CredentialStore, Option<&SessionSnapshot>) -> Result<T, CloudError>,
        mut recover: impl FnMut(RecoveryPhase) -> Result<(), CloudError>,
        check: impl Fn() -> Result<(), CloudError>,
    ) -> Result<T, CloudError> {
        check()?;
        if kind == SessionWorkKind::Logout {
            return self.with_provider(provider, || work(store, None));
        }
        if is_manually_disconnected(store, provider)? {
            return Err(CloudError::NotAuthorized);
        }
        if self.is_due(store, provider)? {
            let revision = self.record(store, provider)?.session_revision;
            // A proactive refresh failure must not stop still-valid ordinary reads.
            let _ = self.maintain(store, provider, Some(revision), false, || {
                check()?;
                recover(RecoveryPhase::Proactive)
            });
        }
        let snapshot = self.snapshot(store, provider)?;
        let revision = snapshot.revision;
        let record = self.record(store, provider)?;
        let mut continuation = if matches!(record.deferred_failure, Some(DeferredMaintenanceFailure::ProfilePending))
            && record.session_revision == revision && record.account_id == snapshot.account_id
        {
            Some(ProfileContinuation {
                revision,
                account_id: record.account_id,
                failures: record.consecutive_failures,
                next_allowed: record.next_allowed_attempt_at,
                binding: snapshot.binding,
            })
        } else {
            None
        };
        check()?;
        let result = work(&snapshot, Some(&snapshot));
        check()?;
        let revision = match self.persist_snapshot(store, provider, revision, &snapshot, &check) {
            Ok(revision) => revision,
            Err(CloudError::Storage) => {
                self.warn_persistence_failure(provider, result.is_ok());
                // The provider may already have deleted/created/uploaded data.
                // Report its actual result; never recover or replay this work
                // because saving its rotated credentials failed locally.
                return result;
            }
            Err(error) => return Err(error),
        };
        if let Some(ticket) = continuation.as_mut() {
            // Only this operation's successful CAS may carry its ticket to a
            // new revision. This does not grant recovery: the original durable
            // attempt, account and lineage are rechecked under the lock below.
            if revision != snapshot.revision
                && ticket.revision == snapshot.revision
                && ticket.account_id == snapshot.account_id
                && ticket.binding == snapshot.binding
            {
                ticket.revision = revision;
            }
        }
        match result {
            Err(CloudError::BaiduUploadAuthorizationRequired(
                BaiduUploadAuthStage::Token,
            )) if provider == CloudProvider::Baidu && kind == SessionWorkKind::Mutation => {
                let next = self.recover_baidu_upload_token(
                    store,
                    revision,
                    &snapshot,
                    || recover(RecoveryPhase::BaiduUploadToken),
                    &check,
                )?;
                check()?;
                let result = work(&next, Some(&next));
                if let Err(CloudError::Storage) =
                    self.persist_snapshot(store, provider, next.revision, &next, &check)
                {
                    self.warn_persistence_failure(provider, result.is_ok());
                }
                // The second invocation may have dispatched a mutation.
                // Preserve its outcome and do not enter recovery again.
                result
            }
            Err(error @ CloudError::CaptchaRequired) => {
                self.with_provider(provider, || {
                    let mut record = self.record(store, provider)?;
                    if record.session_revision == revision {
                        record.health = AuthHealth::RiskBlocked;
                        self.save(store, &record)?;
                    }
                    Ok(())
                })?;
                Err(error)
            }
            Err(error)
                if matches!(
                    error,
                    CloudError::AuthorizationExpired
                        | CloudError::NotAuthorized
                        | CloudError::WebAuthorizationRequired
                        | CloudError::BaiduUploadWebSessionRequired
                ) || (provider == CloudProvider::Xunlei
                    && matches!(error, CloudError::XunleiVerificationUnavailable(
                        "metadata_missing" | "metadata_expired" | "timestamp_invalid"
                    ))) =>
            {
                let auth_failure_phase = if snapshot.initial.is_none() {
                    // Cold starts can lose the native session while an account-bound
                    // browser profile or a previously saved login still exists.
                    if kind != SessionWorkKind::Read || !matches!(error, CloudError::NotAuthorized) {
                        return Err(error);
                    }
                    let saved = if cloud_session_policy(provider).supports_password_fallback {
                        load_password_credential(store, provider)?
                    } else {
                        None
                    };
                    match (snapshot.account_id.as_deref(), saved.as_ref()) {
                        (None, None) => return Err(error),
                        (Some(owner), Some(login)) if owner != login.account_id() => {
                            RecoveryPhase::ProfileOnly
                        }
                        _ => RecoveryPhase::AuthFailure,
                    }
                } else {
                    if store.load(provider.key())?.is_none() {
                        return Err(error);
                    }
                    RecoveryPhase::AuthFailure
                };
                let mut recover = |phase| {
                    if provider != CloudProvider::Xunlei
                        || !matches!(error, CloudError::WebAuthorizationRequired)
                    {
                        return recover(phase);
                    }
                    let previous_bytes = store.load(provider.key())?.ok_or(CloudError::Storage)?;
                    let previous: xunlei::XunleiSession = serde_json::from_slice(&previous_bytes)
                        .map_err(|_| CloudError::Storage)?;
                    if !previous.profile.is_modern_web() {
                        return recover(phase);
                    }
                    // A successful Bearer refresh does not replace a rejected
                    // web verification ticket. Require the browser recovery to
                    // supply a different usable ticket before reporting renewal.
                    recover(RecoveryPhase::ProfileOnly)?;
                    let current: xunlei::XunleiSession = serde_json::from_slice(
                        &store.load(provider.key())?.ok_or(CloudError::Storage)?,
                    )
                    .map_err(|_| CloudError::Storage)?;
                    if current.web_security.captcha_token.is_empty()
                        || current.web_security.captcha_token == previous.web_security.captcha_token
                        || current.web_security.expires_at <= (self.clock)().max(0) as u64 + 30
                    {
                        store.save(provider.key(), &previous_bytes)?;
                        return Err(CloudError::WebAuthorizationRequired);
                    }
                    Ok(())
                };
                // A confirmed expiry during upload uses ordinary silent maintenance.
                // Retain the file-specific continuation only if maintenance needs the user.
                let upload_recovery_error = |failure| match (&error, failure) {
                    (CloudError::BaiduUploadWebSessionRequired, CloudError::WebAuthorizationRequired) =>
                        CloudError::BaiduUploadWebSessionRequired,
                    (_, other) => other,
                };
                if matches!(error, CloudError::XunleiVerificationUnavailable("metadata_expired")) {
                    let recovery = self.maintain(store, provider, Some(revision), true, || {
                        check()?;
                        recover(RecoveryPhase::OperationVerification).map_err(|failure| {
                            match failure {
                                CloudError::WebAuthorizationRequired | CloudError::AuthorizationPending =>
                                    CloudError::XunleiVerificationUnavailable("metadata_expired"),
                                other => other,
                            }
                        })
                    });
                    if let Err(recovery_error) = recovery {
                        check()?;
                        return Err(match recovery_error {
                            CloudError::WebAuthorizationRequired | CloudError::AuthorizationPending => error,
                            other => other,
                        });
                    }
                } else if let Some(ticket) = continuation.take() {
                    self.with_provider(provider, || {
                        let mut record = self.record(store, provider)?;
                        let current = self.snapshot(store, provider)?;
                        if current.binding != ticket.binding
                            || current.account_id != ticket.account_id
                        {
                            return Err(CloudError::NotAuthorized);
                        }
                        if record.session_revision != ticket.revision {
                            return Ok(());
                        }
                        if record.health == AuthHealth::RiskBlocked {
                            return Err(CloudError::WebAuthorizationRequired);
                        }
                        if !matches!(record.deferred_failure, Some(DeferredMaintenanceFailure::ProfilePending)) {
                            return Err(record.deferred_failure.map_or(
                                CloudError::WebAuthorizationRequired,
                                DeferredMaintenanceFailure::as_error,
                            ));
                        }
                        if record.consecutive_failures != ticket.failures
                            || record.next_allowed_attempt_at != ticket.next_allowed
                        {
                            return Err(CloudError::WebAuthorizationRequired);
                        }
                        check()?;
                        let previous_health = record.health;
                        self.reserve_attempt(store, &mut record)?;
                        match recover(RecoveryPhase::ProfileOnly) {
                            Ok(()) => {
                                self.mark_ready(&mut record, session_expiry(store, provider)?);
                                self.save(store, &record)?;
                                self.publish_locked(store, &record, false)
                            }
                            Err(error) => {
                                self.record_failure(store, &mut record, previous_health, &error)?;
                                Err(error)
                            }
                        }
                    }).map_err(upload_recovery_error)?;
                } else {
                    self.maintain(store, provider, Some(revision), true, || {
                        check()?;
                        recover(auth_failure_phase)
                    }).map_err(upload_recovery_error)?;
                }
                let next = self.snapshot(store, provider)?;
                if next.initial.is_none()
                    || next.binding != snapshot.binding
                    || next.account_id != snapshot.account_id
                {
                    return Err(CloudError::NotAuthorized);
                }
                if kind == SessionWorkKind::Mutation {
                    if cfg!(debug_assertions) && provider == CloudProvider::Xunlei
                        && std::env::var("VERSADOWN_XUNLEI_DIAGNOSTICS").as_deref() == Ok("1")
                    {
                        eprintln!("[xunlei-recovery] session_changed={}", next.initial != snapshot.load(provider.key())?);
                    }
                    return Err(CloudError::SessionRenewedRetryOperation);
                }
                let revision = next.revision;
                check()?;
                let result = work(&next, Some(&next));
                check()?;
                let revision = match self.persist_snapshot(store, provider, revision, &next, &check) {
                    Ok(revision) => revision,
                    Err(CloudError::Storage) => {
                        self.warn_persistence_failure(provider, result.is_ok());
                        return result;
                    }
                    Err(error) => return Err(error),
                };
                if matches!(result, Err(CloudError::CaptchaRequired)) {
                    self.with_provider(provider, || {
                        let mut record = self.record(store, provider)?;
                        if record.session_revision == revision {
                            record.health = AuthHealth::RiskBlocked;
                            self.save(store, &record)?;
                        }
                        Ok(())
                    })?;
                }
                result
            }
            result => result,
        }
    }
    fn recover_baidu_upload_token(
        &self,
        store: &impl CredentialStore,
        revision: u64,
        original: &SessionSnapshot,
        recover: impl FnOnce() -> Result<(), CloudError>,
        check: &impl Fn() -> Result<(), CloudError>,
    ) -> Result<SessionSnapshot, CloudError> {
        let provider = CloudProvider::Baidu;
        let upload_token_error =
            || CloudError::BaiduUploadAuthorizationRequired(BaiduUploadAuthStage::Token);
        self.with_provider(provider, || {
            let current = self.snapshot(store, provider)?;
            let mut record = self.record(store, provider)?;
            if current.account_id != original.account_id || current.binding != original.binding {
                return Err(CloudError::AccountMismatch);
            }
            if record.session_revision != current.revision
                || record.account_id != current.account_id
                || current.initial != store.load(provider.key())?
                || current.account_id.is_none()
                || matches!(record.health, AuthHealth::Disconnected | AuthHealth::RiskBlocked)
            {
                return Err(upload_token_error());
            }
            if current.revision != revision && current.initial.is_some()
                && self.baidu_upload_retry.lock().map_err(|_| CloudError::Storage)?
                    .as_ref().is_some_and(|attempt| attempt.binding == current.binding
                        && attempt.verified_revision == Some(current.revision))
            {
                // Another upload already verified and published this exact revision.
                return Ok(current);
            }
            if current.revision != revision {
                return Err(upload_token_error());
            }
            let previous = current.initial.as_deref().ok_or_else(upload_token_error)?;
            check()?;
            {
                let mut attempt = self.baidu_upload_retry.lock().map_err(|_| CloudError::Storage)?;
                let now = (self.clock)();
                if attempt.as_ref().is_some_and(|attempt| attempt.binding == current.binding && now < attempt.next_attempt_at) {
                    return Err(upload_token_error());
                }
                *attempt = Some(BaiduUploadRecovery {
                    binding: current.binding,
                    next_attempt_at: now.saturating_add(300),
                    verified_revision: None,
                });
            }

            // The browser validates the captured account and write token in
            // isolation before publishing credentials to the store.
            let recovered = recover();
            let checked = check();
            let verified = store.load(provider.key())?;
            if recovered.is_err() || checked.is_err() || verified.is_none() {
                // A completed provider callback may have published just
                // before cancellation. Restore the original credentials
                // before releasing this provider's lock.
                if verified.as_deref() != Some(previous) {
                    store.save(provider.key(), previous)?;
                }
                checked?;
                return match recovered {
                    Err(error @ (CloudError::Transport(_) | CloudError::Storage | CloudError::AccountMismatch
                        | CloudError::BaiduDriveVerificationRequired)) => Err(error),
                    _ => Err(upload_token_error()),
                };
            }
            record.session_revision = record.session_revision.saturating_add(1);
            self.save(store, &record)?;
            self.publish_locked(store, &record, false)?;
            if let Some(attempt) = self.baidu_upload_retry.lock().map_err(|_| CloudError::Storage)?.as_mut() {
                attempt.verified_revision = Some(record.session_revision);
            }
            self.snapshot(store, provider)
        })
    }

    fn persist_snapshot(
        &self,
        store: &impl CredentialStore,
        provider: CloudProvider,
        revision: u64,
        snapshot: &SessionSnapshot,
        check: &impl Fn() -> Result<(), CloudError>,
    ) -> Result<u64, CloudError> {
        let current = snapshot.load(provider.key())?;
        if current == snapshot.initial {
            return Ok(revision);
        }
        self.with_provider(provider, || {
            check()?;
            let record = self.record(store, provider)?;
            let published = self
                .published
                .lock()
                .map_err(|_| CloudError::Storage)?
                .get(&provider)
                .cloned()
                .ok_or(CloudError::Storage)?;
            if record.session_revision != revision
                || record.account_id != snapshot.account_id
                || published.binding != snapshot.binding
                || published.revision != revision
                || published.account_id != snapshot.account_id
                || store.load(provider.key())? != snapshot.initial
            {
                return Ok(revision);
            }
            if let Some(bytes) = current {
                store.save(provider.key(), &bytes)?;
                let mut record = record;
                // A changed action ticket/cookie is a new credential revision,
                // not proof of a successful login. Preserve recovery backoff.
                record.session_revision = record.session_revision.saturating_add(1);
                self.save(store, &record)?;
                self.publish_locked(store, &record, false)?;
                return Ok(record.session_revision);
            }
            Ok(revision)
        })
    }
}

/// Fixed recovery order shared by foreground work and scheduled maintenance.
/// Only explicit authentication failures permit advancing to another method.
pub fn recover_in_order(
    provider: CloudProvider,
    store: &impl CredentialStore,
    auth: &mut dyn driver::ProviderAuth,
    phase: impl Into<RecoveryPhase>,
    mut profile: impl FnMut() -> Result<bool, CloudError>,
) -> Result<(), CloudError> {
    if is_manually_disconnected(store, provider)? {
        return Err(CloudError::NotAuthorized);
    }
    let phase = phase.into();
    fn auth_failure(error: &CloudError) -> bool {
        matches!(
            error,
            CloudError::AuthorizationExpired
                | CloudError::NotAuthorized
                | CloudError::WebAuthorizationRequired
        )
    }
    let prefer_existing_profile = provider == CloudProvider::Baidu && phase == RecoveryPhase::AuthFailure;
    if prefer_existing_profile {
        match profile() {
            Ok(true) => return Ok(()),
            Ok(false) | Err(CloudError::AuthorizationPending) => {}
            Err(error) => return Err(error),
        }
    }
    if !matches!(
        phase,
        RecoveryPhase::ProfileOnly
            | RecoveryPhase::OperationVerification
            | RecoveryPhase::BaiduUploadToken
    ) {
        match auth.refresh_session() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if auth_failure(&error) => {}
            Err(error) => return Err(error),
        }
        if cloud_session_policy(provider).supports_password_fallback {
            if let Some(credential) = load_password_credential(store, provider)? {
                // A browser-only password preference still uses its persistent profile.
                if !browser_password_login_preferred(store, provider)? {
                    match auth.recover_password(&credential) {
                        Ok((account, session)) => {
                            if account.id.is_empty() || account.id != credential.account_id() {
                                return Err(CloudError::WebAuthorizationRequired);
                            }
                            store.save(provider.key(), &session)?;
                            return Ok(());
                        }
                        Err(error) if auth_failure(&error) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }
    if !prefer_existing_profile && phase != RecoveryPhase::Proactive {
        match profile() {
            Ok(true) => return Ok(()),
            Ok(false) | Err(CloudError::AuthorizationPending) => {}
            Err(error) => return Err(error),
        }
    }
    Err(CloudError::WebAuthorizationRequired)
}

/// Operations see a private copy. Writes stay local until revision validation.
pub struct SessionSnapshot {
    provider: CloudProvider,
    initial: Option<Vec<u8>>,
    store: MemoryCredentialStore,
    revision: u64,
    account_id: Option<String>,
    binding: uuid::Uuid,
}
impl SessionSnapshot {
    pub fn new(store: &impl CredentialStore, provider: CloudProvider) -> Result<Self, CloudError> {
        let initial = store.load(provider.key())?;
        Self::from_published(
            provider,
            PublishedSession {
                revision: 0,
                account_id: None,
                binding: uuid::Uuid::new_v4(),
                bytes: initial,
            },
        )
    }
    fn from_published(
        provider: CloudProvider,
        published: PublishedSession,
    ) -> Result<Self, CloudError> {
        let snapshot = Self {
            provider,
            initial: published.bytes,
            store: MemoryCredentialStore::default(),
            revision: published.revision,
            account_id: published.account_id,
            binding: published.binding,
        };
        if let Some(bytes) = &snapshot.initial {
            snapshot.store.save(provider.key(), bytes)?;
        }
        Ok(snapshot)
    }
}
impl CredentialStore for SessionSnapshot {
    fn save(&self, key: &str, bytes: &[u8]) -> Result<(), CloudError> {
        if key != self.provider.key() {
            return Err(CloudError::Storage);
        }
        self.store.save(key, bytes)
    }
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, CloudError> {
        self.store.load(key)
    }
    fn remove(&self, key: &str) -> Result<(), CloudError> {
        self.store.remove(key)
    }
    fn automatic_session_recovery(&self) -> bool {
        false
    }
}

pub fn session_expiry(
    store: &impl CredentialStore,
    provider: CloudProvider,
) -> Result<Option<i64>, CloudError> {
    let Some(bytes) = store.load(provider.key())? else {
        return Ok(None);
    };
    // Cookie providers and legacy sessions may not be JSON or know their TTL.
    Ok(serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("expires_at").and_then(|v| v.as_i64()))
        .filter(|expiry| *expiry > 0))
}

#[test]
fn ordinary_account_reads_preserve_a_pending_security_challenge_across_restart() {
    let provider = CloudProvider::Aliyun;
    let store = MemoryCredentialStore::default();
    store.save(provider.key(), br#"{"expires_at":2000}"#).unwrap();
    let supervisor = AuthSupervisor::new(|| 1000, || 30);
    let account = CloudAccount {
        provider, id: "owner".into(), display_name: "test".into(), auth_method: CloudAuthMethod::Web,
    };
    supervisor.ready(&store, provider, Some(account.id.clone()), Some(account.auth_method), Some(2000)).unwrap();
    let result = supervisor.run::<()>(&store, provider, SessionWorkKind::Read,
        |_| Err(CloudError::CaptchaRequired), |_| panic!("explicit verification required"),
    );
    assert!(matches!(result, Err(CloudError::CaptchaRequired)));
    let observation = supervisor.snapshot(&store, provider).unwrap();
    supervisor.observe_account(&store, &observation, &account).unwrap();
    let restarted = AuthSupervisor::new(|| 4000, || 30);
    restarted.restore(&store, provider).unwrap();
    assert_eq!(restarted.record(&store, provider).unwrap().health, AuthHealth::RiskBlocked);
    assert!(!restarted.is_due(&store, provider).unwrap());
    restarted.ready(&store, provider, Some(account.id), Some(account.auth_method), None).unwrap();
    assert_eq!(restarted.record(&store, provider).unwrap().health, AuthHealth::Ready);
}
