use versadown_lib::cloud::{
    load_password_credential, remove_password_credential, run_with_session_recovery,
    save_password_credential, select_password_credential_account, CloudPasswordCredential,
    CloudProvider, CredentialStore, MemoryCredentialStore, QuarkError, SessionWorkKind,
};
use std::{
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn authorization_recovery_discovers_only_durable_matching_account_failures() {
    use versadown_lib::{NewTask, TaskStatus, TaskStore};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("tasks.db");
    let store = TaskStore::open_path(&path).unwrap();
    let mut eligible = Vec::new();
    for (index, code, provider, owner) in [
        (0, "not_authorized", CloudProvider::Quark, "alice"),
        (1, "credentials_expired", CloudProvider::Quark, "alice"),
        (2, "web_auth_required", CloudProvider::Quark, "alice"),
        (3, "network_error", CloudProvider::Quark, "alice"),
        (4, "not_authorized", CloudProvider::Quark, "bob"),
        (5, "not_authorized", CloudProvider::Uc, "alice"),
        (6, "not_authorized", CloudProvider::Quark, ""),
    ] {
        let source = format!("cloud://{}/{index}", provider.key());
        let task = store
            .create(NewTask {
                source,
                name: format!("cloud-file-{index}"),
                kind: "cloud".into(),
                is_private: false,
            })
            .unwrap();
        if !owner.is_empty() {
            store
                .bind_cloud_download_account(task.id, provider, owner)
                .unwrap();
        }
        store
            .fail_with_code(task.id, Some(code), "fixture")
            .unwrap();
        if index < 3 {
            eligible.push(task.id);
        }
    }
    drop(store); // No CloudState or pending set survives this boundary.
    let store = std::sync::Arc::new(TaskStore::open_path(&path).unwrap());
    let engine = std::sync::Arc::new(AuthorizationRetryEngine::default());
    let service = versadown_lib::DownloadService::new(engine.clone(), store.clone());
    let (tasks, leaves) = store
        .reauthorized_cloud_downloads(CloudProvider::Quark, "alice")
        .unwrap();
    assert_eq!(tasks.len(), 3);
    assert!(leaves.is_empty());
    for task in tasks {
        assert!(eligible.contains(&task.id));
        for attempt in 0..2 {
            let resolved = versadown_lib::cloud::ResolvedCloudDownload {
                url: "https://download.example/file".into(),
                name: "file.txt".into(),
                headers: Default::default(),
            };
            let submitted = versadown_lib::cloud::share::resubmit_current_cloud_download(
                &service,
                &task,
                resolved,
                Default::default(),
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "task {} attempt {attempt} failed after {} engine submissions: {error:?}",
                    task.id,
                    engine.0.load(Ordering::SeqCst)
                )
            });
            assert_eq!(submitted.is_some(), attempt == 0);
        }
    }
    assert!(store
        .reauthorized_cloud_downloads(CloudProvider::Quark, "alice")
        .unwrap()
        .0
        .is_empty());
    assert_eq!(
        store
            .list()
            .unwrap()
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .count(),
        4
    );
    assert_eq!(engine.0.load(Ordering::SeqCst), 3);
}

#[derive(Default)]
struct AuthorizationRetryEngine(AtomicUsize);

#[test]
fn authorization_watchdog_recovers_completion_after_window_closes_but_not_after_new_attempt() {
    use versadown_lib::cloud::{commands::CloudState, CloudAccount, CloudAuthMethod};
    let root = tempfile::tempdir().unwrap();
    let state = CloudState::new(root.path().to_owned()).unwrap();
    state.store.save("quark", b"credential-secret").unwrap();
    state
        .auth_supervisor
        .ready(
            &state.store,
            CloudProvider::Quark,
            Some("alice".into()),
            Some(CloudAuthMethod::Web),
            None,
        )
        .unwrap();
    let account = CloudAccount {
        provider: CloudProvider::Quark,
        id: "alice".into(),
        display_name: "Alice".into(),
        auth_method: CloudAuthMethod::Web,
    };
    state.remember_authorization_completion(&account).unwrap();
    assert_eq!(
        state
            .completed_authorization(CloudProvider::Quark)
            .unwrap()
            .unwrap()
            .id,
        "alice"
    );
    state
        .clear_authorization_completion(CloudProvider::Quark)
        .unwrap();
    assert!(state
        .completed_authorization(CloudProvider::Quark)
        .unwrap()
        .is_none());
    state.remember_authorization_completion(&account).unwrap();
    state
        .auth_supervisor
        .disconnect(&state.store, CloudProvider::Quark)
        .unwrap();
    assert!(state
        .completed_authorization(CloudProvider::Quark)
        .unwrap()
        .is_none());
}

#[test]
fn authorization_event_requires_persisted_credentials_and_committed_supervisor() {
    use versadown_lib::cloud::{
        auth_supervisor::AuthSupervisor, commands::authorization_completion_event, CloudAccount,
        CloudAuthMethod,
    };
    let store = MemoryCredentialStore::default();
    let supervisor = AuthSupervisor::default();
    let account = CloudAccount {
        provider: CloudProvider::Quark,
        id: "alice".into(),
        display_name: "Alice".into(),
        auth_method: CloudAuthMethod::Web,
    };
    assert!(authorization_completion_event(&supervisor, &store, &account).is_err());
    store.save("quark", b"credential-secret").unwrap();
    assert!(authorization_completion_event(&supervisor, &store, &account).is_err());
    supervisor
        .ready(
            &store,
            CloudProvider::Quark,
            Some("alice".into()),
            Some(CloudAuthMethod::Web),
            None,
        )
        .unwrap();
    let event = authorization_completion_event(&supervisor, &store, &account).unwrap();
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        serde_json::json!({ "provider": "quark", "state": "Ready", "account": { "provider": "quark", "id": "alice", "displayName": "Alice", "authMethod": "web" } })
    );
    let other = CloudAccount {
        id: "bob".into(),
        ..account
    };
    assert!(authorization_completion_event(&supervisor, &store, &other).is_err());
    store.remove("quark").unwrap();
    assert!(authorization_completion_event(&supervisor, &store, &other).is_err());
}
#[async_trait::async_trait]
impl versadown_lib::aria2::Aria2Api for AuthorizationRetryEngine {
    async fn add_uri(
        &self,
        _: Vec<String>,
        _: versadown_lib::aria2::Aria2Options,
    ) -> Result<String, versadown_lib::aria2::Aria2Error> {
        Ok(format!("gid-{}", self.0.fetch_add(1, Ordering::SeqCst)))
    }
    async fn add_torrent(
        &self,
        _: String,
        _: Vec<String>,
        _: versadown_lib::aria2::Aria2Options,
    ) -> Result<String, versadown_lib::aria2::Aria2Error> {
        unreachable!()
    }
    async fn tell_status(
        &self,
        gid: &str,
    ) -> Result<versadown_lib::aria2::Aria2Status, versadown_lib::aria2::Aria2Error> {
        Ok(versadown_lib::aria2::Aria2Status {
            gid: gid.into(),
            status: "active".into(),
            total_length: "10".into(),
            completed_length: "0".into(),
            download_speed: "1".into(),
            upload_speed: "0".into(),
            connections: "1".into(),
            files: vec![],
            bittorrent: None,
            media: None,
            followed_by: vec![],
            error_code: None,
            error_message: None,
        })
    }
    async fn tell_active(
        &self,
    ) -> Result<Vec<versadown_lib::aria2::Aria2Status>, versadown_lib::aria2::Aria2Error>
    {
        unreachable!()
    }
    async fn tell_waiting(
        &self,
        _: i64,
        _: u32,
    ) -> Result<Vec<versadown_lib::aria2::Aria2Status>, versadown_lib::aria2::Aria2Error>
    {
        unreachable!()
    }
    async fn tell_stopped(
        &self,
        _: i64,
        _: u32,
    ) -> Result<Vec<versadown_lib::aria2::Aria2Status>, versadown_lib::aria2::Aria2Error>
    {
        unreachable!()
    }
    async fn get_global_stat(
        &self,
    ) -> Result<versadown_lib::aria2::Aria2GlobalStat, versadown_lib::aria2::Aria2Error>
    {
        unreachable!()
    }
    async fn multicall(
        &self,
        _: Vec<versadown_lib::aria2::RpcCall>,
    ) -> Result<
        Vec<Result<serde_json::Value, versadown_lib::aria2::RpcFault>>,
        versadown_lib::aria2::Aria2Error,
    > {
        unreachable!()
    }
    async fn pause(&self, _: &str) -> Result<String, versadown_lib::aria2::Aria2Error> {
        unreachable!()
    }
    async fn unpause(&self, _: &str) -> Result<String, versadown_lib::aria2::Aria2Error> {
        unreachable!()
    }
    async fn remove(&self, _: &str) -> Result<String, versadown_lib::aria2::Aria2Error> {
        unreachable!()
    }
    async fn force_remove(&self, _: &str) -> Result<String, versadown_lib::aria2::Aria2Error> {
        unreachable!()
    }
    async fn change_option(
        &self,
        _: &str,
        _: versadown_lib::aria2::Aria2Options,
    ) -> Result<String, versadown_lib::aria2::Aria2Error> {
        unreachable!()
    }
    async fn get_option(
        &self,
        _: &str,
    ) -> Result<versadown_lib::aria2::Aria2Options, versadown_lib::aria2::Aria2Error> {
        unreachable!()
    }
    async fn save_session(&self) -> Result<String, versadown_lib::aria2::Aria2Error> {
        Ok("OK".into())
    }
    async fn shutdown(&self) -> Result<String, versadown_lib::aria2::Aria2Error> {
        unreachable!()
    }
}

#[test]
fn saved_credential_response_never_serializes_the_saved_password() {
    use versadown_lib::cloud::saved_cloud_credential_info;
    let store = MemoryCredentialStore::default();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "a", "alice", "stored-secret"),
    )
    .unwrap();
    let response = saved_cloud_credential_info(&store, CloudProvider::Baidu)
        .unwrap()
        .unwrap();
    let json = serde_json::to_value(response).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"username": "alice", "hasSavedPassword": true})
    );
}

#[test]
fn replacing_browser_account_clears_previous_session_password_and_profile_owner() {
    use versadown_lib::cloud::{
        baidu_browser_profile_username, set_baidu_browser_profile_username,
    };
    let store = MemoryCredentialStore::default();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "a", "alice", "secret-a"),
    )
    .unwrap();
    store.save("baidu", b"session-a").unwrap();
    set_baidu_browser_profile_username(&store, Some("alice")).unwrap();
    select_password_credential_account(&store, CloudProvider::Baidu, "b").unwrap();
    assert!(store.load("baidu").unwrap().is_none());
    assert!(load_password_credential(&store, CloudProvider::Baidu)
        .unwrap()
        .is_none());
    assert!(baidu_browser_profile_username(&store).unwrap().is_none());
}

#[test]
fn saved_password_handle_is_bound_to_username_and_rejects_ambiguous_inputs() {
    use versadown_lib::cloud::{resolve_password_login, PasswordLoginRequest};
    let store = MemoryCredentialStore::default();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "a", "alice", "saved-secret"),
    )
    .unwrap();
    let resolve = |username: &str, password: Option<&str>, use_saved_password| {
        resolve_password_login(
            &store,
            PasswordLoginRequest {
                provider: CloudProvider::Baidu,
                username: username.into(),
                password: password.map(str::to_owned),
                use_saved_password,
            },
        )
    };
    assert_eq!(
        resolve("alice", None, true).unwrap().1.as_str(),
        "saved-secret"
    );
    assert_eq!(
        resolve("alice", Some(""), true).unwrap().1.as_str(),
        "saved-secret"
    );
    assert_eq!(
        resolve("bob", Some("new-secret"), false)
            .unwrap()
            .1
            .as_str(),
        "new-secret"
    );
    assert!(resolve("bob", None, true).is_err());
    assert!(resolve("alice", Some("new-secret"), true).is_err());
    assert!(resolve("alice", None, false).is_err());
    assert!(resolve("alice", Some(""), false).is_err());
}

#[test]
fn replacement_removes_old_session_even_when_password_persistence_fails() {
    use std::sync::atomic::AtomicBool;
    struct FailingStore {
        inner: MemoryCredentialStore,
        fail_save: AtomicBool,
    }
    impl CredentialStore for FailingStore {
        fn save(&self, key: &str, bytes: &[u8]) -> Result<(), QuarkError> {
            if self.fail_save.load(Ordering::Relaxed) {
                return Err(QuarkError::Storage);
            }
            self.inner.save(key, bytes)
        }
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
            self.inner.load(key)
        }
        fn remove(&self, key: &str) -> Result<(), QuarkError> {
            self.inner.remove(key)
        }
    }
    let store = FailingStore {
        inner: MemoryCredentialStore::default(),
        fail_save: AtomicBool::new(false),
    };
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "a", "alice", "secret-a"),
    )
    .unwrap();
    store.save("baidu", b"session-a").unwrap();
    store.fail_save.store(true, Ordering::Relaxed);
    assert!(save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "b", "bob", "secret-b")
    )
    .is_err());
    assert!(store.load("baidu").unwrap().is_none());
    assert_eq!(
        load_password_credential(&store, CloudProvider::Baidu)
            .unwrap()
            .unwrap()
            .username(),
        "alice"
    );
}

#[test]
fn password_replacement_isolates_the_old_session_but_keeps_validated_login() {
    use versadown_lib::cloud::{
        baidu_browser_profile_username, prepare_password_account_login, save_password_login_draft,
        saved_cloud_credential_info, set_baidu_browser_profile_username,
    };
    let store = MemoryCredentialStore::default();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "a", "alice", "secret-a"),
    )
    .unwrap();
    store.save("baidu", b"session-a").unwrap();
    set_baidu_browser_profile_username(&store, Some("alice")).unwrap();
    assert!(prepare_password_account_login(&store, CloudProvider::Baidu, "bob").unwrap());
    assert!(store.load("baidu").unwrap().is_none());
    assert_eq!(
        load_password_credential(&store, CloudProvider::Baidu)
            .unwrap()
            .unwrap()
            .username(),
        "alice"
    );
    assert!(baidu_browser_profile_username(&store).unwrap().is_none());
    save_password_login_draft(&store, CloudProvider::Baidu, "bob", "secret-b").unwrap();
    assert_eq!(
        saved_cloud_credential_info(&store, CloudProvider::Baidu)
            .unwrap()
            .unwrap()
            .username,
        "alice"
    );
}

#[test]
fn unreadable_old_credentials_abort_persistence_and_clear_the_session() {
    struct UnreadablePassword {
        inner: MemoryCredentialStore,
    }
    impl CredentialStore for UnreadablePassword {
        fn save(&self, key: &str, bytes: &[u8]) -> Result<(), QuarkError> {
            self.inner.save(key, bytes)
        }
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
            if key == "cloud-password-baidu" {
                return Err(QuarkError::Storage);
            }
            self.inner.load(key)
        }
        fn remove(&self, key: &str) -> Result<(), QuarkError> {
            self.inner.remove(key)
        }
    }
    let store = UnreadablePassword {
        inner: MemoryCredentialStore::default(),
    };
    store.save("baidu", b"session").unwrap();
    assert!(save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "b", "bob", "secret-b")
    )
    .is_err());
    assert!(store.load("baidu").unwrap().is_none());
}

#[test]
fn replacing_pikpak_keeps_credentials_without_requesting_an_unsupported_browser_profile() {
    use versadown_lib::cloud::prepare_password_account_login;
    let store = MemoryCredentialStore::default();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Pikpak, "a", "alice", "secret-a"),
    )
    .unwrap();
    store.save("pikpak", b"session-a").unwrap();
    let reset_password_profile =
        prepare_password_account_login(&store, CloudProvider::Pikpak, "bob").unwrap();
    assert!(!reset_password_profile);
    assert!(store.load("pikpak").unwrap().is_none());
    assert_eq!(
        load_password_credential(&store, CloudProvider::Pikpak)
            .unwrap()
            .unwrap()
            .username(),
        "alice"
    );
}

#[derive(Default)]
struct LoadRecordingStore {
    load_calls: AtomicUsize,
}

fn verify_failed_password_profile_reset_after_restart(provider: CloudProvider) {
    use versadown_lib::cloud::{
        prepare_password_account_login, reset_password_profile_if_required, SystemCredentialStore,
    };
    let root = tempfile::tempdir().unwrap();
    let store = SystemCredentialStore::new(root.path().to_owned()).unwrap();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(provider, "old-account", "alice", "old-secret"),
    )
    .unwrap();
    store.save(provider.key(), b"old-session").unwrap();
    let attempt = |store: &SystemCredentialStore| -> Result<&str, QuarkError> {
        let reset = prepare_password_account_login(store, provider, "bob")?;
        reset_password_profile_if_required(store, provider, reset, || Err(QuarkError::Storage))?;
        Ok("old-account")
    };
    assert!(attempt(&store).is_err());
    assert!(store.load(provider.key()).unwrap().is_none());
    drop(store);
    let reopened = SystemCredentialStore::new(root.path().to_owned()).unwrap();
    assert!(
        attempt(&reopened).is_err(),
        "{provider:?} must not accept old-account after a failed profile reset and restart"
    );
    let marker = format!("cloud-password-profile-reset-{}", provider.key());
    assert_eq!(reopened.load(&marker).unwrap(), Some(b"1".to_vec()));
    assert_ne!(
        std::fs::read(root.path().join(format!("{marker}.bin"))).unwrap(),
        b"1"
    );
    // A real successful reset is the sole path allowed to remove the marker.
    let reset_calls = Cell::new(0);
    reset_password_profile_if_required(&reopened, provider, false, || {
        reset_calls.set(reset_calls.get() + 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(reset_calls.get(), 1);
    assert!(reopened.load(&marker).unwrap().is_none());
    // Until Bob logs in successfully, Alice remains the saved login, so a new
    // attempt for Bob still isolates the previous profile.
    assert!(prepare_password_account_login(&reopened, provider, "bob").unwrap());
}

#[test]
fn failed_password_profile_reset_survives_restart_for_baidu() {
    verify_failed_password_profile_reset_after_restart(CloudProvider::Baidu);
}

#[test]
fn failed_profile_marker_removal_keeps_reset_mandatory_and_blocks_login() {
    use versadown_lib::cloud::{
        prepare_password_account_login, reset_password_profile_if_required,
    };
    struct RemovalFailure {
        inner: MemoryCredentialStore,
    }
    impl CredentialStore for RemovalFailure {
        fn save(&self, key: &str, bytes: &[u8]) -> Result<(), QuarkError> {
            self.inner.save(key, bytes)
        }
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
            self.inner.load(key)
        }
        fn remove(&self, key: &str) -> Result<(), QuarkError> {
            if key.starts_with("cloud-password-profile-reset-") {
                return Err(QuarkError::Storage);
            }
            self.inner.remove(key)
        }
    }
    for provider in [CloudProvider::Baidu] {
        let store = RemovalFailure {
            inner: MemoryCredentialStore::default(),
        };
        assert!(reset_password_profile_if_required(&store, provider, true, || Ok(())).is_err());
        assert!(prepare_password_account_login(&store, provider, "bob").unwrap());
    }
}

#[test]
fn failed_profile_marker_write_preserves_old_owner_and_never_starts_reset() {
    use versadown_lib::cloud::{
        prepare_password_account_login, reset_password_profile_if_required,
    };
    struct MarkerWriteFailure {
        inner: MemoryCredentialStore,
    }
    impl CredentialStore for MarkerWriteFailure {
        fn save(&self, key: &str, bytes: &[u8]) -> Result<(), QuarkError> {
            if key.starts_with("cloud-password-profile-reset-") {
                return Err(QuarkError::Storage);
            }
            self.inner.save(key, bytes)
        }
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
            self.inner.load(key)
        }
        fn remove(&self, key: &str) -> Result<(), QuarkError> {
            self.inner.remove(key)
        }
    }
    for provider in [CloudProvider::Baidu] {
        let store = MarkerWriteFailure {
            inner: MemoryCredentialStore::default(),
        };
        save_password_credential(
            &store,
            CloudPasswordCredential::new(provider, "old", "alice", "secret"),
        )
        .unwrap();
        store.save(provider.key(), b"old-session").unwrap();
        assert!(prepare_password_account_login(&store, provider, "bob").is_err());
        assert_eq!(
            store.load(provider.key()).unwrap(),
            Some(b"old-session".to_vec())
        );
        assert_eq!(
            load_password_credential(&store, provider)
                .unwrap()
                .unwrap()
                .username(),
            "alice"
        );
        assert!(
            reset_password_profile_if_required(&store, provider, true, || panic!(
                "must persist obligation before browser reset"
            ))
            .is_err()
        );
    }
}

impl CredentialStore for LoadRecordingStore {
    fn save(&self, _: &str, _: &[u8]) -> Result<(), QuarkError> {
        Ok(())
    }

    fn load(&self, _: &str) -> Result<Option<Vec<u8>>, QuarkError> {
        self.load_calls.fetch_add(1, Ordering::Relaxed);
        Err(QuarkError::Storage)
    }

    fn remove(&self, _: &str) -> Result<(), QuarkError> {
        Ok(())
    }
}

#[test]
fn expired_read_relogs_once_and_retries_once() {
    let store = MemoryCredentialStore::default();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "account-1", "user", "secret"),
    )
    .unwrap();
    let work_calls = Cell::new(0);
    let relogin_calls = Cell::new(0);

    let result = run_with_session_recovery(
        CloudProvider::Baidu,
        &store,
        SessionWorkKind::Read,
        || {
            let call = work_calls.get() + 1;
            work_calls.set(call);
            if call == 1 {
                Err(QuarkError::AuthorizationExpired)
            } else {
                Ok("listed files")
            }
        },
        |credential| {
            relogin_calls.set(relogin_calls.get() + 1);
            assert_eq!(credential.account_id(), "account-1");
            assert_eq!(credential.username(), "user");
            assert_eq!(credential.password(), "secret");
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(result, "listed files");
    assert_eq!(work_calls.get(), 2);
    assert_eq!(relogin_calls.get(), 1);
}

#[test]
fn expired_mutation_relogs_but_never_replays_work() {
    let store = MemoryCredentialStore::default();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Pikpak, "account-1", "user", "secret"),
    )
    .unwrap();
    let work_calls = Cell::new(0);
    let relogin_calls = Cell::new(0);

    let result: Result<(), QuarkError> = run_with_session_recovery(
        CloudProvider::Pikpak,
        &store,
        SessionWorkKind::Mutation,
        || {
            work_calls.set(work_calls.get() + 1);
            Err(QuarkError::NotAuthorized)
        },
        |_| {
            relogin_calls.set(relogin_calls.get() + 1);
            Ok(())
        },
    );

    assert!(matches!(
        result,
        Err(QuarkError::SessionRenewedRetryOperation)
    ));
    assert_eq!(work_calls.get(), 1);
    assert_eq!(relogin_calls.get(), 1);
}

#[test]
fn official_webview_provider_does_not_load_password_or_relogin_on_expired_session() {
    let store = LoadRecordingStore::default();
    let relogin_calls = Cell::new(0);

    let result: Result<(), QuarkError> = run_with_session_recovery(
        CloudProvider::Aliyun,
        &store,
        SessionWorkKind::Read,
        || Err(QuarkError::AuthorizationExpired),
        |_| {
            relogin_calls.set(relogin_calls.get() + 1);
            Ok(())
        },
    );

    assert!(matches!(result, Err(QuarkError::AuthorizationExpired)));
    assert_eq!(store.load_calls.load(Ordering::Relaxed), 0);
    assert_eq!(relogin_calls.get(), 0);
}

#[test]
fn fallback_password_is_redacted_replaced_by_a_new_account_and_removed_on_logout() {
    let store = MemoryCredentialStore::default();
    let credential = CloudPasswordCredential::new(
        CloudProvider::Pikpak,
        "account-1",
        "name@example.com",
        "secret-value",
    );
    let debug = format!("{credential:?}");
    assert!(!debug.contains("name@example.com"));
    assert!(!debug.contains("secret-value"));
    save_password_credential(&store, credential).unwrap();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Pikpak, "account-2", "user", "secret"),
    )
    .unwrap();
    let replacement = load_password_credential(&store, CloudProvider::Pikpak)
        .unwrap()
        .expect("the latest account remains active");
    assert_eq!(replacement.account_id(), "account-2");

    select_password_credential_account(&store, CloudProvider::Pikpak, "account-1").unwrap();
    assert!(load_password_credential(&store, CloudProvider::Pikpak)
        .unwrap()
        .is_none());

    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Pikpak, "account-2", "user", "secret"),
    )
    .unwrap();
    remove_password_credential(&store, CloudProvider::Pikpak).unwrap();
    assert!(load_password_credential(&store, CloudProvider::Pikpak)
        .unwrap()
        .is_none());
}
