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

// This capability never leaves one run invocation or survives a restart.
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
            clock: Box::new(clock),
            jitter: Box::new(jitter),
            locks: ProviderRegistry::global()
                .all()
                .iter()
                .map(|d| (d.provider, tokio::sync::Mutex::new(())))
                .collect(),
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
            if record.consecutive_failures >= 3
                || record.health == AuthHealth::RiskBlocked
                || record
                    .next_allowed_attempt_at
                    .is_some_and(|next| next > (self.clock)())
            {
                return Err(CloudError::WebAuthorizationRequired);
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
            // Reserve the attempt durably before contacting the provider. A crash
            // cannot create a tight login loop on the next launch.
            record.consecutive_failures = record.consecutive_failures.saturating_add(1);
            record.next_allowed_attempt_at = Some((self.clock)().saturating_add(
                match record.consecutive_failures {
                    1 => 300,
                    2 => 1800,
                    _ => 7200,
                },
            ));
            self.save(store, &record)?;
            match work() {
                Ok(()) => {
                    let expires = session_expiry(store, provider)?;
                    self.mark_ready(&mut record, expires);
                    self.save(store, &record)?;
                    self.publish_locked(store, &record, false)
                }
                Err(error) => {
                    record.health = if matches!(
                        error,
                        CloudError::XunleiVerificationUnavailable("metadata_expired")
                    ) {
                        previous_health
                    } else if matches!(error, CloudError::CaptchaRequired) {
                        AuthHealth::RiskBlocked
                    } else if record.consecutive_failures >= 3 {
                        AuthHealth::NeedsInteractiveAuth
                    } else {
                        AuthHealth::NeedsSilentRelogin
                    };
                    self.save(store, &record)?;
                    Err(error)
                }
            }
        })
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
        mut recover: impl FnMut(RecoveryPhase) -> Result<(), CloudError>,
        check: impl Fn() -> Result<(), CloudError>,
    ) -> Result<T, CloudError> {
        check()?;
        if kind == SessionWorkKind::Logout {
            return self.with_provider(provider, || work(store));
        }
        if is_manually_disconnected(store, provider)? {
            return Err(CloudError::NotAuthorized);
        }
        let mut continuation = None;
        if self.is_due(store, provider)? {
            let revision = self.record(store, provider)?.session_revision;
            // A proactive refresh failure must not stop still-valid ordinary reads.
            let _ = self.maintain(store, provider, Some(revision), false, || {
                check()?;
                let record = self.record(store, provider)?;
                let binding = self.snapshot(store, provider)?.binding;
                let result = recover(RecoveryPhase::Proactive);
                if matches!(result, Err(CloudError::WebAuthorizationRequired)) {
                    continuation = Some(ProfileContinuation {
                        revision: record.session_revision,
                        account_id: record.account_id,
                        failures: record.consecutive_failures,
                        next_allowed: record.next_allowed_attempt_at,
                        binding,
                    });
                }
                result
            });
        }
        let snapshot = self.snapshot(store, provider)?;
        let revision = snapshot.revision;
        check()?;
        let result = work(&snapshot);
        check()?;
        let revision = self.persist_snapshot(store, provider, revision, &snapshot, &check)?;
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
                ) || (provider == CloudProvider::Xunlei
                    && matches!(error, CloudError::XunleiVerificationUnavailable(
                        "metadata_missing" | "metadata_expired" | "timestamp_invalid"
                    ))) =>
            {
                if snapshot.initial.is_none() || store.load(provider.key())?.is_none() {
                    return Err(error);
                }
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
                        if record.consecutive_failures != ticket.failures
                            || record.next_allowed_attempt_at != ticket.next_allowed
                            || record.health == AuthHealth::RiskBlocked
                        {
                            return Err(CloudError::WebAuthorizationRequired);
                        }
                        check()?;
                        match recover(RecoveryPhase::ProfileOnly) {
                            Ok(()) => {
                                self.mark_ready(&mut record, session_expiry(store, provider)?);
                                self.save(store, &record)?;
                                self.publish_locked(store, &record, false)
                            }
                            Err(error) => {
                                if matches!(error, CloudError::CaptchaRequired) {
                                    record.health = AuthHealth::RiskBlocked;
                                    self.save(store, &record)?;
                                }
                                Err(error)
                            }
                        }
                    })?;
                } else {
                    self.maintain(store, provider, Some(revision), true, || {
                        check()?;
                        recover(RecoveryPhase::AuthFailure)
                    })?;
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
                let result = work(&next);
                check()?;
                let revision = self.persist_snapshot(store, provider, revision, &next, &check)?;
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
    if !matches!(phase, RecoveryPhase::ProfileOnly | RecoveryPhase::OperationVerification) {
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
    if phase != RecoveryPhase::Proactive {
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
