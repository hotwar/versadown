use versadown_lib::cloud::{
    baidu::BaiduSession,
    commands::{logout_provider, restore_account},
    load_password_credential, parse_stable_cloud_source, provider_capabilities,
    save_password_credential,
    tianyi::TianyiSession,
    uc::UcSession,
    xunlei::{XunleiClientProfile, XunleiSession},
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudPasswordCredential,
    CloudProvider, CredentialStore, MemoryCredentialStore, PasswordLoginMode, QuarkError,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

#[test]
fn baidu_and_quark_wait_for_web_login_without_persisting_an_empty_session() {
    use versadown_lib::cloud::{driver::WebAuthorizationInput, CloudAuthMethod, ProviderRegistry};
    struct NoNetwork;
    impl CloudHttpTransport for NoNetwork {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            panic!("login cookies must exist before validating a web session")
        }
    }
    for provider in [CloudProvider::Baidu, CloudProvider::Quark] {
        let store = MemoryCredentialStore::default();
        let input = WebAuthorizationInput {
            cookies: Vec::new(),
            storage: serde_json::Value::Null,
            browser_context: None,
            auth_method: CloudAuthMethod::Web,
        };
        let result = ProviderRegistry::global()
            .driver(provider, &store, &NoNetwork)
            .auth()
            .complete_web(&input);
        assert!(
            matches!(result, Err(QuarkError::AuthorizationPending)),
            "{provider:?}: {result:?}"
        );
        assert!(store.load(provider.key()).unwrap().is_none());
    }
}

// Breaks caught: a provider missing its download safety ceiling, an incorrect
// ceiling, an accidental zero ceiling, or a camelCase DTO regression.
#[test]
fn provider_download_concurrency_matches_the_literal_safety_matrix() {
    use versadown_lib::cloud::ProviderRegistry;

    let rows = [
        (CloudProvider::Quark, 4, 4),
        (CloudProvider::Pikpak, 4, 4),
        (CloudProvider::Baidu, 4, 4),
        (CloudProvider::Tianyi, 1, 1),
        (CloudProvider::Aliyun, 4, 4),
        (CloudProvider::Xunlei, 1, 4),
        (CloudProvider::Uc, 4, 4),
    ];

    let registry = ProviderRegistry::global();
    for (provider, max_connections_per_file, max_concurrent_tasks) in rows {
        let descriptor = registry.get(provider);
        assert_ne!(descriptor.download_concurrency.max_connections_per_file, 0);
        assert_ne!(descriptor.download_concurrency.max_concurrent_tasks, 0);
        assert_eq!(
            serde_json::to_value(descriptor).unwrap()["downloadConcurrency"],
            serde_json::json!({
                "maxConnectionsPerFile": max_connections_per_file,
                "maxConcurrentTasks": max_concurrent_tasks,
            }),
            "{provider:?}"
        );
    }
}

// Breaks caught: omitting a catalog entry, routing a stable source to the wrong
// provider, leaking/reusing an old password, changing recovery order, or enabling
// cookie keepalive. Expectations are literal compatibility fixtures.
#[test]
fn seven_provider_contract_matrix_preserves_registry_authorization_and_sources() {
    use versadown_lib::cloud::{
        auth_supervisor::{recover_in_order, AuthSupervisor},
        driver::ProviderAuth,
        saved_cloud_credential_info, select_password_credential_account,
        share::parse_stable_cloud_share_source,
        CloudAccount, CloudError, ProviderRegistry, SessionWorkKind,
    };
    use std::sync::Arc;
    struct NoNetwork;
    impl CloudHttpTransport for NoNetwork {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
            panic!("unconnected capability must reject before network IO")
        }
    }
    struct RejectedRecovery(Arc<Mutex<Vec<&'static str>>>);
    impl ProviderAuth for RejectedRecovery {
        fn refresh_session(&mut self) -> Result<bool, CloudError> {
            self.0.lock().unwrap().push("refresh");
            Err(CloudError::AuthorizationExpired)
        }
        fn recover_password(
            &mut self,
            _: &CloudPasswordCredential,
        ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
            self.0.lock().unwrap().push("password");
            Err(CloudError::AuthorizationExpired)
        }
    }
    use CloudProvider::*;
    use PasswordLoginMode::{None as NoPassword, OfficialWebView, RustDirect};
    let rows = [
        (
            Quark,
            "quark",
            "0",
            NoPassword,
            false,
            false,
            true,
            "https://pan.quark.cn/s/example",
        ),
        (
            Pikpak,
            "pikpak",
            "0",
            RustDirect,
            true,
            true,
            true,
            "https://mypikpak.com/s/example",
        ),
        (
            Baidu,
            "baidu",
            "/",
            RustDirect,
            true,
            false,
            true,
            "https://pan.baidu.com/s/1example",
        ),
        (
            Tianyi,
            "tianyi",
            "-11",
            RustDirect,
            true,
            true,
            false,
            "https://cloud.189.cn/t/example",
        ),
        (
            Aliyun,
            "aliyun",
            "root",
            OfficialWebView,
            false,
            true,
            true,
            "https://www.alipan.com/s/example",
        ),
        (
            Xunlei,
            "xunlei",
            "0",
            OfficialWebView,
            true,
            true,
            true,
            "https://pan.xunlei.com/s/example",
        ),
        (
            Uc,
            "uc",
            "0",
            OfficialWebView,
            false,
            false,
            true,
            "https://drive.uc.cn/s/example",
        ),
    ];
    let registry = ProviderRegistry::global();
    let descriptors = registry.all();
    for (provider, key, root, password_mode, password, scheduled, transfer, share_url) in rows {
        let descriptor = descriptors
            .iter()
            .find(|d| d.provider == provider)
            .unwrap_or_else(|| panic!("missing provider descriptor: {key}"));
        assert_eq!(
            descriptors
                .iter()
                .filter(|d| d.provider == provider)
                .count(),
            1
        );
        assert_eq!(registry.by_key(key).unwrap().provider, provider);
        assert_eq!(descriptor.root_directory_id, root);
        let recycle = format!("__versa_down_{key}_recycle_bin__");
        assert_eq!(descriptor.recycle_bin_directory_id, recycle);
        assert_eq!(descriptor.authorization.password_login_mode, password_mode);
        assert!(descriptor.authorization.web);
        assert_eq!(descriptor.authorization.password, password);
        assert!(descriptor.capabilities.list && descriptor.capabilities.download);
        assert!(descriptor.capabilities.upload);
        assert_eq!(descriptor.max_upload_concurrency, 2);
        let source = format!("cloud://{key}/folder%2Ffile%20name");
        assert_eq!(
            parse_stable_cloud_source(&source).unwrap(),
            (provider, "folder/file name".into())
        );
        let share = url::Url::parse(share_url).unwrap();
        if provider.is_enabled() {
            assert_eq!(registry.for_share_url(&share).unwrap().provider, provider);
        } else {
            assert!(registry.for_share_url(&share).is_none());
        }
        let share_hash = "a".repeat(64);
        let entry_hash = "b".repeat(64);
        assert_eq!(
            parse_stable_cloud_share_source(&format!(
                "cloud-share://{key}/{share_hash}/{entry_hash}"
            ))
            .unwrap(),
            (provider, share_hash, entry_hash)
        );
        if !provider.is_enabled() {
            continue;
        }

        let store = MemoryCredentialStore::default();
        let capabilities = registry.share_capabilities(provider, &store, &NoNetwork);
        assert!(descriptor.capabilities.share_inspect && capabilities.inspect.is_some());
        assert!(capabilities.resolve.is_some());
        assert_eq!(descriptor.capabilities.share_transfer, transfer);
        assert_eq!(descriptor.capabilities.recorded_transfer_resolve, transfer);
        assert_eq!(capabilities.transfer.is_some(), transfer);
        assert_eq!(capabilities.recorded.is_some(), transfer);
        let mut driver = registry.driver(provider, &store, &NoNetwork);
        assert!(matches!(driver.account(), Err(CloudError::NotAuthorized)));
        for directory in [root, recycle.as_str()] {
            assert!(
                matches!(
                    driver.list_directory(directory),
                    Err(CloudError::NotAuthorized | CloudError::AuthorizationExpired)
                ),
                "{key}: {directory}"
            );
        }
        assert!(
            matches!(
                driver.resolve_download("file"),
                Err(CloudError::NotAuthorized | CloudError::AuthorizationExpired)
            ),
            "{key}"
        );

        let direct = password_mode == RustDirect;
        if direct {
            save_password_credential(
                &store,
                CloudPasswordCredential::new(provider, "old", "alice", "old-secret"),
            )
            .unwrap();
            store.save(key, b"old-session").unwrap();
            select_password_credential_account(&store, provider, "new").unwrap();
            assert!(store.load(key).unwrap().is_none());
            assert!(load_password_credential(&store, provider)
                .unwrap()
                .is_none());
            save_password_credential(
                &store,
                CloudPasswordCredential::new(provider, "new", "bob", "new-secret"),
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(
                    saved_cloud_credential_info(&store, provider)
                        .unwrap()
                        .unwrap()
                )
                .unwrap(),
                serde_json::json!({"username":"bob", "hasSavedPassword":true})
            );
        } else {
            assert!(saved_cloud_credential_info(&store, provider)
                .unwrap()
                .is_none());
        }
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let mut auth = RejectedRecovery(attempts.clone());
        assert!(matches!(
            recover_in_order(provider, &store, &mut auth, true, || {
                attempts.lock().unwrap().push("profile");
                Ok(false)
            }),
            Err(CloudError::WebAuthorizationRequired)
        ));
        let expected = if direct {
            vec!["refresh", "password", "profile"]
        } else {
            vec!["refresh", "profile"]
        };
        assert_eq!(*attempts.lock().unwrap(), expected, "{key}");

        let supervisor = AuthSupervisor::new(|| 1000, || 30);
        store.save(key, b"old-session").unwrap();
        supervisor
            .ready(&store, provider, Some("old".into()), None, None)
            .unwrap();
        assert!(supervisor
            .record(&store, provider)
            .unwrap()
            .next_refresh_at
            .is_none());
        store.save(key, b"new-session").unwrap();
        supervisor
            .ready(&store, provider, Some("new".into()), None, Some(4600))
            .unwrap();
        let record = supervisor.record(&store, provider).unwrap();
        assert_eq!(record.account_id.as_deref(), Some("new"));
        assert_eq!(
            record.next_refresh_at,
            if scheduled { Some(3850) } else { None },
            "{key}"
        );
        let session = supervisor
            .run(
                &store,
                provider,
                SessionWorkKind::Read,
                |snapshot| snapshot.load(key),
                |_| panic!("new session is not due"),
            )
            .unwrap();
        assert_eq!(session.unwrap(), b"new-session");
        logout_provider(provider, &store).unwrap();
        assert!(store.load(key).unwrap().is_none());
        assert_eq!(
            saved_cloud_credential_info(&store, provider)
                .unwrap()
                .is_some(),
            direct
        );
    }
    assert_eq!(descriptors.len(), 7);
    assert_eq!(registry.catalog().len(), 6);
}

#[test]
fn share_capabilities_report_actual_operations_and_reject_absent_transfer_before_io() {
    use versadown_lib::cloud::{share::*, ProviderRegistry};
    struct NoIo;
    impl CloudHttpTransport for NoIo {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            panic!("absent capability must be rejected before protocol IO")
        }
    }
    let credentials = MemoryCredentialStore::default();
    let directory = tempfile::tempdir().unwrap();
    let plans = CloudSharePlanStore::new(
        versadown_lib::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
    );
    for (provider, transfer) in [
        (CloudProvider::Quark, true),
        (CloudProvider::Pikpak, true),
        (CloudProvider::Baidu, true),
        (CloudProvider::Tianyi, false),
        (CloudProvider::Aliyun, true),
        (CloudProvider::Xunlei, true),
        (CloudProvider::Uc, true),
    ] {
        let capabilities =
            ProviderRegistry::global().share_capabilities(provider, &credentials, &NoIo);
        assert!(capabilities.inspect.is_some());
        assert!(capabilities.resolve.is_some());
        assert_eq!(capabilities.transfer.is_some(), transfer, "{provider:?}");
        assert_eq!(capabilities.recorded.is_some(), transfer, "{provider:?}");
        if !transfer {
            let plan = CloudSharePlan::new(
                provider,
                "share".into(),
                None,
                vec![CloudShareEntry::new(
                    "share",
                    "file".into(),
                    "file.txt".into(),
                    vec![],
                    None,
                )],
                serde_json::json!({}),
            );
            let entry = CloudShareResolvedEntry {
                provider,
                entry: plan.entries[0].clone(),
                plan,
            };
            assert!(matches!(
                capabilities.transfer_owner_fingerprint(&entry),
                Err(QuarkError::ProviderUnsupported)
            ));
            assert!(matches!(
                capabilities.transfer(&entry, &plans),
                Err(QuarkError::ProviderUnsupported)
            ));
            assert!(matches!(
                capabilities.refresh_recorded_transfer(&entry, "recorded"),
                Err(QuarkError::ProviderUnsupported)
            ));
        }
    }
    assert!(!directory.path().join("cloud-share-plans.sqlite").exists());
}

// A wrong provider selection, dropped argument, or capability bypass must be observable here.
#[test]
fn capability_facade_dispatches_and_rejects_missing_operations_before_io() {
    use versadown_lib::cloud::driver::*;
    use versadown_lib::cloud::{
        CloudAccount, CloudAuthMethod, CloudDirectory, CloudError, CloudFile, ProviderRegistry,
    };
    use std::sync::Arc;
    struct RecordingFactory(Arc<Mutex<Vec<String>>>);
    struct RecordingDriver(Arc<Mutex<Vec<String>>>);
    impl DriveOps for RecordingDriver {
        fn account(&mut self) -> Result<CloudAccount, CloudError> {
            self.0.lock().unwrap().push("account".into());
            Ok(CloudAccount {
                provider: CloudProvider::Quark,
                id: "recorded".into(),
                display_name: "recorded".into(),
                auth_method: CloudAuthMethod::Web,
            })
        }
        fn list_directory(&mut self, parent: &str) -> Result<CloudDirectory, CloudError> {
            self.0.lock().unwrap().push(format!("list:{parent}"));
            Ok(CloudDirectory {
                id: parent.into(),
                items: vec![],
            })
        }
        fn rename(&mut self, file: &str, name: &str) -> Result<(), CloudError> {
            self.0.lock().unwrap().push(format!("rename:{file}:{name}"));
            Ok(())
        }
        fn trash_transferred_files(&mut self, files: &[String]) -> Result<(), CloudError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("cleanup:{}", files.join(",")));
            Ok(())
        }
    }
    impl DownloadOps for RecordingDriver {
        fn resolve_download(
            &mut self,
            file: &str,
        ) -> Result<versadown_lib::cloud::commands::ResolvedCloudDownload, CloudError> {
            self.0.lock().unwrap().push(format!("download:{file}"));
            Ok(versadown_lib::cloud::commands::ResolvedCloudDownload {
                url: "https://download.example/file".into(),
                name: file.into(),
                headers: vec![],
            })
        }
    }
    impl ShareOps for RecordingDriver {}
    impl ProviderAuth for RecordingDriver {
        fn recover_password(
            &mut self,
            credential: &CloudPasswordCredential,
        ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("auth:{}", credential.username()));
            Ok((
                CloudAccount {
                    provider: CloudProvider::Quark,
                    id: "recorded".into(),
                    display_name: "recorded".into(),
                    auth_method: CloudAuthMethod::Password,
                },
                b"snapshot".to_vec(),
            ))
        }
    }
    impl UploadOps for RecordingDriver {
        fn upload(
            &mut self,
            _state: &versadown_lib::cloud::commands::CloudState,
            _http: &dyn UploadHttp,
            parent: &str,
            path: &std::path::Path,
        ) -> Result<CloudFile, CloudError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("upload:{parent}:{}", path.display()));
            Ok(CloudFile {
                id: "uploaded".into(),
                parent_id: parent.into(),
                name: "file".into(),
                kind: "file".into(),
                size: Some(3),
                modified_at: None,
            })
        }
    }
    impl ProviderBackend for RecordingDriver {}
    impl ProviderDriverFactory for RecordingFactory {
        fn create<'a>(
            &self,
            provider: CloudProvider,
            _store: &'a dyn CredentialStore,
            _http: &'a dyn CloudHttpTransport,
        ) -> Box<dyn ProviderBackend + 'a> {
            self.0
                .lock()
                .unwrap()
                .push(format!("provider:{}", provider.key()));
            Box::new(RecordingDriver(self.0.clone()))
        }
    }
    struct NoNetwork;
    impl CloudHttpTransport for NoNetwork {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
            panic!("unsupported operations must not perform network I/O")
        }
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = ProviderRegistry::with_driver_factory(Arc::new(RecordingFactory(calls.clone())));
    let store = MemoryCredentialStore::default();
    let mut facade = registry.driver(CloudProvider::Quark, &store, &NoNetwork);
    assert_eq!(facade.account().unwrap().id, "recorded");
    assert_eq!(facade.list_directory("parent").unwrap().id, "parent");
    facade.rename("file", "renamed").unwrap();
    facade
        .trash_transferred_files(&["transferred".into()])
        .unwrap();
    assert_eq!(facade.resolve_download("leaf").unwrap().name, "leaf");
    assert_eq!(
        facade
            .auth()
            .recover_password(&CloudPasswordCredential::new(
                CloudProvider::Quark,
                "recorded",
                "user",
                "secret"
            ))
            .unwrap()
            .1,
        b"snapshot"
    );
    assert!(matches!(
        facade.copy_files(&["file".into()], "parent"),
        Err(CloudError::ProviderUnsupported)
    ));
    let upload_root = tempfile::tempdir().unwrap();
    let state =
        versadown_lib::cloud::commands::CloudState::new(upload_root.path().into()).unwrap();
    let upload_http = versadown_lib::cloud::ReqwestTransport::new().unwrap();
    assert_eq!(
        facade
            .upload(
                &state,
                &upload_http,
                "parent",
                std::path::Path::new("file.bin")
            )
            .unwrap()
            .id,
        "uploaded"
    );
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "provider:quark",
            "account",
            "list:parent",
            "rename:file:renamed",
            "cleanup:transferred",
            "download:leaf",
            "auth:user",
            "upload:parent:file.bin"
        ]
    );
    let mut disabled = registry.get(CloudProvider::Quark).capabilities;
    disabled.rename = false;
    disabled.upload = false;
    let mut unsupported = ProviderDriver::new(disabled, Box::new(RecordingDriver(calls.clone())));
    let before = calls.lock().unwrap().len();
    assert!(matches!(
        unsupported.rename("file", "forbidden"),
        Err(CloudError::ProviderUnsupported)
    ));
    assert!(matches!(
        unsupported.upload(
            &state,
            &upload_http,
            "parent",
            std::path::Path::new("forbidden.bin")
        ),
        Err(CloudError::ProviderUnsupported)
    ));
    assert_eq!(calls.lock().unwrap().len(), before);
}

#[test]
fn public_command_exports_keep_all_provider_codes_and_file_kind_json() {
    use versadown_lib::cloud::{commands, CloudFileKind};
    let store = MemoryCredentialStore::default();
    for provider in CloudProvider::ALL {
        let capability =
            serde_json::to_value(commands::cloud_provider_capabilities(provider)).unwrap();
        assert_eq!(capability["provider"], provider.key());
        let error =
            commands::restore_account(provider, &store, &RecordingHttp::sequence([])).unwrap_err();
        assert_eq!(
            serde_json::to_value(error).unwrap()["code"],
            "not_authorized"
        );
        if provider.is_enabled() {
            commands::logout_provider(provider, &store).unwrap();
        } else {
            assert!(matches!(
                commands::logout_provider(provider, &store),
                Err(QuarkError::ProviderUnsupported)
            ));
        }
    }
    assert_eq!(serde_json::to_value(CloudFileKind::File).unwrap(), "file");
    assert_eq!(
        serde_json::to_value(CloudFileKind::Folder).unwrap(),
        "folder"
    );
    assert!(serde_json::from_str::<CloudFileKind>("\"invalid\"").is_err());
}

#[test]
fn native_capability_drivers_work_in_a_consumer_without_a_tauri_host() {
    use versadown_lib::cloud::{CloudError, ProviderRegistry};
    struct NoNetwork;
    impl CloudHttpTransport for NoNetwork {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
            panic!("an unauthorized native driver must fail before network I/O")
        }
    }
    let store = MemoryCredentialStore::default();
    for provider in CloudProvider::ALL {
        let mut driver = ProviderRegistry::global().driver(provider, &store, &NoNetwork);
        let _recipe = driver.auth().web_authorization_recipe();
        assert!(
            matches!(driver.account(), Err(CloudError::NotAuthorized)),
            "{provider:?}"
        );
        let directory_error = driver
            .list_directory(ProviderRegistry::global().get(provider).root_directory_id)
            .unwrap_err();
        let expected_code = if matches!(
            provider,
            CloudProvider::Quark | CloudProvider::Pikpak | CloudProvider::Aliyun
        ) {
            "credentials_expired"
        } else {
            "not_authorized"
        };
        assert_eq!(
            serde_json::to_value(directory_error).unwrap()["code"],
            expected_code,
            "{provider:?}"
        );
    }
}

#[test]
fn native_password_auth_outcome_is_usable_without_a_tauri_host() {
    use versadown_lib::cloud::{
        driver::NativeLoginOutcome, set_browser_password_login_preferred, ProviderRegistry,
    };
    let store = MemoryCredentialStore::default();
    set_browser_password_login_preferred(&store, CloudProvider::Baidu, true).unwrap();
    let http = RecordingHttp::sequence([]);
    let mut driver = ProviderRegistry::global().driver(CloudProvider::Baidu, &store, &http);
    assert!(matches!(
        driver
            .auth()
            .login_password("fixture-user", "fixture-password"),
        Ok(NativeLoginOutcome::NeedBrowserFallback(
            QuarkError::WebAuthorizationRequired
        )),
    ));
    assert!(store.load("baidu").unwrap().is_none());
}

#[test]
fn browser_account_replacement_fails_closed_and_attempts_all_owner_cleanup() {
    use versadown_lib::cloud::{
        baidu_browser_profile_username, select_password_credential_account,
        set_baidu_browser_profile_username,
    };
    struct FailingSessionRemoval {
        inner: MemoryCredentialStore,
        removed: Mutex<Vec<String>>,
    }
    impl CredentialStore for FailingSessionRemoval {
        fn save(&self, key: &str, bytes: &[u8]) -> Result<(), QuarkError> {
            self.inner.save(key, bytes)
        }
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
            self.inner.load(key)
        }
        fn remove(&self, key: &str) -> Result<(), QuarkError> {
            self.removed.lock().unwrap().push(key.into());
            if key == "baidu" {
                return Err(QuarkError::Storage);
            }
            self.inner.remove(key)
        }
    }
    let store = FailingSessionRemoval {
        inner: MemoryCredentialStore::default(),
        removed: Mutex::new(Vec::new()),
    };
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "a", "alice", "secret-a"),
    )
    .unwrap();
    set_baidu_browser_profile_username(&store, Some("alice")).unwrap();
    store.save("baidu", b"old-session").unwrap();
    assert!(matches!(
        select_password_credential_account(&store, CloudProvider::Baidu, "b"),
        Err(QuarkError::Storage)
    ));
    assert!(load_password_credential(&store, CloudProvider::Baidu)
        .unwrap()
        .is_none());
    assert!(baidu_browser_profile_username(&store).unwrap().is_none());
    assert!(store
        .removed
        .lock()
        .unwrap()
        .iter()
        .any(|key| key == "cloud-password-draft-baidu"));
}

#[test]
fn saved_credential_metadata_only_exists_for_catalog_direct_password_providers() {
    use versadown_lib::cloud::{saved_cloud_credential_info, ProviderRegistry};
    let store = MemoryCredentialStore::default();
    for descriptor in ProviderRegistry::global().catalog() {
        if descriptor.authorization.password_login_mode == PasswordLoginMode::RustDirect {
            save_password_credential(
                &store,
                CloudPasswordCredential::new(descriptor.provider, "only", "alice", "secret"),
            )
            .unwrap();
            let json = serde_json::to_value(
                saved_cloud_credential_info(&store, descriptor.provider)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                json,
                serde_json::json!({"username": "alice", "hasSavedPassword": true})
            );
        } else {
            assert!(saved_cloud_credential_info(&store, descriptor.provider)
                .unwrap()
                .is_none());
        }
    }
}

#[test]
fn registry_catalog_is_complete_unique_and_routes_each_share() {
    use versadown_lib::cloud::ProviderRegistry;
    use std::collections::HashSet;
    let registry = ProviderRegistry::global();
    let descriptors = registry.all();
    let catalog = registry.catalog();
    assert_eq!(descriptors.len(), 7);
    assert_eq!(catalog.len(), 6);
    assert!(catalog
        .iter()
        .all(|descriptor| descriptor.provider.is_enabled()));
    assert_eq!(
        descriptors
            .iter()
            .map(|d| d.provider)
            .collect::<HashSet<_>>()
            .len(),
        7
    );
    assert_eq!(
        descriptors
            .iter()
            .map(|d| d.provider.key())
            .collect::<HashSet<_>>()
            .len(),
        7
    );
    for descriptor in descriptors {
        assert!(!descriptor.root_directory_id.is_empty());
        assert!(!descriptor.recycle_bin_directory_id.is_empty());
        assert_ne!(
            descriptor.root_directory_id,
            descriptor.recycle_bin_directory_id
        );
        assert_eq!(
            registry.get(descriptor.provider).provider,
            descriptor.provider
        );
        let auth = url::Url::parse(descriptor.authorization_url).unwrap();
        assert_eq!(auth.scheme(), "https");
        for host in descriptor.share_hosts {
            let url = url::Url::parse(&format!(
                "https://{host}{}example",
                descriptor.share_patterns[0]
            ))
            .unwrap();
            if descriptor.provider.is_enabled() {
                assert_eq!(
                    registry.for_share_url(&url).unwrap().provider,
                    descriptor.provider
                );
            } else {
                assert!(registry.for_share_url(&url).is_none());
            }
            let spoof = url::Url::parse(&format!(
                "https://{host}.evil.test{}example",
                descriptor.share_patterns[0]
            ))
            .unwrap();
            assert!(registry.for_share_url(&spoof).is_none());
        }
    }
    assert_eq!(
        serde_json::to_value(registry.catalog()).unwrap()[0]["rootDirectoryId"],
        "0"
    );
}

#[test]
fn registry_only_advertises_implemented_share_transfer_capabilities() {
    use versadown_lib::cloud::ProviderRegistry;
    for (provider, supports_transfer) in [
        (CloudProvider::Quark, true),
        (CloudProvider::Baidu, true),
        (CloudProvider::Uc, true),
        (CloudProvider::Xunlei, true),
        (CloudProvider::Pikpak, true),
        (CloudProvider::Tianyi, false),
        (CloudProvider::Aliyun, true),
    ] {
        let capabilities = &ProviderRegistry::global().get(provider).capabilities;
        assert!(capabilities.share_inspect);
        assert_eq!(
            capabilities.share_transfer, supports_transfer,
            "{provider:?}"
        );
        assert_eq!(
            capabilities.recorded_transfer_resolve, supports_transfer,
            "{provider:?}"
        );
    }
}

#[test]
fn registry_keeps_password_browser_hosts_https_and_provider_scoped() {
    use versadown_lib::cloud::ProviderRegistry;
    let baidu = ProviderRegistry::global().get(CloudProvider::Baidu);
    assert!(baidu
        .accepts_password_login_url(&url::Url::parse("https://passport.baidu.com/login").unwrap()));
    for value in [
        "http://passport.baidu.com/login",
        "https://passport.baidu.com:444/login",
        "https://passport.baidu.com.evil.test/login",
        "https://open.e.189.cn/login",
    ] {
        assert!(!baidu.accepts_password_login_url(&url::Url::parse(value).unwrap()));
    }
    let tianyi = ProviderRegistry::global().get(CloudProvider::Tianyi);
    assert!(
        tianyi.accepts_password_login_url(&url::Url::parse("https://open.e.189.cn/login").unwrap())
    );
    assert!(!ProviderRegistry::global()
        .get(CloudProvider::Quark)
        .accepts_password_login_url(&url::Url::parse("https://pan.quark.cn").unwrap()));
}

struct RecordingHttp(Mutex<VecDeque<&'static str>>);

impl RecordingHttp {
    fn sequence(responses: impl IntoIterator<Item = &'static str>) -> Self {
        Self(Mutex::new(responses.into_iter().collect()))
    }
}

impl CloudHttpTransport for RecordingHttp {
    fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        Ok(CloudHttpResponse {
            status: 200,
            body: self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(QuarkError::InvalidResponse)?
                .into(),
            ..Default::default()
        })
    }
}

struct FailingSessionRemovalStore {
    values: Mutex<BTreeMap<String, Vec<u8>>>,
    removals: Mutex<Vec<String>>,
}

impl FailingSessionRemovalStore {
    fn new() -> Self {
        Self {
            values: Mutex::new(BTreeMap::new()),
            removals: Mutex::new(Vec::new()),
        }
    }

    fn removals(&self) -> Vec<String> {
        self.removals.lock().unwrap().clone()
    }
}

impl CredentialStore for FailingSessionRemovalStore {
    fn save(&self, key: &str, value: &[u8]) -> Result<(), QuarkError> {
        self.values
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .insert(key.into(), value.into());
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
        Ok(self
            .values
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .get(key)
            .cloned())
    }

    fn remove(&self, key: &str) -> Result<(), QuarkError> {
        self.removals
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .push(key.into());
        if key == "baidu" {
            return Err(QuarkError::Storage);
        }
        self.values
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .remove(key);
        Ok(())
    }
}

#[test]
fn new_providers_have_stable_sources_and_declared_login_methods() {
    for (source, provider, password) in [
        ("cloud://baidu/f1", CloudProvider::Baidu, true),
        ("cloud://tianyi/f1", CloudProvider::Tianyi, true),
        ("cloud://aliyun/f1", CloudProvider::Aliyun, false),
        ("cloud://xunlei/f1", CloudProvider::Xunlei, true),
        ("cloud://uc/f1", CloudProvider::Uc, false),
    ] {
        assert_eq!(parse_stable_cloud_source(source).unwrap().0, provider);
        assert_eq!(provider_capabilities(provider).web, provider.is_enabled());
        assert_eq!(
            provider_capabilities(provider).password,
            provider.is_enabled() && password
        );
    }
}

#[test]
fn capabilities_include_provider_and_distinguish_direct_from_official_web_password_login() {
    let baidu = provider_capabilities(CloudProvider::Baidu);
    assert_eq!(serde_json::to_value(baidu).unwrap()["provider"], "baidu");
    assert_eq!(baidu.password_login_mode, PasswordLoginMode::RustDirect);
    assert!(baidu.password);

    let aliyun = provider_capabilities(CloudProvider::Aliyun);
    assert_eq!(
        aliyun.password_login_mode,
        PasswordLoginMode::OfficialWebView
    );
    assert!(!aliyun.password);
}

#[test]
fn logout_for_a_connected_new_provider_removes_its_credential() {
    let store = MemoryCredentialStore::default();
    store.save("baidu", b"still-authorized").unwrap();

    logout_provider(CloudProvider::Baidu, &store).unwrap();

    assert!(store.load("baidu").unwrap().is_none());
}

#[test]
fn logout_keeps_the_password_for_explicit_login() {
    let store = MemoryCredentialStore::default();
    store.save("baidu", b"still-authorized").unwrap();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "account-1", "user", "secret"),
    )
    .unwrap();

    logout_provider(CloudProvider::Baidu, &store).unwrap();

    assert!(store.load("baidu").unwrap().is_none());
    assert_eq!(
        load_password_credential(&store, CloudProvider::Baidu)
            .unwrap()
            .unwrap()
            .username(),
        "user"
    );
}

#[test]
fn failed_logout_never_deletes_the_saved_password() {
    let store = FailingSessionRemovalStore::new();
    store.save("baidu", b"still-authorized").unwrap();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Baidu, "account-1", "user", "secret"),
    )
    .unwrap();

    assert!(matches!(
        logout_provider(CloudProvider::Baidu, &store),
        Err(QuarkError::Storage)
    ));
    assert!(store.removals().contains(&"baidu".to_owned()));
    assert!(!store
        .removals()
        .contains(&"cloud-password-baidu".to_owned()));
    assert_eq!(
        load_password_credential(&store, CloudProvider::Baidu)
            .unwrap()
            .unwrap()
            .username(),
        "user"
    );
}

#[test]
fn each_new_stable_source_resolves_to_its_new_provider() {
    for source in [
        "cloud://baidu/f1",
        "cloud://tianyi/f1",
        "cloud://aliyun/f1",
        "cloud://xunlei/f1",
        "cloud://uc/f1",
    ] {
        let (provider, id) = parse_stable_cloud_source(source).unwrap();
        assert!(!id.is_empty());
        assert!(matches!(
            provider,
            CloudProvider::Baidu
                | CloudProvider::Tianyi
                | CloudProvider::Aliyun
                | CloudProvider::Xunlei
                | CloudProvider::Uc
        ));
    }
}

#[test]
fn restore_account_dispatches_each_new_provider_without_falling_back_to_unsupported() {
    let baidu = MemoryCredentialStore::default();
    baidu
        .save(
            "baidu",
            &serde_json::to_vec(&BaiduSession::cookie("BDUSS=session")).unwrap(),
        )
        .unwrap();
    assert_eq!(
        restore_account(
            CloudProvider::Baidu,
            &baidu,
            &RecordingHttp::sequence([r#"{"errno":0,"user":{"uk":"b1","netdisk_name":"baidu"}}"#]),
        )
        .unwrap()
        .provider,
        CloudProvider::Baidu
    );

    let tianyi = MemoryCredentialStore::default();
    tianyi
        .save(
            "tianyi",
            &serde_json::to_vec(&TianyiSession::cookie("COOKIE_LOGIN_USER=session")).unwrap(),
        )
        .unwrap();
    assert_eq!(
        restore_account(
            CloudProvider::Tianyi,
            &tianyi,
            &RecordingHttp::sequence([
                r#"{"res_code":0,"userInfo":{"userId":"t1","nickName":"tianyi"}}"#
            ]),
        )
        .unwrap()
        .provider,
        CloudProvider::Tianyi
    );

    let aliyun = MemoryCredentialStore::default();
    aliyun
        .save(
            "aliyun",
            br#"{"refresh_token":"refresh","access_token":"old","drive_id":"d1"}"#,
        )
        .unwrap();
    assert_eq!(
        restore_account(
            CloudProvider::Aliyun,
            &aliyun,
            &RecordingHttp::sequence([
                r#"{"access_token":"access","refresh_token":"next","default_drive_id":"d1"}"#,
                r#"{"user_id":"a1","nick_name":"aliyun"}"#,
                r#"{"items":[]}"#,
            ]),
        )
        .unwrap()
        .provider,
        CloudProvider::Aliyun
    );

    let xunlei = MemoryCredentialStore::default();
    let xunlei_session = XunleiSession::with_access(
        "access",
        "refresh",
        XunleiClientProfile {
            client_id: "client".into(),
            client_secret: "secret".into(),
            device_id: "device".into(),
            client_version: "1.0".into(),
            package_name: "package".into(),
            user_agent: "agent".into(),
        },
    );
    xunlei
        .save("xunlei", &serde_json::to_vec(&xunlei_session).unwrap())
        .unwrap();
    assert_eq!(
        restore_account(
            CloudProvider::Xunlei,
            &xunlei,
            &RecordingHttp::sequence([r#"{"id":"x1","name":"xunlei"}"#]),
        )
        .unwrap()
        .provider,
        CloudProvider::Xunlei
    );

    let uc = MemoryCredentialStore::default();
    uc.save(
        "uc",
        &serde_json::to_vec(&UcSession::cookie(
            "__uid=u1; __pus=identity; __puus=session",
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        restore_account(
            CloudProvider::Uc,
            &uc,
            &RecordingHttp::sequence([
                r#"{"code":0,"status":200,"data":{"user_id":"u1","nickname":"uc"}}"#,
            ]),
        )
        .unwrap()
        .provider,
        CloudProvider::Uc
    );
}
