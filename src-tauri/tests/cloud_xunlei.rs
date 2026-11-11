#[path = "support/upload.rs"]
mod upload_support;
use versadown_lib::cloud::{
    xunlei::{
        XunleiClient, XunleiClientProfile, XunleiResumableUpload, XunleiSession, XunleiWebContext,
    },
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudUploadTransport, QuarkError,
    UploadHttpRequest, UploadHttpResponse,
};
use std::{io::Write, sync::Mutex};

struct RecordingHttp {
    responses: Mutex<Vec<CloudHttpResponse>>,
    requests: Mutex<Vec<CloudHttpRequest>>,
    upload_responses: Mutex<Vec<UploadHttpResponse>>,
    upload_requests: Mutex<Vec<UploadHttpRequest>>,
}

impl RecordingHttp {
    fn sequence(bodies: &[&str]) -> Self {
        Self {
            responses: Mutex::new(
                bodies
                    .iter()
                    .map(|body| CloudHttpResponse {
                        status: 200,
                        body: (*body).into(),
                        ..Default::default()
                    })
                    .rev()
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
            upload_responses: Mutex::new(Vec::new()),
            upload_requests: Mutex::new(Vec::new()),
        }
    }

    fn challenge() -> Self {
        Self::sequence(&[r#"{"url":"https://verify.xunlei.example/challenge"}"#])
    }

    fn requests(&self) -> Vec<CloudHttpRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn with_upload_responses(
        mut self,
        responses: impl IntoIterator<Item = UploadHttpResponse>,
    ) -> Self {
        let mut responses = responses.into_iter().collect::<Vec<_>>();
        responses.reverse();
        self.upload_responses = Mutex::new(responses);
        self
    }
}

impl CloudHttpTransport for RecordingHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop()
            .ok_or(QuarkError::InvalidResponse)
    }
}

impl CloudUploadTransport for RecordingHttp {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
        let request = upload_support::consume(request)?;
        self.upload_requests.lock().unwrap().push(request);
        self.upload_responses
            .lock()
            .unwrap()
            .pop()
            .ok_or(QuarkError::InvalidResponse)
    }
}

fn profile() -> XunleiClientProfile {
    XunleiClientProfile {
        client_id: "official-browser-client".into(),
        client_secret: "browser-client-secret".into(),
        device_id: "device-1".into(),
        client_version: "1.0.0".into(),
        package_name: "com.xunlei.pan.web".into(),
        user_agent: "Mozilla/5.0 XunleiBrowser/1.0".into(),
    }
}

fn modern_session() -> XunleiSession {
    let mut profile = profile();
    profile.client_id = "Xqp0kJBXWhwaTpB6".into();
    profile.package_name = "pan.xunlei.com".into();
    profile.client_secret.clear();
    let mut session = XunleiSession::with_access("modern-access", "modern-refresh", profile);
    session.user_id = "modern-user".into();
    session
}

// Break caught: modern SSO must not be gated or polluted by legacy web cookies.
#[test]
fn modern_web_directory_uses_bearer_without_legacy_context() {
    let http = RecordingHttp::sequence(&[r#"{"files":[],"next_page_token":""}"#]);
    let client = XunleiClient::new(modern_session(), &http);
    assert!(client.web_account().is_ok());
    assert!(client.directory("0").is_ok());
    let request = &http.requests()[0];
    let url = url::Url::parse(&request.url).unwrap();
    assert!(url
        .query_pairs()
        .any(|(key, value)| key == "parent_id" && value.is_empty()));
    assert!(!url.query_pairs().any(|(key, _)| key.starts_with('_')));
    for key in ["Credit-Key", "Peer-Id", "Session-Id", "Guid"] {
        assert!(!request.headers.contains_key(key));
    }
    assert_eq!(request.headers["Authorization"], "Bearer modern-access");
    assert_eq!(request.headers["X-Device-Id"], "device-1");
    assert_eq!(request.headers["X-Client-Id"], "Xqp0kJBXWhwaTpB6");
}

// Break caught: public web OAuth refresh omits secret and includes SDK statistics.
#[test]
fn modern_web_refreshes_without_client_secret() {
    let http = RecordingHttp::sequence(&[
        r#"{"error":"unauthenticated"}"#,
        r#"{"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}"#,
        r#"{"files":[],"next_page_token":""}"#,
    ]);
    let mut session = modern_session();
    session.expires_at = 1;
    let client = XunleiClient::new(session, &http);
    assert!(client.directory("0").is_ok());
    let requests = http.requests();
    assert!(requests[1].url.ends_with("/v1/auth/token"));
    let body: serde_json::Value =
        serde_json::from_str(requests[1].body.as_deref().unwrap()).unwrap();
    assert!(body.get("client_secret").is_none());
    assert_eq!(requests[1].headers["X-Sdk-Version"], "3.4.20");
    assert_eq!(requests[1].headers["X-Protocol-Version"], "301");
    assert_eq!(requests[1].headers["X-Action"], "401");
    assert_eq!(requests[2].headers["Authorization"], "Bearer rotated");
}

// Break caught: HTTP 403 and failed_precondition are ordinary provider failures.
#[test]
fn modern_web_classifies_service_errors_without_status_shortcuts() {
    for (code, error) in [(7, "permission_denied"), (9, "failed_precondition")] {
        let http =
            RecordingHttp::sequence(&[&format!(r#"{{"error_code":{code},"error":"{error}"}}"#)]);
        http.responses.lock().unwrap()[0].status = 403;
        assert!(matches!(
            XunleiClient::new(modern_session(), &http).directory("0"),
            Err(QuarkError::Provider(_))
        ));
    }
}

// Break caught: root destinations must map identically for all file mutations.
#[test]
fn modern_web_mutations_map_root_and_keep_official_paths() {
    let http = RecordingHttp::sequence(&["{}", "{}", "{}", "{}", "{}"]);
    let client = XunleiClient::new(modern_session(), &http);
    client.mkdir("0", "folder").unwrap();
    client.rename("id-1", "new").unwrap();
    client.move_files(&["id-1".into()], "0").unwrap();
    client.copy_files(&["id-1".into()], "0").unwrap();
    client.trash(&["id-1".into()]).unwrap();
    let requests = http.requests();
    for index in [0, 2, 3] {
        let body: serde_json::Value =
            serde_json::from_str(requests[index].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            if index == 0 {
                &body["parent_id"]
            } else {
                &body["to"]["parent_id"]
            },
            ""
        );
    }
    assert_eq!(
        requests[1].url,
        "https://api-pan.xunlei.com/drive/v1/files/id-1"
    );
    assert_eq!(
        requests[4].url,
        "https://api-pan.xunlei.com/drive/v1/files/id-1/trash"
    );
}

#[test]
fn xunlei_recycle_list_restore_and_delete_refreshes_the_client_session() {
    let http = RecordingHttp::sequence(&[
        r#"{"error":"unauthenticated"}"#,
        r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
        r#"{"files":[{"id":"trashed-id","name":"trashed.txt","kind":"drive#file","phase":"PHASE_TYPE_COMPLETE","trashed":true}],"next_page_token":""}"#,
        r#"{}"#,
        r#"{}"#,
    ]);
    let mut session = modern_session();
    session.expires_at = 0;
    let client = XunleiClient::new(session, &http);

    let bin = client.list_recycle_bin().unwrap();
    assert_eq!(bin.items[0].id, "trashed-id");
    assert_eq!(bin.items[0].parent_id, bin.id);
    client
        .restore_recycle_entries(&[bin.items[0].id.clone()])
        .unwrap();
    client
        .delete_recycle_entries(&[bin.items[0].id.clone()])
        .unwrap();

    assert_eq!(client.session().access_token, "rotated-access");
    assert_eq!(client.session().refresh_token, "rotated-refresh");
    let requests = http.requests();
    assert!(requests[1].url.ends_with("/v1/auth/token"));
    let list = &requests[2];
    assert_eq!(list.method, "GET");
    assert!(list.url.contains("/drive/v1/files?"));
    let query = url::Url::parse(&list.url).unwrap();
    assert!(query
        .query_pairs()
        .any(|(key, value)| { key == "filters" && value.contains(r#""trashed":{"eq":true}"#) }));
    assert!(requests[3].url.ends_with("/drive/v1/files:batchUntrash"));
    assert!(requests[4].url.ends_with("/drive/v1/files:batchDelete"));
    for request in [&requests[3], &requests[4]] {
        assert_eq!(request.method, "POST");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(request.body.as_deref().unwrap()).unwrap()
                ["ids"],
            serde_json::json!(["trashed-id"])
        );
    }
}

fn modern_signed_session(token: &str, expires_at: u64) -> XunleiSession {
    let mut value = serde_json::to_value(modern_session()).unwrap();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string();
    value["web_security"] = serde_json::json!({
        "captcha_token": token,
        "expires_at": expires_at,
        "captcha_sign": "official-signature",
        "timestamp": timestamp,
        "client_version": "1.92.98",
        "package_name": "pan.xunlei.com"
    });
    serde_json::from_value(value).unwrap()
}

// Break caught: expired operation tokens need shield initialization with genuine metadata.
#[test]
fn modern_web_refreshes_expired_captcha_for_the_requested_action() {
    let http = RecordingHttp::sequence(&[
        r#"{"captcha_token":"new-shield-token","expires_in":300}"#,
        r#"{"files":[],"next_page_token":""}"#,
        r#"{"files":[],"next_page_token":""}"#,
    ]);
    let session = modern_signed_session("old-shield-token", 1);
    let timestamp = serde_json::to_value(&session).unwrap()["web_security"]["timestamp"].clone();
    let client = XunleiClient::new(session, &http);
    client.directory("0").unwrap();
    client.directory("folder-1").unwrap();
    let requests = http.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].url,
        "https://xluser-ssl.xunlei.com/v1/shield/captcha/init"
    );
    let body: serde_json::Value =
        serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["action"], "GET:/drive/v1/files");
    assert_eq!(body["meta"]["captcha_sign"], "official-signature");
    assert_eq!(body["meta"]["timestamp"], timestamp);
    assert_eq!(body["meta"]["user_id"], "modern-user");
    assert_eq!(body["captcha_token"], "old-shield-token");
    assert_eq!(requests[1].headers["X-Captcha-Token"], "new-shield-token");
    assert_eq!(requests[2].headers["X-Captcha-Token"], "new-shield-token");
}

// Break caught: stale signatures must not be retried forever after the browser closes.
#[test]
fn modern_web_requires_fresh_signature_metadata_for_captcha_renewal() {
    let mut value = serde_json::to_value(modern_signed_session("expired-token", 1)).unwrap();
    value["web_security"]["timestamp"] = "1".into();
    let http = RecordingHttp::sequence(&[
        r#"{"captcha_token":"renewed","expires_in":300}"#,
        r#"{"files":[]}"#,
    ]);
    let client = XunleiClient::new(serde_json::from_value(value).unwrap(), &http);
    assert!(matches!(
        client.directory("0"),
        Err(QuarkError::XunleiVerificationUnavailable("metadata_expired"))
    ));
    assert!(http.requests().is_empty());
}

// Break caught: a supported official profile must renew verification after
// the authorization window closes, without depending on its captured timestamp.
#[test]
fn modern_web_supported_profile_renews_captcha_without_a_browser_signature() {
    let mut session = modern_session();
    session.profile.client_version = "1.92.98".into();
    let http = RecordingHttp::sequence(&[
        r#"{"captcha_token":"rust-shield-token","expires_in":300}"#,
        r#"{"files":[]}"#,
    ]);
    let client = XunleiClient::new(session, &http);
    client.directory("0").unwrap();
    let requests = http.requests();
    assert_eq!(
        requests[0].url,
        "https://xluser-ssl.xunlei.com/v1/shield/captcha/init"
    );
    let body: serde_json::Value =
        serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["action"], "GET:/drive/v1/files");
    assert_eq!(body["meta"]["client_version"], "1.92.98");
    assert_eq!(body["meta"]["package_name"], "pan.xunlei.com");
    let signature = body["meta"]["captcha_sign"].as_str().unwrap();
    assert!(signature.starts_with("1."));
    assert_eq!(signature.len(), 34);
    let timestamp = body["meta"]["timestamp"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let current = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(current.abs_diff(timestamp) < 10_000);
    assert_eq!(requests[1].headers["X-Captcha-Token"], "rust-shield-token");
}

// Independent vectors executed through the official webpage's 15-round MD5
// module chain catch salt order, missing fields, encoding, and case changes.
#[test]
fn modern_web_signature_matches_official_javascript_vectors() {
    let mut profile = modern_session().profile;
    profile.client_version = "1.92.98".into();
    for (device, timestamp, expected) in [
        (
            "0123456789abcdef",
            "1788975000000",
            "1.1b11bbf6b08458edaccf7acdaefbbb1e",
        ),
        (
            "synthetic-device",
            "1788975001000",
            "1.124ca821e508ff39cb555e7012101bee",
        ),
    ] {
        profile.device_id = device.into();
        assert_eq!(
            profile.web_captcha_signature(timestamp).as_deref(),
            Some(expected)
        );
    }
}

// Independent vectors evaluated by the current official public JS module 47.
#[test]
fn modern_web_19322_signature_matches_official_javascript_vectors() {
    let mut profile = modern_session().profile;
    profile.client_version = "1.93.22".into();
    for (device, timestamp, expected) in [
        ("synthetic-device", "1793678400000", "1.61380dd25ada7b2245e267fa7240f290"),
        ("test-device-2", "1793678400001", "1.bfef577f82619c14b89bf5e836ec0cdb"),
        ("00000000000000000000000000000000", "1700000000000", "1.84292e395f0e371e1e7fdc3921ca82dd"),
    ] {
        profile.device_id = device.into();
        assert_eq!(profile.web_captcha_signature(timestamp).as_deref(), Some(expected));
    }
}

#[test]
fn modern_web_19324_signature_matches_official_javascript_vectors() {
    let mut profile = modern_session().profile;
    profile.client_version = "1.93.24".into();
    for (device, timestamp, expected) in [
        (
            "synthetic-device",
            "1793678400000",
            "1.52e180e0ef7c34930059ac5114d7e7c1",
        ),
        (
            "test-device-2",
            "1793678400001",
            "1.88ddb9d6b2327d2580a669ce6a64a922",
        ),
        (
            "00000000000000000000000000000000",
            "1700000000000",
            "1.cf75b2644828932ff1403c87f370625f",
        ),
    ] {
        profile.device_id = device.into();
        assert_eq!(
            profile.web_captcha_signature(timestamp).as_deref(),
            Some(expected)
        );
    }
}

// Break caught: a supported profile must replace old captured metadata for a
// new action, even when the browser token itself still has time remaining.
#[test]
fn modern_web_supported_profile_replaces_stale_signature_for_a_new_action() {
    let mut session = modern_signed_session("read-token", u64::MAX);
    session.profile.client_version = "1.92.98".into();
    session.web_security.action = "GET:/drive/v1/files".into();
    session.web_security.timestamp = "1".into();
    session.web_security.captcha_sign = "stale-captured-signature".into();
    let http =
        RecordingHttp::sequence(&[r#"{"captcha_token":"write-token","expires_in":300}"#, "{}"]);
    let client = XunleiClient::new(session, &http);
    client.rename("file-1", "renamed").unwrap();
    let requests = http.requests();
    let body: serde_json::Value =
        serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["action"], "PATCH:/drive/v1/files/file-1");
    assert_ne!(body["meta"]["timestamp"], "1");
    assert_ne!(body["meta"]["captcha_sign"], "stale-captured-signature");
    assert_eq!(requests[1].headers["X-Captcha-Token"], "write-token");
}

// Break caught: reusing a web signature for unverified products or versions
// mixes client identities and must fail closed instead of inventing compatibility.
#[test]
fn modern_web_signature_is_limited_to_verified_profile_and_version() {
    let mut profile = modern_session().profile;
    profile.client_version = "1.92.98".into();
    for field in ["client_id", "package_name", "client_version", "device_id"] {
        let mut unsupported = profile.clone();
        match field {
            "client_id" => unsupported.client_id = "native-client".into(),
            "package_name" => unsupported.package_name = "com.xunlei.browser".into(),
            "client_version" => unsupported.client_version = "1.92.99".into(),
            _ => unsupported.device_id.clear(),
        }
        assert!(unsupported.web_captcha_signature("1788975000000").is_none());
    }
    assert!(profile.web_captcha_signature("").is_none());
    assert!(profile.web_captcha_signature("invalid timestamp").is_none());
    profile.client_version = "1.93.6".into();
    profile.device_id = "synthetic-device".into();
    assert_eq!(
        profile.web_captcha_signature("1788975001000").as_deref(),
        Some("1.bd4592fbb60e031cab7aaf80fa4bd37c")
    );
}

// Break caught: refreshed credentials must remain bound to the authorized account.
#[test]
fn modern_web_rejects_refresh_for_a_different_user() {
    let mut session = modern_session();
    session.expires_at = 1;
    let http = RecordingHttp::sequence(&[
        r#"{"error":"unauthenticated"}"#,
        r#"{"access_token":"foreign-token","sub":"foreign-user","expires_in":300}"#,
        r#"{"files":[]}"#,
    ]);
    let client = XunleiClient::new(session, &http);
    assert!(matches!(
        client.directory("0"),
        Err(QuarkError::AuthorizationExpired)
    ));
    assert_eq!(client.session().user_id, "modern-user");
    assert_eq!(http.requests().len(), 2);
}

// Break caught: captcha renewal for a changed action must use the actual write path.
#[test]
fn modern_web_captcha_action_tracks_mutations_without_reusing_a_read_action() {
    let mut value = serde_json::to_value(modern_signed_session("read-token", u64::MAX)).unwrap();
    value["web_security"]["action"] = "GET:/drive/v1/files".into();
    let http =
        RecordingHttp::sequence(&[r#"{"captcha_token":"write-token","expires_in":300}"#, "{}"]);
    let client = XunleiClient::new(serde_json::from_value(value).unwrap(), &http);
    client.rename("file-1", "name").unwrap();
    let requests = http.requests();
    let body: serde_json::Value =
        serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["action"], "PATCH:/drive/v1/files/file-1");
    assert_eq!(requests[1].headers["X-Captcha-Token"], "write-token");
}

#[test]
fn supervised_xunlei_keeps_action_ticket_preflight_but_never_replays_a_rejected_mutation() {
    use versadown_lib::cloud::{
        auth_supervisor::SessionSnapshot, CloudProvider, CredentialStore, MemoryCredentialStore,
        ProviderRegistry,
    };
    for rejected in [false, true] {
        let mut session =
            serde_json::to_value(modern_signed_session("read-token", u64::MAX)).unwrap();
        session["web_security"]["action"] = "GET:/drive/v1/files".into();
        let store = MemoryCredentialStore::default();
        store
            .save("xunlei", &serde_json::to_vec(&session).unwrap())
            .unwrap();
        let snapshot = SessionSnapshot::new(&store, CloudProvider::Xunlei).unwrap();
        let reply = if rejected {
            r#"{"error_code":16,"error":"unauthenticated"}"#
        } else {
            "{}"
        };
        let http = RecordingHttp::sequence(&[
            r#"{"captcha_token":"write-token","expires_in":300}"#,
            reply,
        ]);
        let result = ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &snapshot, &http)
            .rename("file-1", "name");
        if rejected {
            assert!(matches!(result, Err(QuarkError::AuthorizationExpired)));
        } else {
            result.unwrap();
        }
        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].url.ends_with("/v1/shield/captcha/init"));
        assert_eq!(requests[1].method, "PATCH");
        assert_eq!(requests[1].headers["X-Captcha-Token"], "write-token");
        let action: serde_json::Value =
            serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(action["action"], "PATCH:/drive/v1/files/file-1");
    }
}

// Break caught: a service captcha_invalid response must allow one bounded renewal.
#[test]
fn modern_web_retries_captcha_rejection_once_and_preserves_review_url() {
    let http = RecordingHttp::sequence(&[
        r#"{"error_code":4002,"error":"captcha_invalid"}"#,
        r#"{"captcha_token":"new-shield-token","expires_in":300}"#,
        r#"{"error_code":4001,"error":"captcha_required"}"#,
    ]);
    let client = XunleiClient::new(modern_signed_session("existing", u64::MAX), &http);
    assert!(matches!(
        client.directory("0"),
        Err(QuarkError::WebAuthorizationRequired)
    ));
    assert_eq!(http.requests().len(), 3);

    let review = RecordingHttp::sequence(&[
        r#"{"url":"https://xluser-ssl.xunlei.com/challenge","captcha_token":"review-token","expires_in":300}"#,
    ]);
    let client = XunleiClient::new(modern_signed_session("expired", 1), &review);
    assert!(matches!(
        client.directory("0"),
        Err(QuarkError::WebAuthorizationRequired)
    ));
    assert_eq!(
        serde_json::to_value(client.session()).unwrap()["web_security"]["verification_url"],
        "https://xluser-ssl.xunlei.com/challenge"
    );
    assert_eq!(review.requests().len(), 1);
}

fn web_context() -> XunleiWebContext {
    XunleiWebContext {
        session_id: "web-session".into(),
        peer_id: "official-peer".into(),
        device_id: "official-device".into(),
        credit_key: "official-credit".into(),
    }
}

// Break caught: omitting the native refresh exchange, token rotation, or either
// required authenticated-drive header makes the directory request invalid.
#[test]
fn xunlei_refreshes_with_official_client_profile_and_sends_native_headers() {
    let http = RecordingHttp::sequence(&[
        r#"{"access_token":"access","refresh_token":"next","expires_in":7200,"sub":"user-1"}"#,
        r#"{"files":[{"id":"f1","name":"archive.zip","kind":"drive#file","phase":"PHASE_TYPE_COMPLETE","size":32,"parent_id":"0"}],"next_page_token":""}"#,
    ]);
    let client = XunleiClient::new(XunleiSession::refresh("refresh", profile()), &http);

    let directory = client.directory("0").unwrap();

    assert_eq!(directory.items[0].name, "archive.zip");
    assert_eq!(client.session().refresh_token, "next");
    let requests = http.requests();
    assert!(requests[0].url.ends_with("/v1/auth/token"));
    let refresh: serde_json::Value =
        serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["client_id"], "official-browser-client");
    assert_eq!(refresh["client_secret"], "browser-client-secret");
    assert_eq!(
        requests[0].headers["X-Client-Id"],
        "official-browser-client"
    );
    assert_eq!(requests[0].headers["X-Client-Version"], "1.0.0");
    assert_eq!(requests[0].headers["X-Device-Id"], "device-1");
    assert_eq!(
        requests[0].headers["User-Agent"],
        "Mozilla/5.0 XunleiBrowser/1.0"
    );
    assert_eq!(requests[1].headers["Authorization"], "Bearer access");
    assert_eq!(requests[1].headers["X-Device-Id"], "device-1");
    assert_eq!(
        requests[1].headers["X-Client-Id"],
        "official-browser-client"
    );
    assert_eq!(requests[1].headers["X-Client-Version"], "1.0.0");
    assert_eq!(
        requests[1].headers["User-Agent"],
        "Mozilla/5.0 XunleiBrowser/1.0"
    );
}

// Break caught: Xunlei-specific expired-session codes would otherwise leak as
// generic provider errors and prevent the one native refresh retry.
#[test]
fn xunlei_expiry_codes_map_to_authorization_expired_before_generic_json_mapping() {
    for code in [4121, 10] {
        let http =
            RecordingHttp::sequence(&[&format!(r#"{{"error_code":{code},"error":"expired"}}"#)]);
        assert!(matches!(
            XunleiClient::new(XunleiSession::with_access("access", "", profile()), &http)
                .directory("0"),
            Err(QuarkError::AuthorizationExpired)
        ));
    }
}

// Break caught: diagnostics must not disclose session tokens, client secrets,
// or one-shot resumable-upload credentials.
#[test]
fn xunlei_sensitive_debug_output_is_redacted() {
    let mut session = XunleiSession::with_access("access-secret", "refresh-secret", profile());
    session.security_token = "captcha-secret".into();
    session.web_context = XunleiWebContext {
        session_id: "session-cookie-secret".into(),
        peer_id: "peer-cookie-secret".into(),
        device_id: "device-cookie-secret".into(),
        credit_key: "credit-cookie-secret".into(),
    };
    let upload = XunleiResumableUpload {
        upload_type: "UPLOAD_TYPE_RESUMABLE".into(),
        kind: "drive#resumable".into(),
        provider: "aws".into(),
        access_key_id: "access-key-secret".into(),
        access_key_secret: "upload-key-secret".into(),
        bucket: "bucket".into(),
        endpoint: "https://oss.xunlei.example".into(),
        expiration: "2030-01-01T00:00:00Z".into(),
        key: "object-key".into(),
        security_token: "upload-security-secret".into(),
    };

    let debug = format!("{session:?} {upload:?}");
    for sensitive in [
        "access-secret",
        "refresh-secret",
        "browser-client-secret",
        "captcha-secret",
        "session-cookie-secret",
        "peer-cookie-secret",
        "device-cookie-secret",
        "credit-cookie-secret",
        "access-key-secret",
        "upload-key-secret",
        "upload-security-secret",
    ] {
        assert!(!debug.contains(sensitive));
    }
}

#[test]
fn xunlei_web_session_retains_a_current_browser_access_token_without_a_client_secret() {
    // Break caught: requiring a native-only client secret rejects a valid
    // browser session before the command layer can save it and close the
    // official authorization window.
    let mut web_profile = profile();
    web_profile.client_secret.clear();

    let session = XunleiSession::from_web_authorization(
        "current-browser-access",
        "browser-refresh",
        "web-user-42",
        "",
        web_profile,
        0,
    );

    assert!(session.is_ok());
    assert_eq!(session.unwrap().access_token, "current-browser-access");
}

#[test]
fn xunlei_web_session_without_native_profile_uses_the_current_browser_bearer() {
    // The official browser client exposes the user session but deliberately
    // does not expose its native client identity.  A completed browser login
    // must still be storable and use its current bearer for the first drive
    // request instead of remaining permanently pending.
    let mut session = XunleiSession::from_web_authorization(
        "current-browser-access",
        "browser-refresh",
        "web-user-42",
        "",
        XunleiClientProfile::default(),
        0,
    )
    .expect("official browser session without native identity is usable");
    session.web_context = web_context();
    let http = RecordingHttp::sequence(&[r#"{"files":[],"next_page_token":""}"#]);
    let client = XunleiClient::new(session, &http);

    assert!(client.directory("0").is_ok());
    let request = http.requests().pop().expect("one directory request");
    assert_eq!(
        request.headers.get("Authorization"),
        Some(&"Bearer current-browser-access".into())
    );
    assert!(!request.headers.contains_key("X-Client-Id"));
}

#[test]
fn xunlei_old_web_session_without_browser_cookie_context_requires_reauthorization() {
    // A pre-cookie-capture session must fail closed. Sending guessed peer or
    // session headers reaches Xunlei but is rejected as `invalid_argument`.
    let session = XunleiSession::from_web_authorization(
        "current-browser-access",
        "browser-refresh",
        "web-user-42",
        "",
        XunleiClientProfile::default(),
        0,
    )
    .unwrap();
    let http = RecordingHttp::sequence(&[r#"{"files":[],"next_page_token":""}"#]);

    assert!(matches!(
        XunleiClient::new(session, &http).directory("0"),
        Err(QuarkError::WebAuthorizationRequired)
    ));
    assert!(http.requests().is_empty());
}

#[test]
fn xunlei_old_web_session_without_browser_cookie_context_cannot_restore_an_account() {
    let session = XunleiSession::from_web_authorization(
        "current-browser-access",
        "browser-refresh",
        "web-user-42",
        "",
        XunleiClientProfile::default(),
        0,
    )
    .unwrap();
    let http = RecordingHttp::sequence(&[]);

    assert!(matches!(
        XunleiClient::new(session, &http).web_account(),
        Err(QuarkError::WebAuthorizationRequired)
    ));
    assert!(http.requests().is_empty());
}

#[test]
fn xunlei_web_session_prefers_the_captured_sessionid_over_the_bearer() {
    let mut session = XunleiSession::from_web_authorization(
        "current-browser-access",
        "browser-refresh",
        "web-user-42",
        "",
        XunleiClientProfile {
            user_agent: "OfficialBrowser/1.0".into(),
            ..XunleiClientProfile::default()
        },
        0,
    )
    .unwrap();
    session.web_context = XunleiWebContext {
        session_id: "web-session".into(),
        peer_id: "official-peer".into(),
        device_id: "official-device".into(),
        credit_key: "official-credit".into(),
    };
    let http = RecordingHttp::sequence(&[r#"{"files":[],"next_page_token":""}"#]);

    XunleiClient::new(session, &http).directory("0").unwrap();

    let request = http.requests().pop().unwrap();
    assert_eq!(
        request.headers.get("Peer-Id"),
        Some(&"official-peer".into())
    );
    assert_eq!(
        request.headers.get("Guid"),
        Some(&format!("{:x}", md5::compute(b"official-peer")))
    );
    assert_eq!(request.headers.get("User-Id"), Some(&"web-user-42".into()));
    assert_eq!(
        request.headers.get("Session-Id"),
        Some(&"web-session".into())
    );
    assert_eq!(
        request.headers.get("Credit-Key"),
        Some(&"official-credit".into())
    );
    assert_eq!(request.headers.get("Version-Code"), Some(&"1.92.98".into()));
    assert_eq!(
        request.headers.get("User-Agent"),
        Some(&"OfficialBrowser/1.0".into())
    );
    assert!(!request.headers.contains_key("Cookie"));
    let query = url::Url::parse(&request.url)
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    assert!(query.contains(&("_sessid".into(), "web-session".into())));
    assert!(query.contains(&("_uid".into(), "web-user-42".into())));
    assert!(query
        .iter()
        .any(|(key, value)| key == "_h[]" && value == "Credit-Key: official-credit"));
}

#[test]
fn xunlei_web_context_without_sessionid_falls_back_to_the_current_bearer() {
    let mut session = XunleiSession::from_web_authorization(
        "current-browser-access",
        "browser-refresh",
        "web-user-42",
        "",
        XunleiClientProfile::default(),
        0,
    )
    .unwrap();
    session.web_context = XunleiWebContext {
        session_id: String::new(),
        peer_id: "official-peer".into(),
        device_id: "official-device".into(),
        credit_key: "official-credit".into(),
    };
    let http = RecordingHttp::sequence(&[r#"{"files":[],"next_page_token":""}"#]);

    XunleiClient::new(session, &http).directory("0").unwrap();

    let request = http.requests().pop().unwrap();
    assert_eq!(
        request.headers.get("Session-Id"),
        Some(&"current-browser-access".into())
    );
}

#[test]
fn xunlei_web_session_with_real_browser_identity_but_no_secret_uses_current_bearer() {
    // The official web client may expose its browser device/profile without
    // exposing a native refresh secret. That identity must be forwarded to
    // the drive request, but it must never make a still-current web bearer
    // look refreshable or expired.
    let mut web_profile = profile();
    web_profile.client_secret.clear();
    let mut session = XunleiSession::from_web_authorization(
        "current-browser-access",
        "browser-refresh",
        "web-user-42",
        "",
        web_profile,
        0,
    )
    .expect("official web identity without a native secret is usable");
    session.web_context = web_context();
    let http = RecordingHttp::sequence(&[r#"{"files":[],"next_page_token":""}"#]);
    let client = XunleiClient::new(session, &http);

    assert!(client.directory("0").is_ok());
    let request = http.requests().pop().expect("one drive request");
    assert_eq!(
        request.headers.get("Authorization"),
        Some(&"Bearer current-browser-access".into())
    );
    assert_eq!(
        request.headers.get("X-Client-Id"),
        Some(&"official-browser-client".into())
    );
}

#[test]
fn xunlei_web_session_builds_a_display_account_without_network_or_refresh() {
    // Break caught: using account() to finish browser authorization tries a
    // native refresh requiring a secret that the web client never exposes.
    let mut web_profile = profile();
    web_profile.client_secret.clear();
    let mut session = XunleiSession::from_web_authorization(
        "current-browser-access",
        "browser-refresh",
        "web-user-42",
        "",
        web_profile,
        0,
    )
    .unwrap();
    session.web_context = web_context();
    let http = RecordingHttp::sequence(&[]);
    let client = XunleiClient::new(session, &http);

    let account = client.web_account().unwrap();

    assert_eq!(account.id, "web-user-42");
    assert_eq!(account.display_name, "web-user-42");
    assert!(http.requests().is_empty());
}

// Break caught: treating an interactive security challenge as an ordinary
// provider failure prevents the UI from returning the user to official web auth.
#[test]
fn xunlei_security_challenge_is_web_auth_not_link_expiry() {
    assert!(matches!(
        XunleiClient::new(
            XunleiSession::with_access("access", "refresh", profile()),
            &RecordingHttp::challenge()
        )
        .directory("0"),
        Err(QuarkError::WebAuthorizationRequired)
    ));
}

// Break caught: accepting a non-TLS content link would expose downloads to a
// transport downgrade rather than returning a fresh secure URL.
#[test]
fn xunlei_download_returns_only_fresh_https_content_links() {
    let https = RecordingHttp::sequence(&[
        r#"{"id":"f1","name":"archive.zip","web_content_link":"https://cdn.xunlei.example/archive.zip?signature=short"}"#,
    ]);
    let client = XunleiClient::new(
        XunleiSession::with_access("access", "refresh", profile()),
        &https,
    );
    assert_eq!(
        client.download("f1").unwrap(),
        (
            "https://cdn.xunlei.example/archive.zip?signature=short".into(),
            "archive.zip".into()
        )
    );
    assert_eq!(https.requests()[0].headers["X-Device-Id"], "device-1");

    let insecure = RecordingHttp::sequence(&[
        r#"{"id":"f1","name":"archive.zip","web_content_link":"http://cdn.xunlei.example/archive.zip"}"#,
    ]);
    assert!(matches!(
        XunleiClient::new(
            XunleiSession::with_access("access", "refresh", profile()),
            &insecure
        )
        .download("f1"),
        Err(QuarkError::InvalidResponse)
    ));
}

#[test]
fn modern_xunlei_download_waits_for_a_complete_files_empty_link() {
    let http = RecordingHttp::sequence(&[
        r#"{"id":"f1","name":"archive.zip","phase":"PHASE_TYPE_COMPLETE","web_content_link":""}"#,
        r#"{"id":"f1","name":"archive.zip","phase":"PHASE_TYPE_COMPLETE","web_content_link":"https://cdn.example/ready"}"#,
    ]);
    let result = XunleiClient::new(modern_session(), &http).download("f1");
    assert_eq!(
        result.unwrap(),
        ("https://cdn.example/ready".into(), "archive.zip".into())
    );
    assert_eq!(http.requests().len(), 2);
}

#[test]
fn modern_xunlei_download_waits_only_for_known_processing_phases() {
    for phase in ["PHASE_TYPE_PENDING", "PHASE_TYPE_RUNNING"] {
        let waiting =
            serde_json::json!({"id":"f1", "name":"archive.zip", "phase":phase}).to_string();
        let http = RecordingHttp::sequence(&[
            &waiting,
            r#"{"id":"f1","name":"archive.zip","phase":"PHASE_TYPE_COMPLETE","web_content_link":"https://cdn.example/ready"}"#,
        ]);
        assert!(XunleiClient::new(modern_session(), &http)
            .download("f1")
            .is_ok());
        assert_eq!(http.requests().len(), 2);
    }
}

#[test]
fn modern_xunlei_download_does_not_retry_invalid_metadata_or_links() {
    for response in [
        r#"{"name":"archive.zip","phase":"PHASE_TYPE_COMPLETE","web_content_link":"http://cdn.example/file"}"#,
        r#"{"name":"archive.zip","phase":"PHASE_TYPE_COMPLETE","web_content_link":42}"#,
        r#"{"phase":"PHASE_TYPE_COMPLETE","web_content_link":""}"#,
        r#"{"name":"archive.zip","phase":"PHASE_TYPE_ERROR","web_content_link":""}"#,
        r#"{"name":"archive.zip","phase":"unexpected","web_content_link":""}"#,
    ] {
        let http = RecordingHttp::sequence(&[response]);
        assert!(matches!(
            XunleiClient::new(modern_session(), &http).download("f1"),
            Err(QuarkError::InvalidResponse)
        ));
        assert_eq!(http.requests().len(), 1);
    }
    let native = RecordingHttp::sequence(&[
        r#"{"name":"archive.zip","phase":"PHASE_TYPE_COMPLETE","web_content_link":""}"#,
    ]);
    assert!(matches!(
        XunleiClient::new(
            XunleiSession::with_access("access", "refresh", profile()),
            &native
        )
        .download("f1"),
        Err(QuarkError::InvalidResponse)
    ));
    assert_eq!(native.requests().len(), 1);
}

#[test]
fn modern_xunlei_download_ready_link_uses_one_request_and_stops_on_http_errors() {
    let ready = RecordingHttp::sequence(&[
        r#"{"id":"f1","name":"archive.zip","phase":"PHASE_TYPE_COMPLETE","web_content_link":"https://cdn.example/ready"}"#,
    ]);
    assert!(XunleiClient::new(modern_session(), &ready)
        .download("f1")
        .is_ok());
    assert_eq!(ready.requests().len(), 1);

    let http = RecordingHttp::sequence(&[
        r#"{"id":"f1","name":"archive.zip","phase":"PHASE_TYPE_COMPLETE","web_content_link":""}"#,
        "{}",
    ]);
    http.responses.lock().unwrap()[0].status = 500;
    assert!(matches!(
        XunleiClient::new(modern_session(), &http).download("f1"),
        Err(QuarkError::Provider(_))
    ));
    assert_eq!(http.requests().len(), 2);
}

// Break caught: changing a native mutation endpoint or payload would silently
// perform the wrong file operation or the wrong destination.
#[test]
fn xunlei_file_mutations_and_upload_initialization_use_native_drive_contracts() {
    let http = RecordingHttp::sequence(&[
        "{}",
        "{}",
        "{}",
        "{}",
        "{}",
        "{}",
        r#"{"upload_type":"UPLOAD_TYPE_RESUMABLE","resumable":{"kind":"drive#resumable","provider":"aws","params":{"access_key_id":"key-id","access_key_secret":"key-secret","bucket":"bucket","endpoint":"https://oss.xunlei.example","expiration":"2030-01-01T00:00:00Z","key":"object-key","security_token":"upload-token"}}}"#,
    ]);
    let client = XunleiClient::new(
        XunleiSession::with_access("access", "refresh", profile()),
        &http,
    );
    let ids = vec!["file-1".to_owned(), "folder-2".to_owned()];

    client.mkdir("parent", "new-folder").unwrap();
    client.rename("file-1", "renamed.zip").unwrap();
    client.move_files(&ids, "destination").unwrap();
    client.copy_files(&ids, "destination").unwrap();
    client.trash(&ids).unwrap();
    let upload = client
        .upload_init("parent", "upload.bin", 64, "content-gcid")
        .unwrap();
    assert_eq!(upload.provider, "aws");
    assert_eq!(upload.endpoint, "https://oss.xunlei.example");
    assert_eq!(upload.key, "object-key");

    let requests = http.requests();
    assert!(requests[0].url.ends_with("/drive/v1/files"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(requests[0].body.as_deref().unwrap()).unwrap(),
        serde_json::json!({"kind":"drive#folder","parent_id":"parent","name":"new-folder"})
    );
    assert_eq!(requests[1].method, "PATCH");
    assert!(requests[1].url.ends_with("/drive/v1/files/file-1"));
    assert!(requests[2].url.ends_with("/drive/v1/files:batchMove"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(requests[2].body.as_deref().unwrap()).unwrap()
            ["to"]["parent_id"],
        "destination"
    );
    assert!(requests[3].url.ends_with("/drive/v1/files:batchCopy"));
    assert_eq!(requests[4].method, "PATCH");
    assert!(requests[4].url.ends_with("/drive/v1/files/file-1/trash"));
    assert_eq!(requests[4].body.as_deref(), Some("{}"));
    assert_eq!(requests[5].method, "PATCH");
    assert!(requests[5].url.ends_with("/drive/v1/files/folder-2/trash"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(requests[6].body.as_deref().unwrap()).unwrap(),
        serde_json::json!({"kind":"drive#file","parent_id":"parent","name":"upload.bin","size":64,"hash":"content-gcid","upload_type":"UPLOAD_TYPE_RESUMABLE"})
    );
}

// Break caught: showing an unfinished offline/upload task as a ready cloud
// file makes a non-downloadable object available to the user.
#[test]
fn xunlei_directory_requests_and_maps_only_complete_downloadable_files() {
    let http = RecordingHttp::sequence(&[
        r#"{"files":[{"id":"ready","name":"ready.zip","kind":"drive#file","phase":"PHASE_TYPE_COMPLETE","size":"7"},{"id":"running","name":"running.zip","kind":"drive#file","phase":"PHASE_TYPE_RUNNING","size":"3"}],"next_page_token":""}"#,
    ]);
    let client = XunleiClient::new(
        XunleiSession::with_access("access", "refresh", profile()),
        &http,
    );

    let directory = client.directory("0").unwrap();

    assert!(directory.items.iter().any(|item| item.id == "ready"));
    assert!(directory.items.iter().any(|item| {
        item.id == "__versa_down_xunlei_recycle_bin__"
            && item.parent_id == "0"
            && item.name == "回收站"
            && item.kind == "folder"
    }));
    let request = http.requests().pop().unwrap();
    assert!(request.url.contains("__type=drive"));
    assert!(request.url.contains("with_audit=true"));
    assert!(request
        .url
        .contains("%22phase%22%3A%7B%22eq%22%3A%22PHASE_TYPE_COMPLETE%22%7D"));
}

#[test]
fn xunlei_upload_file_signs_https_s3_multipart_transfer_and_completes_it() {
    let mut local = tempfile::NamedTempFile::new().unwrap();
    local.write_all(b"xunlei-bytes").unwrap();
    let http = RecordingHttp::sequence(&[
        r#"{"upload_type":"UPLOAD_TYPE_RESUMABLE","file":{"id":"file-1","parent_id":"parent","name":"upload.bin","kind":"drive#file","size":12},"resumable":{"kind":"drive#resumable","provider":"aws","params":{"access_key_id":"access-key","access_key_secret":"secret-key","bucket":"bucket","endpoint":"https://s3.xunlei.example","expiration":"2030-01-01T00:00:00Z","key":"objects/upload.bin","security_token":"session-token"}}}"#,
    ])
    .with_upload_responses([
        UploadHttpResponse {
            status: 200,
            headers: Default::default(),
            body: "<InitiateMultipartUploadResult><UploadId>upload-id</UploadId></InitiateMultipartUploadResult>".into(),
        },
        UploadHttpResponse {
            status: 200,
            headers: [("etag".into(), "part-etag".into())].into(),
            body: String::new(),
        },
        UploadHttpResponse {
            status: 200,
            headers: Default::default(),
            body: "<CompleteMultipartUploadResult/>".into(),
        },
    ]);
    let client = XunleiClient::new(
        XunleiSession::with_access("access", "refresh", profile()),
        &http,
    );

    let file = client.upload_file("parent", local.path()).unwrap();

    assert_eq!(file.id, "file-1");
    let uploads = http.upload_requests.lock().unwrap();
    assert_eq!(uploads.len(), 3);
    assert_eq!(uploads[0].method, "POST");
    assert_eq!(
        uploads[0].url,
        "https://bucket.s3.xunlei.example/objects/upload.bin?uploads"
    );
    assert!(
        uploads[0].headers["Authorization"].starts_with("AWS4-HMAC-SHA256 Credential=access-key/")
    );
    assert_eq!(uploads[1].method, "PUT");
    assert!(uploads[1].url.contains("partNumber=1&uploadId=upload-id"));
    assert_eq!(upload_support::bytes(&uploads[1].body), b"xunlei-bytes");
    assert_eq!(uploads[2].method, "POST");
    assert!(upload_support::bytes(&uploads[2].body)
        .windows("part-etag".len())
        .any(|part| part == b"part-etag"));
}

#[test]
fn account_card_reads_official_name_and_active_download_membership_together() {
    let http = RecordingHttp::sequence(&[
        r#"{"sub":"modern-user","name":"中文昵称","username":"123456","vip_info":[{"user_vas":306,"is_vip":1,"level":"9"},{"user_vas":"2","is_vip":"1","vas_type":"5","level":"1"}]}"#,
    ]);
    let profile = XunleiClient::new(modern_session(), &http)
        .account_card_profile()
        .unwrap();
    assert_eq!(
        serde_json::to_value(profile).unwrap(),
        serde_json::json!({
            "accountId":"modern-user","nickname":"中文昵称","membership":{"kind":"svip","level":1}
        })
    );
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://xluser-ssl.xunlei.com/v1/user/me");
}

#[test]
fn account_card_distinguishes_unknown_expired_and_cloud_membership() {
    for (fields, expected) in [
        (r#"{}"#, serde_json::Value::Null),
        (
            r#"{"vip_info":[{"user_vas":2,"is_vip":0,"vas_type":5,"level":8}]}"#,
            serde_json::json!({"kind":"none","level":null}),
        ),
        (
            r#"{"vip_info":[{"user_vas":306,"is_vip":1,"level":3}]}"#,
            serde_json::json!({"kind":"cloud","level":3}),
        ),
    ] {
        let http = RecordingHttp::sequence(&[fields]);
        let response = XunleiClient::new(modern_session(), &http)
            .account_card_profile()
            .unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap()["membership"],
            expected
        );
    }
    let http = RecordingHttp::sequence(&[r#"{"sub":"different-account","name":"别的账号"}"#]);
    assert!(matches!(
        XunleiClient::new(modern_session(), &http).account_card_profile(),
        Err(QuarkError::AccountMismatch)
    ));
}
