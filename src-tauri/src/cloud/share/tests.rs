use super::*;
#[test]
fn sqlite_failed_journal_claim_rolls_back_plan_and_remains_retryable() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        CloudSharePlanStore::new(SystemCredentialStore::new(directory.path().into()).unwrap());
    let plan = fixture("journal-rollback");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    store
        .record_transfer_with_owner(&source, "personal-file", &"a".repeat(64))
        .unwrap();
    let database =
        rusqlite::Connection::open(directory.path().join("cloud-share-plans.sqlite")).unwrap();
    let before: Vec<u8> = database
        .query_row("SELECT secret_payload FROM share_plans", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        !before
            .windows(b"secret-code".len())
            .any(|bytes| bytes == b"secret-code"),
        "the SQLite row must not contain the plan secret in plaintext"
    );
    database.execute_batch("CREATE TRIGGER reject_journal_claim BEFORE UPDATE ON share_journals BEGIN SELECT RAISE(FAIL, 'journal rejected'); END;").unwrap();
    assert!(store.mark_cleanup_attempted(&source).is_err());
    let after: Vec<u8> = database
        .query_row("SELECT secret_payload FROM share_plans", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        after, before,
        "journal failure must roll back the earlier plan update"
    );
    assert!(store
        .reusable_transfer(&source, &"a".repeat(64))
        .unwrap()
        .is_some());
    database
        .execute_batch("DROP TRIGGER reject_journal_claim")
        .unwrap();
    assert_eq!(store.mark_cleanup_attempted(&source).unwrap().len(), 1);
    drop(store);
    let reopened =
        CloudSharePlanStore::new(SystemCredentialStore::new(directory.path().into()).unwrap());
    assert!(reopened.mark_cleanup_attempted(&source).unwrap().is_empty());
}
#[test]
fn sqlite_failed_write_preserves_previous_row_and_other_plan() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        CloudSharePlanStore::new(SystemCredentialStore::new(directory.path().into()).unwrap());
    let mut plan = fixture("row-write");
    store.upsert(&plan).unwrap();
    store.upsert(&fixture("untouched-row")).unwrap();
    let database = directory.path().join("cloud-share-plans.sqlite");
    assert!(database.exists(), "new plans must use SQLite rows");
    let connection = rusqlite::Connection::open(database).unwrap();
    let untouched: Vec<u8> = connection
        .query_row(
            "SELECT secret_payload FROM share_plans WHERE share_fingerprint=?1",
            [fixture("untouched-row").share_fingerprint],
            |row| row.get(0),
        )
        .unwrap();
    connection.execute_batch("CREATE TRIGGER reject_share_update BEFORE UPDATE ON share_plans BEGIN SELECT RAISE(FAIL, 'write rejected'); END;").unwrap();
    plan.access_code = Some("replacement-secret".into());
    assert!(store.upsert(&plan).is_err());
    connection
        .execute_batch("DROP TRIGGER reject_share_update")
        .unwrap();
    assert_eq!(
        store
            .resolve_entry(&plan.source_for("f1"))
            .unwrap()
            .access_code(),
        Some("secret-code")
    );
    let after: Vec<u8> = connection
        .query_row(
            "SELECT secret_payload FROM share_plans WHERE share_fingerprint=?1",
            [fixture("untouched-row").share_fingerprint],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        after, untouched,
        "changing one plan must not rewrite another encrypted row"
    );
}

use crate::cloud::{
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, MemoryCredentialStore,
};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct TimeoutThenSuccessfulReadTransport {
    attempts: AtomicU64,
}

impl CloudHttpTransport for TimeoutThenSuccessfulReadTransport {
    fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            return Err(QuarkError::Transport("连接超时".into()));
        }
        Ok(CloudHttpResponse {
            status: 200,
            body: "share-page".into(),
            ..Default::default()
        })
    }
}

#[test]
fn share_read_retries_transient_transport_timeouts_without_replaying_a_plan() {
    let inner = TimeoutThenSuccessfulReadTransport {
        attempts: AtomicU64::new(0),
    };
    let retrying = RetryingShareReadTransport::new(&inner);

    let response = retrying
        .send(CloudHttpRequest {
            method: "GET",
            url: "https://example.test/share-page".into(),
            headers: BTreeMap::new(),
            body: None,
        })
        .expect("the final transient-read retry must return the provider response");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, "share-page");
    assert_eq!(inner.attempts.load(Ordering::SeqCst), 3);
}

#[test]
fn aliyun_forbidden_direct_url_transfers_once_and_reuses_the_personal_copy() {
    use crate::cloud::aliyun::AliyunShareAdapter;
    use std::collections::VecDeque;

    struct StatusHttp {
        replies: Mutex<VecDeque<CloudHttpResponse>>,
        requests: Mutex<Vec<CloudHttpRequest>>,
    }
    impl CloudHttpTransport for StatusHttp {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            self.requests.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(QuarkError::InvalidResponse)
        }
    }

    let (store, credentials, _) = memory_store();
    credentials
        .save(
            "aliyun",
            br#"{"refresh_token":"refresh","access_token":"personal","drive_id":"drive-7"}"#,
        )
        .unwrap();
    let plan = CloudSharePlan::new(
        CloudProvider::Aliyun,
        "share-7".into(),
        None,
        vec![CloudShareEntry::new(
            "share-7",
            "shared-file".into(),
            "sample.bin".into(),
            vec![],
            Some(7),
        )],
        serde_json::json!({"share_token":"share-token"}),
    );
    let source = plan.source_for("shared-file");
    store.upsert(&plan).unwrap();
    let http = StatusHttp {
        replies: Mutex::new(
            vec![
                CloudHttpResponse {
                    status: 403,
                    body: serde_json::json!({"code":"Forbidden"}).to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"drive_id":"drive-7","file_id":"personal-file"})
                        .to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"name":"sample (1).bin","domain_id":"bj29"})
                        .to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"url":""}).to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"url":"https://cdn.example/personal-file"})
                        .to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"name":"sample (1).bin","domain_id":"bj29"})
                        .to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"url":"https://cdn.example/personal-file-refreshed"})
                        .to_string(),
                    set_cookies: vec![],
                },
            ]
            .into(),
        ),
        requests: Mutex::new(vec![]),
    };
    let adapter = AliyunShareAdapter {
        credentials: credentials.as_ref(),
        http: &http,
    };

    let download = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(download.url, "https://cdn.example/personal-file");
    assert_eq!(download.name, "sample.bin");
    assert_eq!(
        download.headers,
        vec![("Referer".into(), "https://www.aliyundrive.com/".into())]
    );
    let records = store.resolve_entry(&source).unwrap().plan.transfers;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].created_file_id, "personal-file");
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests[0].url.ends_with("/v2/file/get_download_url"));
    assert!(requests[1].url.ends_with("/v2/file/copy"));
    assert!(requests[2].url.ends_with("/v2/file/get"));
    assert!(requests[3].url.ends_with("/v2/file/get_download_url"));
    assert_eq!(
        requests[4].url,
        "https://bj29.api.aliyunpds.com/v2/file/get_download_url"
    );
    assert_eq!(requests[3].body, requests[4].body);
    let signed: serde_json::Value =
        serde_json::from_str(requests[4].body.as_ref().unwrap()).unwrap();
    assert_eq!(signed["drive_id"], "drive-7");
    assert_eq!(signed["file_id"], "personal-file");
    assert_eq!(signed["file_name"], "sample (1).bin");
    assert!(requests[2..].iter().all(|request| {
        request.headers.get("Authorization").map(String::as_str) == Some("Bearer personal")
            && !request.headers.contains_key("x-share-token")
    }));
    assert_eq!(
        requests[1].headers.get("Authorization").map(String::as_str),
        Some("Bearer personal")
    );
    assert_eq!(
        requests[1].headers.get("x-share-token").map(String::as_str),
        Some("share-token")
    );
    let copy: serde_json::Value = serde_json::from_str(requests[1].body.as_ref().unwrap()).unwrap();
    assert_eq!(copy["share_id"], "share-7");
    assert_eq!(copy["file_id"], "shared-file");
    assert_eq!(copy["to_drive_id"], "drive-7");
    assert_eq!(copy["to_parent_file_id"], "root");
    assert_eq!(copy["auto_rename"], true);
    assert!(copy.get("drive_id").is_none());
    drop(requests);

    let refreshed = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(refreshed.url, "https://cdn.example/personal-file-refreshed");
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.ends_with("/v2/file/copy"))
            .count(),
        1
    );
}

#[test]
fn aliyun_transfer_waits_until_the_personal_copy_has_a_download_url() {
    use crate::cloud::aliyun::AliyunShareAdapter;
    use std::collections::VecDeque;

    struct StatusHttp {
        replies: Mutex<VecDeque<CloudHttpResponse>>,
        requests: Mutex<Vec<CloudHttpRequest>>,
    }
    impl CloudHttpTransport for StatusHttp {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            self.requests.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(QuarkError::InvalidResponse)
        }
    }

    let (store, credentials, _) = memory_store();
    credentials
        .save(
            "aliyun",
            br#"{"refresh_token":"refresh","access_token":"personal","drive_id":"drive-7"}"#,
        )
        .unwrap();
    let plan = CloudSharePlan::new(
        CloudProvider::Aliyun,
        "share-7".into(),
        None,
        vec![CloudShareEntry::new(
            "share-7",
            "shared-file".into(),
            "sample.bin".into(),
            vec![],
            Some(7),
        )],
        serde_json::json!({"share_token":"share-token"}),
    );
    let source = plan.source_for("shared-file");
    store.upsert(&plan).unwrap();
    let http = StatusHttp {
        replies: Mutex::new(
            vec![
                CloudHttpResponse {
                    status: 403,
                    body: serde_json::json!({"code":"Forbidden"}).to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"drive_id":"drive-7","file_id":"personal-file"})
                        .to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 404,
                    body: serde_json::json!({"code":"NotFound.File"}).to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"name":"sample (1).bin","domain_id":"bj29"})
                        .to_string(),
                    set_cookies: vec![],
                },
                CloudHttpResponse {
                    status: 200,
                    body: serde_json::json!({"url":"https://cdn.example/personal-file"})
                        .to_string(),
                    set_cookies: vec![],
                },
            ]
            .into(),
        ),
        requests: Mutex::new(vec![]),
    };
    let adapter = AliyunShareAdapter {
        credentials: credentials.as_ref(),
        http: &http,
    };

    let download = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(download.url, "https://cdn.example/personal-file");
    assert_eq!(
        download.headers,
        vec![("Referer".into(), "https://www.aliyundrive.com/".into())]
    );
    let records = store.resolve_entry(&source).unwrap().plan.transfers;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].created_file_id, "personal-file");
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.ends_with("/v2/file/copy"))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.ends_with("/v2/file/get_download_url"))
            .count(),
        2
    );
}

#[derive(Default)]
struct SagaAria2 {
    add_calls: AtomicU64,
    reject_add: AtomicBool,
    drop_task_files: Mutex<Option<std::path::PathBuf>>,
}

#[async_trait]
impl crate::aria2::Aria2Api for SagaAria2 {
    async fn add_uri(
        &self,
        _: Vec<String>,
        _: crate::aria2::Aria2Options,
    ) -> Result<String, crate::aria2::Aria2Error> {
        self.add_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(path) = self.drop_task_files.lock().unwrap().take() {
            rusqlite::Connection::open(path)
                .unwrap()
                .execute_batch("DROP TABLE task_files")
                .unwrap();
        }
        if self.reject_add.load(Ordering::SeqCst) {
            Err(crate::aria2::Aria2Error::Transport)
        } else {
            Ok("saga-gid".into())
        }
    }
    async fn tell_status(
        &self,
        gid: &str,
    ) -> Result<crate::aria2::Aria2Status, crate::aria2::Aria2Error> {
        Ok(crate::aria2::Aria2Status {
            gid: gid.into(),
            status: "active".into(),
            total_length: "1".into(),
            completed_length: "0".into(),
            download_speed: "0".into(),
            upload_speed: "0".into(),
            connections: "0".into(),
            files: vec![],
            bittorrent: None,
            followed_by: vec![],
            error_code: None,
            error_message: None,
        })
    }
    async fn add_torrent(
        &self,
        _: String,
        _: Vec<String>,
        _: crate::aria2::Aria2Options,
    ) -> Result<String, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn tell_active(
        &self,
    ) -> Result<Vec<crate::aria2::Aria2Status>, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn tell_waiting(
        &self,
        _: i64,
        _: u32,
    ) -> Result<Vec<crate::aria2::Aria2Status>, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn tell_stopped(
        &self,
        _: i64,
        _: u32,
    ) -> Result<Vec<crate::aria2::Aria2Status>, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn get_global_stat(
        &self,
    ) -> Result<crate::aria2::Aria2GlobalStat, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn multicall(
        &self,
        _: Vec<crate::aria2::RpcCall>,
    ) -> Result<Vec<Result<serde_json::Value, crate::aria2::RpcFault>>, crate::aria2::Aria2Error>
    {
        unreachable!()
    }
    async fn pause(&self, _: &str) -> Result<String, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn unpause(&self, _: &str) -> Result<String, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn remove(&self, gid: &str) -> Result<String, crate::aria2::Aria2Error> {
        Ok(gid.into())
    }
    async fn force_remove(&self, _: &str) -> Result<String, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn change_option(
        &self,
        _: &str,
        _: crate::aria2::Aria2Options,
    ) -> Result<String, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn get_option(
        &self,
        _: &str,
    ) -> Result<crate::aria2::Aria2Options, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn save_session(&self) -> Result<String, crate::aria2::Aria2Error> {
        unreachable!()
    }
    async fn shutdown(&self) -> Result<String, crate::aria2::Aria2Error> {
        unreachable!()
    }
}

struct SagaAdapter {
    resolve_calls: AtomicU64,
    transfer_calls: AtomicU64,
    fail_resolve: bool,
}

struct FailNthSaveCredentials {
    inner: MemoryCredentialStore,
    saves: AtomicU64,
    fail_at: u64,
}

impl CredentialStore for FailNthSaveCredentials {
    fn save(&self, key: &str, value: &[u8]) -> Result<(), QuarkError> {
        let save = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
        if save == self.fail_at {
            Err(QuarkError::Storage)
        } else {
            self.inner.save(key, value)
        }
    }

    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
        self.inner.load(key)
    }

    fn remove(&self, key: &str) -> Result<(), QuarkError> {
        self.inner.remove(key)
    }
}

impl ShareInspect for SagaAdapter {
    fn inspect(
        &self,
        _: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        unreachable!()
    }
}
impl ShareResolve for SagaAdapter {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_resolve {
            Err(QuarkError::AuthorizationExpired)
        } else {
            Ok(ShareResolution::Direct(
                super::super::commands::ResolvedCloudDownload {
                    url: "https://cdn.example/file".into(),
                    name: entry.entry.name.clone(),
                    headers: vec![],
                },
            ))
        }
    }
}
impl ShareTransfer for SagaAdapter {
    fn transfer(
        &self,
        _: &CloudShareResolvedEntry,
        _: &CloudSharePlanStore,
    ) -> Result<super::super::commands::ResolvedCloudDownload, QuarkError> {
        self.transfer_calls.fetch_add(1, Ordering::SeqCst);
        unreachable!()
    }
    fn transfer_owner_fingerprint(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        Ok(None)
    }
}
impl CloudShareAdapter for SagaAdapter {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
}

struct RecordedTransferAdapter {
    resolve_calls: AtomicU64,
    transfer_calls: AtomicU64,
    refresh_calls: AtomicU64,
}

fn recorded_transfer_owner() -> String {
    stable_owner_fingerprint(CloudProvider::Baidu, "account-7").unwrap()
}

impl ShareInspect for RecordedTransferAdapter {
    fn inspect(
        &self,
        _: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        unreachable!()
    }
}
impl ShareResolve for RecordedTransferAdapter {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ShareResolution::TransferRequired(TransferRequest {
            source: entry.plan.source_for(&entry.entry.entry_id),
        }))
    }
}
impl ShareTransfer for RecordedTransferAdapter {
    fn transfer_owner_fingerprint(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        Ok(Some(recorded_transfer_owner()))
    }

    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<super::super::commands::ResolvedCloudDownload, QuarkError> {
        self.transfer_calls.fetch_add(1, Ordering::SeqCst);
        store.record_transfer_with_owner(
            &entry.plan.source_for(&entry.entry.entry_id),
            "personal-id",
            &recorded_transfer_owner(),
        )?;
        Ok(super::super::commands::ResolvedCloudDownload {
            url: "https://cdn.example/new-transfer".into(),
            name: entry.entry.name.clone(),
            headers: vec![],
        })
    }
}
impl RecordedTransferResolver for RecordedTransferAdapter {
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        if created_file_id == "missing-id" {
            return Ok(ShareOperationResult {
                result: Err(QuarkError::SourceFileMissing),
                updated_context: None,
            });
        }
        assert_eq!(created_file_id, "personal-id");
        Ok(ShareOperationResult {
            result: Ok(RecordedTransferDownload::Available(
                super::super::commands::ResolvedCloudDownload {
                    url: "https://cdn.example/reused-transfer".into(),
                    name: entry.entry.name.clone(),
                    headers: vec![],
                },
            )),
            updated_context: None,
        })
    }
}
impl CloudShareAdapter for RecordedTransferAdapter {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        Some(self)
    }
}

#[test]
fn pending_recorded_transfer_refreshes_its_personal_url_without_transferring_again() {
    let (store, _, _) = memory_store();
    let plan = fixture("recorded-transfer-retry");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    let adapter = RecordedTransferAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        refresh_calls: AtomicU64::new(0),
    };

    let first = resolve_current_share_download(&store, &adapter, &source).unwrap();
    let retried = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(first.url, "https://cdn.example/new-transfer");
    assert_eq!(retried.url, "https://cdn.example/reused-transfer");
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.refresh_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn pending_recorded_transfer_refreshes_without_resolving_an_expired_share() {
    let (store, _, _) = memory_store();
    let plan = fixture("recorded-transfer-before-share-resolve");
    let source = plan.source_for("f1");
    let owner = recorded_transfer_owner();
    store.upsert(&plan).unwrap();
    store
        .record_transfer_with_owner(&source, "personal-id", &owner)
        .unwrap();
    let adapter = RecordedTransferAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        refresh_calls: AtomicU64::new(0),
    };

    let resolved = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(resolved.url, "https://cdn.example/reused-transfer");
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.refresh_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn missing_recorded_transfer_is_forgotten_before_a_fresh_transfer() {
    let (store, _, _) = memory_store();
    let plan = fixture("recorded-transfer-recycle-bin");
    let source = plan.source_for("f1");
    let owner = recorded_transfer_owner();
    store.upsert(&plan).unwrap();
    store
        .record_transfer_with_owner(&source, "missing-id", &owner)
        .unwrap();
    let adapter = RecordedTransferAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        refresh_calls: AtomicU64::new(0),
    };

    let resolved = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(resolved.url, "https://cdn.example/new-transfer");
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.refresh_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .reusable_transfer(&source, &owner)
            .unwrap()
            .unwrap()
            .created_file_id,
        "personal-id"
    );
}

#[test]
fn stable_account_owner_reuses_pending_transfer_after_credential_renewal() {
    let (store, _, _) = memory_store();
    let plan = fixture("recorded-transfer-renewed-account");
    let source = plan.source_for("f1");
    // The credential changed, but the provider's stable account identity
    // did not. Only the identity-derived journal hash may gate reuse.
    let before = stable_owner_fingerprint(CloudProvider::Baidu, "account-7").unwrap();
    let after = stable_owner_fingerprint(CloudProvider::Baidu, "account-7").unwrap();
    assert_eq!(before, after);
    store.upsert(&plan).unwrap();
    store
        .record_transfer_with_owner(&source, "personal-id", &before)
        .unwrap();
    let adapter = RecordedTransferAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        refresh_calls: AtomicU64::new(0),
    };

    let resolved = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(resolved.url, "https://cdn.example/reused-transfer");
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.refresh_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn pending_transfer_from_another_credential_uses_the_normal_share_path() {
    let (store, _, _) = memory_store();
    let plan = fixture("recorded-transfer-owner-mismatch");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    store
        .record_transfer_with_owner(&source, "other-account-id", &"b".repeat(64))
        .unwrap();
    let adapter = RecordedTransferAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        refresh_calls: AtomicU64::new(0),
    };

    let resolved = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(resolved.url, "https://cdn.example/new-transfer");
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.refresh_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn legacy_session_owner_hash_never_matches_an_account_owner() {
    let (store, credentials, _) = memory_store();
    let plan = fixture("recorded-transfer-legacy-session-hash");
    let source = plan.source_for("f1");
    let owner = recorded_transfer_owner();
    // Simulate a pre-account-v2 encrypted record: it has a 64-character owner
    // hash but no owner-scheme field at all.
    store.upsert(&plan).unwrap();
    store
        .record_transfer_with_owner(&source, "legacy-personal-id", &owner)
        .unwrap();
    let mut persisted: serde_json::Value =
        serde_json::from_slice(&credentials.load(SHARE_PLAN_SECRET).unwrap().unwrap()).unwrap();
    persisted["plans"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()["plan"]["transfers"][0]
        .as_object_mut()
        .unwrap()
        .remove("owner_scheme");
    credentials
        .save(SHARE_PLAN_SECRET, &serde_json::to_vec(&persisted).unwrap())
        .unwrap();
    let adapter = RecordedTransferAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        refresh_calls: AtomicU64::new(0),
    };

    let resolved = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(resolved.url, "https://cdn.example/new-transfer");
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.refresh_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn legacy_pending_transfer_without_an_owner_uses_the_normal_share_path() {
    let (store, _, _) = memory_store();
    let plan = fixture("recorded-transfer-legacy-owner");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    store.record_transfer(&source, "legacy-id").unwrap();
    let adapter = RecordedTransferAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        refresh_calls: AtomicU64::new(0),
    };

    let resolved = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(resolved.url, "https://cdn.example/new-transfer");
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.refresh_calls.load(Ordering::SeqCst), 0);
}

struct IdentityUnavailableTransferAdapter {
    transfer_calls: AtomicU64,
}

impl ShareInspect for IdentityUnavailableTransferAdapter {
    fn inspect(
        &self,
        _: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        unreachable!()
    }
}
impl ShareResolve for IdentityUnavailableTransferAdapter {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        Ok(ShareResolution::TransferRequired(TransferRequest {
            source: entry.plan.source_for(&entry.entry.entry_id),
        }))
    }
}
impl ShareTransfer for IdentityUnavailableTransferAdapter {
    fn transfer_owner_fingerprint(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        Ok(None)
    }

    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<super::super::commands::ResolvedCloudDownload, QuarkError> {
        self.transfer_calls.fetch_add(1, Ordering::SeqCst);
        store.record_transfer(
            &entry.plan.source_for(&entry.entry.entry_id),
            "identity-unavailable-id",
        )?;
        Ok(super::super::commands::ResolvedCloudDownload {
            url: "https://cdn.example/identity-unavailable-transfer".into(),
            name: entry.entry.name.clone(),
            headers: vec![],
        })
    }
}
impl CloudShareAdapter for IdentityUnavailableTransferAdapter {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
}

#[test]
fn identity_unavailable_still_performs_the_first_transfer_as_legacy() {
    let (store, _, _) = memory_store();
    let plan = fixture("identity-unavailable-first-transfer");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    let adapter = IdentityUnavailableTransferAdapter {
        transfer_calls: AtomicU64::new(0),
    };

    let resolved = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(
        resolved.url,
        "https://cdn.example/identity-unavailable-transfer"
    );
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 1);
    let transfers = store.resolve_entry(&source).unwrap().plan.transfers;
    assert_eq!(transfers.len(), 1);
    assert!(transfers[0].owner_fingerprint.is_empty());
    assert_eq!(
        transfers[0].owner_scheme,
        CloudShareTransferOwnerScheme::LegacyUnknown
    );
}

#[test]
fn succeeded_transfer_is_not_reused_for_a_new_download() {
    let (store, _, _) = memory_store();
    let plan = fixture("succeeded-transfer-retry");
    let source = plan.source_for("f1");
    let owner = recorded_transfer_owner();
    store.upsert(&plan).unwrap();
    store
        .record_transfer_with_owner(&source, "personal-id", &owner)
        .unwrap();
    store.mark_cleanup_attempted(&source).unwrap();
    store
        .finish_cleanup(&source, &["personal-id".into()], true)
        .unwrap();
    let adapter = RecordedTransferAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        refresh_calls: AtomicU64::new(0),
    };

    let resolved = resolve_current_share_download(&store, &adapter, &source).unwrap();

    assert_eq!(resolved.url, "https://cdn.example/new-transfer");
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.refresh_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn cleanup_claim_skips_a_transfer_created_by_another_credential() {
    let (store, _, _) = memory_store();
    let plan = fixture("owner-filter");
    let source = plan.source_for("f1");
    let owner_a = "a".repeat(64);
    let owner_b = "b".repeat(64);
    let local_task = uuid::Uuid::new_v4();
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(std::slice::from_ref(&source))
        .unwrap();
    store.link_local_task(&source, local_task).unwrap();
    store
        .record_transfer_with_owner(&source, "personal-id", &owner_a)
        .unwrap();

    assert!(store
        .mark_local_completion_for_owner(&source, false, Some(&owner_b), |_, _| true)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .mark_local_completion_for_owner(&source, false, Some(&owner_a), |_, _| true)
            .unwrap(),
        vec!["personal-id"]
    );
}

#[test]
fn cleanup_claim_is_durable_and_one_shot_across_concurrent_completions() {
    let (store, _, _) = memory_store();
    let plan = fixture("share");
    store.upsert(&plan).unwrap();
    let source = plan.source_for("f1");
    store
        .link_local_sources(std::slice::from_ref(&source))
        .unwrap();
    store
        .link_local_task(&source, uuid::Uuid::new_v4())
        .unwrap();
    store.record_transfer(&source, "exact-created-id").unwrap();
    store.upsert(&plan).unwrap();
    let store = Arc::new(store);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let source = source.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .mark_local_completion(&source, false, |_, _| true)
                    .unwrap()
            })
        })
        .collect();
    let claims: Vec<_> = threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(claims, vec!["exact-created-id"]);
    assert_eq!(
        store.resolve_entry(&source).unwrap().plan.transfers[0].cleanup_state,
        CloudShareCleanupState::Attempted
    );
}

#[test]
fn cleanup_pending_is_scoped_to_the_exact_source_and_linked_task() {
    let (store, _, _) = memory_store();
    let mut plan = fixture("cleanup-status");
    plan.entries.push(CloudShareEntry::new(
        "cleanup-status",
        "f2".into(),
        "other.txt".into(),
        vec![],
        Some(3),
    ));
    let source = plan.source_for("f1");
    let other_source = plan.source_for("f2");
    let linked_task = uuid::Uuid::new_v4();
    let unrelated_task = uuid::Uuid::new_v4();
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(std::slice::from_ref(&source))
        .unwrap();
    store.link_local_task(&source, linked_task).unwrap();
    store.record_transfer(&source, "created-id").unwrap();

    assert!(!store
        .cleanup_pending_for_task(&source, linked_task)
        .unwrap());
    let ids = store
        .mark_local_completion(&source, false, |_, _| true)
        .unwrap();
    assert!(store
        .cleanup_pending_for_task(&source, linked_task)
        .unwrap());
    assert!(!store
        .cleanup_pending_for_task(&source, unrelated_task)
        .unwrap());
    assert!(!store
        .cleanup_pending_for_task(&other_source, linked_task)
        .unwrap());

    let queries = vec![
        (source.clone(), linked_task),
        (source.clone(), unrelated_task),
        (other_source.clone(), linked_task),
    ];
    assert_eq!(
        store.cleanup_pending_for_tasks(&queries).unwrap(),
        vec![true, false, false]
    );

    store.finish_cleanup(&source, &ids, false).unwrap();
    assert!(store
        .cleanup_pending_for_task(&source, linked_task)
        .unwrap());
    let retry_ids = store
        .mark_local_completion(&source, true, |_, _| true)
        .unwrap();
    store.finish_cleanup(&source, &retry_ids, true).unwrap();
    assert!(!store
        .cleanup_pending_for_task(&source, linked_task)
        .unwrap());
    assert_eq!(
        store.cleanup_pending_for_tasks(&queries).unwrap(),
        vec![false, false, false]
    );
}

#[test]
fn cleanup_pending_batch_rejects_invalid_sources_and_missing_plans() {
    let (store, _, _) = memory_store();
    let id = uuid::Uuid::new_v4();
    assert!(store
        .cleanup_pending_for_tasks(&[("not-a-share-source".into(), id)])
        .is_err());
    let source = fixture("missing-batch-plan").source_for("f1");
    assert!(store.cleanup_pending_for_tasks(&[(source, id)]).is_err());
}

#[test]
fn cleanup_cannot_release_ids_when_encrypted_claim_save_fails() {
    use std::sync::atomic::AtomicBool;
    struct FailingCredentials {
        inner: MemoryCredentialStore,
        fail: AtomicBool,
    }
    impl CredentialStore for FailingCredentials {
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
            self.inner.load(key)
        }
        fn remove(&self, key: &str) -> Result<(), QuarkError> {
            self.inner.remove(key)
        }
        fn save(&self, key: &str, bytes: &[u8]) -> Result<(), QuarkError> {
            if self.fail.load(Ordering::SeqCst) {
                Err(QuarkError::Storage)
            } else {
                self.inner.save(key, bytes)
            }
        }
    }
    let credentials = Arc::new(FailingCredentials {
        inner: MemoryCredentialStore::default(),
        fail: AtomicBool::new(false),
    });
    let store = CloudSharePlanStore {
        repository: None,
        credentials: credentials.clone(),
        mutation: Mutex::new(()),
        now_ms: Arc::new(|| 1_000),
    };
    let plan = fixture("share");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(std::slice::from_ref(&source))
        .unwrap();
    store
        .link_local_task(&source, uuid::Uuid::new_v4())
        .unwrap();
    store.record_transfer(&source, "created-id").unwrap();
    credentials.fail.store(true, Ordering::SeqCst);
    assert!(store
        .mark_local_completion(&source, false, |_, _| true)
        .is_err());
    credentials.fail.store(false, Ordering::SeqCst);
    assert_eq!(
        store.resolve_entry(&source).unwrap().plan.transfers[0].cleanup_state,
        CloudShareCleanupState::Pending
    );
}

#[test]
fn rotated_share_context_must_be_persisted_before_transfer_or_download_handoff() {
    use std::sync::atomic::AtomicBool;
    struct FailingStore {
        inner: MemoryCredentialStore,
        fail: Arc<AtomicBool>,
    }
    impl CredentialStore for FailingStore {
        fn save(&self, key: &str, value: &[u8]) -> Result<(), QuarkError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(QuarkError::Provider("private-write-failure".into()));
            }
            self.inner.save(key, value)
        }
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
            self.inner.load(key)
        }
        fn remove(&self, key: &str) -> Result<(), QuarkError> {
            self.inner.remove(key)
        }
    }
    struct UpdatingAdapter {
        fail: Arc<AtomicBool>,
        at_transfer: bool,
        operation_error: bool,
    }
    impl ShareInspect for UpdatingAdapter {
        fn inspect(
            &self,
            _: &crate::parser::CloudShareCandidate,
        ) -> Result<ShareInspection, QuarkError> {
            unreachable!()
        }
    }
    impl ShareResolve for UpdatingAdapter {
        fn resolve(&self, _: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
            unreachable!()
        }
        fn resolve_with_context(
            &self,
            entry: &CloudShareResolvedEntry,
        ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
            if !self.at_transfer {
                self.fail.store(true, Ordering::SeqCst);
            }
            Ok(ShareOperationResult {
                result: if self.operation_error && !self.at_transfer {
                    Err(QuarkError::WebAuthorizationRequired)
                } else {
                    Ok(if self.at_transfer {
                        ShareResolution::TransferRequired(TransferRequest {
                            source: entry.plan.source_for(&entry.entry.entry_id),
                        })
                    } else {
                        ShareResolution::Direct(super::super::commands::ResolvedCloudDownload {
                            url: "https://cdn.example/file".into(),
                            name: "file".into(),
                            headers: vec![],
                        })
                    })
                },
                updated_context: Some(serde_json::json!({"token":"rotated-before-transfer"})),
            })
        }
    }
    impl ShareTransfer for UpdatingAdapter {
        fn transfer_with_context(
            &self,
            entry: &CloudShareResolvedEntry,
            store: &CloudSharePlanStore,
        ) -> Result<ShareOperationResult<super::super::commands::ResolvedCloudDownload>, QuarkError>
        {
            assert_eq!(
                entry.plan.provider_context["token"],
                "rotated-before-transfer"
            );
            assert_eq!(
                store
                    .resolve_entry(&entry.plan.source_for(&entry.entry.entry_id))
                    .unwrap()
                    .plan
                    .provider_context["token"],
                "rotated-before-transfer"
            );
            self.fail.store(true, Ordering::SeqCst);
            Ok(ShareOperationResult {
                result: if self.operation_error {
                    Err(QuarkError::WebAuthorizationRequired)
                } else {
                    Ok(super::super::commands::ResolvedCloudDownload {
                        url: "https://cdn.example/personal".into(),
                        name: "file".into(),
                        headers: vec![],
                    })
                },
                updated_context: Some(serde_json::json!({"token":"rotated-after-transfer"})),
            })
        }
        fn transfer_owner_fingerprint(
            &self,
            _: &CloudShareResolvedEntry,
        ) -> Result<Option<String>, QuarkError> {
            Ok(Some("a".repeat(64)))
        }
        fn transfer(
            &self,
            entry: &CloudShareResolvedEntry,
            store: &CloudSharePlanStore,
        ) -> Result<crate::cloud::commands::ResolvedCloudDownload, QuarkError> {
            self.transfer_with_context(entry, store)?.result
        }
    }
    impl CloudShareAdapter for UpdatingAdapter {
        fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
            Some(self)
        }
    }
    for (at_transfer, operation_error) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let fail = Arc::new(AtomicBool::new(false));
        let credentials = Arc::new(FailingStore {
            inner: MemoryCredentialStore::default(),
            fail: fail.clone(),
        });
        let store = CloudSharePlanStore {
            repository: None,
            credentials,
            mutation: Mutex::new(()),
            now_ms: Arc::new(|| 1000),
        };
        let plan = fixture("private-share");
        store.upsert(&plan).unwrap();
        let adapter = UpdatingAdapter {
            fail,
            at_transfer,
            operation_error,
        };
        assert!(matches!(
            resolve_current_share_download(&store, &adapter, &plan.source_for("f1")),
            Err(QuarkError::Storage)
        ));
    }
}

struct DirectAdapter(AtomicU64);
impl ShareInspect for DirectAdapter {
    fn inspect(
        &self,
        _: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        Ok(ShareInspection::Plan(fixture("private-share")))
    }
}
impl ShareResolve for DirectAdapter {
    fn resolve(&self, _: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        let revision = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ShareResolution::Direct(
            super::super::commands::ResolvedCloudDownload {
                url: format!("https://cdn.example/file?signature={revision}"),
                name: "file.txt".into(),
                headers: vec![("x-share-token".into(), format!("token-{revision}"))],
            },
        ))
    }
}
impl CloudShareAdapter for DirectAdapter {}

#[test]
fn expired_share_url_refreshes_once_with_new_headers_without_persisting_url() {
    let (store, credentials, _) = memory_store();
    let plan = fixture("private-share");
    store.upsert(&plan).unwrap();
    let source = plan.source_for("f1");
    let adapter = DirectAdapter(AtomicU64::new(0));
    let first = resolve_current_share_download(&store, &adapter, &source).unwrap();
    let refresh = CloudShareRefreshBudget::default();
    assert!(refresh.claim(&source, Some("22")).unwrap());
    let second = resolve_current_share_download(&store, &adapter, &source).unwrap();
    assert_eq!(first.url, "https://cdn.example/file?signature=0");
    assert_eq!(second.url, "https://cdn.example/file?signature=1");
    assert_eq!(
        second.headers,
        vec![("x-share-token".into(), "token-1".into())]
    );
    assert!(!refresh.claim(&source, Some("22")).unwrap());
    let saved = String::from_utf8(credentials.load(SHARE_PLAN_SECRET).unwrap().unwrap()).unwrap();
    assert!(!saved.contains("cdn.example") && !saved.contains("token-1"));
    assert!(!source.contains("signature") && !source.contains("private-share"));
}

#[test]
fn refresh_budget_ignores_non_share_and_non_http_failures() {
    let budget = CloudShareRefreshBudget::default();
    assert!(!budget.claim("cloud://baidu/file", Some("22")).unwrap());
    let source = fixture("share").source_for("f1");
    for code in [None, Some("3"), Some("403")] {
        assert!(!budget.claim(&source, code).unwrap());
    }
    assert!(budget.claim(&source, Some("22")).unwrap());
    assert!(!budget.claim(&source, Some("22")).unwrap());

    let http_forbidden = fixture("http-forbidden").source_for("f1");
    assert!(budget.claim(&http_forbidden, Some("24")).unwrap());
    assert!(!budget.claim(&http_forbidden, Some("24")).unwrap());

    let resume_unsupported = fixture("share").source_for("f2");
    assert!(budget.claim(&resume_unsupported, Some("8")).unwrap());
    assert!(!budget.claim(&resume_unsupported, Some("8")).unwrap());

    let interrupted = fixture("share").source_for("interrupted");
    assert!(budget.claim(&interrupted, Some("6")).unwrap());
    assert!(!budget.claim(&interrupted, Some("6")).unwrap());
    assert!(!budget.claim(&interrupted, Some("22")).unwrap());
}

#[test]
fn inspection_projects_only_opaque_file_ids_and_validates_confirmation_selection() {
    let (store, _, _) = memory_store();
    let adapter = DirectAdapter(AtomicU64::new(0));
    let candidate =
        crate::parser::extract_cloud_share_candidate("https://pan.baidu.com/s/private-share", None)
            .unwrap()
            .unwrap();
    let ShareConfirmationOutcome::Confirmation(confirmation) =
        inspect_share_with_adapter(&store, &adapter, &candidate).unwrap()
    else {
        panic!("expected plan")
    };
    let json = serde_json::to_string(&confirmation).unwrap();
    for secret in ["private-share", "secret-code", "secret-token", "https://"] {
        assert!(!json.contains(secret));
    }
    let sources = selected_share_sources(
        &store,
        &confirmation.plan_source,
        &[confirmation.resources[0].id.clone()],
    )
    .unwrap();
    assert_eq!(sources, vec![confirmation.plan_source.clone()]);
    assert!(selected_share_sources(
        &store,
        &confirmation.plan_source,
        &["forged-file-id".into()]
    )
    .is_err());
    assert!(selected_share_sources(&store, &confirmation.plan_source, &[]).is_err());
}

#[test]
fn confirmation_size_uses_human_readable_binary_units() {
    assert_eq!(format_confirmation_size(0), "0 B");
    assert_eq!(format_confirmation_size(1023), "1023 B");
    assert_eq!(format_confirmation_size(1024), "1.0 KB");
    assert_eq!(format_confirmation_size(1_572_864), "1.5 MB");
}

#[test]
fn selecting_share_sources_is_read_only_and_does_not_create_a_reservation() {
    let (store, _, _) = memory_store();
    let plan = fixture("read-only-selection");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    let selected_id = plan.entries[0].entry_fingerprint.clone();

    assert_eq!(
        selected_share_sources(&store, &source, &[selected_id]).unwrap(),
        vec![source.clone()]
    );

    assert!(store
        .resolve_entry(&source)
        .unwrap()
        .plan
        .linked_local_tasks
        .is_empty());
}

struct PlanAdapter(CloudSharePlan);
impl ShareInspect for PlanAdapter {
    fn inspect(
        &self,
        _: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        Ok(ShareInspection::Plan(self.0.clone()))
    }
}
impl ShareResolve for PlanAdapter {
    fn resolve(&self, _: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        unreachable!("inspection does not resolve a download")
    }
}
impl CloudShareAdapter for PlanAdapter {}

#[test]
fn only_an_explicit_exact_root_file_is_selected_for_automatic_submission() {
    let candidate =
        crate::parser::extract_cloud_share_candidate("https://pan.baidu.com/s/private-share", None)
            .unwrap()
            .unwrap();

    let mut file = fixture("private-share");
    file.root_kind = CloudShareRootKind::File {
        entry_id: "f1".into(),
    };
    let (store, _, _) = memory_store();
    let expected = file.source_for("f1");
    assert!(matches!(
        inspect_share_with_adapter(&store, &PlanAdapter(file), &candidate).unwrap(),
        ShareConfirmationOutcome::AutoSubmit { source, .. } if source == expected
    ));

    for root_kind in [
        CloudShareRootKind::Unknown,
        CloudShareRootKind::Directory,
        CloudShareRootKind::Collection,
    ] {
        let mut plan = fixture("private-share");
        plan.root_kind = root_kind;
        let (store, _, _) = memory_store();
        assert!(matches!(
            inspect_share_with_adapter(&store, &PlanAdapter(plan), &candidate).unwrap(),
            ShareConfirmationOutcome::Confirmation(_)
        ));
    }
}

#[test]
fn legacy_plan_without_root_kind_defaults_to_unknown_and_requires_confirmation() {
    let candidate =
        crate::parser::extract_cloud_share_candidate("https://pan.baidu.com/s/private-share", None)
            .unwrap()
            .unwrap();
    let mut value = serde_json::to_value(fixture("private-share")).unwrap();
    value.as_object_mut().unwrap().remove("root_kind");
    let legacy: CloudSharePlan = serde_json::from_value(value).unwrap();
    assert!(matches!(legacy.root_kind, CloudShareRootKind::Unknown));
    let (store, _, _) = memory_store();
    assert!(matches!(
        inspect_share_with_adapter(&store, &PlanAdapter(legacy), &candidate).unwrap(),
        ShareConfirmationOutcome::Confirmation(_)
    ));
}

#[test]
fn reinspection_of_a_root_file_keeps_older_linked_entries_loadable() {
    let (store, _, _) = memory_store();
    let mut previous = fixture("private-share");
    previous.root_kind = CloudShareRootKind::Directory;
    previous.entries.push(CloudShareEntry::new(
        "private-share",
        "old-file".into(),
        "old.txt".into(),
        vec![],
        Some(3),
    ));
    store.upsert(&previous).unwrap();

    let mut current = fixture("private-share");
    current.root_kind = CloudShareRootKind::File {
        entry_id: "f1".into(),
    };
    store.upsert(&current).unwrap();

    assert!(store.resolve_entry(&current.source_for("f1")).is_ok());
    assert!(store
        .resolve_entry(&previous.source_for("old-file"))
        .is_ok());
}

struct FixedAdapter(fn() -> Result<ShareResolution, QuarkError>);
impl ShareInspect for FixedAdapter {
    fn inspect(
        &self,
        _: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        Ok(ShareInspection::AccessCodeRequired)
    }
}
impl ShareResolve for FixedAdapter {
    fn resolve(&self, _: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        (self.0)()
    }
}
impl CloudShareAdapter for FixedAdapter {}

#[test]
fn share_pages_and_transfer_requirements_never_become_direct_downloads() {
    let (store, _, _) = memory_store();
    let plan = fixture("private-share");
    store.upsert(&plan).unwrap();
    let source = plan.source_for("f1");
    let page = FixedAdapter(|| {
        Ok(ShareResolution::Direct(
            super::super::commands::ResolvedCloudDownload {
                url: "https://pan.baidu.com/s/private-share?pwd=secret".into(),
                name: "file.txt".into(),
                headers: vec![],
            },
        ))
    });
    assert!(matches!(
        resolve_current_share_download(&store, &page, &source),
        Err(QuarkError::InvalidResponse)
    ));
    let transfer = FixedAdapter(|| {
        Ok(ShareResolution::TransferRequired(TransferRequest {
            source: fixture("private-share").source_for("f1"),
        }))
    });
    assert!(matches!(
        resolve_current_share_download(&store, &transfer, &source),
        Err(QuarkError::ProviderUnsupported)
    ));
}

#[test]
fn challenge_errors_are_returned_without_an_automatic_adapter_retry() {
    let (store, _, _) = memory_store();
    let plan = fixture("private-share");
    store.upsert(&plan).unwrap();
    let source = plan.source_for("f1");
    for error in [
        QuarkError::CaptchaRequired,
        QuarkError::WebAuthorizationRequired,
        QuarkError::PikpakRegionalMembershipRestricted,
        QuarkError::InvalidInput("访问码错误".into()),
        QuarkError::Provider("forbidden".into()),
    ] {
        struct OnceAdapter(Mutex<Option<QuarkError>>);
        impl ShareInspect for OnceAdapter {
            fn inspect(
                &self,
                _: &crate::parser::CloudShareCandidate,
            ) -> Result<ShareInspection, QuarkError> {
                unreachable!()
            }
        }
        impl ShareResolve for OnceAdapter {
            fn resolve(&self, _: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
                Err(self
                    .0
                    .lock()
                    .unwrap()
                    .take()
                    .expect("must not retry challenge"))
            }
        }
        impl CloudShareAdapter for OnceAdapter {}
        let adapter = OnceAdapter(Mutex::new(Some(error)));
        assert!(resolve_current_share_download(&store, &adapter, &source).is_err());
    }
}

fn fixture(id: &str) -> CloudSharePlan {
    CloudSharePlan::new(
        CloudProvider::Baidu,
        id.into(),
        Some("secret-code".into()),
        vec![CloudShareEntry::new(
            id,
            "f1".into(),
            "file.txt".into(),
            vec![],
            Some(7),
        )],
        serde_json::json!({"url":"https://pan.baidu.com/s/private-share", "token":"secret-token"}),
    )
}

fn memory_store() -> (
    CloudSharePlanStore,
    Arc<MemoryCredentialStore>,
    Arc<AtomicU64>,
) {
    let credentials = Arc::new(MemoryCredentialStore::default());
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = now.clone();
    let store = CloudSharePlanStore {
        repository: None,
        credentials: credentials.clone(),
        mutation: Mutex::new(()),
        now_ms: Arc::new(move || clock.load(Ordering::SeqCst)),
    };
    (store, credentials, now)
}

#[test]
fn discard_plan_removes_only_an_unlinked_prepared_share() {
    let (store, _, _) = memory_store();
    let plan = fixture("discard-prepared");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();

    store.discard_plan(&source).unwrap();
    assert!(store.resolve_entry(&source).is_err());

    let linked = fixture("discard-linked");
    let linked_source = linked.source_for("f1");
    store.upsert(&linked).unwrap();
    store
        .link_local_sources(std::slice::from_ref(&linked_source))
        .unwrap();
    store
        .link_local_task(&linked_source, uuid::Uuid::new_v4())
        .unwrap();
    assert!(store.discard_plan(&linked_source).is_err());
    assert!(store.resolve_entry(&linked_source).is_ok());
}

#[test]
fn selected_directory_share_keeps_safe_paths_and_requests_one_group_root() {
    let (store, _, _) = memory_store();
    let mut plan = CloudSharePlan::new(
        CloudProvider::Quark,
        "shared-wallpapers".into(),
        None,
        vec![
            CloudShareEntry::new(
                "shared-wallpapers",
                "cover".into(),
                "cover.jpg".into(),
                vec![],
                Some(11),
            ),
            CloudShareEntry::new(
                "shared-wallpapers",
                "nested".into(),
                "nested.jpg".into(),
                vec!["mobile".into()],
                Some(22),
            ),
        ],
        serde_json::json!({}),
    );
    plan.root_kind = CloudShareRootKind::Directory;
    let plan_source = plan.source_for("cover");
    let selected_ids = plan
        .entries
        .iter()
        .map(|entry| entry.entry_fingerprint.clone())
        .collect::<Vec<_>>();
    store.upsert(&plan).unwrap();

    let selected = selected_share_download(&store, &plan_source, &selected_ids).unwrap();

    assert_eq!(selected.provider, CloudProvider::Quark);
    assert_eq!(selected.directory_name.as_deref(), Some("夸克分享目录"));
    assert_eq!(selected.entries.len(), 2);
    assert_eq!(selected.entries[0].selected_id, selected_ids[0]);
    assert_eq!(selected.entries[0].relative_directory, Vec::<String>::new());
    assert_eq!(selected.entries[0].size, Some(11));
    assert!(selected.entries[0]
        .source
        .starts_with("cloud-share://quark/"));
    assert_eq!(selected.entries[1].relative_directory, vec!["mobile"]);
    assert_eq!(selected.entries[1].size, Some(22));
}

#[test]
fn selected_directory_share_uses_its_single_parsed_directory_name() {
    let (store, _, _) = memory_store();
    let mut plan = CloudSharePlan::new(
        CloudProvider::Quark,
        "shared-video".into(),
        None,
        vec![
            CloudShareEntry::new(
                "shared-video",
                "episode".into(),
                "episode.mp4".into(),
                vec!["YM-JFSMBYHS".into()],
                Some(100),
            ),
            CloudShareEntry::new(
                "shared-video",
                "bonus".into(),
                "bonus.mp4".into(),
                vec!["YM-JFSMBYHS".into(), "Extras".into()],
                Some(200),
            ),
        ],
        serde_json::json!({}),
    );
    plan.root_kind = CloudShareRootKind::Directory;
    let plan_source = plan.source_for("episode");
    let selected_ids = plan
        .entries
        .iter()
        .map(|entry| entry.entry_fingerprint.clone())
        .collect::<Vec<_>>();
    store.upsert(&plan).unwrap();

    let selected = selected_share_download(&store, &plan_source, &selected_ids).unwrap();

    assert_eq!(selected.directory_name.as_deref(), Some("YM-JFSMBYHS"));
}

#[test]
fn exact_shared_file_remains_an_individual_download() {
    let (store, _, _) = memory_store();
    let mut plan = fixture("single-file-share");
    plan.root_kind = CloudShareRootKind::File {
        entry_id: "f1".into(),
    };
    let source = plan.source_for("f1");
    let selected_id = plan.entries[0].entry_fingerprint.clone();
    store.upsert(&plan).unwrap();

    let selected = selected_share_download(&store, &source, &[selected_id]).unwrap();

    assert_eq!(selected.directory_name, None);
    assert_eq!(selected.entries.len(), 1);
}

#[test]
fn stable_share_source_has_only_hashes_and_binds_entry_to_share() {
    let source = stable_share_source(CloudProvider::Baidu, "share-id", "file-id");
    let (provider, share, entry) = parse_stable_cloud_share_source(&source).unwrap();
    assert_eq!(provider, CloudProvider::Baidu);
    assert_eq!(share.len(), 64);
    assert_eq!(entry.len(), 64);
    assert!(!source.contains("share-id") && !source.contains("file-id"));
    assert_eq!(source, stable_share_source(provider, "share-id", "file-id"));
    let other = stable_share_source(provider, "different-share", "file-id");
    assert_ne!(entry, parse_stable_cloud_share_source(&other).unwrap().2);
}

#[test]
fn parser_accepts_only_exact_providers_and_lowercase_sha256_segments() {
    let hash = "a".repeat(64);
    for provider in [
        CloudProvider::Quark,
        CloudProvider::Pikpak,
        CloudProvider::Baidu,
        CloudProvider::Tianyi,
        CloudProvider::Aliyun,
        CloudProvider::Xunlei,
        CloudProvider::Uc,
    ] {
        assert_eq!(
            parse_stable_cloud_share_source(&format!(
                "cloud-share://{}/{hash}/{hash}",
                provider.key()
            ))
            .unwrap()
            .0,
            provider
        );
    }
    for source in [
        format!("cloud-share://other/{hash}/{hash}"),
        format!("cloud-share://Baidu/{hash}/{hash}"),
        format!("CLOUD-SHARE://baidu/{hash}/{hash}"),
        format!("cloud-share://user@baidu/{hash}/{hash}"),
        format!("cloud-share://baidu:443/{hash}/{hash}"),
        format!("cloud-share://baidu/{hash}/{hash}/"),
        format!("cloud-share://baidu/{hash}/{hash}?pwd=secret"),
        format!("cloud-share://baidu/{hash}/{hash}#secret"),
        format!(" cloud-share://baidu/{hash}/{hash}"),
        format!("cloud-share://baidu/{}/{hash}", "A".repeat(64)),
        format!("cloud-share://baidu/{}/{hash}", "g".repeat(64)),
        format!("cloud-share://baidu/{}/{hash}", "a".repeat(63)),
        format!("cloud-share://baidu/{}/{hash}", "a".repeat(65)),
        format!("cloud-share://baidu/%61{}/{hash}", "a".repeat(63)),
        "cloud-share://baidu//".into(),
    ] {
        assert!(
            parse_stable_cloud_share_source(&source).is_err(),
            "accepted {source}"
        );
    }
}

#[test]
fn round_trip_uses_fixed_key_and_recovers_secrets_without_exposing_them() {
    let (store, credentials, _) = memory_store();
    let plan = fixture("private-share");
    store.upsert(&plan).unwrap();
    assert!(credentials.load("cloud-share-plans-v1").unwrap().is_some());
    let resolved = store.resolve_entry(&plan.source_for("f1")).unwrap();
    assert_eq!(resolved.access_code(), Some("secret-code"));
    assert_eq!(resolved.plan.provider_context["token"], "secret-token");
    assert_eq!(resolved.entry.entry_id, "f1");
    for secret in ["secret-code", "secret-token", "private-share", "https://"] {
        assert!(!plan.source_for("f1").contains(secret));
        assert!(!format!("{resolved:?}").contains(secret));
    }
    let unknown = stable_share_source(CloudProvider::Baidu, "private-share", "unknown");
    assert!(store.resolve_entry(&unknown).is_err());
}

#[test]
fn plans_expire_at_thirty_days_and_resolution_refreshes_retention() {
    let (store, _, now) = memory_store();
    let old = fixture("old");
    let active = fixture("active");
    store.upsert(&old).unwrap();
    store.upsert(&active).unwrap();
    now.store(1_000 + 29 * 24 * 60 * 60 * 1_000, Ordering::SeqCst);
    store.resolve_entry(&active.source_for("f1")).unwrap();
    now.store(1_000 + 30 * 24 * 60 * 60 * 1_000, Ordering::SeqCst);
    assert_eq!(store.prune().unwrap(), 1);
    assert!(store.resolve_entry(&old.source_for("f1")).is_err());
    assert!(store.resolve_entry(&active.source_for("f1")).is_ok());
}

#[test]
fn ttl_pruning_keeps_every_live_plan_and_removes_only_terminal_or_unbound_plans() {
    let (store, _, now) = memory_store();
    let reservation = fixture("reservation");
    let linked = fixture("linked");
    let pending = fixture("pending");
    let attempted = fixture("attempted");
    let failed = fixture("failed");
    let succeeded = fixture("succeeded");
    let unbound = fixture("unbound");

    for plan in [
        &reservation,
        &linked,
        &pending,
        &attempted,
        &failed,
        &succeeded,
        &unbound,
    ] {
        store.upsert(plan).unwrap();
    }
    store
        .link_local_sources(&[reservation.source_for("f1")])
        .unwrap();
    store
        .link_local_sources(&[linked.source_for("f1")])
        .unwrap();
    store
        .link_local_task(&linked.source_for("f1"), uuid::Uuid::new_v4())
        .unwrap();
    store
        .record_transfer(&pending.source_for("f1"), "pending-id")
        .unwrap();
    store
        .record_transfer(&attempted.source_for("f1"), "attempted-id")
        .unwrap();
    store
        .mark_cleanup_attempted(&attempted.source_for("f1"))
        .unwrap();
    store
        .record_transfer(&failed.source_for("f1"), "failed-id")
        .unwrap();
    store
        .mark_cleanup_attempted(&failed.source_for("f1"))
        .unwrap();
    store
        .finish_cleanup(&failed.source_for("f1"), &["failed-id".into()], false)
        .unwrap();
    store
        .record_transfer(&succeeded.source_for("f1"), "succeeded-id")
        .unwrap();
    store
        .mark_cleanup_attempted(&succeeded.source_for("f1"))
        .unwrap();
    store
        .finish_cleanup(&succeeded.source_for("f1"), &["succeeded-id".into()], true)
        .unwrap();

    now.store(1_000 + RETENTION_MS, Ordering::SeqCst);
    assert_eq!(store.prune().unwrap(), 2);
    for plan in [&reservation, &linked, &pending, &attempted, &failed] {
        assert!(store.resolve_entry(&plan.source_for("f1")).is_ok());
    }
    for plan in [&succeeded, &unbound] {
        assert!(store.resolve_entry(&plan.source_for("f1")).is_err());
    }
}

#[test]
fn distinct_upsert_fails_without_rewriting_a_full_pinned_document_but_update_is_allowed() {
    let (store, credentials, _) = memory_store();
    let mut first = fixture("share-0");
    for index in 0..MAX_PLANS {
        let plan = fixture(&format!("share-{index}"));
        store.upsert(&plan).unwrap();
        store.link_local_sources(&[plan.source_for("f1")]).unwrap();
    }
    let before = credentials.load(SHARE_PLAN_SECRET).unwrap().unwrap();

    assert!(store.upsert(&fixture("overflow")).is_err());
    assert_eq!(
        credentials.load(SHARE_PLAN_SECRET).unwrap().unwrap(),
        before
    );

    first.provider_context = serde_json::json!({"token":"updated"});
    store.upsert(&first).unwrap();
    assert_eq!(
        store
            .resolve_entry(&first.source_for("f1"))
            .unwrap()
            .plan
            .provider_context["token"],
        "updated"
    );
    assert!(store
        .resolve_entry(&fixture("overflow").source_for("f1"))
        .is_err());
}

#[test]
fn unlink_is_exact_and_releases_an_empty_entry_only_after_cleanup_is_terminal() {
    let (store, _, _) = memory_store();
    let mut plan = fixture("unlink");
    plan.entries.push(CloudShareEntry::new(
        "unlink",
        "f2".into(),
        "other.txt".into(),
        vec![],
        Some(9),
    ));
    let first = plan.source_for("f1");
    let second = plan.source_for("f2");
    let first_task = uuid::Uuid::new_v4();
    let second_task = uuid::Uuid::new_v4();
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(&[first.clone(), second.clone()])
        .unwrap();
    store.link_local_task(&first, first_task).unwrap();
    store.link_local_task(&second, second_task).unwrap();
    store.record_transfer(&first, "created-id").unwrap();

    store.unlink_local_task(&first, first_task).unwrap();
    let pending = store.resolve_entry(&second).unwrap().plan;
    assert_eq!(
        pending.linked_local_tasks[&parse_stable_cloud_share_source(&first).unwrap().2].len(),
        0
    );
    assert_eq!(
        pending.linked_local_tasks[&parse_stable_cloud_share_source(&second).unwrap().2],
        BTreeSet::from([second_task])
    );

    store.mark_cleanup_attempted(&first).unwrap();
    store
        .finish_cleanup(&first, &["created-id".into()], true)
        .unwrap();
    let terminal = store.resolve_entry(&second).unwrap().plan;
    assert!(!terminal
        .linked_local_tasks
        .contains_key(&parse_stable_cloud_share_source(&first).unwrap().2));
    assert_eq!(terminal.linked_local_tasks.len(), 1);
}

#[test]
fn unlinking_many_local_tasks_saves_the_share_document_once() {
    let credentials = Arc::new(FailNthSaveCredentials {
        inner: MemoryCredentialStore::default(),
        saves: AtomicU64::new(0),
        fail_at: u64::MAX,
    });
    let store = CloudSharePlanStore {
        repository: None,
        credentials: credentials.clone(),
        mutation: Mutex::new(()),
        now_ms: Arc::new(|| 1_000),
    };
    let mut plan = fixture("batched-unlink");
    plan.entries.push(CloudShareEntry::new(
        "batched-unlink",
        "f2".into(),
        "other.txt".into(),
        vec![],
        Some(9),
    ));
    let first = plan.source_for("f1");
    let second = plan.source_for("f2");
    let first_task = uuid::Uuid::new_v4();
    let second_task = uuid::Uuid::new_v4();
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(&[first.clone(), second.clone()])
        .unwrap();
    store.link_local_task(&first, first_task).unwrap();
    store.link_local_task(&second, second_task).unwrap();
    let saves_before_unlink = credentials.saves.load(Ordering::SeqCst);

    store
        .unlink_local_tasks(&[(first.clone(), first_task), (second.clone(), second_task)])
        .unwrap();

    assert_eq!(
        credentials.saves.load(Ordering::SeqCst),
        saves_before_unlink + 1
    );
    let current = store.resolve_entry(&first).unwrap().plan;
    assert!(current.linked_local_tasks.is_empty());
}

#[test]
fn restoring_many_local_task_links_saves_the_share_document_once() {
    let credentials = Arc::new(FailNthSaveCredentials {
        inner: MemoryCredentialStore::default(),
        saves: AtomicU64::new(0),
        fail_at: u64::MAX,
    });
    let store = CloudSharePlanStore {
        repository: None,
        credentials: credentials.clone(),
        mutation: Mutex::new(()),
        now_ms: Arc::new(|| 1_000),
    };
    let mut plan = fixture("batched-relink");
    plan.entries.push(CloudShareEntry::new(
        "batched-relink",
        "f2".into(),
        "other.txt".into(),
        vec![],
        Some(9),
    ));
    let first = plan.source_for("f1");
    let second = plan.source_for("f2");
    let first_task = uuid::Uuid::new_v4();
    let second_task = uuid::Uuid::new_v4();
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(&[first.clone(), second.clone()])
        .unwrap();
    let saves_before_restore = credentials.saves.load(Ordering::SeqCst);

    store
        .link_local_tasks(&[(first.clone(), first_task), (second.clone(), second_task)])
        .unwrap();

    assert_eq!(
        credentials.saves.load(Ordering::SeqCst),
        saves_before_restore + 1
    );
    let current = store.resolve_entry(&first).unwrap().plan;
    assert!(current
        .linked_local_tasks
        .values()
        .any(|ids| ids.contains(&first_task)));
    assert!(current
        .linked_local_tasks
        .values()
        .any(|ids| ids.contains(&second_task)));
}

#[test]
fn finishing_one_unlinked_transfer_keeps_an_unrelated_empty_submission_reservation() {
    let (store, _, _) = memory_store();
    let mut plan = fixture("reservation-isolation");
    plan.entries.push(CloudShareEntry::new(
        "reservation-isolation",
        "f2".into(),
        "reserved.txt".into(),
        vec![],
        Some(9),
    ));
    let transferred = plan.source_for("f1");
    let reserved = plan.source_for("f2");
    let task_id = uuid::Uuid::new_v4();
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(&[transferred.clone(), reserved.clone()])
        .unwrap();
    store.link_local_task(&transferred, task_id).unwrap();
    store.record_transfer(&transferred, "created-id").unwrap();
    store.unlink_local_task(&transferred, task_id).unwrap();
    store.mark_cleanup_attempted(&transferred).unwrap();

    store
        .finish_cleanup(&transferred, &["created-id".into()], true)
        .unwrap();

    let current = store.resolve_entry(&reserved).unwrap().plan;
    let reserved_fingerprint = parse_stable_cloud_share_source(&reserved).unwrap().2;
    assert!(current
        .linked_local_tasks
        .contains_key(&reserved_fingerprint));
    assert!(current.linked_local_tasks[&reserved_fingerprint].is_empty());
}

#[test]
fn unlink_with_an_unregistered_task_id_cannot_release_a_submission_reservation() {
    let (store, _, _) = memory_store();
    let plan = fixture("reserved-without-task");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(std::slice::from_ref(&source))
        .unwrap();

    store
        .unlink_local_task(&source, uuid::Uuid::new_v4())
        .unwrap();

    let current = store.resolve_entry(&source).unwrap().plan;
    let fingerprint = parse_stable_cloud_share_source(&source).unwrap().2;
    assert!(current.linked_local_tasks.contains_key(&fingerprint));
    assert!(current.linked_local_tasks[&fingerprint].is_empty());
}

#[test]
fn atomic_task_reservation_survives_an_old_same_source_task_being_unlinked_first() {
    let (store, _, _) = memory_store();
    let plan = fixture("same-source-replacement");
    let source = plan.source_for("f1");
    let old_task = uuid::Uuid::new_v4();
    let new_task = uuid::Uuid::new_v4();
    store.upsert(&plan).unwrap();
    store.reserve_local_task(&source, old_task).unwrap();

    store.unlink_local_task(&source, old_task).unwrap();
    store.reserve_local_task(&source, new_task).unwrap();

    let current = store.resolve_entry(&source).unwrap().plan;
    let fingerprint = parse_stable_cloud_share_source(&source).unwrap().2;
    assert_eq!(
        current.linked_local_tasks[&fingerprint],
        BTreeSet::from([new_task])
    );
}

#[tokio::test]
async fn unavailable_engine_prevents_task_creation_and_provider_resolution() {
    let (plans, _, _) = memory_store();
    let plan = fixture("engine-unavailable");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let tasks = Arc::new(crate::TaskStore::open_in_memory().unwrap());
    let resolver_calls = Arc::new(AtomicU64::new(0));
    let calls = resolver_calls.clone();

    let result = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source,
        crate::UriSubmissionOptions::default(),
        || async {
            Err::<Arc<crate::DownloadService>, _>(QuarkError::Transport(
                "engine unavailable".into(),
            ))
        },
        || async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(QuarkError::InvalidResponse)
        },
    )
    .await;

    assert!(result.is_err());
    assert!(tasks.list().unwrap().is_empty());
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    assert!(plans
        .resolve_entry(&plan.source_for("f1"))
        .unwrap()
        .plan
        .transfers
        .is_empty());
}

#[tokio::test]
async fn share_directory_leaf_failure_does_not_project_the_complete_directory_tree() {
    let (plans, _, _) = memory_store();
    let plan = fixture("directory-engine-unavailable");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let tasks = Arc::new(crate::TaskStore::open_in_memory().unwrap());
    let destination = tempfile::tempdir().unwrap();
    let group = tasks
        .create_cloud_directory_group(
            crate::models::NewCloudDirectoryGroup::new(
                CloudProvider::Baidu,
                source.clone(),
                "百度网盘分享目录",
                destination.path(),
            ),
            &[crate::models::NewCloudDirectoryGroupItem::new(
                source.clone(),
                &["百度网盘分享目录"],
                "file.txt",
                Some(7),
            )],
        )
        .unwrap();
    let item = tasks
        .cloud_directory_group_items(group.id)
        .unwrap()
        .remove(0);
    let resolver_calls = Arc::new(AtomicU64::new(0));
    let calls = resolver_calls.clone();

    let outcome = crate::cloud::commands::submit_new_cloud_share_file_scoped_with(
        tasks.clone(),
        &plans,
        source,
        crate::UriSubmissionOptions {
            directory: Some(destination.path().join("百度网盘分享目录")),
            ..Default::default()
        },
        Some(crate::cloud::commands::DirectoryLeafContext {
            group_id: group.id,
            item_id: item.id,
        }),
        false,
        false,
        None,
        || async {
            Err::<Arc<crate::DownloadService>, _>(QuarkError::Transport(
                "engine unavailable".into(),
            ))
        },
        || async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(QuarkError::InvalidResponse)
        },
    )
    .await
    .unwrap();

    assert_eq!(outcome.payload.task.status, crate::TaskStatus::Failed);
    assert!(
        outcome.queue_task.is_none(),
        "directory scheduling only needs the leaf task; rebuilding the whole tree for every failed leaf is quadratic"
    );
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        tasks
            .cloud_directory_group_for_task(outcome.payload.task.id)
            .unwrap()
            .map(|linked| linked.id),
        Some(group.id)
    );
}

#[tokio::test]
async fn named_share_task_is_acknowledged_before_provider_resolution() {
    let (plans, _, _) = memory_store();
    let plan = fixture("early-ack");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let tasks = Arc::new(crate::TaskStore::open_in_memory().unwrap());
    let service = Arc::new(crate::DownloadService::new(
        Arc::new(SagaAria2::default()),
        tasks.clone(),
    ));
    let acknowledged = Arc::new(Mutex::new(None));
    let received = acknowledged.clone();
    let expected_name = "file.txt";
    let result = crate::cloud::commands::submit_new_cloud_share_file_scoped_with(
        tasks.clone(),
        &plans,
        source,
        Default::default(),
        None,
        true,
        false,
        Some(Arc::new(move |task: &crate::DownloadTask| {
            *received.lock().unwrap() = Some((task.id, task.name.clone()));
        })),
        || async { Ok(service) },
        || async {
            let (id, name) = acknowledged
                .lock()
                .unwrap()
                .clone()
                .expect("task creation must precede provider requests");
            assert_eq!(name, expected_name);
            assert_eq!(tasks.find(id).unwrap().status, crate::TaskStatus::Parsing);
            Err(QuarkError::NotAuthorized)
        },
    )
    .await
    .unwrap();
    assert_eq!(result.payload.task.status, crate::TaskStatus::Failed);
    assert_eq!(
        tasks.list().unwrap().len(),
        1,
        "failure retains a retryable task"
    );
}

#[tokio::test]
async fn failed_atomic_reservation_removes_the_unsubmitted_task_without_provider_calls() {
    let credentials = Arc::new(FailNthSaveCredentials {
        inner: MemoryCredentialStore::default(),
        saves: AtomicU64::new(0),
        fail_at: 3,
    });
    let plans = CloudSharePlanStore {
        repository: None,
        credentials,
        mutation: Mutex::new(()),
        now_ms: Arc::new(|| 1_000),
    };
    let plan = fixture("reservation-failure");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let tasks = Arc::new(crate::TaskStore::open_in_memory().unwrap());
    let api = Arc::new(SagaAria2::default());
    let service = Arc::new(crate::DownloadService::new(api.clone(), tasks.clone()));
    let adapter = SagaAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        fail_resolve: false,
    };
    let resolve_source = source.clone();

    let result = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source,
        crate::UriSubmissionOptions::default(),
        || async { Ok(service) },
        || async { resolve_current_share_download(&plans, &adapter, &resolve_source) },
    )
    .await;

    assert!(result.is_err());
    assert!(tasks.list().unwrap().is_empty());
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 0);
    assert_eq!(api.add_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_reservation_and_failed_rollback_returns_a_durable_failed_task() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("tasks.sqlite");
    let tasks = Arc::new(crate::TaskStore::open_path(&database).unwrap());
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_task_delete BEFORE DELETE ON tasks \
                 BEGIN SELECT RAISE(FAIL, 'delete rejected'); END;",
        )
        .unwrap();
    let credentials = Arc::new(FailNthSaveCredentials {
        inner: MemoryCredentialStore::default(),
        saves: AtomicU64::new(0),
        fail_at: 3,
    });
    let plans = CloudSharePlanStore {
        repository: None,
        credentials,
        mutation: Mutex::new(()),
        now_ms: Arc::new(|| 1_000),
    };
    let plan = fixture("reservation-rollback-failure");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let api = Arc::new(SagaAria2::default());
    let service = Arc::new(crate::DownloadService::new(api.clone(), tasks.clone()));

    let outcome = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source.clone(),
        crate::UriSubmissionOptions::default(),
        || async { Ok(service) },
        || async { unreachable!("provider must not run before reservation succeeds") },
    )
    .await
    .expect("a failed rollback must return the task which may still be durable");

    assert_eq!(outcome.payload.task.status, crate::TaskStatus::Failed);
    assert_eq!(
        outcome.payload.task.error.as_deref(),
        Some("网盘分享下载准备失败，请重试")
    );
    assert_eq!(
        tasks.find(outcome.payload.task.id).unwrap().status,
        crate::TaskStatus::Failed
    );
    assert_eq!(api.add_calls.load(Ordering::SeqCst), 0);
    assert!(plans
        .resolve_entry(&source)
        .unwrap()
        .plan
        .linked_local_tasks
        .is_empty());
}

#[tokio::test]
async fn failed_prepare_retry_reuses_and_reserves_the_exact_task() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("tasks.sqlite");
    let tasks = Arc::new(crate::TaskStore::open_path(&database).unwrap());
    let administration = rusqlite::Connection::open(&database).unwrap();
    administration
        .execute_batch(
            "CREATE TRIGGER reject_task_delete BEFORE DELETE ON tasks \
                 BEGIN SELECT RAISE(FAIL, 'delete rejected'); END;",
        )
        .unwrap();
    let credentials = Arc::new(FailNthSaveCredentials {
        inner: MemoryCredentialStore::default(),
        saves: AtomicU64::new(0),
        fail_at: 3,
    });
    let plans = CloudSharePlanStore {
        repository: None,
        credentials,
        mutation: Mutex::new(()),
        now_ms: Arc::new(|| 1_000),
    };
    let plan = fixture("retry-failed-prepare");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let api = Arc::new(SagaAria2::default());
    let service = Arc::new(crate::DownloadService::new(api.clone(), tasks.clone()));

    let failed = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source.clone(),
        crate::UriSubmissionOptions::default(),
        || async { Ok(service.clone()) },
        || async { unreachable!("provider must not run before reservation succeeds") },
    )
    .await
    .unwrap();
    administration
        .execute_batch("DROP TRIGGER reject_task_delete")
        .unwrap();
    let adapter = SagaAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        fail_resolve: false,
    };
    let resolve_source = source.clone();

    let retried = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source.clone(),
        crate::UriSubmissionOptions::default(),
        || async { Ok(service) },
        || async { resolve_current_share_download(&plans, &adapter, &resolve_source) },
    )
    .await
    .unwrap();

    assert_eq!(retried.payload.task.id, failed.payload.task.id);
    assert_eq!(retried.payload.task.status, crate::TaskStatus::Downloading);
    assert_eq!(api.add_calls.load(Ordering::SeqCst), 1);
    assert!(plans
        .resolve_entry(&source)
        .unwrap()
        .plan
        .linked_local_tasks
        .values()
        .any(|ids| ids.contains(&failed.payload.task.id)));
}

#[tokio::test]
async fn provider_resolution_failure_returns_the_reserved_failed_task() {
    let (plans, _, _) = memory_store();
    let plan = fixture("provider-resolution-failure");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let tasks = Arc::new(crate::TaskStore::open_in_memory().unwrap());
    let api = Arc::new(SagaAria2::default());
    let service = Arc::new(crate::DownloadService::new(api.clone(), tasks.clone()));
    let adapter = SagaAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        fail_resolve: true,
    };
    let resolve_source = source.clone();

    let outcome = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source.clone(),
        crate::UriSubmissionOptions::default(),
        || async { Ok(service) },
        || async { resolve_current_share_download(&plans, &adapter, &resolve_source) },
    )
    .await
    .expect("a prepared cloud-share task must be returned after resolution fails");

    assert_eq!(outcome.payload.task.status, crate::TaskStatus::Failed);
    assert_eq!(outcome.payload.task.source, source);
    assert_eq!(
        outcome.payload.task.error.as_deref(),
        Some("网盘会话已过期，请重新登录授权")
    );
    assert_eq!(
        tasks.find(outcome.payload.task.id).unwrap().status,
        crate::TaskStatus::Failed
    );
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.transfer_calls.load(Ordering::SeqCst), 0);
    assert_eq!(api.add_calls.load(Ordering::SeqCst), 0);
    assert!(plans
        .resolve_entry(&source)
        .unwrap()
        .plan
        .linked_local_tasks
        .values()
        .any(|ids| ids.contains(&outcome.payload.task.id)));
}

#[tokio::test]
async fn aria2_rejection_returns_the_reserved_failed_task() {
    let (plans, _, _) = memory_store();
    let plan = fixture("aria2-rejection");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let tasks = Arc::new(crate::TaskStore::open_in_memory().unwrap());
    let api = Arc::new(SagaAria2::default());
    api.reject_add.store(true, Ordering::SeqCst);
    let service = Arc::new(crate::DownloadService::new(api.clone(), tasks.clone()));
    let adapter = SagaAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        fail_resolve: false,
    };
    let resolve_source = source.clone();

    let outcome = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source.clone(),
        crate::UriSubmissionOptions::default(),
        || async { Ok(service) },
        || async { resolve_current_share_download(&plans, &adapter, &resolve_source) },
    )
    .await
    .expect("a prepared cloud-share task must be returned after aria2 rejects it");

    assert_eq!(outcome.payload.task.status, crate::TaskStatus::Failed);
    assert_eq!(outcome.payload.task.source, source);
    assert_eq!(
        outcome.payload.task.error.as_deref(),
        Some("aria2 未能接受网盘分享下载任务")
    );
    assert_eq!(
        tasks.find(outcome.payload.task.id).unwrap().status,
        crate::TaskStatus::Failed
    );
    assert_eq!(adapter.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(api.add_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_store_recovery_does_not_fabricate_a_failed_task_in_memory() {
    let (plans, _, _) = memory_store();
    let plan = fixture("failed-store-recovery");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("tasks.sqlite");
    let tasks = Arc::new(crate::TaskStore::open_path(&database).unwrap());
    let api = Arc::new(SagaAria2::default());
    let service = Arc::new(crate::DownloadService::new(api, tasks.clone()));
    let drop_database = database.clone();

    let outcome = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks,
        &plans,
        source.clone(),
        crate::UriSubmissionOptions::default(),
        || async { Ok(service) },
        || async move {
            rusqlite::Connection::open(drop_database)
                .unwrap()
                .execute_batch("DROP TABLE tasks")
                .unwrap();
            Err(QuarkError::AuthorizationExpired)
        },
    )
    .await
    .expect("a durable-create result must not be reported as no task when recovery fails");

    assert_eq!(outcome.payload.task.source, source);
    assert_eq!(outcome.payload.task.status, crate::TaskStatus::Parsing);
    assert_eq!(outcome.payload.task.error, None);
    assert!(outcome.payload.files.is_empty());
}

#[tokio::test]
async fn stale_parsing_task_after_failed_failure_write_is_reused_on_retry() {
    let (plans, _, _) = memory_store();
    let plan = fixture("stale-parsing-retry");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("tasks.sqlite");
    let tasks = Arc::new(crate::TaskStore::open_path(&database).unwrap());
    let administration = rusqlite::Connection::open(&database).unwrap();
    let api = Arc::new(SagaAria2::default());
    let service = Arc::new(crate::DownloadService::new(api.clone(), tasks.clone()));
    let create_failure_trigger = database.clone();

    let failed = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source.clone(),
        crate::UriSubmissionOptions::default(),
        || async { Ok(service.clone()) },
        || async move {
            rusqlite::Connection::open(create_failure_trigger)
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER reject_failed_status BEFORE UPDATE OF status ON tasks \
                         WHEN NEW.status = 'failed' \
                         BEGIN SELECT RAISE(FAIL, 'failed status rejected'); END;",
                )
                .unwrap();
            Err(QuarkError::AuthorizationExpired)
        },
    )
    .await
    .unwrap();
    let durable = tasks.find(failed.payload.task.id).unwrap();
    assert_eq!(failed.payload.task.id, durable.id);
    assert_eq!(failed.payload.task.status, durable.status);
    assert_eq!(failed.payload.task.error, durable.error);
    administration
        .execute_batch("DROP TRIGGER reject_failed_status")
        .unwrap();
    let adapter = SagaAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        fail_resolve: false,
    };
    let resolve_source = source.clone();

    let retried = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks.clone(),
        &plans,
        source,
        crate::UriSubmissionOptions::default(),
        || async { Ok(service) },
        || async { resolve_current_share_download(&plans, &adapter, &resolve_source) },
    )
    .await
    .unwrap();

    assert_eq!(retried.payload.task.id, failed.payload.task.id);
    assert_eq!(retried.payload.task.status, crate::TaskStatus::Downloading);
    assert_eq!(api.add_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn files_read_failure_falls_back_to_an_empty_outcome_file_list() {
    let (plans, _, _) = memory_store();
    let plan = fixture("files-read-failure");
    let source = plan.source_for("f1");
    plans.upsert(&plan).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("tasks.sqlite");
    let tasks = Arc::new(crate::TaskStore::open_path(&database).unwrap());
    let api = Arc::new(SagaAria2::default());
    *api.drop_task_files.lock().unwrap() = Some(database);
    let service = Arc::new(crate::DownloadService::new(api.clone(), tasks.clone()));
    let adapter = SagaAdapter {
        resolve_calls: AtomicU64::new(0),
        transfer_calls: AtomicU64::new(0),
        fail_resolve: false,
    };
    let resolve_source = source.clone();

    let outcome = crate::cloud::commands::submit_new_cloud_share_file_with(
        tasks,
        &plans,
        source,
        crate::UriSubmissionOptions::default(),
        || async { Ok(service) },
        || async { resolve_current_share_download(&plans, &adapter, &resolve_source) },
    )
    .await
    .expect("files projection failure must not discard the prepared task outcome");

    assert_eq!(outcome.payload.task.status, crate::TaskStatus::Failed);
    assert!(outcome.payload.files.is_empty());
    assert_eq!(api.add_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_unlink_save_preserves_the_durable_task_pin() {
    use std::sync::atomic::AtomicBool;
    struct FailingCredentials {
        inner: MemoryCredentialStore,
        fail: AtomicBool,
    }
    impl CredentialStore for FailingCredentials {
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
            self.inner.load(key)
        }
        fn remove(&self, key: &str) -> Result<(), QuarkError> {
            self.inner.remove(key)
        }
        fn save(&self, key: &str, bytes: &[u8]) -> Result<(), QuarkError> {
            if self.fail.load(Ordering::SeqCst) {
                Err(QuarkError::Storage)
            } else {
                self.inner.save(key, bytes)
            }
        }
    }
    let credentials = Arc::new(FailingCredentials {
        inner: MemoryCredentialStore::default(),
        fail: AtomicBool::new(false),
    });
    let store = CloudSharePlanStore {
        repository: None,
        credentials: credentials.clone(),
        mutation: Mutex::new(()),
        now_ms: Arc::new(|| 1_000),
    };
    let plan = fixture("unlink-save-failure");
    let source = plan.source_for("f1");
    let task_id = uuid::Uuid::new_v4();
    store.upsert(&plan).unwrap();
    store
        .link_local_sources(std::slice::from_ref(&source))
        .unwrap();
    store.link_local_task(&source, task_id).unwrap();

    credentials.fail.store(true, Ordering::SeqCst);
    assert!(store.unlink_local_task(&source, task_id).is_err());
    credentials.fail.store(false, Ordering::SeqCst);
    let durable = store.resolve_entry(&source).unwrap().plan;
    assert!(durable
        .linked_local_tasks
        .values()
        .any(|ids| ids.contains(&task_id)));
}

#[test]
fn lru_caps_plans_at_256_even_when_accesses_share_a_timestamp() {
    let (store, _, _) = memory_store();
    for index in 0..256 {
        store.upsert(&fixture(&format!("share-{index}"))).unwrap();
    }
    let first = fixture("share-0").source_for("f1");
    store.resolve_entry(&first).unwrap();
    store.upsert(&fixture("share-256")).unwrap();
    assert!(store.resolve_entry(&first).is_ok());
    assert!(store
        .resolve_entry(&fixture("share-1").source_for("f1"))
        .is_err());
    assert!(store
        .resolve_entry(&fixture("share-256").source_for("f1"))
        .is_ok());
}

#[test]
fn transfer_records_survive_reinspection_and_cleanup_claims_only_once() {
    let (store, _, _) = memory_store();
    let plan = fixture("private-share");
    let source = plan.source_for("f1");
    store.upsert(&plan).unwrap();
    store.record_transfer(&source, "created-id").unwrap();
    store.record_transfer(&source, "created-id").unwrap();
    store.upsert(&plan).unwrap();
    let transfers = store.mark_cleanup_attempted(&source).unwrap();
    assert_eq!(transfers.len(), 1);
    assert_eq!(transfers[0].created_file_id, "created-id");
    assert_eq!(
        transfers[0].cleanup_state,
        CloudShareCleanupState::Attempted
    );
    assert!(store.mark_cleanup_attempted(&source).unwrap().is_empty());
    store.record_transfer(&source, "created-id").unwrap();
    assert!(store.mark_cleanup_attempted(&source).unwrap().is_empty());
    assert!(store.record_transfer(&source, "").is_err());
    assert!(store
        .record_transfer("cloud-share://baidu/bad/bad", "created-id")
        .is_err());
}

#[test]
fn corrupt_or_unsupported_documents_fail_closed_without_leaking_contents() {
    let (store, credentials, _) = memory_store();
    for bytes in [
        b"secret-token".as_slice(),
        br#"{"version":99,"plans":{}}"#.as_slice(),
    ] {
        credentials.save(SHARE_PLAN_SECRET, bytes).unwrap();
        let error = store.upsert(&fixture("share")).unwrap_err();
        assert!(!error.to_string().contains("secret-token"));
        assert_eq!(credentials.load(SHARE_PLAN_SECRET).unwrap().unwrap(), bytes);
    }
}

#[cfg(windows)]
#[test]
fn dpapi_plan_round_trip_recovers_access_code_but_disk_and_source_do_not() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        CloudSharePlanStore::new(SystemCredentialStore::new(directory.path().into()).unwrap());
    let plan = fixture("private-share");
    store.upsert(&plan).unwrap();
    let encrypted = std::fs::read(directory.path().join("cloud-share-plans.sqlite")).unwrap();
    for secret in ["secret-code", "secret-token", "private-share"] {
        assert!(!encrypted
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()));
    }
    drop(store);
    let reopened =
        CloudSharePlanStore::new(SystemCredentialStore::new(directory.path().into()).unwrap());
    assert_eq!(
        reopened
            .resolve_entry(&plan.source_for("f1"))
            .unwrap()
            .access_code(),
        Some("secret-code")
    );
}
