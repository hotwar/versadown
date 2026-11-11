//! Native Xunlei cloud-drive protocol adapter.
//!
//! Interactive authorization intentionally stays in Xunlei's official web or
//! QR-code flow.  This module only consumes the resulting refresh token,
//! identity, device id, and short-lived security state; it never accepts or
//! stores an account password.

#[path = "xunlei_account_profile.rs"]
mod account_profile;
pub use account_profile::XunleiAccountProfile;

use super::{
    http::response_json,
    types::{download_source_missing_from_http_404, encoded, required},
    *,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    fmt,
    io::Read,
    path::Path,
};

#[cfg(test)]
#[path = "xunlei_diagnostics_tests.rs"]
mod diagnostics_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::share::{
        test_support::{entry, Http},
        *,
    };

    #[test]
    fn native_identity_markers_are_shared_by_drive_and_share_downloads() {
        let credentials = credentials();
        let session: XunleiSession =
            serde_json::from_slice(&credentials.load("xunlei").unwrap().unwrap()).unwrap();

        let markers = xunlei_native_identity_markers(&session);

        assert!(markers.iter().any(|(name, value)| {
            name == crate::xunlei_native::ACCESS_TOKEN_MARKER_HEADER && value == "current-access"
        }));
        assert!(markers.iter().any(|(name, value)| {
            name == crate::xunlei_native::USER_ID_MARKER_HEADER && !value.is_empty()
        }));
        assert!(markers.iter().any(|(name, value)| {
            name == crate::xunlei_native::PEER_ID_MARKER_HEADER && !value.is_empty()
        }));
    }

    #[test]
    fn xunlei_share_http_challenge_without_json_preserves_authorization_error() {
        struct StatusHttp(u16);
        impl CloudHttpTransport for StatusHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                Ok(CloudHttpResponse {
                    status: self.0,
                    body: "<html>secret</html>".into(),
                    ..Default::default()
                })
            }
        }
        let credentials = credentials();
        for status in [401, 412] {
            let http = StatusHttp(status);
            let result = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http)
                .unwrap()
                .resolve(&selected());
            assert!(if status == 401 {
                matches!(result, Err(QuarkError::AuthorizationExpired))
            } else {
                matches!(result, Err(QuarkError::WebAuthorizationRequired))
            });
        }
    }
    fn credentials() -> MemoryCredentialStore {
        let store = MemoryCredentialStore::default();
        let mut session = XunleiSession::with_access(
            "current-access",
            "refresh",
            XunleiClientProfile {
                client_id: WEB_CLIENT_ID.into(),
                client_version: WEB_VERSION_CODE.into(),
                package_name: WEB_PACKAGE.into(),
                device_id: "actual-device".into(),
                user_agent: "official-browser-ua".into(),
                ..Default::default()
            },
        );
        session.user_id = "user".into();
        session.web_security.captcha_token = "personal-captcha".into();
        store
            .save("xunlei", &serde_json::to_vec(&session).unwrap())
            .unwrap();
        store
    }
    #[test]
    fn xunlei_capacity_reuses_valid_official_root_ticket_for_both_login_methods() {
        struct CapacityHttp {
            reject: bool,
        }
        impl CloudHttpTransport for CapacityHttp {
            fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                assert_eq!(request.method, "GET");
                assert_eq!(request.url, "https://api-pan.xunlei.com/drive/v1/about");
                assert_eq!(request.headers["X-Captcha-Token"], "official-root-ticket");
                Ok(CloudHttpResponse {
                    status: if self.reject { 412 } else { 200 },
                    body: if self.reject {
                        r#"{"error":"captcha_required"}"#
                    } else {
                        r#"{"quota":{"limit":"4096","usage":"1024"}}"#
                    }
                    .into(),
                    ..Default::default()
                })
            }
        }
        for method in [CloudAuthMethod::Web, CloudAuthMethod::Password] {
            for supervised in [false, true] {
                for reject in [false, true] {
                    let mut session: XunleiSession =
                        serde_json::from_slice(&credentials().load("xunlei").unwrap().unwrap())
                            .unwrap();
                    session.auth_method = method;
                    session.profile.client_version = "1.92.101".into();
                    session.expires_at = now() + 3600;
                    session.web_security = XunleiWebSecurity {
                        captcha_token: "official-root-ticket".into(),
                        expires_at: now() + 600,
                        action: "GET:/drive/v1/files".into(),
                        ..Default::default()
                    };
                    let http = CapacityHttp { reject };
                    let client = XunleiClient::new(session, &http);
                    let client = if supervised {
                        client.supervised()
                    } else {
                        client
                    };
                    let result = client.storage_capacity();
                    if reject {
                        if supervised {
                            assert!(matches!(result, Err(QuarkError::WebAuthorizationRequired)));
                        } else {
                            assert!(matches!(
                                result,
                                Err(QuarkError::XunleiVerificationUnavailable("metadata_missing"))
                            ));
                        }
                    } else {
                        assert_eq!(
                            result.unwrap(),
                            CloudStorageCapacity::Limited {
                                total_bytes: 4096,
                                used_bytes: 1024,
                            }
                        );
                    }
                    assert_eq!(client.session().web_security.action, "GET:/drive/v1/files");
                }
            }
        }
    }
    #[test]
    fn xunlei_download_metadata_reuses_valid_root_ticket_without_private_signer() {
        struct DownloadHttp;
        impl CloudHttpTransport for DownloadHttp {
            fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                assert_eq!(request.method, "GET");
                assert_eq!(
                    request.url,
                    "https://api-pan.xunlei.com/drive/v1/files/file-1"
                );
                assert_eq!(request.headers["X-Captcha-Token"], "official-root-ticket");
                Ok(CloudHttpResponse {status:200,body:r#"{"id":"file-1","name":"test.zip","web_content_link":"https://cdn.example/test.zip"}"#.into(),..Default::default()})
            }
        }
        let mut session: XunleiSession =
            serde_json::from_slice(&credentials().load("xunlei").unwrap().unwrap()).unwrap();
        session.profile.client_version = "1.92.101".into();
        session.web_security = XunleiWebSecurity {
            captcha_token: "official-root-ticket".into(),
            expires_at: now() + 600,
            action: "GET:/drive/v1/files".into(),
            ..Default::default()
        };
        let client = XunleiClient::new(session, &DownloadHttp).supervised();
        assert_eq!(
            client.download("file-1").unwrap(),
            ("https://cdn.example/test.zip".into(), "test.zip".into())
        );
    }

    #[test]
    fn xunlei_download_marks_a_missing_file_response_explicitly() {
        struct DownloadHttp;
        impl CloudHttpTransport for DownloadHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                Ok(CloudHttpResponse {
                    status: 404,
                    body: r#"{"error":"not_found"}"#.into(),
                    ..Default::default()
                })
            }
        }
        let mut session: XunleiSession =
            serde_json::from_slice(&credentials().load("xunlei").unwrap().unwrap()).unwrap();
        session.profile.client_version = "1.92.101".into();
        session.web_security = XunleiWebSecurity {
            captcha_token: "official-root-ticket".into(),
            expires_at: now() + 600,
            action: "GET:/drive/v1/files".into(),
            ..Default::default()
        };

        assert!(matches!(
            XunleiClient::new(session, &DownloadHttp)
                .supervised()
                .download("missing-file"),
            Err(QuarkError::SourceFileMissing)
        ));
    }

    #[test]
    fn xunlei_download_hash_is_gcid_only_when_cid_is_absent() {
        struct DownloadHttp;
        impl CloudHttpTransport for DownloadHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                Ok(CloudHttpResponse {
                    status: 200,
                    body: json!({
                        "id": "file-1",
                        "name": "test.zip",
                        "web_content_link": "https://cdn.example/test.zip",
                        "hash": "file-gcid"
                    })
                    .to_string(),
                    ..Default::default()
                })
            }
        }
        let mut session: XunleiSession =
            serde_json::from_slice(&credentials().load("xunlei").unwrap().unwrap()).unwrap();
        session.web_security = XunleiWebSecurity {
            captcha_token: "official-root-ticket".into(),
            expires_at: now() + 600,
            action: "GET:/drive/v1/files".into(),
            ..Default::default()
        };
        let download = XunleiClient::new(session, &DownloadHttp)
            .download_with_acceleration("file-1")
            .unwrap();

        assert_eq!(download.cid, None);
        assert_eq!(download.gcid.as_deref(), Some("file-gcid"));
    }
    #[test]
    fn current_modern_web_bearer_without_expiry_is_probed_before_refresh() {
        struct RefusingHttp;
        impl CloudHttpTransport for RefusingHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                Err(QuarkError::Transport("refresh should not run".into()))
            }
        }
        let session = XunleiSession::from_web_authorization(
            "current-access",
            "current-refresh",
            "current-user",
            "",
            XunleiClientProfile {
                client_id: WEB_CLIENT_ID.into(),
                client_version: "1.92.101".into(),
                package_name: WEB_PACKAGE.into(),
                device_id: "official-device".into(),
                user_agent: "official-browser-ua".into(),
                ..Default::default()
            },
            0,
        )
        .unwrap();
        let client = XunleiClient::new(session, &RefusingHttp);

        assert_eq!(
            client.ready_session().unwrap().access_token,
            "current-access"
        );
    }
    #[test]
    fn supervised_recorded_xunlei_download_defers_expired_session_refresh_to_supervisor() {
        let credentials = credentials();
        let mut session: XunleiSession =
            serde_json::from_slice(&credentials.load("xunlei").unwrap().unwrap()).unwrap();
        session.expires_at = 1;
        credentials
            .save("xunlei", &serde_json::to_vec(&session).unwrap())
            .unwrap();
        let snapshot =
            auth_supervisor::SessionSnapshot::new(&credentials, CloudProvider::Xunlei).unwrap();
        let http = Http::new(vec![
            cap(),
            json!({"error_code":16,"error":"unauthenticated"}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &snapshot, &http).unwrap();
        let result = adapter.refresh_recorded_transfer(&selected(), "recorded-file");
        assert!(matches!(result, Err(QuarkError::AuthorizationExpired)));
        assert!(
            http.requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !request.url.ends_with("/v1/auth/token")),
            "snapshot must not independently refresh credentials"
        );
    }
    fn selected() -> CloudShareResolvedEntry {
        entry(CloudSharePlan::new(
            CloudProvider::Xunlei,
            "share123".into(),
            Some("a123".into()),
            vec![CloudShareEntry::new(
                "share123",
                "f1".into(),
                "file.txt".into(),
                vec![],
                Some(7),
            )],
            json!({"pass_code_token":"old-pass"}),
        ))
    }
    #[test]
    fn xunlei_share_transfer_capacity_uses_drive_about_quota() {
        let credentials = credentials();
        let http = Http::new(vec![
            json!({"captcha_token":"read-token","expires_in":300}),
            json!({"quota":{"limit":"4096","usage":1024}}),
        ]);
        let adapter = XunleiShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        assert_eq!(
            adapter.transfer_capacity(&selected()).unwrap(),
            CloudTransferCapacity::Limited {
                total_bytes: 4096,
                used_bytes: 1024
            }
        );
        let requests = http.requests.lock().unwrap();
        assert!(requests
            .iter()
            .any(|request| request.url.ends_with("/drive/v1/about")));
        assert!(!requests
            .iter()
            .any(|request| request.url.contains("/share/restore")));
    }

    struct CapacityStatusHttp(std::sync::atomic::AtomicUsize);
    impl CapacityStatusHttp {
        fn new() -> Self {
            Self(std::sync::atomic::AtomicUsize::new(0))
        }
    }
    impl CloudHttpTransport for CapacityStatusHttp {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            let index = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if index == 0 {
                CloudHttpResponse {
                    status: 200,
                    body: json!({"captcha_token":"read-token","expires_in":300}).to_string(),
                    ..Default::default()
                }
            } else {
                CloudHttpResponse {
                    status: 412,
                    body: "{}".into(),
                    ..Default::default()
                }
            })
        }
    }

    #[test]
    fn xunlei_share_transfer_capacity_preserves_web_verification() {
        let stored = credentials();
        let http = CapacityStatusHttp::new();
        assert!(matches!(
            XunleiShareAdapter {
                credentials: &stored,
                http: &http
            }
            .transfer_capacity(&selected()),
            Err(QuarkError::WebAuthorizationRequired)
        ));
    }
    fn cap() -> Value {
        json!({"captcha_token":"share-captcha","expires_in":300})
    }
    fn pass() -> Value {
        json!({"share_status":"OK","pass_code_token":"fresh-pass"})
    }
    fn candidate() -> crate::parser::CloudShareCandidate {
        crate::parser::CloudShareCandidate {
            provider: CloudProvider::Xunlei,
            url: "https://pan.xunlei.com/s/share123".into(),
            access_code: Some("a123".into()),
        }
    }
    #[test]
    fn root_only_background_candidate_cannot_replace_operation_ready_session() {
        let mut existing: XunleiSession =
            serde_json::from_slice(&credentials().load("xunlei").unwrap().unwrap()).unwrap();
        existing.profile.client_version = "unsupported-test-version".into();
        existing.expires_at = now() + 3600;
        let root_only = existing.clone();
        existing.web_security.captcha_sign = "synthetic-signature".into();
        existing.web_security.client_version = existing.profile.client_version.clone();
        existing.web_security.package_name = existing.profile.package_name.clone();
        existing.web_security.timestamp = (now() * 1000).to_string();
        assert!(existing.would_downgrade_operation_verification(&root_only));
        assert!(!existing.would_downgrade_operation_verification(&existing));
        existing.expires_at = 1;
        assert!(!existing.would_downgrade_operation_verification(&root_only));
    }

    #[test]
    fn idle_directory_with_missing_signer_recovers_from_a_new_root_ticket() {
        let store = credentials();
        let mut session: XunleiSession =
            serde_json::from_slice(&store.load("xunlei").unwrap().unwrap()).unwrap();
        session.profile.client_version = "unsupported-test-version".into();
        session.expires_at = now() + 3600;
        session.web_security = XunleiWebSecurity {
            action: "GET:/drive/v1/files".into(),
            captcha_token: "expired-root".into(),
            expires_at: now().saturating_sub(1),
            ..Default::default()
        };
        store.save("xunlei", &serde_json::to_vec(&session).unwrap()).unwrap();
        let http = Http::new(vec![json!({"files":[]})]);
        auth_supervisor::AuthSupervisor::default().run(
            &store,
            CloudProvider::Xunlei,
            SessionWorkKind::Read,
            |snapshot| {
                ProviderRegistry::global().driver(CloudProvider::Xunlei, snapshot, &http)
                    .list_directory("0")
            },
            |phase| {
                assert!(phase == auth_supervisor::RecoveryPhase::AuthFailure);
                assert!(http.requests.lock().unwrap().is_empty());
                session.web_security.expires_at = now() + 120;
                session.web_security.captcha_token = "new-root".into();
                store.save("xunlei", &serde_json::to_vec(&session).unwrap())
            },
        ).unwrap();
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].headers["X-Captcha-Token"], "new-root");
    }

    #[test]
    fn expired_operation_verification_renews_only_from_profile_and_never_replays_mutation() {
        for kind in [SessionWorkKind::Read, SessionWorkKind::Mutation] {
            let store = credentials();
            let supervisor = auth_supervisor::AuthSupervisor::default();
            let calls = std::cell::Cell::new(0);
            let result = supervisor.run(
                &store,
                CloudProvider::Xunlei,
                kind,
                |_| {
                    calls.set(calls.get() + 1);
                    if calls.get() == 1 {
                        Err(QuarkError::XunleiVerificationUnavailable("metadata_expired"))
                    } else {
                        Ok(())
                    }
                },
                |phase| {
                    assert!(phase == auth_supervisor::RecoveryPhase::OperationVerification);
                    Ok(())
                },
            );
            if kind == SessionWorkKind::Read {
                assert!(result.is_ok());
                assert_eq!(calls.get(), 2);
            } else {
                assert!(matches!(result, Err(QuarkError::SessionRenewedRetryOperation)));
                assert_eq!(calls.get(), 1);
            }
        }
    }

    #[test]
    fn unsupported_share_profile_does_not_recover_or_replace_personal_session() {
        let store = credentials();
        let mut session: XunleiSession =
            serde_json::from_slice(&store.load("xunlei").unwrap().unwrap()).unwrap();
        session.profile.client_version = "unsupported-test-version".into();
        session.expires_at = now() + 3600;
        session.web_security.action = "GET:/drive/v1/files".into();
        session.web_security.expires_at = now() + 600;
        let original = serde_json::to_vec(&session).unwrap();
        store.save("xunlei", &original).unwrap();

        let http = Http::new(vec![json!({"files":[]})]);
        let supervisor = auth_supervisor::AuthSupervisor::default();
        let result = supervisor.run(
            &store,
            CloudProvider::Xunlei,
            SessionWorkKind::Read,
            |snapshot| {
                provider_share_adapter(CloudProvider::Xunlei, snapshot, &http)?
                    .inspect(&candidate())
            },
            |_| panic!("a local share compatibility error must not trigger account recovery"),
        );
        assert!(matches!(
            result,
            Err(QuarkError::XunleiVerificationUnavailable("xunlei_share_profile_unsupported"))
        ));
        assert!(http.requests.lock().unwrap().is_empty());
        assert_eq!(store.load("xunlei").unwrap().unwrap(), original);
        let client = XunleiClient::new(session, &http).supervised();
        client.directory("0").unwrap();
    }

    #[test]
    fn xunlei_share_direct_reuses_the_inspected_pass_token() {
        let credentials = credentials();
        let before = credentials.load("xunlei").unwrap();
        let http = Http::new(vec![
            cap(),
            json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file","web_content_link":"https://cdn.example/original","medias":[{"link":{"url":"https://cdn.example/transcode"}}]}}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();
        let ShareResolution::Direct(direct) = adapter.resolve(&selected()).unwrap() else {
            panic!()
        };
        assert_eq!(direct.url, "https://cdn.example/original");
        assert!(direct
            .headers
            .contains(&("User-Agent".into(), "official-browser-ua".into())));
        assert!(direct
            .headers
            .contains(&("Referer".into(), "https://pan.xunlei.com/".into())));
        assert!(!direct.headers.iter().any(|(key, _)| {
            matches!(
                key.as_str(),
                "Authorization" | "X-Captcha-Token" | "X-Device-Id"
            )
        }));
        let requests = http.requests.lock().unwrap();
        let body: Value = serde_json::from_str(requests[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["captcha_token"], "");
        assert_eq!(body["action"], "GET:/drive/v1/share");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].url.contains("/drive/v1/share/file_info?"));
        assert!(requests[1].url.contains("pass_code_token=old-pass"));
        assert!(!requests.iter().any(|r| r.url.contains("restore")));
        assert_eq!(credentials.load("xunlei").unwrap(), before);
    }

    #[test]
    fn xunlei_share_refreshes_an_expired_inspected_pass_token_once() {
        let credentials = credentials();
        let http = Http::new(vec![
            cap(),
            json!({"share_status":"PASS_CODE_ERROR"}),
            pass(),
            json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file","web_content_link":"https://cdn.example/file"}}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();

        let resolved = adapter.resolve_with_context(&selected()).unwrap();

        assert!(matches!(resolved.result, Ok(ShareResolution::Direct(_))));
        assert_eq!(
            resolved.updated_context.unwrap()["pass_code_token"],
            "fresh-pass"
        );
        assert_eq!(http.requests.lock().unwrap().len(), 4);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_xunlei_download_uses_only_the_accounts_active_download_vip_type() {
        for (account, active, expected) in [
            ("user", 1, Some("3")),
            ("user", 0, None),
            ("other", 1, None),
        ] {
            let credentials = credentials();
            let http = Http::new(vec![
                cap(),
                json!({"id":"f1","name":"file.txt","web_content_link":"https://cdn.example/f1",
                    "links":{"application/octet-stream":{"token":"file-acceleration"}}}),
                json!({"captcha_token":"account-ticket","expires_in":300}),
                json!({"sub": account, "vip_info": [
                    {"user_vas":306,"is_vip":1,"vas_type":5},
                    {"user_vas":"2","is_vip":active,"vas_type":"3"}]}),
            ]);
            let download = XunleiShareAdapter {
                credentials: &credentials,
                http: &http,
            }
            .personal_download("f1", "file.txt".into())
            .unwrap();
            let marker = download
                .headers
                .iter()
                .find(|(name, _)| name == crate::xunlei_native::VIP_TYPE_MARKER_HEADER);
            assert_eq!(marker.map(|(_, value)| value.as_str()), expected);
            assert!(http
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .url
                .ends_with("/v1/user/me"));
            let saved: XunleiSession =
                serde_json::from_slice(&credentials.load("xunlei").unwrap().unwrap()).unwrap();
            assert_eq!(saved.web_security.captcha_token, "account-ticket");
        }
    }

    #[test]
    fn xunlei_recorded_transfer_refreshes_personal_file_without_share_restore() {
        let credentials = credentials();
        let http = Http::new(vec![
            cap(),
            json!({
                "id": "personal-id",
                "name": "file.txt",
                "web_content_link": "https://cdn.example/personal"
            }),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();

        let refreshed = adapter
            .refresh_recorded_transfer(&selected(), "personal-id")
            .unwrap();

        let Ok(RecordedTransferDownload::Available(download)) = refreshed.result else {
            panic!("a personal direct link is required")
        };
        assert_eq!(download.url, "https://cdn.example/personal");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].url.ends_with("/files/personal-id"));
        assert!(!requests
            .iter()
            .any(|request| request.url.contains("share/restore")));
    }
    #[test]
    fn xunlei_recorded_transfer_waits_for_a_personal_download_link_without_leaking_api_headers() {
        let credentials = credentials();
        let http = Http::new(vec![
            cap(),
            json!({
                "id": "personal-id",
                "name": "file.txt",
                "phase": "PHASE_TYPE_COMPLETE",
                "web_content_link": ""
            }),
            json!({
                "id": "personal-id",
                "name": "file.txt",
                "phase": "PHASE_TYPE_COMPLETE",
                "download_url": "https://cdn.example/personal"
            }),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();

        let refreshed = adapter
            .refresh_recorded_transfer(&selected(), "personal-id")
            .unwrap();

        let Ok(RecordedTransferDownload::Available(download)) = refreshed.result else {
            panic!("a delayed personal direct link is required")
        };
        assert_eq!(download.url, "https://cdn.example/personal");
        assert!(download
            .headers
            .contains(&("User-Agent".into(), "official-browser-ua".into())));
        assert!(download
            .headers
            .contains(&("Referer".into(), "https://pan.xunlei.com/".into())));
        assert!(!download.headers.iter().any(|(key, _)| {
            matches!(
                key.as_str(),
                "Authorization" | "X-Captcha-Token" | "X-Device-Id"
            )
        }));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let captcha: Value = serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(captcha["action"], "GET:/drive/v1/files/personal-id");
        assert!(requests[1].url.ends_with("/files/personal-id"));
        assert!(requests[2].url.ends_with("/files/personal-id"));
        assert!(!requests
            .iter()
            .any(|request| request.url.contains("share/restore")));
    }
    #[test]
    fn xunlei_recorded_transfer_persists_the_refreshed_personal_download_session() {
        let credentials = credentials();
        let http = Http::new(vec![
            cap(),
            json!({
                "id": "personal-id",
                "name": "file.txt",
                "phase": "PHASE_TYPE_COMPLETE",
                "web_content_link": "https://cdn.example/personal"
            }),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();

        adapter
            .refresh_recorded_transfer(&selected(), "personal-id")
            .unwrap();

        let persisted: XunleiSession = serde_json::from_slice(
            &credentials
                .load("xunlei")
                .unwrap()
                .expect("saved Xunlei session"),
        )
        .unwrap();
        assert_eq!(persisted.web_security.captcha_token, "share-captcha");
        assert!(persisted.web_security.expires_at > now());
    }
    #[test]
    fn xunlei_personal_download_rejects_a_response_for_another_file() {
        let credentials = credentials();
        let session: XunleiSession = serde_json::from_slice(
            &credentials
                .load("xunlei")
                .unwrap()
                .expect("saved Xunlei session"),
        )
        .unwrap();
        let http = Http::new(vec![
            cap(),
            json!({
                "id": "other-file",
                "name": "file.txt",
                "phase": "PHASE_TYPE_COMPLETE",
                "web_content_link": "https://cdn.example/other"
            }),
        ]);

        assert!(matches!(
            XunleiClient::new(session, &http).download("personal-id"),
            Err(QuarkError::InvalidResponse)
        ));
    }
    #[test]
    fn xunlei_personal_share_uses_the_drive_share_management_protocol() {
        let store = credentials();
        let http = Http::new(vec![
            json!({"captcha_token":"share-token","expires_in":300}),
            json!({
                "share_id":"created",
                "share_url":"https://pan.xunlei.com/s/created",
                "pass_code":"p4ss"
            }),
        ]);
        let options = CloudShareOptions {
            name: "报告.pdf".into(),
            pass_code: None,
            generate_pass_code: true,
            expires_in_days: Some(7),
        };
        let share = ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &store, &http)
            .create_share(&["file-1".into()], &options)
            .unwrap();
        assert_eq!(share.pass_code.as_deref(), Some("p4ss"));

        let requests = http.requests.lock().unwrap();
        assert!(requests[0].url.ends_with("/v1/shield/captcha/init"));
        assert!(requests[1].url.ends_with("/drive/v1/share"));
        let created: Value = serde_json::from_str(requests[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(created["share_to"], "copy");
        assert!(created.get("pass_code_option").is_none());
        assert_eq!(created["title"], "报告.pdf");
        assert_eq!(created["restore_limit"], "-1");
        assert_eq!(created["expiration_days"], "7");
        assert_eq!(created["params"]["WithPassCodeInLink"], "true");
    }

    #[test]
    fn xunlei_cancel_shares_continues_past_failures_and_treats_missing_as_done() {
        struct ScriptedHttp(std::sync::Mutex<Vec<String>>);
        impl CloudHttpTransport for ScriptedHttp {
            fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                if !request.url.ends_with("/drive/v1/share/delete") {
                    return Ok(CloudHttpResponse {
                        status: 200,
                        body: json!({"captcha_token":"share-token","expires_in":300}).to_string(),
                        ..Default::default()
                    });
                }
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                let id = body["share_id"].as_str().unwrap().to_owned();
                self.0.lock().unwrap().push(id.clone());
                let (status, text) = match id.as_str() {
                    "gone" => (404, r#"{"error":"not_found"}"#),
                    "broken" => (500, r#"{"error":"internal"}"#),
                    _ => (200, "{}"),
                };
                Ok(CloudHttpResponse { status, body: text.into(), ..Default::default() })
            }
        }
        let store = credentials();
        let http = ScriptedHttp(std::sync::Mutex::new(Vec::new()));
        let ids: Vec<String> = ["a", "broken", "gone", "b"].map(String::from).to_vec();
        let outcome = ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &store, &http)
            .cancel_shares_detailed(&ids)
            .unwrap();
        assert_eq!(outcome.cancelled, ["a", "gone", "b"]);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].id, "broken");
        assert_eq!(outcome.failures[0].code, "provider_error");
        assert!(!outcome.failures[0].message.contains("500"));
        assert_eq!(*http.0.lock().unwrap(), ids);
    }

    #[test]
    fn xunlei_cancels_shares_with_the_web_management_contract() {
        let store = credentials();
        let http = Http::new(vec![
            json!({"captcha_token":"share-token","expires_in":300}),
            json!({}),
            json!({}),
        ]);
        ProviderRegistry::global().driver(CloudProvider::Xunlei, &store, &http)
            .cancel_shares(&["share-one".into(), "share-two".into()]).unwrap();
        let requests = http.requests.lock().unwrap();
        for (request, id) in requests[1..].iter().zip(["share-one", "share-two"]) {
            assert!(request.url.ends_with("/drive/v1/share/delete"));
            let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
            assert_eq!(body, json!({"space":"", "share_id":id}));
        }
        assert_eq!(requests.len(), 3);
    }
    #[test]
    fn xunlei_transfer_owner_uses_user_id_not_rotating_access_token() {
        let credentials = credentials();
        let http = Http::new(vec![]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();

        let before = adapter.transfer_owner_fingerprint(&selected()).unwrap();
        let mut renewed: XunleiSession =
            serde_json::from_slice(&credentials.load("xunlei").unwrap().unwrap()).unwrap();
        renewed.access_token = "renewed-access".into();
        credentials
            .save("xunlei", &serde_json::to_vec(&renewed).unwrap())
            .unwrap();
        let after = adapter.transfer_owner_fingerprint(&selected()).unwrap();

        assert_eq!(before, after);
    }
    #[test]
    fn xunlei_share_pages_recursively_and_exchanges_code() {
        let credentials = credentials();
        let http = Http::new(vec![
            cap(),
            pass(),
            json!({"share_status":"OK","files":[{"id":"d","kind":"drive#folder","name":"dir"}],"next_page_token":"p2"}),
            json!({"share_status":"OK","files":[],"next_page_token":""}),
            json!({"share_status":"OK","files":[{"id":"f1","kind":"drive#file","name":"file.txt","size":"7"}],"next_page_token":""}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        assert_eq!(plan.entries[0].relative_directory, vec!["dir"]);
        assert_eq!(plan.entries[0].size, Some(7));
        let requests = http.requests.lock().unwrap();
        assert!(requests[1].url.contains("pass_code=a123"));
        assert!(requests[3].url.contains("page_token=p2"));
        assert!(requests[4].url.contains("parent_id=d"));
    }
    #[test]
    fn xunlei_share_no_login_expiry_risk_and_malformed_file_never_transfer() {
        let empty = MemoryCredentialStore::default();
        let http = Http::new(vec![]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Xunlei, &empty, &http)
                .unwrap()
                .resolve(&selected()),
            Err(QuarkError::NotAuthorized)
        ));
        assert!(http.requests.lock().unwrap().is_empty());
        let credentials = credentials();
        for value in [
            json!({"error_code":4031,"error":"captcha_required"}),
            json!({"error_code":401}),
            json!({"share_status":"EXPIRED"}),
            json!({"error_code":"0"}),
            json!({"share_status":"OK","file_info":{"id":"other","kind":"drive#file"}}),
            json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file","web_content_link":42}}),
        ] {
            let http = Http::new(vec![cap(), value.clone()]);
            let result = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http)
                .unwrap()
                .resolve(&selected());
            assert!(result.is_err());
            if value["error_code"] == 4031 {
                assert!(matches!(result, Err(QuarkError::WebAuthorizationRequired)));
            }
            assert_eq!(http.requests.lock().unwrap().len(), 2);
        }
    }
    #[test]
    fn xunlei_share_no_direct_transfers_selected_file_and_records_exact_personal_id_before_download(
    ) {
        let credentials = credentials();
        let http = Http::new(vec![
            cap(),
            json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file","web_content_link":""}}),
            cap(),
            pass(),
            json!({"task_id":"task1"}),
            json!({"phase":"PHASE_TYPE_COMPLETE","params":{"trace_file_ids":"[\"personal-id\"]"}}),
            json!({"error_code":401}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();
        let (store, _) = crate::cloud::share::test_support::counted_plan_store();
        let entry = selected();
        store.upsert(&entry.plan).unwrap();
        let source = entry.plan.source_for("f1");
        assert!(resolve_current_share_download(&store, adapter.as_ref(), &source).is_err());
        assert_eq!(
            store.resolve_entry(&source).unwrap().plan.transfers[0].created_file_id,
            "personal-id"
        );
        let requests = http.requests.lock().unwrap();
        assert!(requests[4].url.ends_with("/share/restore"));
        let body: Value = serde_json::from_str(requests[4].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["file_ids"], json!(["f1"]));
        let captcha: Value = serde_json::from_str(requests[6].body.as_deref().unwrap()).unwrap();
        assert_eq!(captcha["action"], "GET:/drive/v1/files/personal-id");
        assert!(!source.contains("share123"));
    }
    #[test]
    fn xunlei_share_transfer_waits_for_a_personal_download_link_after_restore() {
        let credentials = credentials();
        let http = Http::new(vec![
            cap(),
            json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file","web_content_link":""}}),
            cap(),
            pass(),
            json!({"restore_status":"RESTORE_START","restore_task_id":"task1","file_id":"target-folder"}),
            json!({"phase":"PHASE_TYPE_COMPLETE","params":{"trace_file_ids":"{\"f1\":[\"personal-id\"]}"}}),
            cap(),
            json!({
                "id": "personal-id",
                "name": "file.txt",
                "phase": "PHASE_TYPE_COMPLETE",
                "web_content_link": ""
            }),
            json!({
                "id": "personal-id",
                "name": "file.txt",
                "phase": "PHASE_TYPE_COMPLETE",
                "web_content_link": "https://cdn.example/personal"
            }),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();
        let (store, _) = crate::cloud::share::test_support::counted_plan_store();
        let entry = selected();
        store.upsert(&entry.plan).unwrap();
        let source = entry.plan.source_for("f1");

        let download = resolve_current_share_download(&store, adapter.as_ref(), &source).unwrap();

        assert_eq!(download.url, "https://cdn.example/personal");
        assert!(!download.headers.iter().any(|(key, _)| {
            matches!(
                key.as_str(),
                "Authorization" | "X-Captcha-Token" | "X-Device-Id"
            )
        }));
        assert_eq!(
            store.resolve_entry(&source).unwrap().plan.transfers[0].created_file_id,
            "personal-id"
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 9);
        let captcha: Value = serde_json::from_str(requests[6].body.as_deref().unwrap()).unwrap();
        assert_eq!(captcha["action"], "GET:/drive/v1/files/personal-id");
        assert!(requests[7].url.ends_with("/files/personal-id"));
        assert!(requests[8].url.ends_with("/files/personal-id"));
    }
    #[test]
    fn xunlei_completed_restore_uses_only_the_selected_file_trace() {
        let credentials = credentials();
        let http = Http::new(vec![cap()]);
        let adapter = XunleiShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        let session = adapter.session().unwrap();
        let mut receipt = json!({"restore_status":"RESTORE_COMPLETE", "file_id":"target-folder"});
        for trace in [
            json!({"other":"unrelated"}),
            json!({"f1":["a", "b"]}),
            json!(["f1"]),
        ] {
            receipt["params"]["trace_file_ids"] = trace;
            assert!(matches!(
                session.restored_file_id(&receipt, "f1"),
                Err(QuarkError::ShareTransferDownloadUnavailable)
            ));
        }
        receipt["params"]["trace_file_ids"] = json!({"f1":"personal-id"});
        assert_eq!(
            session.restored_file_id(&receipt, "f1").unwrap(),
            "personal-id"
        );
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }
    #[test]
    fn xunlei_share_transfer_rechecks_login_after_successful_no_direct() {
        let credentials = credentials();
        let http = Http::new(vec![
            cap(),
            json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file"}}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Xunlei, &credentials, &http).unwrap();
        let entry = selected();
        assert!(matches!(
            adapter.resolve(&entry),
            Ok(ShareResolution::TransferRequired(_))
        ));
        let (store, _) = crate::cloud::share::test_support::counted_plan_store();
        store.upsert(&entry.plan).unwrap();
        credentials.remove("xunlei").unwrap();
        assert!(matches!(
            adapter.transfer(&entry, &store),
            Err(QuarkError::NotAuthorized)
        ));
        assert!(store
            .resolve_entry(&entry.plan.source_for("f1"))
            .unwrap()
            .plan
            .transfers
            .is_empty());
        assert_eq!(http.requests.lock().unwrap().len(), 2);
    }
    #[test]
    fn xunlei_share_access_code_challenge_and_expired_session_fail_closed() {
        let credentials = credentials();
        let mut candidate = candidate();
        candidate.access_code = None;
        let http = Http::new(vec![cap(), json!({"share_status":"PASS_CODE_EMPTY"})]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Xunlei, &credentials, &http)
                .unwrap()
                .inspect(&candidate),
            Ok(ShareInspection::AccessCodeRequired)
        ));
        candidate.access_code = Some("wrong".into());
        let http = Http::new(vec![cap(), json!({"share_status":"PASS_CODE_ERROR"})]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Xunlei, &credentials, &http)
                .unwrap()
                .inspect(&candidate),
            Err(QuarkError::InvalidInput(_))
        ));
        let http = Http::new(vec![
            json!({"url":"https://verify.example/secret","captcha_token":"token"}),
        ]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Xunlei, &credentials, &http)
                .unwrap()
                .resolve(&selected()),
            Err(QuarkError::WebAuthorizationRequired)
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 1);
        let mut session: XunleiSession =
            serde_json::from_slice(&credentials.load("xunlei").unwrap().unwrap()).unwrap();
        session.expires_at = 0;
        credentials
            .save("xunlei", &serde_json::to_vec(&session).unwrap())
            .unwrap();
        let http = Http::new(vec![]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Xunlei, &credentials, &http)
                .unwrap()
                .resolve(&selected()),
            Err(QuarkError::AuthorizationExpired)
        ));
        assert!(http.requests.lock().unwrap().is_empty());
    }
}

use super::share::{
    CloudShareAdapter, CloudShareEntry, CloudSharePlan, CloudSharePlanStore,
    CloudShareResolvedEntry, CloudTransferCapacity, RecordedTransferDownload,
    RecordedTransferResolver, ShareInspect, ShareInspection, ShareOperationResult, ShareResolution,
    ShareResolve, ShareTransfer, TransferRequest,
};

pub struct XunleiShareAdapter<'a> {
    pub credentials: &'a dyn CredentialStore,
    pub http: &'a dyn CloudHttpTransport,
}
struct XunleiShareSession<'a> {
    http: &'a dyn CloudHttpTransport,
    headers: BTreeMap<String, String>,
    user_agent: String,
}

struct XunleiShareTransport<'a>(&'a dyn CloudHttpTransport);

impl CloudHttpTransport for XunleiShareTransport<'_> {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        self.0.send(request)
    }
}

fn xunlei_cdn_headers(user_agent: &str) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    if !user_agent.is_empty() {
        headers.push(("User-Agent".into(), user_agent.into()));
    }
    headers.push(("Referer".into(), "https://pan.xunlei.com/".into()));
    headers
}
impl XunleiShareAdapter<'_> {
    fn session(&self) -> Result<XunleiShareSession<'_>, QuarkError> {
        let bytes = self
            .credentials
            .load("xunlei")?
            .ok_or(QuarkError::NotAuthorized)?;
        let session: XunleiSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        if session.access_token.is_empty() {
            return Err(QuarkError::NotAuthorized);
        }
        if session.profile.device_id.is_empty()
            || session.profile.user_agent.is_empty()
            || session.user_id.is_empty()
        {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .to_string();
        let signature = session
            .profile
            .web_captcha_signature(&timestamp)
            .ok_or(QuarkError::XunleiVerificationUnavailable(
                "xunlei_share_profile_unsupported",
            ))?;
        if session.expires_at <= now() {
            return Err(QuarkError::AuthorizationExpired);
        }
        let mut client = XunleiShareSession {
            http: self.http,
            headers: BTreeMap::from([
                (
                    "Authorization".into(),
                    format!("Bearer {}", session.access_token),
                ),
                ("X-Client-Id".into(), session.profile.client_id.clone()),
                ("X-Device-Id".into(), session.profile.device_id.clone()),
                ("User-Agent".into(), session.profile.user_agent.clone()),
                ("Origin".into(), "https://pan.xunlei.com".into()),
                ("Referer".into(), "https://pan.xunlei.com/".into()),
            ]),
            user_agent: session.profile.user_agent.clone(),
        };
        // A fresh share action context never reuses or persists personal CAPTCHA state.
        let value=client.request("POST",format!("{AUTH}/v1/shield/captcha/init"),Some(json!({"client_id":session.profile.client_id,"device_id":session.profile.device_id,"captcha_token":"","action":"GET:/drive/v1/share","meta":{"package_name":session.profile.package_name,"client_version":session.profile.client_version,"captcha_sign":signature,"timestamp":timestamp,"user_id":session.user_id}})))?;
        client
            .headers
            .insert("X-Captcha-Token".into(), required(&value, "captcha_token")?);
        Ok(client)
    }

    fn personal_download(
        &self,
        file_id: &str,
        name: String,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        let bytes = self
            .credentials
            .load("xunlei")?
            .ok_or(QuarkError::NotAuthorized)?;
        let session: XunleiSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        let transport = XunleiShareTransport(self.http);
        let client = XunleiClient::new(session, &transport);
        let client = if self.credentials.automatic_session_recovery() {
            client
        } else {
            client.supervised()
        };
        let result = client.download_with_acceleration(file_id);
        #[cfg(target_os = "macos")]
        let membership = result
            .as_ref()
            .ok()
            .and_then(|download| client.native_membership_marker(download));
        let refreshed = serde_json::to_vec(&client.session()).map_err(|_| QuarkError::Storage)?;
        self.credentials.save("xunlei", &refreshed)?;
        let download = result?;
        let mut headers = xunlei_cdn_headers(&client.session().profile.user_agent);
        headers.extend(xunlei_native_identity_markers(&client.session()));
        #[cfg(target_os = "macos")]
        headers.extend(membership);
        if let Some(token) = download.acceleration_token {
            headers.push((
                crate::xunlei_native::ACCELERATION_TOKEN_MARKER_HEADER.into(),
                token,
            ));
        }
        if let Some(size) = download.size {
            headers.push((
                crate::xunlei_native::CONTENT_LENGTH_MARKER_HEADER.into(),
                size.to_string(),
            ));
        }
        if let Some(cid) = download.cid {
            headers.push((crate::xunlei_native::CID_MARKER_HEADER.into(), cid));
        }
        if let Some(gcid) = download.gcid {
            headers.push((crate::xunlei_native::GCID_MARKER_HEADER.into(), gcid));
        }
        Ok(super::commands::ResolvedCloudDownload {
            url: download.url,
            name,
            headers,
        })
    }
}
impl XunleiShareSession<'_> {
    fn request(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let response = self.http.send(CloudHttpRequest {
            method,
            url,
            headers: self.headers.clone(),
            body: body.map(|v| v.to_string()),
        })?;
        if response.status == 401 {
            return Err(QuarkError::AuthorizationExpired);
        }
        if response.status == 412 {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let value: Value =
            serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
        if !value.is_object() {
            return Err(QuarkError::InvalidResponse);
        }
        for key in ["code", "error_code", "status"] {
            if value.get(key).is_some_and(|v| v.as_i64().is_none()) {
                return Err(QuarkError::InvalidResponse);
            }
        }
        if let (Some(a), Some(b)) = (value.get("code"), value.get("error_code")) {
            if a != b {
                return Err(QuarkError::InvalidResponse);
            }
        }
        if value.get("error").is_some_and(|v| v.as_str().is_none()) {
            return Err(QuarkError::InvalidResponse);
        }
        let challenge = ["url", "captcha_url", "verification_url"]
            .iter()
            .any(|key| value.get(key).is_some_and(|v| v.as_str() != Some("")));
        let message = ["error", "error_description", "message"]
            .iter()
            .filter_map(|key| value[*key].as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if challenge
            || ["captcha", "risk", "verify", "verification", "安全验证"]
                .iter()
                .any(|key| message.contains(key))
        {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        response_json(&response).map_err(|e| match e {
            QuarkError::CaptchaRequired => QuarkError::WebAuthorizationRequired,
            QuarkError::Provider(_) => {
                QuarkError::Provider("迅雷分享请求失败，请检查有效期及权限".into())
            }
            error => error,
        })
    }
    fn restored_file_id(&self, receipt: &Value, source_id: &str) -> Result<String, QuarkError> {
        let submitted_id = xunlei_restore_trace_id(receipt, source_id);
        let complete = receipt["restore_status"].as_str() == Some("RESTORE_COMPLETE");
        if complete {
            if let Some(id) = &submitted_id {
                return Ok(id.clone());
            }
        } else if receipt.get("restore_status").is_some()
            && receipt["restore_status"].as_str() != Some("RESTORE_START")
        {
            return Err(QuarkError::Provider("迅雷分享转存失败".into()));
        }

        let task_id = ["restore_task_id", "task_id"]
            .iter()
            .find_map(|key| receipt[*key].as_str().filter(|id| !id.is_empty()))
            .ok_or(if complete {
                QuarkError::ShareTransferDownloadUnavailable
            } else {
                QuarkError::InvalidResponse
            })?;
        for _ in 0..20 {
            let task = self.request(
                "GET",
                format!("{DRIVE}/drive/v1/tasks/{}", encoded(task_id)),
                None,
            )?;
            match task["phase"].as_str() {
                Some("PHASE_TYPE_COMPLETE") => {
                    return xunlei_restore_trace_id(&task, source_id)
                        .or(submitted_id)
                        .ok_or(QuarkError::ShareTransferDownloadUnavailable);
                }
                Some("PHASE_TYPE_PENDING" | "PHASE_TYPE_RUNNING") => {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                _ => return Err(QuarkError::Provider("迅雷分享转存失败".into())),
            }
        }
        Err(QuarkError::Provider("迅雷分享转存尚未完成".into()))
    }

    fn pass(&self, id: &str, code: Option<&str>) -> Result<Value, QuarkError> {
        self.request(
            "GET",
            format!(
                "{DRIVE}/drive/v1/share?share_id={}&pass_code={}&limit=100",
                encoded(id),
                encoded(code.unwrap_or(""))
            ),
            None,
        )
    }
}
// The restore receipt's top-level file_id may be the destination folder.
// Only use a trace for the one selected file when recording a cleanup target.
fn xunlei_restore_trace_id(response: &Value, source_id: &str) -> Option<String> {
    let trace = ["trace_file_ids", "restore_file_ids"]
        .iter()
        .find_map(|key| response["params"].get(*key).or_else(|| response.get(*key)))?;
    let parsed;
    let trace = if let Some(encoded) = trace.as_str() {
        parsed = serde_json::from_str::<Value>(encoded).ok()?;
        &parsed
    } else {
        trace
    };
    let target = if trace.is_object() {
        trace.get(source_id)?
    } else {
        trace
    };
    let id = match target {
        Value::Array(ids) if ids.len() == 1 => ids[0].as_str()?,
        Value::String(id) if trace.is_object() => id,
        _ => return None,
    };
    (!id.trim().is_empty() && id != source_id && id != "0").then(|| id.to_owned())
}

fn share_already_gone(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["http 404", "not_found", "not_exist", "not exist"]
        .iter()
        .any(|marker| message.contains(marker))
}
fn xunlei_share_status(value: &Value) -> Result<(), QuarkError> {
    match value["share_status"].as_str() {
        Some("OK") => Ok(()),
        Some("PASS_CODE_EMPTY" | "PASS_CODE_ERROR") => {
            Err(QuarkError::InvalidInput("迅雷分享访问码错误或缺失".into()))
        }
        Some("SENSITIVE_RESOURCE" | "RISK") => Err(QuarkError::WebAuthorizationRequired),
        Some(_) => Err(QuarkError::Provider("迅雷分享已失效或无权访问".into())),
        None => Err(QuarkError::InvalidResponse),
    }
}
fn xunlei_share_pass_token(value: &Value, code: Option<&str>) -> Result<String, QuarkError> {
    xunlei_share_status(value)?;
    let token = value
        .get("pass_code_token")
        .map(|v| v.as_str().ok_or(QuarkError::InvalidResponse))
        .transpose()?
        .unwrap_or("");
    if code.is_some() && token.is_empty() {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(token.into())
}

fn stored_xunlei_share_pass_token(
    context: &Value,
    code: Option<&str>,
) -> Result<Option<String>, QuarkError> {
    let Some(value) = context.get("pass_code_token") else {
        return Ok(None);
    };
    let token = value.as_str().ok_or(QuarkError::InvalidResponse)?;
    if token.chars().any(char::is_control) {
        return Err(QuarkError::InvalidResponse);
    }
    if code.is_some() && token.is_empty() {
        return Ok(None);
    }
    Ok(Some(token.into()))
}

fn xunlei_share_token_needs_refresh(value: &Value) -> bool {
    value["share_status"].as_str() == Some("PASS_CODE_ERROR")
}

fn with_xunlei_share_pass_token(mut context: Value, token: &str) -> Value {
    if let Some(values) = context.as_object_mut() {
        values.insert("pass_code_token".into(), token.into());
        context
    } else {
        json!({"pass_code_token":token})
    }
}

fn xunlei_content_link(file: &Value) -> Result<Option<&str>, QuarkError> {
    for key in ["web_content_link", "download_url"] {
        match file.get(key) {
            Some(Value::String(link)) if !link.is_empty() => return Ok(Some(link)),
            None | Some(Value::Null | Value::String(_)) => {}
            Some(_) => return Err(QuarkError::InvalidResponse),
        }
    }
    Ok(None)
}

fn xunlei_acceleration_token(file: &Value) -> Result<Option<String>, QuarkError> {
    let Some(link) = file
        .get("links")
        .and_then(|links| links.get("application/octet-stream"))
    else {
        return Ok(None);
    };
    let Some(token) = link.get("token") else {
        return Ok(None);
    };
    token
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .map(Some)
        .ok_or(QuarkError::InvalidResponse)
}

pub(crate) fn xunlei_native_identity_markers(session: &XunleiSession) -> Vec<(String, String)> {
    let peer_id = if session.web_context.peer_id.is_empty() {
        if session.web_context.device_id.is_empty() {
            session.profile.device_id.clone()
        } else {
            session.web_context.device_id.clone()
        }
    } else {
        session.web_context.peer_id.clone()
    };
    vec![
        (
            crate::xunlei_native::ACCESS_TOKEN_MARKER_HEADER.into(),
            session.access_token.clone(),
        ),
        (
            crate::xunlei_native::USER_ID_MARKER_HEADER.into(),
            session.user_id.clone(),
        ),
        (crate::xunlei_native::PEER_ID_MARKER_HEADER.into(), peer_id),
    ]
}

impl ShareInspect for XunleiShareAdapter<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        let id = super::share::share_id(candidate, CloudProvider::Xunlei)?;
        let session = self.session()?;
        let pass = session.pass(&id, candidate.access_code.as_deref())?;
        if candidate.access_code.is_none()
            && matches!(
                pass["share_status"].as_str(),
                Some("PASS_CODE_EMPTY" | "PASS_CODE_ERROR")
            )
        {
            return Ok(ShareInspection::AccessCodeRequired);
        }
        let token = xunlei_share_pass_token(&pass, candidate.access_code.as_deref())?;
        let mut queue = std::collections::VecDeque::from([(String::new(), Vec::<String>::new())]);
        let mut seen = HashSet::new();
        let mut entries = vec![];
        let mut requests = 0;
        while let Some((parent, directory)) = queue.pop_front() {
            let mut page = String::new();
            let mut pages = HashSet::new();
            loop {
                requests += 1;
                if requests > 10_000 || directory.len() > 128 {
                    return Err(QuarkError::InvalidResponse);
                }
                let value=session.request("GET",format!("{DRIVE}/drive/v1/share/detail?share_id={}&parent_id={}&pass_code_token={}&page_token={}&limit=100",encoded(&id),encoded(&parent),encoded(&token),encoded(&page)),None)?;
                xunlei_share_status(&value)?;
                for raw in value["files"]
                    .as_array()
                    .ok_or(QuarkError::InvalidResponse)?
                {
                    let fid = required(raw, "id")?;
                    let name = required(raw, "name")?;
                    if !seen.insert(fid.clone()) {
                        return Err(QuarkError::InvalidResponse);
                    }
                    match raw["kind"].as_str() {
                        Some("drive#folder") => {
                            let mut path = directory.clone();
                            path.push(name);
                            queue.push_back((fid, path));
                        }
                        Some("drive#file") => entries.push(CloudShareEntry::new(
                            &id,
                            fid,
                            name,
                            directory.clone(),
                            raw["size"]
                                .as_u64()
                                .or_else(|| raw["size"].as_str().and_then(|s| s.parse().ok())),
                        )),
                        _ => return Err(QuarkError::InvalidResponse),
                    }
                }
                page = value
                    .get("next_page_token")
                    .map(|v| v.as_str().ok_or(QuarkError::InvalidResponse))
                    .transpose()?
                    .unwrap_or("")
                    .into();
                if page.is_empty() {
                    break;
                }
                if !pages.insert(page.clone()) {
                    return Err(QuarkError::InvalidResponse);
                }
            }
        }
        Ok(ShareInspection::Plan(CloudSharePlan::new(
            CloudProvider::Xunlei,
            id,
            candidate.access_code.clone(),
            entries,
            json!({"pass_code_token":token}),
        )))
    }
}
impl ShareResolve for XunleiShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        resolve_xunlei_share(self.credentials, self.http, entry)
    }
    fn resolve_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
        resolve_xunlei_share_with_context(self.credentials, self.http, entry)
    }
}
impl ShareTransfer for XunleiShareAdapter<'_> {
    fn transfer_capacity(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<CloudTransferCapacity, QuarkError> {
        let bytes = self
            .credentials
            .load("xunlei")?
            .ok_or(QuarkError::NotAuthorized)?;
        let session: XunleiSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        let transport = XunleiShareTransport(self.http);
        let client = XunleiClient::new(session, &transport);
        let client = if self.credentials.automatic_session_recovery() {
            client
        } else {
            client.supervised()
        };
        let result = client.storage_capacity();
        self.credentials.save(
            "xunlei",
            &serde_json::to_vec(&client.session()).map_err(|_| QuarkError::Storage)?,
        )?;
        result
    }

    fn transfer_owner_fingerprint(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        let bytes = self
            .credentials
            .load("xunlei")?
            .ok_or(QuarkError::NotAuthorized)?;
        let session: XunleiSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        if session.user_id.trim().is_empty() {
            Ok(None)
        } else {
            super::share::stable_owner_fingerprint(CloudProvider::Xunlei, &session.user_id)
                .map(Some)
        }
    }
    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        if entry.provider != CloudProvider::Xunlei || entry.plan.provider != CloudProvider::Xunlei {
            return Err(QuarkError::InvalidResponse);
        }
        let owner = self.transfer_owner_fingerprint(entry)?;
        // Re-check login immediately before the mutation; no refresh or CAPTCHA retry.
        let session = self.session()?;
        let token = xunlei_share_pass_token(
            &session.pass(&entry.plan.canonical_share_id, entry.access_code())?,
            entry.access_code(),
        )?;
        let receipt = session.request(
            "POST",
            format!("{DRIVE}/drive/v1/share/restore"),
            Some(json!({
                "share_id": entry.plan.canonical_share_id,
                "pass_code_token": token,
                "file_ids": [entry.entry.entry_id],
                "parent_id": "",
                "specify_parent_id": true,
                "ancestor_ids": [],
                "folder_type": "NORMAL"
            })),
        )?;
        let id = session.restored_file_id(&receipt, &entry.entry.entry_id)?;
        let source = entry.plan.source_for(&entry.entry.entry_id);
        if let Some(owner) = owner {
            store.record_transfer_with_owner(&source, &id, &owner)?;
        } else {
            store.record_transfer(&source, &id)?;
        }
        self.personal_download(&id, entry.entry.name.clone())
            .map_err(|error| match error {
                QuarkError::InvalidResponse | QuarkError::SourceFileMissing => {
                    QuarkError::ShareTransferDownloadUnavailable
                }
                other => other,
            })
    }
}
impl RecordedTransferResolver for XunleiShareAdapter<'_> {
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        if entry.provider != CloudProvider::Xunlei
            || entry.plan.provider != CloudProvider::Xunlei
            || created_file_id.is_empty()
        {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(ShareOperationResult {
            result: Ok(RecordedTransferDownload::Available(
                self.personal_download(created_file_id, entry.entry.name.clone())?,
            )),
            updated_context: None,
        })
    }
}
impl CloudShareAdapter for XunleiShareAdapter<'_> {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        Some(self)
    }
}
pub fn resolve_xunlei_share(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareResolution, QuarkError> {
    resolve_xunlei_share_with_context(credentials, http, entry)?.result
}

fn resolve_xunlei_share_with_context(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
    if entry.provider != CloudProvider::Xunlei || entry.plan.provider != CloudProvider::Xunlei {
        return Err(QuarkError::InvalidResponse);
    }
    let adapter = XunleiShareAdapter { credentials, http };
    let session = adapter.session()?;
    let mut context = entry.plan.provider_context.clone();
    let mut rotated_context = false;
    let mut token = match stored_xunlei_share_pass_token(&context, entry.access_code())? {
        Some(token) => token,
        None => {
            let token = xunlei_share_pass_token(
                &session.pass(&entry.plan.canonical_share_id, entry.access_code())?,
                entry.access_code(),
            )?;
            context = with_xunlei_share_pass_token(context, &token);
            rotated_context = true;
            token
        }
    };
    let mut value = session.request(
        "GET",
        format!(
            "{DRIVE}/drive/v1/share/file_info?share_id={}&file_id={}&pass_code_token={}",
            encoded(&entry.plan.canonical_share_id),
            encoded(&entry.entry.entry_id),
            encoded(&token)
        ),
        None,
    )?;
    if !rotated_context && xunlei_share_token_needs_refresh(&value) {
        token = xunlei_share_pass_token(
            &session.pass(&entry.plan.canonical_share_id, entry.access_code())?,
            entry.access_code(),
        )?;
        context = with_xunlei_share_pass_token(context, &token);
        rotated_context = true;
        value = session.request(
            "GET",
            format!(
                "{DRIVE}/drive/v1/share/file_info?share_id={}&file_id={}&pass_code_token={}",
                encoded(&entry.plan.canonical_share_id),
                encoded(&entry.entry.entry_id),
                encoded(&token)
            ),
            None,
        )?;
    }
    let result = (|| {
        xunlei_share_status(&value)?;
        let file = &value["file_info"];
        if file["id"].as_str() != Some(entry.entry.entry_id.as_str())
            || file["kind"] != "drive#file"
        {
            return Err(QuarkError::InvalidResponse);
        }
        let url = xunlei_content_link(file)?.unwrap_or("");
        if url.is_empty() {
            return Ok(ShareResolution::TransferRequired(TransferRequest {
                source: entry.plan.source_for(&entry.entry.entry_id),
            }));
        }
        super::share::validate_direct_url(url)?;
        Ok(ShareResolution::Direct(
            super::commands::ResolvedCloudDownload {
                url: url.into(),
                name: entry.entry.name.clone(),
                headers: xunlei_cdn_headers(&session.user_agent),
            },
        ))
    })();
    Ok(ShareOperationResult {
        result,
        updated_context: rotated_context.then_some(context),
    })
}

const AUTH: &str = "https://xluser-ssl.xunlei.com";
const DRIVE: &str = "https://api-pan.xunlei.com";
const WEB_VERSION_CODE: &str = "1.92.98";
const WEB_CLIENT_ID: &str = "Xqp0kJBXWhwaTpB6";
const WEB_PACKAGE: &str = "pan.xunlei.com";
// Verified 2026-09-10 against the official module1618 signer and module25's
// pub1iwjKbiXrwP9i86t19Ao0HtnZnkjCjZk chain, using independent JS vectors.
// Source: utils-async.413fd3b1f17c7388b322.js and
// utils-initial.38a2added9b0686c9067.js on static-pan.xunlei.com.
// This is the public `filters` algorithm, not the separate dynamic-salt branch.
const WEB_SIGNATURE_SALTS: [&str; 15] = [
    "rgui",
    "GBbHjL+W3+IJcgUgydd8zuV2hrhnID9yPI",
    "KRDkNzr29JqPNUq2mxw+YFtueKYZlzmWJIaHkj3NSh/ogclHVm",
    "JMGo+UYkPo6/I4s1enZg",
    "ZcfXzUZq+yx9VKk1fV4xAQhq+TwyE42MP7JXORDQy",
    "Z",
    "WCvfOrvOIFJWrKDen7WBq17dX",
    "3J07h/7",
    "Im3qdyHl9DZH",
    "Jg",
    "Y+KS",
    "/1lcxUF8",
    "C",
    "GVEo",
    "8k4v5dntCyPof87pZ0gc/EixX3qv5RxoaGzZ",
];
// Official pan.xunlei.com 1.93.6, utils-initial.f800c7326181a3f081fe.js:
// filters.pubmtQgYZVqqN7uXVBgJsabTHcrjkRqS9XC. The chain's final
// pubYvy... delegate returns its input unchanged; these are the ten MD5 rounds.
const WEB_SIGNATURE_SALTS_1936: [&str; 10] = [
    "hHx0aqfr+FQxmZZFd8uC",
    "yCv9LPWcueqAqyAf0",
    "4eXtCtkptUMWj0PHm+k6PlBQ8MLxub1yhPhhHYseH6JbG",
    "CUO7z+eyFzwGn6yDreEVZdOTWJdInHLbLgmIyfm9YuQ8Op",
    "NvvziNyP",
    "tPlbjA7+D5m+Ex4A5j/r3YFzrYvS4GVyZZdObvm6v96P2qZ13Yoj",
    "IW3GOz",
    "86Qw6R3FFacF",
    "/icv8Er",
    "ydvTBcED6CZWZ/JvIA0txHGGqplHXpDKf2Oy95",
];
// Official pan.xunlei.com 1.93.22, verified 2026-11-03 against
// utils-initial.f23af17a7de85831b2fa.js, module 47's
// pubcDq1NhpQpJdHLOWFKjCkWqT5G22jzTPW. These are its eleven MD5 rounds.
const WEB_SIGNATURE_SALTS_19322: [&str; 11] = [
    "MPU73ZbElcZF7JedNMcGuyTMntNnS/mU2",
    "lhQAQ0OOC24YAVLh1gypuQMq/L/Ptfpxl8",
    "JC",
    "Ec4WmtmExQdeUHcmad1IVqc",
    "WDvgJcYwX8bX2/IMstopPNhLR2dN7F",
    "kK5LVVja3fq1roF9NVBSu/Bh4dkr1wXfd2fzQ",
    "quG9",
    "IAIbdVy/QfeYL1jRWtH",
    "+0chaj/F3RkLKJQoo0Ntpc8EWx6",
    "MO4U9wvvDmigDsj3nl",
    "jmN3FbGRU9AuQbk33mAdimq",
];
// Official pan.xunlei.com 1.93.24, utils-initial.ddef3a9e719ae0793f69.js,
// module 30 (code-res), pubguaFIj6W50w0U7t6gIfxVSqP4VppjTgR.
const WEB_SIGNATURE_SALTS_19324: [&str; 12] = [
    "ozbRAVToaQ8Wq5DH7Jn",
    "D3iC1",
    "YqFjx0uc",
    "AVYZ2uMkJa3/OuVs85/M",
    "s690MneC4LYfrcu+",
    "Cth/UmT76igaOhRRmMHnLSvjSeTrvi2",
    "1DrjvWQh+l4fdKqD0/DBYbqJAeZES",
    "PY1Z9j2rzBOb4aTfdQf/",
    "XTBzwAlzlzNMqAbcGy5qqd8WnCZk2IP2ZVX",
    "2BnvqFgo9MODPk8pJK97",
    "zWH1xTadmtNTwNq",
    "WDZ5DOCCBaLSjhtSniGaaGerOs+xx56d",
];
/// Non-secret identity values captured from the official browser/QR flow.
/// They are retained because Xunlei binds refresh and drive calls to that
/// profile; callers must never fabricate a replacement profile.
#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct XunleiClientProfile {
    pub client_id: String,
    /// Sensitive client credential captured by the official authorization flow.
    /// Task 7 persists this only in the encrypted session store.
    #[serde(default)]
    pub client_secret: String,
    pub device_id: String,
    pub client_version: String,
    pub package_name: String,
    pub user_agent: String,
}

/// Browser-only context selected from the official Xunlei web session. These
/// values remain inside the encrypted provider session and are never logged.
#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct XunleiWebContext {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub peer_id: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub credit_key: String,
}

impl XunleiWebContext {
    /// The official web bundle uses the `sessionid` cookie when it exists and
    /// otherwise uses the current browser bearer. This fallback is valid only
    /// for an otherwise complete, allowlisted browser context.
    pub(crate) fn use_access_token_for_missing_session_id(&mut self, access_token: &str) {
        if self.session_id.is_empty()
            && (!self.peer_id.is_empty() || !self.device_id.is_empty())
            && !self.credit_key.is_empty()
            && !access_token.is_empty()
        {
            self.session_id = access_token.into();
        }
    }

    fn is_complete(&self) -> bool {
        !self.session_id.is_empty()
            && (!self.peer_id.is_empty() || !self.device_id.is_empty())
            && !self.credit_key.is_empty()
    }
}

impl fmt::Debug for XunleiWebContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XunleiWebContext")
            .field("session_id", &"<redacted>")
            .field("peer_id", &"<redacted>")
            .field("device_id", &"<redacted>")
            .field("credit_key", &"<redacted>")
            .finish()
    }
}

impl XunleiClientProfile {
    pub fn is_modern_web(&self) -> bool {
        self.client_id == WEB_CLIENT_ID && self.package_name == WEB_PACKAGE
    }

    /// Returns the version-pinned official webpage signature, when supported.
    /// Unknown versions require the official bridge's fresh signing metadata.
    pub fn web_captcha_signature(&self, timestamp: &str) -> Option<String> {
        if !self.supports_web_signature()
            || timestamp.is_empty()
            || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Some(self.web_captcha_signature_with_known_salts(timestamp))
    }

    fn web_captcha_signature_with_known_salts(&self, timestamp: &str) -> String {
        let mut current = format!(
            "{}{}{}{}{}",
            self.client_id, self.client_version, WEB_PACKAGE, self.device_id, timestamp
        );
        let salts: &[&str] = match self.client_version.as_str() {
            "1.93.24" => &WEB_SIGNATURE_SALTS_19324,
            "1.93.22" => &WEB_SIGNATURE_SALTS_19322,
            "1.93.6" => &WEB_SIGNATURE_SALTS_1936,
            _ => &WEB_SIGNATURE_SALTS,
        };
        for salt in salts {
            current.push_str(salt);
            current = format!("{:x}", md5::compute(current.as_bytes()));
        }
        format!("1.{current}")
    }

    #[cfg(test)]
    pub(crate) fn captured_web_signature_matches_known_salts(
        &self,
        timestamp: &str,
        signature: &str,
    ) -> bool {
        !timestamp.is_empty()
            && timestamp.bytes().all(|byte| byte.is_ascii_digit())
            && signature == self.web_captcha_signature_with_known_salts(timestamp)
    }

    fn supports_web_signature(&self) -> bool {
        self.is_modern_web()
            && matches!(self.client_version.as_str(), WEB_VERSION_CODE | "1.93.6" | "1.93.22" | "1.93.24")
            && !self.device_id.is_empty()
    }
    /// Headers accepted by the drive APIs.  The official browser does not
    /// expose a client secret, but it does supply these identity fields with a
    /// current bearer token.
    pub(crate) fn is_drive_complete(&self) -> bool {
        !self.client_id.is_empty()
            && !self.device_id.is_empty()
            && !self.client_version.is_empty()
            && !self.package_name.is_empty()
            && !self.user_agent.is_empty()
    }

    /// The public web OAuth client omits client_secret; native identities
    /// retain their original confidential-client refresh requirement.
    fn can_refresh(&self) -> bool {
        self.is_drive_complete() && (self.is_modern_web() || !self.client_secret.is_empty())
    }
}

impl fmt::Debug for XunleiClientProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XunleiClientProfile")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("client_version", &self.client_version)
            .field("package_name", &self.package_name)
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

/// Captured from the official captcha service for the same account/device.
/// Rust can renew signatures for the verified webpage version. Other versions
/// retain the bridge's signature with its original timestamp for bounded reuse.
#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct XunleiWebSecurity {
    pub captcha_token: String,
    /// Unix seconds; the bridge converts the official storage date to seconds.
    pub expires_at: u64,
    /// Empty for an official globally cached token, otherwise METHOD:/path.
    pub action: String,
    pub captcha_sign: String,
    pub timestamp: String,
    pub client_version: String,
    pub package_name: String,
    pub verification_url: String,
}

impl fmt::Debug for XunleiWebSecurity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XunleiWebSecurity")
            .field("captcha_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("action", &self.action)
            .field("captcha_sign", &"<redacted>")
            .field("timestamp", &self.timestamp)
            .field("verification_url", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
struct XunleiActionTicket {
    token: String,
    expires_at: u64,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct XunleiSession {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub profile: XunleiClientProfile,
    #[serde(default)]
    pub user_id: String,
    /// A security token issued during the official web/QR authorization flow.
    /// It is sent only to Xunlei and is never logged or placed in task sources.
    #[serde(default)]
    pub security_token: String,
    /// The narrow cookie allowlist required by the official web drive API.
    #[serde(default)]
    pub web_context: XunleiWebContext,
    #[serde(default)]
    pub web_security: XunleiWebSecurity,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    action_tickets: BTreeMap<String, XunleiActionTicket>,
    #[serde(default)]
    pub auth_method: CloudAuthMethod,
}

impl fmt::Debug for XunleiSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XunleiSession")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("profile", &self.profile)
            .field("user_id", &self.user_id)
            .field("security_token", &"<redacted>")
            .field("web_context", &self.web_context)
            .field("web_security", &self.web_security)
            .field("auth_method", &self.auth_method)
            .finish()
    }
}

impl XunleiSession {
    pub(crate) fn can_prepare_operation_verification(&self) -> bool {
        let at = now();
        let security = &self.web_security;
        self.profile.supports_web_signature()
            || (!security.captcha_sign.is_empty()
                && !security.client_version.is_empty()
                && security.package_name == self.profile.package_name
                && security.timestamp.parse::<u64>().is_ok_and(|millis| {
                    let signed_at = millis / 1000;
                    signed_at <= at.saturating_add(30)
                        && at.saturating_sub(signed_at) <= 300
                }))
    }

    /// Background validation only probes the root directory. Keep an
    /// unexpired session with operation verification when that probe yields
    /// a weaker candidate for the same account.
    pub(crate) fn would_downgrade_operation_verification(&self, candidate: &Self) -> bool {
        !self.user_id.is_empty()
            && self.user_id == candidate.user_id
            && self.expires_at > now()
            && self.can_prepare_operation_verification()
            && !candidate.can_prepare_operation_verification()
    }

    pub fn refresh(refresh_token: impl Into<String>, profile: XunleiClientProfile) -> Self {
        Self {
            access_token: String::new(),
            refresh_token: refresh_token.into(),
            expires_at: 0,
            profile,
            user_id: String::new(),
            security_token: String::new(),
            web_context: XunleiWebContext::default(),
            web_security: XunleiWebSecurity::default(),
            action_tickets: BTreeMap::new(),
            auth_method: CloudAuthMethod::Web,
        }
    }

    pub fn with_access(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        profile: XunleiClientProfile,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            expires_at: u64::MAX,
            profile,
            user_id: String::new(),
            security_token: String::new(),
            web_context: XunleiWebContext::default(),
            web_security: XunleiWebSecurity::default(),
            action_tickets: BTreeMap::new(),
            auth_method: CloudAuthMethod::Web,
        }
    }

    /// Creates a session from data returned by the official web/QR flow.
    pub fn from_web_authorization(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        user_id: impl Into<String>,
        security_token: impl Into<String>,
        profile: XunleiClientProfile,
        expires_at: u64,
    ) -> Result<Self, QuarkError> {
        let mut session = Self {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            expires_at,
            profile,
            user_id: user_id.into(),
            security_token: security_token.into(),
            web_context: XunleiWebContext::default(),
            web_security: XunleiWebSecurity::default(),
            action_tickets: BTreeMap::new(),
            auth_method: CloudAuthMethod::Web,
        };
        if session.access_token.is_empty() || session.user_id.is_empty() {
            return Err(QuarkError::AuthorizationPending);
        }
        // Preserve historical incomplete browser sessions for migration. The
        // explicit modern profile keeps its actual expiry and can refresh.
        if !session.profile.is_modern_web()
            && (session.refresh_token.is_empty() || !session.profile.is_drive_complete())
        {
            session.expires_at = u64::MAX;
        }
        Ok(session)
    }
}

/// Credentials required to start the provider's resumable object upload.
/// This is deliberately not serializable: it contains short-lived upload
/// credentials and must be consumed immediately by the upload executor.
#[derive(Clone, PartialEq, Eq)]
pub struct XunleiResumableUpload {
    pub upload_type: String,
    pub kind: String,
    pub provider: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub bucket: String,
    pub endpoint: String,
    pub expiration: String,
    pub key: String,
    pub security_token: String,
}

impl fmt::Debug for XunleiResumableUpload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XunleiResumableUpload")
            .field("upload_type", &self.upload_type)
            .field("kind", &self.kind)
            .field("provider", &self.provider)
            .field("access_key_id", &"<redacted>")
            .field("access_key_secret", &"<redacted>")
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint)
            .field("expiration", &self.expiration)
            .field("key", &self.key)
            .field("security_token", &"<redacted>")
            .finish()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) const XUNLEI_RECYCLE_BIN_ID: &str = "__versa_down_xunlei_recycle_bin__";
const XUNLEI_RECYCLE_BIN_NAME: &str = "回收站";

/// A blocking client whose refreshed session can be persisted by the command
/// layer with [`Self::session`].
pub struct XunleiClient<'a, T: CloudHttpTransport> {
    session: RefCell<XunleiSession>,
    http: &'a T,
    automatic_session_recovery: bool,
}

impl<'a, T: CloudHttpTransport> XunleiClient<'a, T> {
    fn trace_response(method: &str, path: &str, session: &XunleiSession, response: &CloudHttpResponse) {
        if !cfg!(debug_assertions) || std::env::var("VERSADOWN_XUNLEI_DIAGNOSTICS").as_deref() != Ok("1") {
            return;
        }
        let route = match path {
            "/drive/v1/files" => "files",
            "/drive/v1/share/list" => "shares",
            "/drive/v1/about" => "capacity",
            "/v1/shield/captcha/init" => "captcha_init",
            "/v1/auth/token" => "oauth_refresh",
            _ => "other",
        };
        let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        let ticket = format!("{:x}", Sha256::digest(session.web_security.captcha_token.as_bytes()));
        eprintln!("[xunlei-diagnostic] {}", json!({
            "method": method, "route": route, "status": response.status,
            "code": body["error_code"].as_i64().or_else(|| body["code"].as_i64()),
            "captcha_rejected": matches!(body["error"].as_str(), Some("captcha_required" | "captcha_invalid")),
            "challenge_url_present": body["url"].as_str().is_some_and(|value| !value.is_empty()),
            "ticket_id": &ticket[..12],
            "ticket_seconds_left": session.web_security.expires_at.saturating_sub(now()),
            "root_ticket": session.web_security.action == "GET:/drive/v1/files",
            "signer_available": session.can_prepare_operation_verification(),
            "local_signer_supported": session.profile.supports_web_signature(),
        }));
    }

    pub fn new(mut session: XunleiSession, http: &'a T) -> Self {
        let access_token = session.access_token.clone();
        if !session.profile.is_modern_web() {
            session
                .web_context
                .use_access_token_for_missing_session_id(&access_token);
        }
        Self {
            session: RefCell::new(session),
            http,
            automatic_session_recovery: true,
        }
    }
    pub(crate) fn supervised(mut self) -> Self {
        self.automatic_session_recovery = false;
        self
    }
    pub(crate) fn refresh_session(&self) -> Result<XunleiSession, QuarkError> {
        self.refresh(self.session())
    }

    pub fn session(&self) -> XunleiSession {
        self.session.borrow().clone()
    }

    fn is_browser_bearer_session(session: &XunleiSession) -> bool {
        !session.profile.is_modern_web()
            && session.auth_method == CloudAuthMethod::Web
            && !session.access_token.is_empty()
            && !session.user_id.is_empty()
            // A web session can expose its genuine browser device/profile
            // while deliberately withholding the native-only refresh secret.
            // It is usable until the provider rejects that current bearer;
            // never turn it into an impossible native refresh attempt.
            && (session.refresh_token.is_empty() || !session.profile.can_refresh())
    }

    /// Builds the account used to finish an official browser authorization.
    ///
    /// This only formats identity. The command layer must successfully read
    /// the root directory before considering the authorization complete.
    pub fn web_account(&self) -> Result<CloudAccount, QuarkError> {
        let session = self.session();
        if session.access_token.is_empty() || session.user_id.is_empty() {
            return Err(QuarkError::AuthorizationPending);
        }
        if session.profile.is_modern_web() && !session.profile.is_drive_complete() {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        if Self::is_browser_bearer_session(&session) && !session.web_context.is_complete() {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let id = (!session.user_id.is_empty())
            .then_some(session.user_id)
            .ok_or(QuarkError::AuthorizationPending)?;
        Ok(CloudAccount {
            provider: CloudProvider::Xunlei,
            display_name: id.clone(),
            id,
            auth_method: session.auth_method,
        })
    }

    fn profile_headers(profile: &XunleiClientProfile) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("X-Device-Id".into(), profile.device_id.clone()),
            ("X-Client-Id".into(), profile.client_id.clone()),
            ("X-Client-Version".into(), profile.client_version.clone()),
            ("User-Agent".into(), profile.user_agent.clone()),
            ("Accept".into(), "application/json;charset=UTF-8".into()),
        ])
    }

    fn browser_protocol_headers(&self, session: &XunleiSession) -> Vec<(String, String)> {
        let peer_id = if session.web_context.peer_id.is_empty() {
            session.web_context.device_id.clone()
        } else {
            session.web_context.peer_id.clone()
        };
        vec![
            ("Peer-Id".into(), peer_id.clone()),
            (
                "Guid".into(),
                format!("{:x}", md5::compute(peer_id.as_bytes())),
            ),
            ("User-Id".into(), session.user_id.clone()),
            ("Session-Id".into(), session.web_context.session_id.clone()),
            ("Version-Code".into(), WEB_VERSION_CODE.into()),
            ("Credit-Key".into(), session.web_context.credit_key.clone()),
        ]
    }

    fn drive_url(&self, path: &str) -> String {
        let session = self.session();
        let raw = format!("{DRIVE}{path}");
        if !Self::is_browser_bearer_session(&session) {
            return raw;
        }
        let mut url = url::Url::parse(&raw).expect("Xunlei drive URL is constant");
        let mut query = url.query_pairs_mut();
        query.append_pair("_sessid", &session.web_context.session_id);
        query.append_pair("_uid", &session.user_id);
        for (name, value) in self.browser_protocol_headers(&session) {
            query.append_pair("_h[]", &format!("{name}: {value}"));
        }
        drop(query);
        url.into()
    }

    pub fn storage_capacity(&self) -> Result<CloudStorageCapacity, QuarkError> {
        let value = self.request("GET", self.drive_url("/drive/v1/about"), None)?;
        CloudStorageCapacity::from_json(&value["quota"]["limit"], &value["quota"]["usage"], false)
    }

    fn challenge(value: &Value, _status: u16) -> bool {
        let error = value
            .get("error")
            .or_else(|| value.get("error_description"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        value
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|url| !url.is_empty())
            || value
                .get("captcha_url")
                .and_then(Value::as_str)
                .is_some_and(|url| !url.is_empty())
            || value
                .get("verification_url")
                .and_then(Value::as_str)
                .is_some_and(|url| !url.is_empty())
            || error.contains("captcha")
            || error.contains("verify")
            || error.contains("risk")
    }

    fn value(&self, response: CloudHttpResponse) -> Result<Value, QuarkError> {
        let parsed: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        let code = parsed
            .get("error_code")
            .or_else(|| parsed.get("code"))
            .and_then(Value::as_i64)
            .unwrap_or_default();
        if self.session.borrow().profile.is_modern_web() {
            let error = parsed["error"].as_str().unwrap_or("");
            if response.status == 401
                || code == 16
                || matches!(
                    error,
                    "unauthenticated" | "invalid_grant" | "invalid_token" | "unauthorized_client"
                )
            {
                return Err(QuarkError::AuthorizationExpired);
            }
            if response.status == 412
                || matches!(code, 4001 | 4002)
                || matches!(error, "captcha_required" | "captcha_invalid")
            {
                return Err(QuarkError::WebAuthorizationRequired);
            }
            if Self::challenge(&parsed, response.status) && error.is_empty() {
                if let Some(url) = parsed["url"].as_str() {
                    self.session.borrow_mut().web_security.verification_url = url.into();
                }
                return Err(QuarkError::WebAuthorizationRequired);
            }
            if !(200..300).contains(&response.status) || code != 0 || !error.is_empty() {
                return Err(QuarkError::Provider(format!(
                    "迅雷服务拒绝请求（HTTP {} / code {}）",
                    response.status, code
                )));
            }
            return if parsed.is_null() {
                if response.body.is_empty() {
                    Ok(json!({}))
                } else {
                    Err(QuarkError::InvalidResponse)
                }
            } else {
                Ok(parsed)
            };
        }
        if matches!(code, 4121 | 10) {
            return Err(QuarkError::AuthorizationExpired);
        }
        if Self::challenge(&parsed, response.status) {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        response_json(&response).map_err(|error| match error {
            QuarkError::CaptchaRequired => QuarkError::WebAuthorizationRequired,
            error => error,
        })
    }

    fn refresh(&self, previous: XunleiSession) -> Result<XunleiSession, QuarkError> {
        if previous.refresh_token.is_empty() || !previous.profile.can_refresh() {
            return Err(QuarkError::AuthorizationExpired);
        }
        let mut headers = Self::profile_headers(&previous.profile);
        if previous.profile.is_modern_web() {
            headers.insert("X-Sdk-Version".into(), "3.4.20".into());
            headers.insert("X-Protocol-Version".into(), "301".into());
            headers.insert("X-Action".into(), "401".into());
        } else if !previous.security_token.is_empty() {
            headers.insert("X-Captcha-Token".into(), previous.security_token.clone());
        }
        let mut body = json!({
            "grant_type": "refresh_token",
            "refresh_token": previous.refresh_token,
            "client_id": previous.profile.client_id,
        });
        if !previous.profile.is_modern_web() {
            body["client_secret"] = previous.profile.client_secret.clone().into();
        }
        let response = self.http.send(CloudHttpRequest {
            method: "POST",
            url: format!("{AUTH}/v1/auth/token"),
            headers,
            body: Some(body.to_string()),
        })?;
        Self::trace_response("POST", "/v1/auth/token", &previous, &response);
        let value = self.value(response)?;
        let mut refreshed = previous;
        refreshed.access_token = required(&value, "access_token")?;
        if let Ok(token) = required(&value, "refresh_token") {
            refreshed.refresh_token = token;
        }
        if let Some(id) = value
            .get("sub")
            .or_else(|| value.get("user_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            if refreshed.profile.is_modern_web()
                && !refreshed.user_id.is_empty()
                && refreshed.user_id != id
            {
                return Err(QuarkError::AuthorizationExpired);
            }
            refreshed.user_id = id.into();
        }
        if let Some(token) = value
            .get("captcha_token")
            .or_else(|| value.get("security_token"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
        {
            refreshed.security_token = token.into();
        }
        refreshed.expires_at = now().saturating_add(value["expires_in"].as_u64().unwrap_or(0));
        *self.session.borrow_mut() = refreshed.clone();
        Ok(refreshed)
    }

    fn ready_session(&self) -> Result<XunleiSession, QuarkError> {
        let session = self.session();
        if (!session.access_token.is_empty() && session.expires_at > now().saturating_add(60))
            // Official web OAuth credentials can omit an expiry even while
            // their current bearer is valid. Probe that bearer first; a 401
            // below still performs the bounded refresh path.
            || (session.profile.is_modern_web() && !session.access_token.is_empty())
            || Self::is_browser_bearer_session(&session)
        {
            return Ok(session);
        }
        if !session.profile.can_refresh() {
            return Err(QuarkError::AuthorizationExpired);
        }
        self.refresh(session)
    }

    fn raw(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
        session: &XunleiSession,
    ) -> Result<Value, QuarkError> {
        self.value(self.send_raw(method, url, body, session)?)
    }

    /// Retry only the request explicitly rejected by the captcha service.
    /// Never replay a composite upload/batch operation or an ambiguous failure.
    fn raw_retry_rejected_captcha(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
        session: &XunleiSession,
        allow_renewal: bool,
    ) -> Result<Value, QuarkError> {
        let response = self.send_raw(method, url.clone(), body.clone(), session)?;
        let parsed: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        let explicitly_rejected = matches!(response.status, 400 | 403 | 412)
            && matches!(parsed["error"].as_str(), Some("captcha_invalid" | "captcha_required"))
            && !["url", "captcha_url", "verification_url"].iter().any(|key| {
                parsed[*key].as_str().is_some_and(|url| !url.is_empty())
            });
        if allow_renewal && explicitly_rejected && session.can_prepare_operation_verification() {
            let path = url::Url::parse(&url).map_err(|_| QuarkError::InvalidResponse)?;
            let renewed = self.refresh_web_captcha(&format!("{method}:{}", path.path()))?;
            if renewed.web_security.captcha_token == session.web_security.captcha_token {
                return Err(QuarkError::WebAuthorizationRequired);
            }
            return self.raw(method, url, body, &renewed);
        }
        self.value(response)
    }

    fn send_raw(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
        session: &XunleiSession,
    ) -> Result<CloudHttpResponse, QuarkError> {
        if session.access_token.is_empty()
            || (!session.profile.is_drive_complete() && !Self::is_browser_bearer_session(session))
        {
            return Err(QuarkError::AuthorizationExpired);
        }
        if Self::is_browser_bearer_session(session) && !session.web_context.is_complete() {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let mut headers = session
            .profile
            .is_drive_complete()
            .then(|| Self::profile_headers(&session.profile))
            .unwrap_or_default();
        headers.insert(
            "Authorization".into(),
            format!("Bearer {}", session.access_token),
        );
        if session.profile.is_modern_web() {
            if !session.web_security.captcha_token.is_empty()
                && session.web_security.expires_at > now()
            {
                headers.insert(
                    "X-Captcha-Token".into(),
                    session.web_security.captcha_token.clone(),
                );
            }
            headers.insert("Origin".into(), "https://pan.xunlei.com".into());
            headers.insert("Referer".into(), "https://pan.xunlei.com/".into());
        } else if !session.security_token.is_empty() {
            headers.insert("X-Captcha-Token".into(), session.security_token.clone());
        }
        if Self::is_browser_bearer_session(session) {
            headers.extend(self.browser_protocol_headers(session));
            headers.insert("Origin".into(), "https://pan.xunlei.com".into());
            headers.insert("Referer".into(), "https://pan.xunlei.com/".into());
            headers
                .entry("Accept".into())
                .or_insert_with(|| "application/json;charset=UTF-8".into());
            if !session.profile.user_agent.is_empty() {
                headers.insert("User-Agent".into(), session.profile.user_agent.clone());
            }
        }
        let path = url::Url::parse(&url).map(|url| url.path().to_owned()).unwrap_or_default();
        let response = self.http.send(CloudHttpRequest {
            method,
            url,
            headers,
            body: body.map(|value| value.to_string()),
        })?;
        Self::trace_response(method, &path, session, &response);
        Ok(response)
    }

    fn request(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let mut session = if self.automatic_session_recovery {
            self.ready_session()?
        } else {
            self.session()
        };
        if session.profile.is_modern_web() {
            let parsed = url::Url::parse(&url).map_err(|_| QuarkError::InvalidResponse)?;
            let action = format!("{method}:{}", parsed.path());
            // Use a previously issued ticket only for the exact action it
            // was issued for. Keep the current signing metadata independent.
            if session.web_security.action != action {
                if let Some(ticket) = session.action_tickets.get(&action).filter(|ticket| {
                    !ticket.token.is_empty() && ticket.expires_at > now().saturating_add(30)
                }) {
                    session.web_security.captcha_token = ticket.token.clone();
                    session.web_security.expires_at = ticket.expires_at;
                    session.web_security.action = action.clone();
                    let mut result = if self.automatic_session_recovery {
                        self.raw(method, url.clone(), body.clone(), &session)
                    } else {
                        self.raw_retry_rejected_captcha(method, url.clone(), body.clone(), &session, true)
                    };
                    if matches!(result, Err(QuarkError::AuthorizationExpired))
                        && self.automatic_session_recovery
                        && session.profile.can_refresh()
                    {
                        let mut refreshed = self.refresh(self.session())?;
                        refreshed.web_security = session.web_security;
                        result = self.raw(method, url.clone(), body.clone(), &refreshed);
                    }
                    if matches!(result, Err(QuarkError::WebAuthorizationRequired)) {
                        self.session.borrow_mut().action_tickets.remove(&action);
                        if self.automatic_session_recovery {
                            let renewed = self.refresh_web_captcha(&action)?;
                            return self.raw(method, url, body, &renewed);
                        }
                    }
                    return result;
                }
            }
            let security = &session.web_security;
            // Observed official web requests reuse the root ticket for share
            // listing, folder creation and upload initialization. Renew on
            // expiry or a server challenge, rather than changing it per action.
            let reuse_root_ticket = security.action == "GET:/drive/v1/files"
                && !security.captcha_token.is_empty()
                && parsed.origin().ascii_serialization() == "https://api-pan.xunlei.com"
                && matches!(action.as_str(), "GET:/drive/v1/share/list" | "POST:/drive/v1/files");
            let mut renewed = false;
            if (session.profile.supports_web_signature()
                || !security.captcha_token.is_empty()
                || !security.captcha_sign.is_empty())
                && (security.expires_at <= now().saturating_add(30)
                    // Quota and file-detail reads accept the current official
                    // root ticket. Do not require private signing metadata before
                    // trying it; retain normal server rejection/renewal handling.
                    || (!security.action.is_empty()
                        && security.action != action
                        && action != "GET:/drive/v1/about"
                        && !reuse_root_ticket
                        && !(method == "GET" && parsed.path().starts_with("/drive/v1/files/"))))
            {
                session = self.refresh_web_captcha(&action)?;
                renewed = true;
            }
            // Long-lived account renewal stays supervised. An explicit captcha
            // rejection can renew and retry this exact request once, without
            // restarting a mutation that may have already completed other steps.
            if !self.automatic_session_recovery || (reuse_root_ticket && method == "POST") {
                return self.raw_retry_rejected_captcha(method, url, body, &session, !renewed);
            }
            let mut refreshed = false;
            loop {
                match self.raw(method, url.clone(), body.clone(), &session) {
                    Err(QuarkError::AuthorizationExpired)
                        if !refreshed && session.profile.can_refresh() =>
                    {
                        session = self.refresh(session)?;
                        refreshed = true;
                    }
                    Err(QuarkError::WebAuthorizationRequired) if !renewed => {
                        session = self.refresh_web_captcha(&action)?;
                        renewed = true;
                    }
                    result => return result,
                }
            }
        }
        if !self.automatic_session_recovery {
            return self.raw(method, url, body, &session);
        }
        match self.raw(method, url.clone(), body.clone(), &session) {
            Err(QuarkError::AuthorizationExpired) if session.profile.can_refresh() => {
                let refreshed = self.refresh(session)?;
                self.raw(method, url, body, &refreshed)
            }
            result => result,
        }
    }

    fn refresh_web_captcha(&self, action: &str) -> Result<XunleiSession, QuarkError> {
        let mut session = self.session();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .to_string();
        if let Some(signature) = session.profile.web_captcha_signature(&timestamp) {
            session.web_security.captcha_sign = signature;
            session.web_security.timestamp = timestamp;
            session.web_security.client_version = session.profile.client_version.clone();
            session.web_security.package_name = session.profile.package_name.clone();
        }
        let security = &session.web_security;
        if security.captcha_sign.is_empty()
            || security.timestamp.is_empty()
            || security.client_version.is_empty()
            || security.package_name != session.profile.package_name
        {
            return Err(QuarkError::XunleiVerificationUnavailable("metadata_missing"));
        }
        // A local conservative reuse bound for unknown versions, not a claim
        // about the provider's TTL. Supported profiles just generated a fresh
        // pair; other profiles require a recent pair from the official bridge.
        let signed_at = security
            .timestamp
            .parse::<u64>()
            .map(|millis| millis / 1000)
            .map_err(|_| QuarkError::XunleiVerificationUnavailable("timestamp_invalid"))?;
        if signed_at > now().saturating_add(30) {
            return Err(QuarkError::XunleiVerificationUnavailable("timestamp_invalid"));
        }
        if now().saturating_sub(signed_at) > 300 {
            return Err(QuarkError::XunleiVerificationUnavailable("metadata_expired"));
        }
        let response = self.http.send(CloudHttpRequest {
            method: "POST",
            url: format!("{AUTH}/v1/shield/captcha/init"),
            headers: BTreeMap::from([
                ("Content-Type".into(), "application/json".into()),
                ("User-Agent".into(), session.profile.user_agent.clone()),
                ("Origin".into(), "https://pan.xunlei.com".into()),
            ]),
            body: Some(
                json!({
                    "client_id": session.profile.client_id,
                    "device_id": session.profile.device_id,
                    "action": action,
                    "captcha_token": security.captcha_token,
                    "meta": {
                        "package_name": security.package_name,
                        "client_version": security.client_version,
                        "captcha_sign": security.captcha_sign,
                        "timestamp": security.timestamp,
                        "user_id": session.user_id,
                        "username": "", "phone_number": "", "email": ""
                    }
                })
                .to_string(),
            ),
        })?;
        Self::trace_response("POST", "/v1/shield/captcha/init", &session, &response);
        let parsed: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        if let Some(url) = parsed["url"].as_str().filter(|url| !url.is_empty()) {
            session.web_security.verification_url = url.into();
            *self.session.borrow_mut() = session;
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let value = self.value(response)?;
        let token = required(&value, "captcha_token")?;
        let expires_in = value["expires_in"]
            .as_u64()
            .filter(|value| *value > 30)
            .ok_or(QuarkError::InvalidResponse)?;
        let at = now();
        let previous = &session.web_security;
        session.action_tickets.retain(|_, ticket| ticket.expires_at > at);
        session.action_tickets.remove(action);
        if !previous.action.is_empty()
            && previous.action != action
            && !previous.captcha_token.is_empty()
            && previous.expires_at > at
        {
            session.action_tickets.insert(
                previous.action.clone(),
                XunleiActionTicket {
                    token: previous.captcha_token.clone(),
                    expires_at: previous.expires_at,
                },
            );
        }
        if session.action_tickets.len() > 8 {
            if let Some(oldest) = session.action_tickets.iter()
                .min_by_key(|(_, ticket)| ticket.expires_at)
                .map(|(action, _)| action.clone())
            {
                session.action_tickets.remove(&oldest);
            }
        }
        session.web_security.captcha_token = token;
        session.web_security.expires_at = at.saturating_add(expires_in);
        session.web_security.action = action.into();
        session.web_security.verification_url.clear();
        *self.session.borrow_mut() = session.clone();
        Ok(session)
    }

    fn request_parent<'b>(&self, parent: &'b str) -> &'b str {
        if parent == "0"
            && (self.session.borrow().profile.is_modern_web()
                || Self::is_browser_bearer_session(&self.session()))
        {
            ""
        } else {
            parent
        }
    }

    pub fn account(&self) -> Result<CloudAccount, QuarkError> {
        let value = self.request("GET", format!("{AUTH}/v1/user/me"), None)?;
        let session = self.session();
        let id = value
            .get("sub")
            .or_else(|| value.get("id"))
            .or_else(|| value.get("user_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .or_else(|| (!session.user_id.is_empty()).then_some(session.user_id))
            .ok_or(QuarkError::InvalidResponse)?;
        let display_name = value
            .get("nickname")
            .or_else(|| value.get("name"))
            .or_else(|| value.get("username"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| id.clone());
        Ok(CloudAccount {
            provider: CloudProvider::Xunlei,
            id,
            display_name,
            auth_method: CloudAuthMethod::Web,
        })
    }

    pub fn directory(&self, parent: &str) -> Result<CloudDirectory, QuarkError> {
        if parent == XUNLEI_RECYCLE_BIN_ID {
            return self.list_recycle_bin();
        }
        self.list_files(parent, false)
    }

    fn list_files(&self, parent: &str, trashed: bool) -> Result<CloudDirectory, QuarkError> {
        let mut items = Vec::new();
        let mut page_token = String::new();
        let mut pages = HashSet::new();
        let request_parent = if trashed {
            ""
        } else {
            self.request_parent(parent)
        };
        let extras = if self.session.borrow().profile.is_modern_web() {
            ""
        } else {
            "__type=drive&refresh=true&__sync=true&"
        };
        let filter = if trashed {
            r#"{"phase":{"eq":"PHASE_TYPE_COMPLETE"},"trashed":{"eq":true}}"#
        } else {
            r#"{"phase":{"eq":"PHASE_TYPE_COMPLETE"},"trashed":{"eq":false}}"#
        };
        loop {
            let value = self.request(
                "GET",
                self.drive_url(&format!(
                    "/drive/v1/files?space=&{extras}parent_id={}&page_token={}&with_audit=true&limit=100&filters={}",
                    encoded(request_parent),
                    encoded(&page_token),
                    encoded(filter),
                )),
                None,
            )?;
            for file in value["files"]
                .as_array()
                .ok_or(QuarkError::InvalidResponse)?
            {
                if file["phase"].as_str() != Some("PHASE_TYPE_COMPLETE") {
                    continue;
                }
                let mut item = Self::file(
                    file,
                    if trashed {
                        XUNLEI_RECYCLE_BIN_ID
                    } else {
                        parent
                    },
                )?;
                if trashed {
                    item.parent_id = XUNLEI_RECYCLE_BIN_ID.into();
                }
                items.push(item);
            }
            page_token = value["next_page_token"].as_str().unwrap_or("").into();
            if page_token.is_empty() {
                if parent == "0" && !trashed {
                    items.push(CloudFile {
                        id: XUNLEI_RECYCLE_BIN_ID.into(),
                        parent_id: "0".into(),
                        name: XUNLEI_RECYCLE_BIN_NAME.into(),
                        kind: "folder".into(),
                        size: None,
                        modified_at: None,
                    });
                }
                return Ok(CloudDirectory {
                    id: parent.into(),
                    items,
                });
            }
            if !pages.insert(page_token.clone()) || pages.len() > 10_000 {
                return Err(QuarkError::InvalidResponse);
            }
        }
    }

    pub fn list_recycle_bin(&self) -> Result<CloudDirectory, QuarkError> {
        self.list_files(XUNLEI_RECYCLE_BIN_ID, true)
    }

    pub fn create_share(
        &self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, QuarkError> {
        if file_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件或目录".into()));
        }
        // Xunlei's web client uses `copy`, not PikPak's encryptedlink /
        // publiclink enum. The server generates the access code; these are
        // the actual fields and value types used by its share dialog.
        let value = self.request(
            "POST",
            self.drive_url("/drive/v1/share"),
            Some(json!({
                "file_ids": file_ids,
                "share_to": "copy",
                "title": options.name,
                "restore_limit": "-1",
                "expiration_days": options.expires_in_days.map(|days| days.to_string()).unwrap_or_else(|| "-1".into()),
                "params": {"subscribe_push": "false", "WithPassCodeInLink": "true"},
            })),
        )?;
        let mut share = xunlei_share_from_value(&value, Some(file_ids), None)?;
        share.name = options.name.clone();
        Ok(share)
    }

    pub fn list_shares(&self) -> Result<Vec<CloudShare>, QuarkError> {
        let mut shares = Vec::new();
        let mut page_token = String::new();
        let mut seen_tokens = HashSet::new();
        let modern_web = self.session.borrow().profile.is_modern_web();
        for _ in 0..10_000 {
            let path = if modern_web {
                let mut path = "/drive/v1/share/list?space=&limit=100&withCaptcha=true&withCredentials=true".to_owned();
                if !page_token.is_empty() {
                    path.push_str(&format!("&page_token={}", encoded(&page_token)));
                }
                path
            } else {
                format!(
                    "/drive/v1/share/list?limit=100&thumbnail_size=SIZE_SMALL&page_token={}",
                    encoded(&page_token)
                )
            };
            let value = self.request("GET", self.drive_url(&path), None)?;
            let list = value
                .get("data")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            shares.extend(
                list.iter()
                    .map(|share| xunlei_share_from_value(share, None, None))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            page_token = value
                .get("next_page_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if page_token.is_empty() {
                return Ok(shares);
            }
            if !seen_tokens.insert(page_token.clone()) {
                return Err(QuarkError::InvalidResponse);
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    /// Cancels shares one by one without aborting on an item failure. Ids the
    /// provider no longer has count as cancelled (idempotent). A session-level
    /// error stops the batch; the untouched ids are reported as failed.
    pub fn cancel_shares(
        &self,
        share_ids: &[String],
    ) -> Result<CloudShareCancelOutcome, QuarkError> {
        if share_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个分享".into()));
        }
        let mut outcome = CloudShareCancelOutcome::default();
        let mut halted: Option<CloudShareCancelFailure> = None;
        for share_id in share_ids {
            if let Some(stop) = &halted {
                outcome.failures.push(CloudShareCancelFailure { id: share_id.clone(), ..stop.clone() });
                continue;
            }
            match self.cancel_one_share(share_id) {
                Ok(()) => outcome.cancelled.push(share_id.clone()),
                Err(error) => {
                    let failure = CloudShareCancelFailure::from_error(share_id, &error);
                    if error.is_session_level() {
                        if outcome.cancelled.is_empty() {
                            return Err(error);
                        }
                        halted = Some(failure.clone());
                    }
                    outcome.failures.push(failure);
                }
            }
        }
        Ok(outcome)
    }

    fn cancel_one_share(&self, share_id: &str) -> Result<(), QuarkError> {
        match self.request(
            "POST",
            self.drive_url("/drive/v1/share/delete"),
            Some(json!({"space": "", "share_id": share_id})),
        ) {
            Err(QuarkError::Provider(message)) if share_already_gone(&message) => Ok(()),
            result => result.map(|_| ()),
        }
    }

    fn file(value: &Value, parent: &str) -> Result<CloudFile, QuarkError> {
        let kind = required(value, "kind")?;
        Ok(CloudFile {
            id: required(value, "id")?,
            parent_id: value["parent_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .unwrap_or(parent)
                .into(),
            name: required(value, "name")?,
            kind: if matches!(kind.as_str(), "drive#folder" | "FOLDER") {
                "folder"
            } else {
                "file"
            }
            .into(),
            size: value["size"]
                .as_u64()
                .or_else(|| value["size"].as_str().and_then(|size| size.parse().ok())),
            modified_at: value["modified_time"]
                .as_str()
                .or_else(|| value["modified_at"].as_str())
                .map(str::to_owned),
        })
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> Result<(), QuarkError> {
        self.request(
            "POST",
            self.drive_url("/drive/v1/files"),
            Some(
                json!({"kind":"drive#folder","parent_id":self.request_parent(parent),"name":name}),
            ),
        )?;
        Ok(())
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), QuarkError> {
        self.request(
            "PATCH",
            self.drive_url(&format!("/drive/v1/files/{}", encoded(id))),
            Some(json!({"name":name})),
        )?;
        Ok(())
    }

    pub fn move_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        self.request(
            "POST",
            self.drive_url("/drive/v1/files:batchMove"),
            Some(json!({"ids":ids,"to":{"parent_id":self.request_parent(parent)}})),
        )?;
        Ok(())
    }

    pub fn copy_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        self.request(
            "POST",
            self.drive_url("/drive/v1/files:batchCopy"),
            Some(json!({"ids":ids,"to":{"parent_id":self.request_parent(parent)}})),
        )?;
        Ok(())
    }

    pub fn trash(&self, ids: &[String]) -> Result<(), QuarkError> {
        for id in ids {
            self.request(
                "PATCH",
                self.drive_url(&format!("/drive/v1/files/{}/trash", encoded(id))),
                Some(json!({})),
            )?;
        }
        Ok(())
    }

    pub fn restore_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        if ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件".into()));
        }
        self.request(
            "POST",
            self.drive_url("/drive/v1/files:batchUntrash"),
            Some(json!({"ids": ids})),
        )?;
        Ok(())
    }

    pub fn delete_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        if ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件".into()));
        }
        self.request(
            "POST",
            self.drive_url("/drive/v1/files:batchDelete"),
            Some(json!({"ids": ids})),
        )?;
        Ok(())
    }

    /// Initiates an upload and returns the provider-issued resumable contract.
    /// Its short-lived credentials must be consumed immediately and never
    /// stored with the stable cloud source.
    pub fn upload_init(
        &self,
        parent: &str,
        name: &str,
        size: u64,
        gcid: &str,
    ) -> Result<XunleiResumableUpload, QuarkError> {
        if gcid.is_empty() {
            return Err(QuarkError::InvalidInput("缺少上传内容 GCID".into()));
        }
        let value = self.request(
            "POST",
            self.drive_url("/drive/v1/files"),
            Some(upload_request_body(
                self.request_parent(parent),
                name,
                size,
                gcid,
                self.session().profile.is_modern_web(),
            )),
        )?;
        let upload = resumable_upload(&value)?;
        if upload.upload_type != "UPLOAD_TYPE_RESUMABLE" || upload.kind != "drive#resumable" {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(upload)
    }

    /// Retrieves a short-lived direct link afresh for every download request.
    pub fn download(&self, id: &str) -> Result<(String, String), QuarkError> {
        let download = self.download_with_acceleration(id)?;
        Ok((download.url, download.name))
    }

    /// The acceleration credential remains separate from the CDN request. It
    /// is consumed only by the local native backend and never by aria2.
    pub fn download_with_acceleration(&self, id: &str) -> Result<XunleiDownload, QuarkError> {
        let modern_web = self.session().profile.is_modern_web();
        let started = std::time::Instant::now();
        loop {
            if modern_web {
                download_poll_delay(started.elapsed())?;
            }
            let value = self
                .request(
                    "GET",
                    self.drive_url(&format!("/drive/v1/files/{}", encoded(id))),
                    None,
                )
                .map_err(download_source_missing_from_http_404)?;
            let file = value.get("data").unwrap_or(&value);
            if file
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                != Some(id)
            {
                return Err(QuarkError::InvalidResponse);
            }
            let name = required(file, "name")?;
            match xunlei_content_link(file)? {
                Some(link) => {
                    if !link.starts_with("https://") {
                        return Err(QuarkError::InvalidResponse);
                    }
                    return Ok(XunleiDownload {
                        url: link.to_owned(),
                        name,
                        acceleration_token: xunlei_acceleration_token(file)?,
                        cid: file
                            .get("cid")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned),
                        gcid: file
                            .get("gcid")
                            .or_else(|| file.get("hash"))
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned),
                        size: file.get("size").and_then(|value| {
                            value.as_u64().or_else(|| {
                                value.as_str().and_then(|value| value.parse::<u64>().ok())
                            })
                        }),
                    });
                }
                None => {}
            }
            // Modern web metadata can report COMPLETE before (or between)
            // requests with a generated link. Only this known empty-link
            // state is transient; protocol, auth and transport errors escape.
            if !modern_web
                || !matches!(
                    file["phase"].as_str(),
                    Some("PHASE_TYPE_COMPLETE" | "PHASE_TYPE_PENDING" | "PHASE_TYPE_RUNNING")
                )
            {
                return Err(QuarkError::InvalidResponse);
            }
            std::thread::sleep(download_poll_delay(started.elapsed())?);
        }
    }
}

pub struct XunleiDownload {
    pub url: String,
    pub name: String,
    pub acceleration_token: Option<String>,
    pub cid: Option<String>,
    pub gcid: Option<String>,
    pub size: Option<u64>,
}

fn xunlei_share_from_value(
    value: &Value,
    fallback_file_ids: Option<&[String]>,
    pass_code: Option<&str>,
) -> Result<CloudShare, QuarkError> {
    let id = required(value, "share_id")?;
    Ok(CloudShare {
        id: id.clone(),
        url: value
            .get("share_url")
            .or_else(|| value.get("url"))
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("https://pan.xunlei.com/s/{id}")),
        name: value
            .get("title")
            .or_else(|| value.get("share_name"))
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("分享文件")
            .into(),
        kind: if value.get("file_type").and_then(Value::as_str) == Some("folder") {
            CloudFileKind::Folder
        } else {
            CloudFileKind::File
        },
        source_file_ids: value
            .get("file_ids")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .or_else(|| fallback_file_ids.map(|ids| ids.to_vec()))
            .unwrap_or_default(),
        pass_code: value
            .get("pass_code")
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .or_else(|| pass_code.map(str::to_owned)),
        created_at: value
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_at: value
            .get("expiration")
            .or_else(|| value.get("expire_at"))
            .and_then(Value::as_str)
            .filter(|time| !time.is_empty())
            .map(str::to_owned),
    })
}

fn download_poll_delay(elapsed: std::time::Duration) -> Result<std::time::Duration, QuarkError> {
    let remaining = std::time::Duration::from_secs(30).saturating_sub(elapsed);
    if remaining.is_zero() {
        return Err(QuarkError::Provider(
            "文件可见但下载链接仍在生成，请稍后重试。".into(),
        ));
    }
    Ok(remaining.min(std::time::Duration::from_millis(250)))
}

#[cfg(test)]
mod download_poll_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn xunlei_download_poll_delay_rejects_deadline_and_caps_last_sleep() {
        assert_eq!(
            download_poll_delay(Duration::ZERO).unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(
            download_poll_delay(Duration::from_millis(29_999)).unwrap(),
            Duration::from_millis(1)
        );
        for elapsed in [Duration::from_secs(30), Duration::from_secs(31)] {
            assert!(
                matches!(download_poll_delay(elapsed), Err(QuarkError::Provider(message))
                if message == "文件可见但下载链接仍在生成，请稍后重试。")
            );
        }
    }
}

impl<'a, T: CloudHttpTransport + CloudUploadTransport> XunleiClient<'a, T> {
    /// Computes the provider's GCID, obtains one-shot STS credentials, and
    /// completes the negotiated OSS or AWS multipart upload. The STS credentials
    /// and upload URLs live only on this stack and are never serialized.
    pub fn upload_file(&self, parent: &str, path: &Path) -> Result<CloudFile, QuarkError> {
        let snapshot = super::UploadFileSnapshot::capture(path)?;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(QuarkError::InvalidInput("请选择普通文件上传".into()));
        }
        let name = path
            .file_name()
            .ok_or_else(|| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?
            .to_string_lossy()
            .into_owned();
        let size = metadata.len();
        let gcid = xunlei_gcid(path, size)?;
        snapshot.validate()?;
        let value = self.request(
            "POST",
            self.drive_url("/drive/v1/files"),
            Some(upload_request_body(
                self.request_parent(parent),
                &name,
                size,
                &gcid,
                self.session().profile.is_modern_web(),
            )),
        )?;
        let file_value = value.get("file").ok_or(QuarkError::InvalidResponse)?;
        snapshot.validate()?;
        let file = Self::file(file_value, parent)?;
        // The official web upload flow returns a completed file before
        // examining upload_type or any object-storage credentials.
        if file_value["phase"] == "PHASE_TYPE_COMPLETE" {
            return Ok(file);
        }
        let upload = resumable_upload(&value)?;
        if upload.upload_type != "UPLOAD_TYPE_RESUMABLE" || upload.kind != "drive#resumable" {
            return Err(QuarkError::InvalidResponse);
        }
        match upload.provider.as_str() {
            "aws" => AwsMultipart::from_upload(&upload)?.upload(self.http, &snapshot, size)?,
            "PROVIDER_ALIYUN" => upload_oss(&upload, self.http, &snapshot, size)?,
            _ => return Err(QuarkError::InvalidResponse),
        }
        if self.session().profile.is_modern_web() {
            return self.wait_for_uploaded_file(&file.id, parent);
        }
        Ok(file)
    }

    fn wait_for_uploaded_file(&self, id: &str, parent: &str) -> Result<CloudFile, QuarkError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(QuarkError::Provider(
                    "数据已上传但云盘仍在处理，请稍后刷新目录。".into(),
                ));
            }
            let value = self.request(
                "GET",
                self.drive_url(&format!("/drive/v1/files/{}", encoded(id))),
                None,
            )?;
            match value["phase"].as_str() {
                Some("PHASE_TYPE_COMPLETE") => return Self::file(&value, parent),
                Some("PHASE_TYPE_ERROR") => {
                    return Err(QuarkError::Provider(
                        "上传数据的云盘处理失败，请在迅雷网页中查看详情。".into(),
                    ));
                }
                Some("PHASE_TYPE_PENDING" | "PHASE_TYPE_RUNNING") => {}
                _ => return Err(QuarkError::InvalidResponse),
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            std::thread::sleep(remaining.min(std::time::Duration::from_millis(250)));
        }
    }
}

fn upload_request_body(parent: &str, name: &str, size: u64, gcid: &str, modern_web: bool) -> Value {
    let mut body = json!({
        "kind":"drive#file", "parent_id":parent, "name":name,
        "size":size, "hash":gcid, "upload_type":"UPLOAD_TYPE_RESUMABLE",
    });
    if modern_web {
        body["resumable"] = json!({"supported_providers":["PROVIDER_ALIYUN"]});
    }
    body
}

fn resumable_upload(value: &Value) -> Result<XunleiResumableUpload, QuarkError> {
    let resumable = &value["resumable"];
    let params = &resumable["params"];
    Ok(XunleiResumableUpload {
        upload_type: required(value, "upload_type")?,
        kind: required(resumable, "kind")?,
        provider: required(resumable, "provider")?,
        access_key_id: required(params, "access_key_id")?,
        access_key_secret: required(params, "access_key_secret")?,
        bucket: required(params, "bucket")?,
        endpoint: required(params, "endpoint")?,
        expiration: required(params, "expiration")?,
        key: required(params, "key")?,
        security_token: required(params, "security_token")?,
    })
}

const AWS_PART_SIZE: u64 = 8 * 1024 * 1024;

struct AwsMultipart {
    endpoint: url::Url,
    bucket: String,
    key: String,
    access_key_id: String,
    access_key_secret: String,
    security_token: String,
}

impl AwsMultipart {
    fn from_upload(upload: &XunleiResumableUpload) -> Result<Self, QuarkError> {
        Ok(Self {
            endpoint: object_storage_endpoint(upload)?,
            bucket: upload.bucket.clone(),
            key: upload.key.clone(),
            access_key_id: upload.access_key_id.clone(),
            access_key_secret: upload.access_key_secret.clone(),
            security_token: upload.security_token.clone(),
        })
    }

    fn object_path(&self) -> String {
        format!(
            "/{}",
            self.key
                .split('/')
                .map(aws_percent)
                .collect::<Vec<_>>()
                .join("/")
        )
    }

    fn object_host(&self) -> Result<String, QuarkError> {
        let host = self
            .endpoint
            .host_str()
            .ok_or(QuarkError::InvalidResponse)?;
        if host.contains(':') {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(match self.endpoint.port() {
            Some(port) => format!("{}.{}:{port}", self.bucket, host),
            None => format!("{}.{}", self.bucket, host),
        })
    }

    fn request(
        &self,
        method: &'static str,
        raw_query: &str,
        canonical_query: &str,
        body: super::UploadBody,
    ) -> Result<UploadHttpRequest, QuarkError> {
        let timestamp = aws_timestamp();
        let date = &timestamp[..8];
        let payload_hash = body.sha256()?;
        let host = self.object_host()?;
        let canonical_headers = format!(
            "host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{timestamp}\nx-amz-security-token:{}\n",
            self.security_token
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date;x-amz-security-token";
        let canonical = format!(
            "{method}\n{}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            self.object_path()
        );
        let scope = format!("{date}/xunlei/s3/aws4_request");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            hex(&Sha256::digest(canonical.as_bytes()))
        );
        let date_key = hmac_sha256(
            format!("AWS4{}", self.access_key_secret).as_bytes(),
            date.as_bytes(),
        )?;
        let region_key = hmac_sha256(&date_key, b"xunlei")?;
        let service_key = hmac_sha256(&region_key, b"s3")?;
        let signing_key = hmac_sha256(&service_key, b"aws4_request")?;
        let signature = hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes())?);
        let mut url = format!("https://{host}{}", self.object_path());
        if !raw_query.is_empty() {
            url.push('?');
            url.push_str(raw_query);
        }
        Ok(UploadHttpRequest {
            method,
            url,
            headers: BTreeMap::from([
                ("Host".into(), host),
                ("Content-Type".into(), "application/octet-stream".into()),
                ("x-amz-content-sha256".into(), payload_hash),
                ("x-amz-date".into(), timestamp),
                ("x-amz-security-token".into(), self.security_token.clone()),
                (
                    "Authorization".into(),
                    format!(
                        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                        self.access_key_id
                    ),
                ),
            ]),
            body,
        })
    }

    fn upload(
        &self,
        http: &impl CloudUploadTransport,
        snapshot: &super::UploadFileSnapshot,
        size: u64,
    ) -> Result<(), QuarkError> {
        upload_multipart(
            http,
            snapshot,
            size,
            |method, raw_query, canonical_query, body| {
                self.request(method, raw_query, canonical_query, body)
            },
        )
    }
}

fn object_storage_endpoint(upload: &XunleiResumableUpload) -> Result<url::Url, QuarkError> {
    // Xunlei's official Aliyun contract can return a bare DNS endpoint,
    // including a custom Xunlei host. The web SDK's secure option uses HTTPS.
    // Preserve the existing strict absolute-HTTPS contract for other providers.
    let bare_dns = !upload.endpoint.is_empty()
        && upload.endpoint.len() <= 253
        && upload.endpoint.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    let endpoint_value = if upload.provider == "PROVIDER_ALIYUN" && bare_dns {
        format!("https://{}", upload.endpoint)
    } else {
        upload.endpoint.clone()
    };
    let mut endpoint = url::Url::parse(&endpoint_value).map_err(|_| QuarkError::InvalidResponse)?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.path() != "/"
        || upload.bucket.is_empty()
        || upload.key.is_empty()
        || !upload
            .bucket
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(QuarkError::InvalidResponse);
    }
    let host = endpoint
        .host_str()
        .ok_or(QuarkError::InvalidResponse)?
        .to_owned();
    if let Some(host) = host.strip_prefix(&format!("{}.", upload.bucket)) {
        endpoint
            .set_host(Some(host))
            .map_err(|_| QuarkError::InvalidResponse)?;
    }
    Ok(endpoint)
}

fn upload_oss(
    upload: &XunleiResumableUpload,
    http: &impl CloudUploadTransport,
    snapshot: &super::UploadFileSnapshot,
    size: u64,
) -> Result<(), QuarkError> {
    let mut endpoint = object_storage_endpoint(upload)?;
    let host = endpoint.host_str().ok_or(QuarkError::InvalidResponse)?;
    endpoint
        .set_host(Some(&format!("{}.{host}", upload.bucket)))
        .map_err(|_| QuarkError::InvalidResponse)?;
    let oss = super::upload::Oss {
        endpoint: endpoint.into(),
        bucket: upload.bucket.clone(),
        key: upload.key.clone(),
        id: upload.access_key_id.clone(),
        secret: upload.access_key_secret.clone(),
        token: upload.security_token.clone(),
    };
    upload_multipart(http, snapshot, size, |method, raw_query, _, body| {
        let query = if raw_query.is_empty() {
            String::new()
        } else {
            format!("?{raw_query}")
        };
        oss.request(method, &query, body)
    })
}

fn upload_multipart(
    http: &impl CloudUploadTransport,
    snapshot: &super::UploadFileSnapshot,
    size: u64,
    request: impl Fn(
        &'static str,
        &str,
        &str,
        super::UploadBody,
    ) -> Result<UploadHttpRequest, QuarkError>,
) -> Result<(), QuarkError> {
    if size == 0 {
        let response = http.send_upload(request("PUT", "", "", snapshot.slice(0, 0)?)?)?;
        return aws_checked(response).map(|_| ());
    }
    let started = aws_checked(http.send_upload(request(
        "POST",
        "uploads",
        "uploads=",
        Vec::new().into(),
    )?)?)?;
    let upload_id = xml_tag(&started.body, "UploadId")?;
    let mut remaining = size;
    let mut parts = Vec::new();
    let mut number = 1_u64;
    while remaining > 0 {
        let length = remaining.min(AWS_PART_SIZE);
        let body = snapshot.slice(size - remaining, length)?;
        remaining = remaining.saturating_sub(length);
        let encoded_upload = aws_percent(&upload_id);
        let query = format!("partNumber={number}&uploadId={encoded_upload}");
        let response = aws_checked(http.send_upload(request("PUT", &query, &query, body)?)?)?;
        let etag = response
            .headers
            .iter()
            .find(|(key, value)| key.eq_ignore_ascii_case("etag") && !value.is_empty())
            .map(|(_, value)| value.clone())
            .ok_or(QuarkError::InvalidResponse)?;
        parts.push((number, etag));
        number += 1;
    }
    let mut complete = String::from("<CompleteMultipartUpload>");
    for (number, etag) in parts {
        complete.push_str(&format!(
            "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
            xml_escape(&etag)
        ));
    }
    complete.push_str("</CompleteMultipartUpload>");
    snapshot.validate()?;
    let encoded_upload = aws_percent(&upload_id);
    let query = format!("uploadId={encoded_upload}");
    aws_checked(http.send_upload(request(
        "POST",
        &query,
        &query,
        complete.into_bytes().into(),
    )?)?)?;
    Ok(())
}

fn aws_checked(response: UploadHttpResponse) -> Result<UploadHttpResponse, QuarkError> {
    if !(200..300).contains(&response.status) || response.body.contains("<Error>") {
        Err(QuarkError::Provider(format!(
            "迅雷云盘上传服务拒绝请求（HTTP {}），请重试",
            response.status
        )))
    } else {
        Ok(response)
    }
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> Result<Vec<u8>, QuarkError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| QuarkError::InvalidResponse)?;
    mac.update(value);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn aws_percent(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn xml_tag(body: &str, tag: &str) -> Result<String, QuarkError> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open).ok_or(QuarkError::InvalidResponse)? + open.len();
    let end = body[start..]
        .find(&close)
        .ok_or(QuarkError::InvalidResponse)?
        + start;
    let value = &body[start..end];
    if value.is_empty() {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(value.into())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xunlei_gcid(path: &Path, size: u64) -> Result<String, QuarkError> {
    let mut block = 0x40000_u64;
    while size > block * 0x200 && block < 0x200000 {
        block <<= 1;
    }
    let mut file = std::fs::File::open(path)
        .map_err(|_| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?;
    let mut total = Sha1::new();
    let mut remaining = size;
    while remaining > 0 {
        let length = remaining.min(block) as usize;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)
            .map_err(|_| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?;
        total.update(Sha1::digest(&bytes));
        remaining = remaining.saturating_sub(length as u64);
    }
    Ok(hex(&total.finalize()).to_uppercase())
}

fn aws_timestamp() -> String {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (elapsed / 86_400) as i64;
    let seconds = elapsed % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}
