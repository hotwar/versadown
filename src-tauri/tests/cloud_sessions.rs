use versadown_lib::cloud::{
    credential_key_is_supported, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport,
    CredentialStore, MemoryCredentialStore, PikPakClient, QuarkError, SystemCredentialStore,
};
use std::sync::Mutex;

// A session that leaves and returns to identical bytes still has a new lineage.
#[test]
fn snapshot_rotation_cannot_write_back_after_unversioned_account_round_trip() {
    use versadown_lib::cloud::{
        auth_supervisor::AuthSupervisor, CloudProvider, SessionWorkKind,
    };
    let supervisor = AuthSupervisor::new(|| 1000, || 30);
    let store = MemoryCredentialStore::default();
    store.save("pikpak", b"original").unwrap();
    supervisor
        .run(
            &store,
            CloudProvider::Pikpak,
            SessionWorkKind::Mutation,
            |old| {
                for bytes in [b"replacement".as_slice(), b"original".as_slice()] {
                    store.save("pikpak", bytes)?;
                    supervisor.run(
                        &store,
                        CloudProvider::Pikpak,
                        SessionWorkKind::Read,
                        |current| current.load("pikpak"),
                        |_| panic!("no recovery"),
                    )?;
                }
                old.save("pikpak", b"obsolete-rotation")
            },
            |_| panic!("no recovery"),
        )
        .unwrap();
    assert_eq!(store.load("pikpak").unwrap().unwrap(), b"original");
}

#[test]
fn failed_transfer_does_not_commit_its_private_session_rotation() {
    use versadown_lib::cloud::{
        auth_supervisor::AuthSupervisor, CloudProvider, SessionWorkKind,
    };
    for message in ["share polling cancelled", "share polling timed out"] {
        let supervisor = AuthSupervisor::new(|| 1000, || 30);
        let store = MemoryCredentialStore::default();
        store.save("pikpak", b"current").unwrap();
        let stopped = std::cell::Cell::new(false);
        let result: Result<(), QuarkError> = supervisor.run_checked(
            &store,
            CloudProvider::Pikpak,
            SessionWorkKind::Mutation,
            |snapshot| {
                snapshot.save("pikpak", b"abandoned-rotation")?;
                stopped.set(true);
                Err(QuarkError::Transport(message.into()))
            },
            |_| panic!("transport errors must not recover"),
            || {
                if stopped.get() {
                    Err(QuarkError::Transport(message.into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(QuarkError::Transport(_))));
        assert_eq!(store.load("pikpak").unwrap().unwrap(), b"current");
    }
}

#[test]
fn ordinary_provider_failure_keeps_a_successfully_rotated_session() {
    use versadown_lib::cloud::{
        auth_supervisor::AuthSupervisor, CloudProvider, SessionWorkKind,
    };
    let supervisor = AuthSupervisor::new(|| 1000, || 30);
    let store = MemoryCredentialStore::default();
    store.save("pikpak", b"old").unwrap();
    let result: Result<(), QuarkError> = supervisor.run(
        &store,
        CloudProvider::Pikpak,
        SessionWorkKind::Mutation,
        |snapshot| {
            snapshot.save("pikpak", b"valid-rotation")?;
            Err(QuarkError::Provider("file unavailable".into()))
        },
        |_| panic!("provider failures do not recover"),
    );
    assert!(matches!(result, Err(QuarkError::Provider(_))));
    assert_eq!(store.load("pikpak").unwrap().unwrap(), b"valid-rotation");
}

#[test]
fn action_ticket_rotation_does_not_hide_a_failed_mutations_auth_recovery() {
    use versadown_lib::cloud::{
        auth_supervisor::AuthSupervisor, CloudProvider, SessionWorkKind,
    };
    let supervisor = AuthSupervisor::new(|| 1000, || 30);
    let store = MemoryCredentialStore::default();
    let refreshes = std::cell::Cell::new(0);
    store.save("pikpak", b"old").unwrap();
    // CloudState initializes persisted authorization before executing session work.
    supervisor.restore(&store, CloudProvider::Pikpak).unwrap();
    let result: Result<(), QuarkError> = supervisor.run(
        &store,
        CloudProvider::Pikpak,
        SessionWorkKind::Mutation,
        |snapshot| {
            snapshot.save("pikpak", b"new-ticket-expired-session")?;
            Err(QuarkError::AuthorizationExpired)
        },
        |_| {
            refreshes.set(refreshes.get() + 1);
            store.save("pikpak", b"renewed")
        },
    );
    assert!(matches!(
        result,
        Err(QuarkError::SessionRenewedRetryOperation)
    ));
    assert_eq!(refreshes.get(), 1);
    assert_eq!(store.load("pikpak").unwrap().unwrap(), b"renewed");
}

#[test]
fn credential_store_supports_every_persisted_cloud_key() {
    for key in [
        "quark",
        "pikpak",
        "baidu",
        "tianyi",
        "aliyun",
        "xunlei",
        "uc",
        "cloud-password-pikpak",
        "cloud-password-baidu",
        "cloud-password-tianyi",
        "cloud-password-draft-pikpak",
        "cloud-password-draft-baidu",
        "cloud-password-draft-tianyi",
        "cloud-password-baidu-browser-preferred",
        "cloud-password-baidu-browser-profile-username",
        "cloud-share-plans-v1",
        "cloud-auth-maintenance-quark",
        "cloud-auth-maintenance-pikpak",
        "cloud-auth-maintenance-baidu",
        "cloud-auth-maintenance-tianyi",
        "cloud-auth-maintenance-aliyun",
        "cloud-auth-maintenance-xunlei",
        "cloud-auth-maintenance-uc",
    ] {
        assert!(
            credential_key_is_supported(key),
            "missing credential key {key}"
        );
    }
}

#[test]
fn legacy_pikpak_password_session_recovers_its_login_method() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","client_id":"YNxT9w7GMdWvEOKa"}"#).unwrap();
    let http = Sequence::new(&[r#"{"sub":"42","name":"alice"}"#]);
    let account = PikPakClient::new(&http, &store).account().unwrap();
    assert_eq!(
        account.auth_method,
        versadown_lib::cloud::CloudAuthMethod::Password
    );
}

#[cfg(windows)]
#[test]
fn windows_session_roundtrip_across_reopen_never_writes_local_secret_files() {
    let root = tempfile::tempdir().unwrap();
    {
        let store = SystemCredentialStore::new(root.path().into()).unwrap();
        store
            .save("quark", b"cookie-secret-do-not-persist-in-cleartext")
            .unwrap();
    }
    let store = SystemCredentialStore::new(root.path().into()).unwrap();
    assert_eq!(
        store.load("quark").unwrap().unwrap(),
        b"cookie-secret-do-not-persist-in-cleartext"
    );
    assert!(
        std::fs::read_dir(root.path()).unwrap().next().is_none(),
        "Windows credentials must remain in Credential Manager, not local files"
    );
    store.remove("quark").unwrap();
    assert!(store.load("quark").unwrap().is_none());
}

struct Sequence(Mutex<(Vec<CloudHttpResponse>, Vec<CloudHttpRequest>)>);

#[test]
fn saved_password_recovery_reuses_the_provider_account_device() {
    use versadown_lib::cloud::{CloudPasswordCredential, CloudProvider, ProviderRegistry};
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"old","refresh_token":"old-refresh","expires_at":0,"device_id":"stable-account-device","user_id":"account"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"captcha_token":"captcha","expires_in":300}"#,
        r#"{"access_token":"new","refresh_token":"new-refresh","expires_in":3600,"sub":"account"}"#,
        r#"{"sub":"account","name":"alice"}"#,
        r#"{"files":[],"next_page_token":""}"#,
    ]);
    let credential = CloudPasswordCredential::new(
        CloudProvider::Pikpak,
        "account",
        "alice",
        "backend-password",
    );
    let (_, bytes) = ProviderRegistry::global()
        .driver(CloudProvider::Pikpak, &store, &http)
        .auth()
        .recover_password(&credential)
        .unwrap();
    let session: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(session["device_id"], "stable-account-device");
}

#[test]
fn browser_token_capture_preserves_a_known_absolute_expiry() {
    use versadown_lib::cloud::commands::parse_web_tokens;
    let session = parse_web_tokens(
        r#"{"access_token":"access","refresh_token":"refresh","expires_at":2000000000}"#,
    )
    .unwrap();
    assert_eq!(session.expires_at, 2000000000);
}

#[test]
fn risk_limited_refreshes_action_captcha_once_and_persists_it() {
    assert_eq!(
        versadown_lib::cloud::pikpak_captcha_sign(
            "YNxT9w7GMdWvEOKa",
            "device",
            "1700000000000"
        ),
        "1.c78aafdaece3299f59a7bc4e4c10b2e8"
    );
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"old-cap","user_id":"user-42"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"error":"riskLimited","error_code":9}"#,
        r#"{"captcha_token":"fresh-cap","expires_in":300}"#,
        r#"{"files":[],"next_page_token":""}"#,
    ]);
    let directory = PikPakClient::new(&http, &store).list("0").unwrap();
    assert_eq!(directory.items.len(), 1);
    assert_eq!(
        directory.items[0].id,
        "__versa_down_pikpak_recycle_bin__"
    );
    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 3);
    let payload: serde_json::Value =
        serde_json::from_str(requests[1].body.as_ref().unwrap()).unwrap();
    assert_eq!(payload["action"], "GET:/drive/v1/files");
    assert_eq!(payload["captcha_token"], "old-cap");
    assert_eq!(payload["meta"]["user_id"], "user-42");
    assert_eq!(payload["meta"]["client_version"], "1.53.2");
    assert!(payload["meta"]["captcha_sign"]
        .as_str()
        .unwrap()
        .starts_with("1."));
    assert_eq!(requests[1].headers["Authorization"], "Bearer access");
    assert_eq!(requests[1].headers["X-Device-Id"], "device");
    assert_eq!(requests[2].headers["X-Captcha-Token"], "fresh-cap");
    let session: serde_json::Value =
        serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
    assert_eq!(session["captcha_token"], "fresh-cap");
}

#[test]
fn risk_limited_retry_is_bounded_and_interactive_challenge_stops_retry() {
    for (middle, expected_requests) in [
        (r#"{"captcha_token":"fresh-cap","expires_in":300}"#, 3),
        (
            r#"{"url":"https://captcha.mypikpak.com/challenge","captcha_token":"pending"}"#,
            2,
        ),
    ] {
        let store = MemoryCredentialStore::default();
        store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"old-cap"}"#).unwrap();
        let http = Sequence::new(&[
            r#"{"error":"riskLimited","error_code":9}"#,
            middle,
            r#"{"error":"riskLimited","error_code":9}"#,
        ]);
        assert!(matches!(
            PikPakClient::new(&http, &store).list("0"),
            Err(QuarkError::PikpakOperationVerificationRequired(_))
        ));
        assert_eq!(http.0.lock().unwrap().1.len(), expected_requests);
    }
}

#[test]
fn pikpak_delete_uses_the_permanent_batch_delete_endpoint() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap","captcha_expires_at":9999999999,"captcha_action":"GET:/drive/v1/files"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"files":[{"id":"trashed-file-1","name":"one","kind":"drive#file"},{"id":"trashed-file-2","name":"two","kind":"drive#file"}],"next_page_token":""}"#,
        r#"{"captcha_token":"delete-cap","expires_in":300}"#,
        r#"{}"#,
    ]);

    PikPakClient::new(&http, &store)
        .delete(&["trashed-file-1".to_owned(), "trashed-file-2".to_owned()])
        .unwrap();

    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].url.contains("/drive/v1/files?"));
    assert!(requests[0]
        .url
        .contains("%22trashed%22%3A%7B%22eq%22%3Atrue%7D"));
    assert!(requests[1].url.contains("/v1/shield/captcha/init"));
    let request = &requests[2];
    assert!(request.url.ends_with("/drive/v1/files:batchDelete"));
    assert_eq!(request.method, "POST");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(request.body.as_deref().unwrap()).unwrap()["ids"],
        serde_json::json!(["trashed-file-1", "trashed-file-2"])
    );
}

#[test]
fn pikpak_delete_refreshes_the_mutation_captcha_after_recycle_listing() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"0123456789abcdef0123456789abcdef","captcha_token":"old-cap","client_id":"YUMx5nI8ZU8Ap8pm","user_id":"owner","auth_method":"web"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"error_code":9}"#,
        r#"{"captcha_token":"read-cap","expires_in":300}"#,
        r#"{"files":[{"id":"trashed-folder","name":"old","kind":"drive#folder"}],"next_page_token":""}"#,
        r#"{"captcha_token":"delete-cap","expires_in":300}"#,
        r#"{}"#,
    ]);

    PikPakClient::new(&http, &store)
        .delete(&["trashed-folder".to_owned()])
        .unwrap();

    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 5);
    assert!(requests[3].url.contains("/v1/shield/captcha/init"));
    let captcha: serde_json::Value =
        serde_json::from_str(requests[3].body.as_deref().unwrap()).unwrap();
    assert_eq!(captcha["action"], "POST:/drive/v1/files:batchDelete");
    assert!(requests[4].url.ends_with("/drive/v1/files:batchDelete"));
    assert_eq!(requests[4].headers["X-Captcha-Token"], "delete-cap");
}

#[test]
fn pikpak_delete_does_not_reuse_an_unscoped_login_captcha_for_the_mutation() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"0123456789abcdef0123456789abcdef","captcha_token":"login-cap","client_id":"YUMx5nI8ZU8Ap8pm","user_id":"owner","auth_method":"web"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"files":[{"id":"trashed-folder","name":"old","kind":"drive#folder"}],"next_page_token":""}"#,
        r#"{"captcha_token":"delete-cap","expires_in":300}"#,
        r#"{}"#,
    ]);

    PikPakClient::new(&http, &store)
        .delete(&["trashed-folder".to_owned()])
        .unwrap();

    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 3);
    assert!(requests[1].url.contains("/v1/shield/captcha/init"));
    let captcha: serde_json::Value =
        serde_json::from_str(requests[1].body.as_deref().unwrap()).unwrap();
    assert_eq!(captcha["action"], "POST:/drive/v1/files:batchDelete");
    assert!(requests[2].url.ends_with("/drive/v1/files:batchDelete"));
    assert_eq!(requests[2].headers["X-Captcha-Token"], "delete-cap");
}

#[test]
fn pikpak_delete_rejects_items_outside_the_recycle_bin_without_batch_delete() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"files":[{"id":"trashed-file","name":"old","kind":"drive#file"}],"next_page_token":""}"#,
    ]);

    let error = PikPakClient::new(&http, &store)
        .delete(&["trashed-file".to_owned(), "live-file".to_owned()])
        .unwrap_err();

    assert!(matches!(error, QuarkError::InvalidInput(_)));
    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert!(requests
        .iter()
        .all(|request| !request.url.contains("batchDelete")));
}

#[test]
fn pikpak_maps_explicit_regional_to_global_membership_restriction() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let http = Sequence::new(&[r#"{
        "error":"forbidden",
        "error_description":"Regional Premium is restricted in the current location.",
        "details":[{"reason":"Upgrade to Global Premium to download."}]
    }"#]);

    let error = PikPakClient::new(&http, &store).list("0").unwrap_err();

    assert!(matches!(
        error,
        QuarkError::PikpakRegionalMembershipRestricted
    ));
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["code"], "pikpak_regional_membership_restricted");
    assert_eq!(
        value["message"],
        "区域会员在当前 IP 地区下载受限；请切换到有权限 IP 后重新登录重试，或升级全球会员"
    );
}

#[test]
fn pikpak_does_not_map_captcha_or_incomplete_membership_reasons() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    for bodies in [
        vec![
            r#"{"error":"captcha_invalid","error_code":4031,"error_description":"Regional Premium requires Global Premium upgrade"}"#,
        ],
        vec![r#"{"error":"forbidden","message":"Regional Premium is restricted"}"#],
        vec![r#"{"error":"forbidden","details":[{"reason":"Upgrade to Global Premium"}]}"#],
        vec![
            r#"{"error":"unauthenticated","message":"Regional Premium is restricted; upgrade to Global Premium"}"#,
            r#"{"error":"forbidden"}"#,
        ],
        vec![
            r#"{"error":"riskLimited","error_code":4031,"error_description":"Regional Premium is restricted; upgrade to Global Premium"}"#,
            r#"{"url":"https://user.mypikpak.com/verify"}"#,
        ],
    ] {
        let error = PikPakClient::new(&Sequence::new(&bodies), &store)
            .list("0")
            .unwrap_err();
        assert!(
            !matches!(error, QuarkError::PikpakRegionalMembershipRestricted),
            "captcha, authorization, and incomplete membership reasons must retain existing errors"
        );
    }
}

#[test]
fn pikpak_rejects_the_virtual_recycle_bin_as_a_source_before_network() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let client = PikPakClient::new(&NoNetwork, &store);
    let recycle = "__versa_down_pikpak_recycle_bin__";

    assert!(matches!(
        client.rename(recycle, "name"),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.move_files(&[recycle.into()], "0"),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.copy_files(&[recycle.into()], "0"),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.trash(&[recycle.into()]),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.delete(&[recycle.into()]),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.download(recycle),
        Err(QuarkError::InvalidInput(_))
    ));
}

#[test]
fn pikpak_root_includes_the_virtual_recycle_bin() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"files":[{"id":"normal","name":"normal.txt","kind":"drive#file"}],"next_page_token":""}"#,
    ]);

    let directory = PikPakClient::new(&http, &store).list("0").unwrap();

    assert!(directory.items.iter().any(|file| {
        file.id == "__versa_down_pikpak_recycle_bin__"
            && file.name == "回收站"
            && file.kind == "folder"
            && file.parent_id == "0"
    }));
}

#[test]
fn pikpak_virtual_recycle_bin_lists_all_trashed_pages() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"files":[{"id":"deleted-1","name":"old-a.txt","kind":"drive#file"}],"next_page_token":"next"}"#,
        r#"{"files":[{"id":"deleted-2","name":"old-b.txt","kind":"drive#file"}],"next_page_token":""}"#,
    ]);

    let directory = PikPakClient::new(&http, &store)
        .list("__versa_down_pikpak_recycle_bin__")
        .unwrap();

    assert_eq!(directory.id, "__versa_down_pikpak_recycle_bin__");
    assert_eq!(directory.items.len(), 2);
    assert!(directory
        .items
        .iter()
        .all(|file| { file.parent_id == "__versa_down_pikpak_recycle_bin__" }));
    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 2);
    assert!(requests[0]
        .url
        .contains("%22trashed%22%3A%7B%22eq%22%3Atrue%7D"));
    assert!(requests[1].url.contains("page_token=next"));
}

struct NoNetwork;
impl CloudHttpTransport for NoNetwork {
    fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        Err(QuarkError::Transport("network must not be used".into()))
    }
}

#[test]
fn pikpak_rejects_the_virtual_recycle_bin_as_a_write_target() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let client = PikPakClient::new(&NoNetwork, &store);
    let recycle = "__versa_down_pikpak_recycle_bin__";

    assert!(matches!(
        client.mkdir(recycle, "new"),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.move_files(&["file".into()], recycle),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.copy_files(&["file".into()], recycle),
        Err(QuarkError::InvalidInput(_))
    ));
}

#[test]
fn pikpak_download_refreshes_the_collection_captcha_action_before_retrying_cache() {
    // Catches sending a per-file action such as `GET:/drive/v1/files/file-1`.
    // PikPak's drive captcha contract scopes a file fetch to the collection
    // endpoint (`GET:/drive/v1/files/`), even though the request URL includes
    // the concrete file id.
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"old-cap","client_id":"YNxT9w7GMdWvEOKa","user_id":"user-42"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"error":"riskLimited","error_code":9}"#,
        r#"{"captcha_token":"fresh-cap","expires_in":300}"#,
        r#"{"name":"payload.txt","web_content_link":"https://download.example/payload.txt"}"#,
    ]);

    let (url, name) = PikPakClient::new(&http, &store).download("file-1").unwrap();

    assert_eq!(url, "https://download.example/payload.txt");
    assert_eq!(name, "payload.txt");
    let requests = &http.0.lock().unwrap().1;
    let captcha: serde_json::Value =
        serde_json::from_str(requests[1].body.as_deref().unwrap()).unwrap();
    assert!(requests[0]
        .url
        .ends_with("/drive/v1/files/file-1?usage=CACHE"));
    assert_eq!(captcha["action"], "GET:/drive/v1/files/");
    assert!(requests[2]
        .url
        .ends_with("/drive/v1/files/file-1?usage=CACHE"));
    assert_eq!(requests[2].headers["X-Captcha-Token"], "fresh-cap");
}

#[test]
fn pikpak_cache_risk_for_browser_session_never_falls_back_to_fetch() {
    // Catches swallowing a CACHE action-verification failure and issuing a
    // second, more privileged FETCH request with the same browser OAuth
    // session. The server must decide whether interactive verification is
    // needed; the client must not bypass it by changing usage.
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap","client_id":"browser-client","auth_method":"web"}"#).unwrap();
    let http = Sequence::new(&[r#"{"error":"riskLimited","error_code":9}"#]);

    assert!(matches!(
        PikPakClient::new(&http, &store).download("file-1"),
        Err(QuarkError::PikpakOperationVerificationRequired(_))
    ));

    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .url
        .ends_with("/drive/v1/files/file-1?usage=CACHE"));
}

#[test]
fn pikpak_web_delete_renews_each_action_without_relogging_or_changing_client() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"0123456789abcdef0123456789abcdef","captcha_token":"old","client_id":"YUMx5nI8ZU8Ap8pm","user_id":"owner","auth_method":"web"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"error_code":9}"#,
        r#"{"captcha_token":"read-cap","expires_in":300}"#,
        r#"{"files":[{"id":"deleted-file","name":"old","kind":"drive#file"}],"next_page_token":""}"#,
        r#"{"captcha_token":"delete-cap","expires_in":300}"#,
        r#"{}"#,
    ]);
    PikPakClient::new(&http, &store)
        .delete(&["deleted-file".into()])
        .unwrap();
    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 5);
    for (index, action) in [
        (1, "GET:/drive/v1/files"),
        (3, "POST:/drive/v1/files:batchDelete"),
    ] {
        let body: serde_json::Value =
            serde_json::from_str(requests[index].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["client_id"], "YUMx5nI8ZU8Ap8pm");
        assert_eq!(body["action"], action);
        assert_eq!(body["device_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(body["meta"]["package_name"], "drive.mypikpak.com");
        assert_eq!(body["meta"]["client_version"], "undefined");
        assert_eq!(body["meta"]["timestamp"], "1789110095225");
        assert_eq!(
            body["meta"]["captcha_sign"],
            "1.a7fcbb4b535e3c24e3e1b3020ec33bcf"
        );
        assert_eq!(body["meta"]["user_id"], "owner");
    }
    assert_eq!(requests[4].headers["X-Captcha-Token"], "delete-cap");
    assert_eq!(requests[4].headers["X-Client-Id"], "YUMx5nI8ZU8Ap8pm");
    assert_eq!(requests[4].headers["Authorization"], "Bearer access");
    assert!(requests[4].headers["User-Agent"].contains("Mozilla/"));
    assert_eq!(requests[4].headers["Origin"], "https://mypikpak.com");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(requests[4].body.as_deref().unwrap()).unwrap()
            ["ids"],
        serde_json::json!(["deleted-file"])
    );
}

#[test]
fn pikpak_web_delete_challenge_stops_before_deletion_and_preserves_verification_context() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"0123456789abcdef0123456789abcdef","captcha_token":"old","client_id":"YUMx5nI8ZU8Ap8pm","user_id":"owner","auth_method":"web"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"error_code":9}"#,
        r#"{"captcha_token":"pending-cap","expires_in":300,"url":"https://user.mypikpak.com/verify?token=private-challenge"}"#,
    ]);
    let error = PikPakClient::new(&http, &store)
        .delete(&["deleted-file".into()])
        .unwrap_err();
    let public = serde_json::to_value(&error).unwrap();
    assert_eq!(public["code"], "pikpak_operation_verification_required");
    assert!(public["message"].as_str().unwrap().contains("读取文件列表"));
    assert!(!public.to_string().contains("private-challenge"));
    assert!(!format!("{error:?}").contains("private-challenge"));
    assert_eq!(http.0.lock().unwrap().1.len(), 2);
    let saved: serde_json::Value =
        serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
    assert_eq!(saved["captcha_token"], "pending-cap");
    assert_eq!(saved["access_token"], "access");
    assert_eq!(
        saved["verification_url"],
        "https://user.mypikpak.com/verify?token=private-challenge"
    );
}

#[test]
fn pikpak_operation_verification_rejects_other_accounts_and_unsafe_destinations() {
    use versadown_lib::cloud::allows_pikpak_operation_verification_url;
    for url in [
        "http://user.mypikpak.com/check",
        "https://user.mypikpak.com.evil.test/check",
        "https://mypikpak.com@evil.test/",
        "https://user.mypikpak.com:444/check",
        "file:///C:/secret",
        "https://user:secret@user.mypikpak.com/check",
    ] {
        assert!(!allows_pikpak_operation_verification_url(url), "{url}");
    }
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"0123456789abcdef0123456789abcdef","captcha_token":"cap","captcha_expires_at":9999999999,"verification_url":"https://user.mypikpak.com/check","client_id":"YUMx5nI8ZU8Ap8pm","auth_method":"web"}"#).unwrap();
    let http = Sequence::new(&[r#"{"sub":"other-owner","name":"other"}"#]);
    assert!(matches!(
        PikPakClient::new(&http, &store).operation_verification_url("original-owner"),
        Err(QuarkError::InvalidInput(_))
    ));
    assert_eq!(http.0.lock().unwrap().1.len(), 1);
}

#[test]
fn pikpak_operation_verification_uses_only_current_unexpired_official_challenge() {
    for (challenge, expiry, expected) in [
        (
            "https://user.mypikpak.com/check",
            9999999999_u64,
            "https://user.mypikpak.com/check",
        ),
        (
            "https://user.mypikpak.com/check",
            1,
            "https://mypikpak.com/drive",
        ),
        (
            "https://evil.test/check",
            9999999999,
            "https://mypikpak.com/drive",
        ),
    ] {
        let store = MemoryCredentialStore::default();
        store.save("pikpak", &serde_json::to_vec(&serde_json::json!({
            "access_token":"access", "refresh_token":"refresh", "expires_at":9999999999_u64,
            "device_id":"device", "captcha_token":"cap", "captcha_expires_at":expiry,
            "verification_url":challenge, "client_id":"YUMx5nI8ZU8Ap8pm", "auth_method":"web"
        })).unwrap()).unwrap();
        let http = Sequence::new(&[r#"{"sub":"owner","name":"owner"}"#]);
        assert_eq!(
            PikPakClient::new(&http, &store)
                .operation_verification_url("owner")
                .unwrap(),
            expected
        );
    }
}

#[test]
fn pikpak_verified_token_is_used_on_explicit_retry_without_creating_another_challenge() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"0123456789abcdef0123456789abcdef","captcha_token":"verified-cap","captcha_expires_at":9999999999,"captcha_action":"POST:/drive/v1/files:batchDelete","operation_verifications":{"POST:/drive/v1/files:batchDelete":{"token":"verified-cap","expires_at":9999999999,"url":null}},"client_id":"YUMx5nI8ZU8Ap8pm","auth_method":"web"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"captcha_token":"read-cap","expires_in":300}"#,
        r#"{"files":[{"id":"original-file","name":"old","kind":"drive#file"}],"next_page_token":""}"#,
        r#"{}"#,
    ]);
    PikPakClient::new(&http, &store)
        .delete(&["original-file".into()])
        .unwrap();
    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 3);
    assert!(requests[2].url.ends_with("/drive/v1/files:batchDelete"));
    assert_eq!(requests[2].headers["X-Captcha-Token"], "verified-cap");
}

#[test]
fn pikpak_delete_challenge_survives_metadata_captcha_rotation_on_explicit_retry() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"0123456789abcdef0123456789abcdef","captcha_token":"old","client_id":"YUMx5nI8ZU8Ap8pm","user_id":"owner","auth_method":"web"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"files":[{"id":"file","name":"old","kind":"drive#file"}],"next_page_token":""}"#,
        r#"{"captcha_token":"verify-delete","expires_in":300,"url":"https://user.mypikpak.com/check"}"#,
        // The next explicit retry refreshes the recycle listing action first.
        r#"{"captcha_token":"read-only-cap","expires_in":300}"#,
        r#"{"files":[{"id":"file","name":"old","kind":"drive#file"}],"next_page_token":""}"#,
        r#"{}"#,
    ]);
    let client = PikPakClient::new(&http, &store);
    assert_eq!(
        serde_json::to_value(client.delete(&["file".into()]).unwrap_err()).unwrap()["code"],
        "pikpak_operation_verification_required"
    );
    client.delete(&["file".into()]).unwrap();
    let requests = &http.0.lock().unwrap().1;
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[3].headers["X-Captcha-Token"], "read-only-cap");
    assert_eq!(requests[4].headers["X-Captcha-Token"], "verify-delete");
}

#[test]
fn pikpak_download_prefers_cache_usage_for_a_direct_file_link() {
    // Catches forcing FETCH, which turns an ordinary local download into a
    // separately captcha-gated action for browser-authorized sessions.
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"name":"payload.txt","web_content_link":"https://download.example/payload.txt"}"#,
    ]);

    let (url, name) = PikPakClient::new(&http, &store).download("file-1").unwrap();

    assert_eq!(url, "https://download.example/payload.txt");
    assert_eq!(name, "payload.txt");
    assert!(http.0.lock().unwrap().1[0]
        .url
        .ends_with("/drive/v1/files/file-1?usage=CACHE"));
}

#[test]
fn pikpak_download_falls_back_to_fetch_when_cache_has_no_direct_link() {
    // Catches treating CACHE's empty link (common for non-media uploads) as
    // an invalid response instead of trying the provider's raw-file path.
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"name":"payload.txt","web_content_link":""}"#,
        r#"{"name":"payload.txt","web_content_link":"https://download.example/payload.txt"}"#,
    ]);

    let (url, name) = PikPakClient::new(&http, &store).download("file-1").unwrap();

    assert_eq!(url, "https://download.example/payload.txt");
    assert_eq!(name, "payload.txt");
    let requests = &http.0.lock().unwrap().1;
    assert!(requests[0]
        .url
        .ends_with("/drive/v1/files/file-1?usage=CACHE"));
    assert!(requests[1]
        .url
        .ends_with("/drive/v1/files/file-1?usage=FETCH"));
}

#[test]
fn pikpak_rename_refreshes_the_collection_captcha_action_before_retrying_file_patch() {
    // Catches the same per-file action bug for mutations; otherwise a fresh
    // action token obtained while downloading is rejected by a later rename.
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"old-cap","client_id":"YNxT9w7GMdWvEOKa","user_id":"user-42"}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"error":"riskLimited","error_code":9}"#,
        r#"{"captcha_token":"fresh-cap","expires_in":300}"#,
        r#"{}"#,
    ]);

    PikPakClient::new(&http, &store)
        .rename("file-1", "renamed.txt")
        .unwrap();

    let requests = &http.0.lock().unwrap().1;
    let captcha: serde_json::Value =
        serde_json::from_str(requests[1].body.as_deref().unwrap()).unwrap();
    assert_eq!(captcha["action"], "PATCH:/drive/v1/files/");
    assert_eq!(requests[2].headers["X-Captcha-Token"], "fresh-cap");
}

impl Sequence {
    fn new(bodies: &[&str]) -> Self {
        Self(Mutex::new((
            bodies
                .iter()
                .map(|b| CloudHttpResponse {
                    status: 200,
                    body: (*b).into(),
                    ..Default::default()
                })
                .rev()
                .collect(),
            Vec::new(),
        )))
    }
}
impl CloudHttpTransport for Sequence {
    fn send(&self, r: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let mut s = self.0.lock().unwrap();
        s.1.push(r);
        Ok(s.0.pop().expect("unexpected network request"))
    }
}

#[test]
fn pikpak_password_signin_persists_tokens_but_never_password() {
    let http = Sequence::new(&[
        r#"{"captcha_token":"cap"}"#,
        r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600,"sub":"42"}"#,
    ]);
    let store = MemoryCredentialStore::default();
    PikPakClient::new(&http, &store)
        .login("me@example.com", "ephemeral-password")
        .unwrap();
    let session = String::from_utf8(store.load("pikpak").unwrap().unwrap()).unwrap();
    assert!(session.contains("refresh"));
    assert!(!session.contains("ephemeral-password"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&session).unwrap()["auth_method"],
        "password"
    );
    let s = http.0.lock().unwrap();
    assert!(s.1[0].url.ends_with("/v1/shield/captcha/init"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(s.1[0].body.as_ref().unwrap()).unwrap()["action"],
        "POST:https://user.mypikpak.com/v1/auth/signin"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(s.1[1].body.as_ref().unwrap()).unwrap()
            ["captcha_token"],
        "cap"
    );
}

#[test]
fn pikpak_interactive_captcha_does_not_attempt_password_signin() {
    let http = Sequence::new(&[
        r#"{"url":"https://captcha.mypikpak.com/challenge","captcha_token":"cap"}"#,
    ]);
    let store = MemoryCredentialStore::default();
    assert!(matches!(
        PikPakClient::new(&http, &store).login("me@example.com", "password"),
        Err(QuarkError::WebAuthorizationRequired)
    ));
    assert_eq!(http.0.lock().unwrap().1.len(), 1);
}

#[test]
fn pikpak_lists_all_pages_and_rotates_expired_refresh_token() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"old","refresh_token":"old-refresh","expires_at":0,"device_id":"device","captcha_token":""}"#).unwrap();
    let http = Sequence::new(&[
        r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#,
        r#"{"files":[{"id":"one","name":"a","kind":"drive#folder","parent_id":""}],"next_page_token":"next"}"#,
        r#"{"files":[{"id":"two","name":"b","kind":"drive#file","size":"12","parent_id":""}],"next_page_token":""}"#,
    ]);
    let directory = PikPakClient::new(&http, &store).list("0").unwrap();
    assert_eq!(directory.items.len(), 3);
    assert_eq!(directory.items[1].size, Some(12));
    assert!(String::from_utf8(store.load("pikpak").unwrap().unwrap())
        .unwrap()
        .contains("rotated"));
    assert!(http.0.lock().unwrap().1[2].url.contains("page_token=next"));
}

#[test]
fn cloud_enqueue_returns_real_task_and_records_engine_failure() {
    use versadown_lib::{cloud::commands::enqueue_cloud_download, TaskStatus, TaskStore};
    let store = TaskStore::open_in_memory().unwrap();
    let task = enqueue_cloud_download(
        &store,
        "cloud://quark/real-file",
        "video.mp4",
        std::path::Path::new("D:/Downloads"),
        |source| {
            assert_eq!(source, "cloud://quark/real-file");
            Ok("0123456789abcdef".into())
        },
    )
    .unwrap();
    assert_eq!(task.source, "cloud://quark/real-file");
    assert_eq!(task.status, TaskStatus::Downloading);
    assert_eq!(task.aria_gid.as_deref(), Some("0123456789abcdef"));
    assert!(enqueue_cloud_download(
        &store,
        "https://example.com/fail",
        "bad",
        std::path::Path::new("D:/Downloads"),
        |_| Err("engine offline".into())
    )
    .is_err());
    assert_eq!(store.list().unwrap()[0].status, TaskStatus::Failed);
}

#[test]
fn web_tokens_are_required_and_password_fields_are_discarded() {
    use versadown_lib::cloud::commands::parse_web_tokens;
    let s = parse_web_tokens(
        r#"{"access_token":"access","refresh_token":"refresh","password":"must-not-persist"}"#,
    )
    .unwrap();
    assert!(!serde_json::to_string(&s)
        .unwrap()
        .contains("must-not-persist"));
    assert_eq!(serde_json::to_value(&s).unwrap()["auth_method"], "web");
    assert!(parse_web_tokens(r#"{"access_token":"access"}"#).is_err());
    assert_eq!(
        serde_json::to_value(parse_web_tokens("null").err().unwrap()).unwrap()["code"],
        "authorization_pending"
    );
}

#[test]
fn web_refresh_keeps_public_web_client_instead_of_android_client() {
    use versadown_lib::cloud::commands::parse_web_tokens;
    let s = parse_web_tokens(
        r#"{"access_token":"web-access","refresh_token":"web-refresh","client_id":"web-client"}"#,
    )
    .unwrap();
    assert_eq!(serde_json::to_value(s).unwrap()["client_id"], "web-client");
}

#[test]
fn anonymous_web_cookie_keeps_authorization_pending() {
    use versadown_lib::cloud::commands::authorization_result;
    let http = Sequence::new(&[r#"{"status":401,"code":31001}"#]);
    let error = authorization_result(
        versadown_lib::cloud::QuarkClient::new("anonymous=1".into(), &http).account(),
    )
    .err()
    .unwrap();
    assert_eq!(
        serde_json::to_value(error).unwrap()["code"],
        "authorization_pending"
    );
}

#[test]
fn pikpak_copy_uses_the_native_batch_copy_endpoint_and_destination() {
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let http = Sequence::new(&[r#"{}"#]);
    let ids = vec!["file-1".to_owned(), "folder-2".to_owned()];

    PikPakClient::new(&http, &store)
        .copy_files(&ids, "target-folder")
        .unwrap();

    let requests = &http.0.lock().unwrap().1;
    assert!(requests[0].url.ends_with("/drive/v1/files:batchCopy"));
    let body: serde_json::Value =
        serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["ids"], serde_json::json!(["file-1", "folder-2"]));
    assert_eq!(body["to"]["parent_id"], "target-folder");
}

#[test]
fn cloud_module_is_available_without_a_second_aria2_engine() {
    let provider = versadown_lib::cloud::CloudProvider::Quark;
    assert_eq!(provider.key(), "quark");
}
