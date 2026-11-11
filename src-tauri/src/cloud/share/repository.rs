use super::*;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::path::{Path, PathBuf};

/// Each plan and transfer journal owns an independently encrypted payload.
/// Connections are short-lived; the service serializes its mutation closure.
pub struct SqliteSharePlanRepository {
    root: PathBuf,
    #[cfg(test)]
    test_key: Option<[u8; 32]>,
}

impl SqliteSharePlanRepository {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.into(),
            #[cfg(test)]
            test_key: None,
        }
    }

    fn protect(&self, input: &[u8], encrypt: bool) -> Result<Vec<u8>, QuarkError> {
        #[cfg(test)]
        if let Some(key) = self.test_key {
            return crate::cloud::credentials::protect_with_key(input, encrypt, key);
        }
        crate::cloud::credentials::protect(input, encrypt)
    }

    fn open(&self) -> Result<Connection, QuarkError> {
        let connection = Connection::open(self.root.join("cloud-share-plans.sqlite"))
            .map_err(|_| QuarkError::Storage)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| QuarkError::Storage)?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS share_plans (
                 provider TEXT NOT NULL, share_fingerprint TEXT NOT NULL,
                 last_used_order INTEGER NOT NULL, created_at_ms INTEGER NOT NULL,
                 last_used_at_ms INTEGER NOT NULL, pinned INTEGER NOT NULL,
                 secret_payload BLOB NOT NULL, PRIMARY KEY(provider,share_fingerprint));
             CREATE INDEX IF NOT EXISTS share_plans_retention ON share_plans(pinned,last_used_at_ms,last_used_order);
             CREATE TABLE IF NOT EXISTS share_journals (
                 provider TEXT NOT NULL, share_fingerprint TEXT NOT NULL, ordinal INTEGER NOT NULL,
                 entry_fingerprint TEXT NOT NULL, owner_fingerprint TEXT NOT NULL,
                 owner_scheme TEXT NOT NULL, cleanup_state TEXT NOT NULL, secret_payload BLOB NOT NULL,
                 PRIMARY KEY(provider,share_fingerprint,ordinal),
                 FOREIGN KEY(provider,share_fingerprint) REFERENCES share_plans(provider,share_fingerprint) ON DELETE CASCADE);
             CREATE INDEX IF NOT EXISTS share_journals_cleanup ON share_journals(provider,share_fingerprint,entry_fingerprint,cleanup_state);
             CREATE TABLE IF NOT EXISTS share_metadata (key TEXT PRIMARY KEY, value INTEGER NOT NULL);"
        ).map_err(|_| QuarkError::Storage)?;
        Ok(connection)
    }

    fn load(&self) -> Result<PlanDocument, QuarkError> {
        self.read_document(&self.open()?)
    }

    fn save(&self, document: &PlanDocument) -> Result<(), QuarkError> {
        validate_document(document)?;
        let mut connection = self.open()?;
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| QuarkError::Storage)?;
        self.write_document(&transaction, document)?;
        transaction.commit().map_err(|_| QuarkError::Storage)
    }

    fn read_document(&self, connection: &Connection) -> Result<PlanDocument, QuarkError> {
        let mut document = PlanDocument::default();
        document.sequence = connection
            .query_row(
                "SELECT value FROM share_metadata WHERE key='sequence'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| QuarkError::Storage)?
            .unwrap_or(0);
        let mut query = connection.prepare("SELECT provider, share_fingerprint, last_used_order, secret_payload FROM share_plans").map_err(|_| QuarkError::Storage)?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|_| QuarkError::Storage)?;
        for row in rows {
            let (provider, fingerprint, order, payload) = row.map_err(|_| QuarkError::Storage)?;
            let bytes = Zeroizing::new(self.protect(&payload, false)?);
            let mut plan: CloudSharePlan =
                serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
            if plan.provider.key() != provider
                || plan.share_fingerprint != fingerprint
                || !plan.transfers.is_empty()
            {
                return Err(QuarkError::Storage);
            }
            let mut journals = connection.prepare("SELECT secret_payload FROM share_journals WHERE provider=?1 AND share_fingerprint=?2 ORDER BY ordinal").map_err(|_| QuarkError::Storage)?;
            let rows = journals
                .query_map(params![provider, fingerprint], |row| {
                    row.get::<_, Vec<u8>>(0)
                })
                .map_err(|_| QuarkError::Storage)?;
            for row in rows {
                let bytes = Zeroizing::new(self.protect(
                    &row.map_err(|_| QuarkError::Storage)?,
                    false,
                )?);
                plan.transfers
                    .push(serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?);
            }
            document.plans.insert(
                plan_key(plan.provider, &plan.share_fingerprint),
                StoredPlan {
                    plan,
                    last_used_order: order,
                },
            );
        }
        validate_document(&document)?;
        Ok(document)
    }

    fn write_document(
        &self,
        transaction: &Transaction<'_>,
        document: &PlanDocument,
    ) -> Result<(), QuarkError> {
        let previous = self.read_document(transaction)?;
        for (key, stored) in &previous.plans {
            if !document.plans.contains_key(key) {
                transaction
                    .execute(
                        "DELETE FROM share_plans WHERE provider=?1 AND share_fingerprint=?2",
                        params![stored.plan.provider.key(), stored.plan.share_fingerprint],
                    )
                    .map_err(|_| QuarkError::Storage)?;
            }
        }
        for (key, stored) in &document.plans {
            let prior = previous.plans.get(key);
            let mut plan = stored.plan.clone();
            let journals = std::mem::take(&mut plan.transfers);
            let bytes = Zeroizing::new(serde_json::to_vec(&plan).map_err(|_| QuarkError::Storage)?);
            let unchanged = prior.is_some_and(|prior| {
                let mut old = prior.plan.clone();
                old.transfers.clear();
                prior.last_used_order == stored.last_used_order
                    && serde_json::to_vec(&old).ok().as_deref() == Some(bytes.as_slice())
            });
            if !unchanged {
                let ciphertext = self.protect(&bytes, true)?;
                transaction.execute("INSERT INTO share_plans(provider,share_fingerprint,last_used_order,created_at_ms,last_used_at_ms,pinned,secret_payload) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(provider,share_fingerprint) DO UPDATE SET last_used_order=excluded.last_used_order,created_at_ms=excluded.created_at_ms,last_used_at_ms=excluded.last_used_at_ms,pinned=excluded.pinned,secret_payload=excluded.secret_payload", params![plan.provider.key(), plan.share_fingerprint, stored.last_used_order, plan.created_at_ms, plan.last_used_at_ms, stored.plan.is_pinned(), ciphertext]).map_err(|_| QuarkError::Storage)?;
            }
            for (ordinal, journal) in journals.iter().enumerate() {
                let bytes =
                    Zeroizing::new(serde_json::to_vec(journal).map_err(|_| QuarkError::Storage)?);
                if prior
                    .and_then(|prior| prior.plan.transfers.get(ordinal))
                    .and_then(|old| serde_json::to_vec(old).ok())
                    .as_deref()
                    == Some(bytes.as_slice())
                {
                    continue;
                }
                let ciphertext = self.protect(&bytes, true)?;
                transaction.execute("INSERT INTO share_journals(provider,share_fingerprint,ordinal,entry_fingerprint,owner_fingerprint,owner_scheme,cleanup_state,secret_payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(provider,share_fingerprint,ordinal) DO UPDATE SET entry_fingerprint=excluded.entry_fingerprint,owner_fingerprint=excluded.owner_fingerprint,owner_scheme=excluded.owner_scheme,cleanup_state=excluded.cleanup_state,secret_payload=excluded.secret_payload", params![plan.provider.key(), plan.share_fingerprint, ordinal as u64, journal.entry_fingerprint, journal.owner_fingerprint, serde_json::to_string(&journal.owner_scheme).map_err(|_| QuarkError::Storage)?, serde_json::to_string(&journal.cleanup_state).map_err(|_| QuarkError::Storage)?, ciphertext]).map_err(|_| QuarkError::Storage)?;
            }
            transaction.execute("DELETE FROM share_journals WHERE provider=?1 AND share_fingerprint=?2 AND ordinal>=?3", params![plan.provider.key(), plan.share_fingerprint, journals.len() as u64]).map_err(|_| QuarkError::Storage)?;
        }
        transaction.execute("INSERT INTO share_metadata(key,value) VALUES('sequence',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [document.sequence]).map_err(|_| QuarkError::Storage)?;
        Ok(())
    }
}

fn validate_document(document: &PlanDocument) -> Result<(), QuarkError> {
    if document.version != VERSION {
        return Err(QuarkError::Storage);
    }
    for (key, stored) in &document.plans {
        stored.plan.validate()?;
        if *key != plan_key(stored.plan.provider, &stored.plan.share_fingerprint)
            || stored.last_used_order > document.sequence
        {
            return Err(QuarkError::Storage);
        }
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
pub(super) struct StoredPlan {
    pub(super) plan: CloudSharePlan,
    pub(super) last_used_order: u64,
}

#[derive(Serialize, Deserialize)]
pub(super) struct PlanDocument {
    pub(super) version: u8,
    pub(super) sequence: u64,
    pub(super) plans: BTreeMap<String, StoredPlan>,
}

impl Default for PlanDocument {
    fn default() -> Self {
        Self {
            version: VERSION,
            sequence: 0,
            plans: BTreeMap::new(),
        }
    }
}

pub(super) fn plan_key(provider: CloudProvider, fingerprint: &str) -> String {
    format!("{}/{fingerprint}", provider.key())
}
pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

impl PlanDocument {
    pub(super) fn touch(&mut self, key: &str, now: u64) -> Result<(), QuarkError> {
        self.sequence = self.sequence.checked_add(1).ok_or(QuarkError::Storage)?;
        let stored = self.plans.get_mut(key).ok_or_else(missing_plan)?;
        stored.last_used_order = self.sequence;
        stored.plan.last_used_at_ms = now;
        Ok(())
    }

    pub(super) fn prune(&mut self, now: u64) -> usize {
        let previous = self.plans.len();
        self.plans.retain(|_, stored| {
            stored.plan.is_pinned()
                || now.saturating_sub(stored.plan.last_used_at_ms) < RETENTION_MS
        });
        while self.plans.len() > MAX_PLANS {
            let Some(oldest) = self
                .plans
                .iter()
                .filter(|(_, stored)| !stored.plan.is_pinned())
                .min_by_key(|(_, stored)| stored.last_used_order)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.plans.remove(&oldest);
        }
        previous - self.plans.len()
    }
}

pub(super) fn missing_plan() -> QuarkError {
    QuarkError::InvalidInput("网盘分享计划已失效或文件不存在，请重新解析分享链接".into())
}

/// Keep one store in CloudState. Its lock serializes mutations and row commits,
/// including claiming cleanup, so concurrent workers cannot lose updates.
pub struct CloudSharePlanStore {
    pub(super) repository: Option<SqliteSharePlanRepository>,
    #[cfg(test)]
    pub(super) credentials: Arc<dyn CredentialStore>,
    pub(super) mutation: Mutex<()>,
    pub(super) now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl CloudSharePlanStore {
    /// Production uses the system credential store; the plaintext backing is test-only.
    pub fn new(credentials: SystemCredentialStore) -> Self {
        Self {
            repository: Some(SqliteSharePlanRepository::new(credentials.root())),
            #[cfg(test)]
            credentials: Arc::new(credentials),
            mutation: Mutex::new(()),
            now_ms: Arc::new(now_ms),
        }
    }

    /// Exercise encrypted SQLite transactions without accessing the OS keychain.
    /// Reopening the fixture uses the same test-only key.
    #[cfg(test)]
    pub(crate) fn new_for_test(credentials: SystemCredentialStore) -> Self {
        let mut store = Self::new(credentials);
        store.repository.as_mut().unwrap().test_key = Some([9; 32]);
        store
    }

    pub(super) fn load(&self) -> Result<PlanDocument, QuarkError> {
        if let Some(repository) = &self.repository {
            return repository.load();
        }
        #[cfg(test)]
        {
            let Some(bytes) = self.credentials.load(SHARE_PLAN_SECRET)? else {
                return Ok(PlanDocument::default());
            };
            let bytes = Zeroizing::new(bytes);
            let document: PlanDocument =
                serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
            validate_document(&document)?;
            return Ok(document);
        }
        #[cfg(not(test))]
        Err(QuarkError::Storage)
    }

    pub(super) fn save(&self, document: &PlanDocument) -> Result<(), QuarkError> {
        if let Some(repository) = &self.repository {
            return repository.save(document);
        }
        #[cfg(test)]
        {
            let bytes =
                Zeroizing::new(serde_json::to_vec(document).map_err(|_| QuarkError::Storage)?);
            return self.credentials.save(SHARE_PLAN_SECRET, &bytes);
        }
        #[cfg(not(test))]
        Err(QuarkError::Storage)
    }

    pub(super) fn update<T>(
        &self,
        action: impl FnOnce(&mut PlanDocument, u64) -> Result<T, QuarkError>,
    ) -> Result<T, QuarkError> {
        let _guard = self.mutation.lock().map_err(|_| QuarkError::Storage)?;
        let now = (self.now_ms)();
        let mut document = self.load()?;
        let pruned = document.prune(now);
        let result = action(&mut document, now);
        // Persist expiration even when the requested source no longer resolves.
        if result.is_ok() {
            document.prune(now);
            self.save(&document)?;
        } else if pruned != 0 {
            self.save(&document)?;
        }
        result
    }

    pub fn upsert(&self, plan: &CloudSharePlan) -> Result<(), QuarkError> {
        plan.validate()?;
        let _guard = self.mutation.lock().map_err(|_| QuarkError::Storage)?;
        let now = (self.now_ms)();
        let mut document = self.load()?;
        document.prune(now);
        let key = plan_key(plan.provider, &plan.share_fingerprint);
        if !document.plans.contains_key(&key)
            && document.plans.len() >= MAX_PLANS
            && !document
                .plans
                .values()
                .any(|stored| !stored.plan.is_pinned())
        {
            return Err(QuarkError::InvalidInput(
                "网盘分享计划存储已满，请先永久删除已完成的相关下载任务".into(),
            ));
        }
        let mut incoming = plan.clone();
        // Reinspection refreshes secrets/metadata without discarding recovery
        // context for existing tasks or rearming an already claimed cleanup.
        if let Some(previous) = document.plans.get(&key) {
            incoming.created_at_ms = previous.plan.created_at_ms;
            incoming.transfers = previous.plan.transfers.clone();
            incoming.linked_local_tasks = previous.plan.linked_local_tasks.clone();
            for entry in &previous.plan.entries {
                if !incoming
                    .entries
                    .iter()
                    .any(|current| current.entry_fingerprint == entry.entry_fingerprint)
                {
                    incoming.entries.push(entry.clone());
                }
            }
        } else {
            incoming.created_at_ms = now;
            incoming.transfers.clear();
            incoming.linked_local_tasks.clear();
        }
        document.plans.insert(
            key.clone(),
            StoredPlan {
                plan: incoming,
                last_used_order: 0,
            },
        );
        document.touch(&key, now)?;
        document.prune(now);
        self.save(&document)
    }

    /// Drops every encrypted share plan and transfer journal for one provider.
    /// The document write remains serialized with ordinary share operations.
    pub fn remove_provider(&self, provider: CloudProvider) -> Result<usize, QuarkError> {
        self.update(|document, _| {
            let before = document.plans.len();
            document
                .plans
                .retain(|_, stored| stored.plan.provider != provider);
            Ok(before.saturating_sub(document.plans.len()))
        })
    }

    pub fn discard_plan(&self, source: &str) -> Result<(), QuarkError> {
        let (provider, share, _) = parse_stable_cloud_share_source(source)?;
        self.update(|document, _| {
            let key = plan_key(provider, &share);
            let stored = document.plans.get(&key).ok_or_else(missing_plan)?;
            if stored.plan.is_pinned() {
                return Err(QuarkError::InvalidInput(
                    "已关联下载任务的网盘分享计划不能清理".into(),
                ));
            }
            document.plans.remove(&key);
            Ok(())
        })
    }

    pub fn resolve_entry(&self, source: &str) -> Result<CloudShareResolvedEntry, QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let entry = document
                .plans
                .get(&key)
                .and_then(|stored| {
                    stored
                        .plan
                        .entries
                        .iter()
                        .find(|entry| entry.entry_fingerprint == fingerprint)
                })
                .cloned()
                .ok_or_else(missing_plan)?;
            document.touch(&key, now)?;
            Ok(CloudShareResolvedEntry {
                provider,
                plan: document.plans[&key].plan.clone(),
                entry,
            })
        })
    }

    /// Returns an immutable plan snapshot for capacity preflight. Unlike
    /// `resolve_entry`, this deliberately does not refresh retention, prune,
    /// or persist any metadata: a rejected submission must not mutate plans.
    pub(super) fn preflight_snapshot(
        &self,
        source: &str,
    ) -> Result<CloudShareResolvedEntry, QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        let _guard = self.mutation.lock().map_err(|_| QuarkError::Storage)?;
        let document = self.load()?;
        let key = plan_key(provider, &share);
        let stored = document.plans.get(&key).ok_or_else(missing_plan)?;
        let entry = stored
            .plan
            .entries
            .iter()
            .find(|entry| entry.entry_fingerprint == fingerprint)
            .cloned()
            .ok_or_else(missing_plan)?;
        Ok(CloudShareResolvedEntry {
            provider,
            plan: stored.plan.clone(),
            entry,
        })
    }

    pub(super) fn update_provider_context(
        &self,
        source: &str,
        context: &serde_json::Value,
    ) -> Result<(), QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            if !plan
                .entries
                .iter()
                .any(|entry| entry.entry_fingerprint == fingerprint)
            {
                return Err(missing_plan());
            }
            plan.provider_context = context.clone();
            document.touch(&key, now)
        })
        .map_err(|_| QuarkError::Storage)
    }

    pub fn record_transfer(&self, source: &str, created_file_id: &str) -> Result<(), QuarkError> {
        self.record_transfer_with_scheme(
            source,
            created_file_id,
            "",
            CloudShareTransferOwnerScheme::LegacyUnknown,
        )
    }

    pub fn record_transfer_with_owner(
        &self,
        source: &str,
        created_file_id: &str,
        owner_fingerprint: &str,
    ) -> Result<(), QuarkError> {
        self.record_transfer_with_scheme(
            source,
            created_file_id,
            owner_fingerprint,
            CloudShareTransferOwnerScheme::AccountV2,
        )
    }

    pub(super) fn record_transfer_with_scheme(
        &self,
        source: &str,
        created_file_id: &str,
        owner_fingerprint: &str,
        owner_scheme: CloudShareTransferOwnerScheme,
    ) -> Result<(), QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        if created_file_id.trim().is_empty()
            || (owner_scheme == CloudShareTransferOwnerScheme::AccountV2
                && !valid_owner_fingerprint(owner_fingerprint))
            || (owner_scheme == CloudShareTransferOwnerScheme::LegacyUnknown
                && !owner_fingerprint.is_empty()
                && !valid_owner_fingerprint(owner_fingerprint))
        {
            return Err(QuarkError::InvalidResponse);
        }
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            if !plan
                .entries
                .iter()
                .any(|entry| entry.entry_fingerprint == fingerprint)
            {
                return Err(missing_plan());
            }
            if !plan.transfers.iter().any(|transfer| {
                transfer.entry_fingerprint == fingerprint
                    && transfer.created_file_id == created_file_id
                    && transfer.owner_fingerprint == owner_fingerprint
                    && transfer.owner_scheme == owner_scheme
            }) {
                plan.transfers.push(CloudShareTransfer {
                    entry_fingerprint: fingerprint,
                    created_file_id: created_file_id.into(),
                    owner_fingerprint: owner_fingerprint.into(),
                    owner_scheme,
                    cleanup_state: CloudShareCleanupState::Pending,
                });
            }
            document.touch(&key, now)
        })
    }

    /// Returns only a transfer created by the exact current credential. Old
    /// journal records deliberately cannot be reused across a session change.
    pub fn reusable_transfer(
        &self,
        source: &str,
        owner_fingerprint: &str,
    ) -> Result<Option<CloudShareTransfer>, QuarkError> {
        if !valid_owner_fingerprint(owner_fingerprint) {
            return Err(QuarkError::InvalidResponse);
        }
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        let _guard = self.mutation.lock().map_err(|_| QuarkError::Storage)?;
        let document = self.load()?;
        let plan = &document
            .plans
            .get(&plan_key(provider, &share))
            .ok_or_else(missing_plan)?
            .plan;
        if !plan
            .entries
            .iter()
            .any(|entry| entry.entry_fingerprint == fingerprint)
        {
            return Err(missing_plan());
        }
        Ok(plan
            .transfers
            .iter()
            .rev()
            .find(|transfer| {
                transfer.entry_fingerprint == fingerprint
                    && transfer.owner_scheme == CloudShareTransferOwnerScheme::AccountV2
                    && transfer.owner_fingerprint == owner_fingerprint
                    && transfer.cleanup_state == CloudShareCleanupState::Pending
            })
            .cloned())
    }

    /// Checks only whether this entry has an unfinished recorded transfer.
    /// The caller must still verify its current credential through
    /// [`Self::reusable_transfer`] before it can reuse or delete that file.
    pub(super) fn has_pending_transfer(&self, source: &str) -> Result<bool, QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        let _guard = self.mutation.lock().map_err(|_| QuarkError::Storage)?;
        let document = self.load()?;
        let plan = &document
            .plans
            .get(&plan_key(provider, &share))
            .ok_or_else(missing_plan)?
            .plan;
        if !plan
            .entries
            .iter()
            .any(|entry| entry.entry_fingerprint == fingerprint)
        {
            return Err(missing_plan());
        }
        Ok(plan.transfers.iter().any(|transfer| {
            transfer.entry_fingerprint == fingerprint
                && transfer.cleanup_state == CloudShareCleanupState::Pending
        }))
    }

    /// Forget one exact recorded personal-drive file after the provider confirms
    /// that it no longer exists. A stale record must not keep suppressing a
    /// new transfer for the same share entry.
    pub(super) fn forget_unavailable_reusable_transfer(
        &self,
        source: &str,
        created_file_id: &str,
        owner_fingerprint: &str,
    ) -> Result<(), QuarkError> {
        if !valid_owner_fingerprint(owner_fingerprint) {
            return Err(QuarkError::InvalidResponse);
        }
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            let before = plan.transfers.len();
            plan.transfers.retain(|transfer| {
                !(transfer.entry_fingerprint == fingerprint
                    && transfer.created_file_id == created_file_id
                    && transfer.owner_scheme == CloudShareTransferOwnerScheme::AccountV2
                    && transfer.owner_fingerprint == owner_fingerprint
                    && transfer.cleanup_state == CloudShareCleanupState::Pending)
            });
            if plan.transfers.len() != before {
                document.touch(&key, now)?;
            }
            Ok(())
        })
    }

    /// Claims only the exact entry's recorded objects. Completion/settings checks
    /// belong to the caller; this function performs no provider deletion itself.
    pub fn mark_cleanup_attempted(
        &self,
        source: &str,
    ) -> Result<Vec<CloudShareTransfer>, QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            if !plan
                .entries
                .iter()
                .any(|entry| entry.entry_fingerprint == fingerprint)
            {
                return Err(missing_plan());
            }
            let mut claimed = Vec::new();
            for transfer in &mut plan.transfers {
                if transfer.entry_fingerprint == fingerprint
                    && transfer.cleanup_state == CloudShareCleanupState::Pending
                {
                    transfer.cleanup_state = CloudShareCleanupState::Attempted;
                    claimed.push(transfer.clone());
                }
            }
            document.touch(&key, now)?;
            Ok(claimed)
        })
    }

    pub fn link_local_sources(&self, sources: &[String]) -> Result<(), QuarkError> {
        self.update(|document, now| {
            for source in sources {
                let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
                let key = plan_key(provider, &share);
                let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
                if !plan
                    .entries
                    .iter()
                    .any(|entry| entry.entry_fingerprint == fingerprint)
                {
                    return Err(missing_plan());
                }
                plan.linked_local_tasks.entry(fingerprint).or_default();
                document.touch(&key, now)?;
            }
            Ok(())
        })
    }

    pub fn link_local_task(&self, source: &str, task_id: uuid::Uuid) -> Result<(), QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            plan.linked_local_tasks
                .get_mut(&fingerprint)
                .ok_or_else(missing_plan)?
                .insert(task_id);
            document.touch(&key, now)
        })
    }

    /// Restores many local task links in one encrypted-document update. This
    /// is the compensating operation when a later SQLite transaction fails.
    pub fn link_local_tasks(&self, tasks: &[(String, uuid::Uuid)]) -> Result<(), QuarkError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let links = tasks
            .iter()
            .map(|(source, task_id)| {
                let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
                Ok((plan_key(provider, &share), fingerprint, *task_id))
            })
            .collect::<Result<Vec<_>, QuarkError>>()?;
        self.update(|document, now| {
            for (key, fingerprint, _) in &links {
                let plan = &document.plans.get(key).ok_or_else(missing_plan)?.plan;
                if !plan
                    .entries
                    .iter()
                    .any(|entry| entry.entry_fingerprint == *fingerprint)
                {
                    return Err(missing_plan());
                }
            }

            let mut touched = BTreeSet::new();
            for (key, fingerprint, task_id) in &links {
                let plan = &mut document.plans.get_mut(key).ok_or_else(missing_plan)?.plan;
                plan.linked_local_tasks
                    .entry(fingerprint.clone())
                    .or_default()
                    .insert(*task_id);
                touched.insert(key.clone());
            }
            for key in touched {
                document.touch(&key, now)?;
            }
            Ok(())
        })
    }

    pub fn reserve_local_task(&self, source: &str, task_id: uuid::Uuid) -> Result<(), QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            if !plan
                .entries
                .iter()
                .any(|entry| entry.entry_fingerprint == fingerprint)
            {
                return Err(missing_plan());
            }
            plan.linked_local_tasks
                .entry(fingerprint)
                .or_default()
                .insert(task_id);
            document.touch(&key, now)
        })
    }

    pub fn unlink_local_task(&self, source: &str, task_id: uuid::Uuid) -> Result<(), QuarkError> {
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            if plan
                .linked_local_tasks
                .get_mut(&fingerprint)
                .is_some_and(|ids| ids.remove(&task_id))
            {
                plan.remove_terminal_empty_link(&fingerprint);
            }
            document.touch(&key, now)
        })
    }

    /// Removes many local task links while loading and saving the encrypted
    /// share-plan document only once. Each source is parsed before mutation so
    /// a malformed or expired plan cannot persist a partial unlink.
    pub fn unlink_local_tasks(&self, tasks: &[(String, uuid::Uuid)]) -> Result<(), QuarkError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let links = tasks
            .iter()
            .map(|(source, task_id)| {
                let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
                Ok((plan_key(provider, &share), fingerprint, *task_id))
            })
            .collect::<Result<Vec<_>, QuarkError>>()?;
        self.update(|document, now| {
            for (key, _, _) in &links {
                if !document.plans.contains_key(key) {
                    return Err(missing_plan());
                }
            }

            let mut touched = BTreeSet::new();
            for (key, fingerprint, task_id) in &links {
                {
                    let plan = &mut document.plans.get_mut(key).ok_or_else(missing_plan)?.plan;
                    if plan
                        .linked_local_tasks
                        .get_mut(fingerprint)
                        .is_some_and(|ids| ids.remove(task_id))
                    {
                        plan.remove_terminal_empty_link(fingerprint);
                    }
                }
                touched.insert(key.clone());
            }
            for key in touched {
                document.touch(&key, now)?;
            }
            Ok(())
        })
    }

    /// Claim under the encrypted document lock, checking every exact linked task
    /// against current SQLite state. The caller holds submission gates while the
    /// claim and provider mutation run. Returning IDs requires a successful save.
    pub fn mark_local_completion(
        &self,
        source: &str,
        retry: bool,
        is_completed: impl Fn(uuid::Uuid, &str) -> bool,
    ) -> Result<Vec<String>, QuarkError> {
        self.mark_local_completion_for_owner(source, retry, None, is_completed)
    }

    /// Production cleanup supplies the current credential fingerprint. Legacy
    /// records and records created by another account are intentionally not
    /// claimed for provider deletion.
    pub fn mark_local_completion_for_owner(
        &self,
        source: &str,
        retry: bool,
        owner_fingerprint: Option<&str>,
        is_completed: impl Fn(uuid::Uuid, &str) -> bool,
    ) -> Result<Vec<String>, QuarkError> {
        if owner_fingerprint.is_some_and(|owner| !valid_owner_fingerprint(owner)) {
            return Err(QuarkError::InvalidResponse);
        }
        let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            if !plan.linked_local_tasks.contains_key(&fingerprint) {
                return Ok(vec![]);
            }
            for (entry, ids) in &plan.linked_local_tasks {
                let source = format!("cloud-share://{}/{}/{}", provider.key(), share, entry);
                if ids.is_empty() || !ids.iter().all(|id| is_completed(*id, &source)) {
                    return Ok(vec![]);
                }
            }
            // Unlinked historical transfers are never swept into this action.
            let mut claimed = BTreeSet::new();
            for transfer in &mut plan.transfers {
                if plan
                    .linked_local_tasks
                    .contains_key(&transfer.entry_fingerprint)
                    && owner_fingerprint
                        .map(|owner| {
                            transfer.owner_scheme == CloudShareTransferOwnerScheme::AccountV2
                                && transfer.owner_fingerprint == owner
                        })
                        .unwrap_or(true)
                    && (transfer.cleanup_state == CloudShareCleanupState::Pending
                        || (retry
                            && matches!(
                                transfer.cleanup_state,
                                CloudShareCleanupState::Failed | CloudShareCleanupState::Attempted
                            )))
                {
                    transfer.cleanup_state = CloudShareCleanupState::Attempted;
                    claimed.insert(transfer.created_file_id.clone());
                }
            }
            document.touch(&key, now)?;
            Ok(claimed.into_iter().collect())
        })
    }

    /// Reports cleanup capability for one exact persisted local task. A task is
    /// never considered pending merely because another entry in the same share
    /// has a failed cleanup journal record.
    pub(crate) fn cleanup_pending_for_task(
        &self,
        source: &str,
        task_id: uuid::Uuid,
    ) -> Result<bool, QuarkError> {
        self.cleanup_pending_for_tasks(&[(source.to_owned(), task_id)])
            .map(|pending| pending[0])
    }

    /// One short-lived snapshot per queue read, never one full decrypt per leaf.
    /// The snapshot is not cached across calls: cleanup retries remain immediately visible.
    pub(crate) fn cleanup_pending_for_tasks(
        &self,
        tasks: &[(String, uuid::Uuid)],
    ) -> Result<Vec<bool>, QuarkError> {
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        let _guard = self.mutation.lock().map_err(|_| QuarkError::Storage)?;
        let document = self.load()?;
        let pending_entries: BTreeMap<_, BTreeSet<_>> = document
            .plans
            .iter()
            .map(|(key, stored)| {
                (
                    key,
                    stored
                        .plan
                        .transfers
                        .iter()
                        .filter(|transfer| {
                            matches!(
                                transfer.cleanup_state,
                                CloudShareCleanupState::Attempted | CloudShareCleanupState::Failed
                            )
                        })
                        .map(|transfer| transfer.entry_fingerprint.as_str())
                        .collect(),
                )
            })
            .collect();
        tasks
            .iter()
            .map(|(source, task_id)| {
                let (provider, share, fingerprint) = parse_stable_cloud_share_source(source)?;
                let key = plan_key(provider, &share);
                let plan = &document.plans.get(&key).ok_or_else(missing_plan)?.plan;
                Ok(plan
                    .linked_local_tasks
                    .get(&fingerprint)
                    .is_some_and(|ids| ids.contains(task_id))
                    && pending_entries[&key].contains(fingerprint.as_str()))
            })
            .collect()
    }

    /// Rebuild safe task messages from the durable journal after a restart.
    /// This reads the document without claiming, pruning, saving or provider IO.
    pub(crate) fn restore_cleanup_pending_messages(
        &self,
        tasks: &crate::TaskStore,
    ) -> Result<(), QuarkError> {
        let _guard = self.mutation.lock().map_err(|_| QuarkError::Storage)?;
        let document = self.load()?;
        for stored in document.plans.values() {
            let plan = &stored.plan;
            let pending = plan.transfers.iter().any(|transfer| {
                plan.linked_local_tasks
                    .contains_key(&transfer.entry_fingerprint)
                    && matches!(
                        transfer.cleanup_state,
                        CloudShareCleanupState::Attempted | CloudShareCleanupState::Failed
                    )
            });
            for (entry, ids) in &plan.linked_local_tasks {
                let source = format!(
                    "cloud-share://{}/{}/{entry}",
                    plan.provider.key(),
                    plan.share_fingerprint
                );
                for id in ids {
                    let Ok(task) = tasks.find(*id) else {
                        continue;
                    };
                    if task.status == crate::TaskStatus::Completed && task.source == source {
                        tasks
                            .set_completed_cleanup_message(*id, pending)
                            .map_err(|_| QuarkError::Storage)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finish_cleanup(
        &self,
        source: &str,
        ids: &[String],
        succeeded: bool,
    ) -> Result<(), QuarkError> {
        let (provider, share, _) = parse_stable_cloud_share_source(source)?;
        self.update(|document, now| {
            let key = plan_key(provider, &share);
            let plan = &mut document.plans.get_mut(&key).ok_or_else(missing_plan)?.plan;
            let mut terminal_entries = BTreeSet::new();
            for transfer in &mut plan.transfers {
                if transfer.cleanup_state == CloudShareCleanupState::Attempted
                    && ids.contains(&transfer.created_file_id)
                {
                    transfer.cleanup_state = if succeeded {
                        CloudShareCleanupState::Succeeded
                    } else {
                        CloudShareCleanupState::Failed
                    };
                    if succeeded {
                        terminal_entries.insert(transfer.entry_fingerprint.clone());
                    }
                }
            }
            for fingerprint in terminal_entries {
                plan.remove_terminal_empty_link(&fingerprint);
            }
            document.touch(&key, now)
        })
    }

    pub fn prune(&self) -> Result<usize, QuarkError> {
        let _guard = self.mutation.lock().map_err(|_| QuarkError::Storage)?;
        let mut document = self.load()?;
        let removed = document.prune((self.now_ms)());
        if removed > 0 {
            self.save(&document)?;
        }
        Ok(removed)
    }
}
