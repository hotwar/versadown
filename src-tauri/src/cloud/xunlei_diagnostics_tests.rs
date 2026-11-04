//! Offline characterization of a readable drive with unavailable operations.
//! All sessions and HTTP responses are synthetic; no system credentials/network.
use super::*;
use std::sync::Mutex;

struct OfflineHttp {
    requests: Mutex<Vec<(String, String)>>,
}

impl OfflineHttp {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl CloudHttpTransport for OfflineHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let path = url::Url::parse(&request.url).unwrap().path().to_owned();
        self.requests
            .lock()
            .unwrap()
            .push((request.method.into(), path.clone()));
        let body = match (request.method, path.as_str()) {
            ("GET", "/drive/v1/files") => json!({"files":[]}),
            ("GET", "/drive/v1/files/offline-file") => json!({
                "id":"offline-file", "name":"fixture.txt",
                "web_content_link":"https://cdn.example/fixture.txt"
            }),
            ("POST", "/v1/shield/captcha/init") => json!({
                "captcha_token":"synthetic-operation-ticket", "expires_in":600
            }),
            ("GET", "/drive/v1/share/list") => json!({"data":[]}),
            ("POST", "/drive/v1/files") => json!({"file":{
                "id":"offline-created", "name":"fixture.txt", "kind":"drive#file",
                "phase":"PHASE_TYPE_COMPLETE", "size":"7"
            }}),
            _ => panic!("unexpected offline request: {} {}", request.method, path),
        };
        Ok(CloudHttpResponse {
            status: 200,
            body: body.to_string(),
            ..Default::default()
        })
    }
}

impl CloudUploadTransport for OfflineHttp {
    fn send_upload(&self, _: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
        panic!("offline fixture must never transfer file bytes")
    }
}

fn session(metadata_age: Option<u64>) -> XunleiSession {
    let mut session = XunleiSession::with_access(
        "synthetic-access",
        "",
        XunleiClientProfile {
            client_id: WEB_CLIENT_ID.into(),
            client_version: "unsupported-test-version".into(),
            package_name: WEB_PACKAGE.into(),
            device_id: "synthetic-device".into(),
            user_agent: "offline-test".into(),
            ..Default::default()
        },
    );
    session.user_id = "synthetic-user".into();
    session.expires_at = now() + 3600;
    session.web_security = XunleiWebSecurity {
        captcha_token: "synthetic-root-ticket".into(),
        expires_at: now() + 600,
        action: "GET:/drive/v1/files".into(),
        ..Default::default()
    };
    if let Some(age) = metadata_age {
        session.web_security.captcha_sign = "synthetic-signature".into();
        session.web_security.timestamp = ((now() - age) * 1000).to_string();
        session.web_security.client_version = session.profile.client_version.clone();
        session.web_security.package_name = WEB_PACKAGE.into();
    }
    session
}

fn check_operations(metadata_age: Option<u64>) {
    for supervised in [false, true] {
        for operation in ["mkdir", "upload"] {
            let http = OfflineHttp::new();
            let client = XunleiClient::new(session(metadata_age), &http);
            let client = if supervised {
                client.supervised()
            } else {
                client
            };
            client.directory("0").unwrap();
            client.download("offline-file").unwrap();
            assert_eq!(http.requests.lock().unwrap().len(), 2);
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join("fixture.txt");
            std::fs::write(&file, b"fixture").unwrap();
            let result = match operation {
                "mkdir" => client.mkdir("0", "offline-test-folder"),
                "upload" => client.upload_file("0", &file).map(|_| ()),
                _ => unreachable!(),
            };
            result.unwrap();
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(
                requests.last().unwrap(),
                &("POST".into(), "/drive/v1/files".into())
            );
        }
    }
}

#[test]
fn missing_metadata_sends_mutation_with_existing_ticket() {
    check_operations(None);
}

#[test]
fn stale_metadata_sends_mutation_with_existing_ticket() {
    check_operations(Some(301));
}

#[test]
fn fallback_mutation_preserves_server_challenge_and_is_never_replayed() {
    struct Challenge(std::sync::atomic::AtomicUsize);
    impl CloudHttpTransport for Challenge {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(request.method, "POST");
            assert_eq!(request.url, "https://api-pan.xunlei.com/drive/v1/files");
            assert_eq!(request.headers["X-Captcha-Token"], "synthetic-root-ticket");
            Ok(CloudHttpResponse {
                status: 412,
                body: r#"{"error":"captcha_required"}"#.into(),
                ..Default::default()
            })
        }
    }
    let http = Challenge(std::sync::atomic::AtomicUsize::new(0));
    assert!(matches!(
        XunleiClient::new(session(None), &http).mkdir("0", "fixture"),
        Err(QuarkError::WebAuthorizationRequired)
    ));
    assert_eq!(http.0.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn expired_root_ticket_is_not_sent_for_a_mutation() {
    let http = OfflineHttp::new();
    let mut state = session(None);
    state.web_security.expires_at = now();
    assert!(matches!(
        XunleiClient::new(state, &http).mkdir("0", "fixture"),
        Err(QuarkError::XunleiVerificationUnavailable(
            "metadata_missing"
        ))
    ));
    assert!(http.requests.lock().unwrap().is_empty());
}

#[test]
fn fresh_metadata_does_not_replace_the_working_ticket_before_mutations() {
    check_operations(Some(0));
}

#[test]
fn supervised_write_renews_a_server_rejected_ticket_and_finishes_without_user_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct RejectedWrite {
        writes: AtomicUsize,
        renewals: AtomicUsize,
        repeat_rejection: bool,
        rejection_status: u16,
    }
    impl CloudHttpTransport for RejectedWrite {
        fn send(&self, r: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            if r.url.ends_with("/v1/shield/captcha/init") {
                self.renewals.fetch_add(1, Ordering::SeqCst);
                let body: Value = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
                assert_eq!(body["action"], "POST:/drive/v1/files");
                return Ok(CloudHttpResponse {
                    status: 200,
                    body: r#"{"captcha_token":"renewed-write-ticket","expires_in":300}"#.into(),
                    ..Default::default()
                });
            }
            assert_eq!(r.method, "POST");
            assert_eq!(r.url, "https://api-pan.xunlei.com/drive/v1/files");
            let attempt = self.writes.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                r.headers["X-Captcha-Token"],
                if attempt == 0 {
                    "synthetic-root-ticket"
                } else {
                    "renewed-write-ticket"
                }
            );
            Ok(if attempt == 0 || self.repeat_rejection {
                CloudHttpResponse {
                    status: self.rejection_status,
                    body: r#"{"error":"captcha_invalid","error_code":9}"#.into(),
                    ..Default::default()
                }
            } else {
                CloudHttpResponse {
                    status: 200,
                    body: r#"{"file":{"id":"created","name":"fixture.txt","kind":"drive#file","phase":"PHASE_TYPE_COMPLETE","size":"7"}}"#.into(),
                    ..Default::default()
                }
            })
        }
    }
    impl CloudUploadTransport for RejectedWrite {
        fn send_upload(&self, _: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
            panic!("completed synthetic upload must not transfer bytes")
        }
    }
    for operation in ["mkdir", "upload"] {
        for (repeat_rejection, status) in [(false, 400), (true, 400), (false, 500)] {
            let http = RejectedWrite {
                writes: AtomicUsize::new(0),
                renewals: AtomicUsize::new(0),
                repeat_rejection,
                rejection_status: status,
            };
            let mut state = session(None);
            state.profile.client_version = "1.93.6".into();
            let client = XunleiClient::new(state, &http).supervised();
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join("fixture.txt");
            std::fs::write(&file, b"fixture").unwrap();
            let result = if operation == "mkdir" {
                client.mkdir("0", "fixture")
            } else {
                client.upload_file("0", &file).map(|_| ())
            };
            assert_eq!(
                result.is_ok(),
                !repeat_rejection && status == 400,
                "{operation}: {result:?}"
            );
            assert_eq!(
                http.renewals.load(Ordering::SeqCst),
                usize::from(status == 400)
            );
            assert_eq!(
                http.writes.load(Ordering::SeqCst),
                if status == 400 { 2 } else { 1 }
            );
        }
    }
}

#[test]
fn supervised_write_never_retries_ordinary_ambiguous_or_interactive_failures() {
    #[derive(Clone, Copy, Debug)]
    enum Failure {
        Precondition,
        Transport,
        HumanChallenge,
    }
    struct RejectedWrite {
        failure: Failure,
        attempts: Mutex<usize>,
    }
    impl CloudHttpTransport for RejectedWrite {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            *self.attempts.lock().unwrap() += 1;
            assert_eq!(request.method, "POST");
            assert_eq!(request.url, "https://api-pan.xunlei.com/drive/v1/files");
            let body = match self.failure {
                Failure::Precondition => {
                    json!({"error":"failed_precondition", "error_code":9})
                }
                Failure::Transport => {
                    return Err(QuarkError::Transport("synthetic response timeout".into()));
                }
                Failure::HumanChallenge => json!({
                    "error":"captcha_invalid", "error_code":9,
                    "url":"https://xluser-ssl.xunlei.com/challenge"
                }),
            };
            Ok(CloudHttpResponse {
                status: 400,
                body: body.to_string(),
                ..Default::default()
            })
        }
    }
    impl CloudUploadTransport for RejectedWrite {
        fn send_upload(&self, _: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
            panic!("a rejected upload initialization must not transfer bytes")
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("fixture.txt");
    std::fs::write(&file, b"fixture").unwrap();
    for operation in ["mkdir", "upload"] {
        for failure in [
            Failure::Precondition,
            Failure::Transport,
            Failure::HumanChallenge,
        ] {
            let http = RejectedWrite {
                failure,
                attempts: Mutex::new(0),
            };
            let mut state = session(None);
            state.profile.client_version = "1.93.6".into();
            let client = XunleiClient::new(state, &http).supervised();
            let result = if operation == "mkdir" {
                client.mkdir("0", "fixture")
            } else {
                client.upload_file("0", &file).map(|_| ())
            };
            assert!(
                matches!(
                    (failure, &result),
                    (Failure::Precondition, Err(QuarkError::Provider(_)))
                        | (Failure::Transport, Err(QuarkError::Transport(_)))
                        | (
                            Failure::HumanChallenge,
                            Err(QuarkError::WebAuthorizationRequired)
                        )
                ),
                "{operation} / {failure:?}: {result:?}"
            );
            assert_eq!(*http.attempts.lock().unwrap(), 1);
        }
    }
}

#[test]
fn supervised_upload_later_failure_never_repeats_initialization_or_transfer() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Upload {
        requests: Mutex<Vec<(String, String)>>,
        transfers: AtomicUsize,
        fail_transfer: bool,
    }
    impl CloudHttpTransport for Upload {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            let path = url::Url::parse(&request.url).unwrap().path().to_owned();
            self.requests
                .lock()
                .unwrap()
                .push((request.method.into(), path.clone()));
            let (status, body) = match (request.method, path.as_str()) {
                ("POST", "/drive/v1/files") => (
                    200,
                    json!({
                        "file": {"id":"pending-file", "name":"fixture.txt", "kind":"drive#file",
                            "phase":"PHASE_TYPE_PENDING", "size":"7"},
                        "upload_type":"UPLOAD_TYPE_RESUMABLE",
                        "resumable": {"kind":"drive#resumable", "provider":"PROVIDER_ALIYUN",
                            "params": {"endpoint":"oss.example.test", "bucket":"fixture-bucket",
                                "key":"fixture-key", "access_key_id":"synthetic-id",
                                "access_key_secret":"synthetic-secret", "security_token":"synthetic-token",
                                "expiration":"2099-01-01T00:00:00Z"}}
                    }),
                ),
                ("POST", "/v1/shield/captcha/init") => (
                    200,
                    json!({
                        "captcha_token":"renewed-read-ticket", "expires_in":300
                    }),
                ),
                ("GET", "/drive/v1/files/pending-file") => (
                    400,
                    json!({
                        "error":"captcha_invalid", "error_code":9
                    }),
                ),
                _ => panic!("unexpected synthetic request: {} {path}", request.method),
            };
            Ok(CloudHttpResponse {
                status,
                body: body.to_string(),
                ..Default::default()
            })
        }
    }
    impl CloudUploadTransport for Upload {
        fn send_upload(
            &self,
            request: UploadHttpRequest,
        ) -> Result<UploadHttpResponse, QuarkError> {
            let attempt = self.transfers.fetch_add(1, Ordering::SeqCst);
            if self.fail_transfer {
                return Err(QuarkError::Transport(
                    "synthetic upload response timeout".into(),
                ));
            }
            let mut response = UploadHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: String::new(),
            };
            match attempt {
                0 => {
                    assert_eq!(request.method, "POST");
                    response.body = "<InitiateMultipartUploadResult><UploadId>synthetic-upload</UploadId></InitiateMultipartUploadResult>".into();
                }
                1 => {
                    assert_eq!(request.method, "PUT");
                    response
                        .headers
                        .insert("etag".into(), "synthetic-part".into());
                }
                2 => {
                    assert_eq!(request.method, "POST");
                    response.body = "<CompleteMultipartUploadResult/>".into();
                }
                _ => panic!("a later failure must not repeat object storage transfer"),
            }
            Ok(response)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("fixture.txt");
    std::fs::write(&file, b"fixture").unwrap();
    for fail_transfer in [true, false] {
        let http = Upload {
            requests: Mutex::new(Vec::new()),
            transfers: AtomicUsize::new(0),
            fail_transfer,
        };
        let mut state = session(None);
        state.profile.client_version = "1.93.6".into();
        let result = XunleiClient::new(state, &http)
            .supervised()
            .upload_file("0", &file);
        if fail_transfer {
            assert!(matches!(result, Err(QuarkError::Transport(_))));
        } else {
            assert!(matches!(result, Err(QuarkError::WebAuthorizationRequired)));
        }
        let requests = http.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(method, path)| method == "POST" && path == "/drive/v1/files")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|(method, _)| method == "GET")
                .count(),
            if fail_transfer { 0 } else { 2 }
        );
        assert_eq!(
            requests
                .iter()
                .filter(|(_, path)| path == "/v1/shield/captcha/init")
                .count(),
            usize::from(!fail_transfer)
        );
        assert_eq!(
            http.transfers.load(Ordering::SeqCst),
            if fail_transfer { 1 } else { 3 }
        );
    }
}

#[test]
fn malformed_or_future_timestamp_is_local_not_server_verification() {
    for timestamp in ["invalid".to_owned(), ((now() + 120) * 1000).to_string()] {
        let http = OfflineHttp::new();
        let mut state = session(Some(0));
        state.web_security.timestamp = timestamp;
        state.web_security.expires_at = now();
        let result = XunleiClient::new(state, &http).list_shares();
        assert!(matches!(
            result,
            Err(QuarkError::XunleiVerificationUnavailable(
                "timestamp_invalid"
            ))
        ));
        assert!(http.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn actual_server_challenge_remains_distinct_from_local_preflight_failure() {
    struct Challenge(OfflineHttp);
    impl CloudHttpTransport for Challenge {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            if request.url.ends_with("/v1/shield/captcha/init") {
                return self.0.send(request);
            }
            Ok(CloudHttpResponse {
                status: 412,
                body: r#"{"error":"captcha_required"}"#.into(),
                ..Default::default()
            })
        }
    }
    for supervised in [false, true] {
        let http = Challenge(OfflineHttp::new());
        let client = XunleiClient::new(session(Some(0)), &http);
        let client = if supervised {
            client.supervised()
        } else {
            client
        };
        assert!(matches!(
            client.list_shares(),
            Err(QuarkError::WebAuthorizationRequired)
        ));
        // Both execution modes renew once; repeated rejection still surfaces.
        assert_eq!(http.0.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn local_verification_failure_serializes_only_a_safe_reason() {
    for reason in ["metadata_missing", "metadata_expired", "timestamp_invalid"] {
        let error = QuarkError::XunleiVerificationUnavailable(reason);
        let value = serde_json::to_value(error).unwrap();
        assert_eq!(
            value,
            json!({"code":"xunlei_verification_unavailable", "message":reason})
        );
    }
}

#[test]
fn restart_reuses_distinct_unexpired_operation_tickets_without_resigning() {
    let http = OfflineHttp::new();
    let client = XunleiClient::new(session(Some(0)), &http).supervised();
    client.mkdir("0", "offline-test-folder").unwrap();
    client.list_shares().unwrap();
    let mut persisted = client.session();
    persisted.web_security.timestamp = ((now() - 301) * 1000).to_string();
    let restored = serde_json::from_slice(&serde_json::to_vec(&persisted).unwrap()).unwrap();

    let after_restart = OfflineHttp::new();
    let client = XunleiClient::new(restored, &after_restart).supervised();
    client.directory("0").unwrap();
    client.list_shares().unwrap();
    client.mkdir("0", "offline-test-folder").unwrap();
    let requests = after_restart.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests
        .iter()
        .all(|(_, path)| path != "/v1/shield/captcha/init"));
}

#[test]
fn share_list_uses_the_official_query_and_valid_root_ticket_without_resigning() {
    struct ShareList;
    impl CloudHttpTransport for ShareList {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            assert_eq!(request.method, "GET");
            assert_eq!(request.url, "https://api-pan.xunlei.com/drive/v1/share/list?space=&limit=100&withCaptcha=true&withCredentials=true");
            assert_eq!(request.headers["X-Captcha-Token"], "synthetic-root-ticket");
            Ok(CloudHttpResponse {
                status: 200,
                body: r#"{"data":[]}"#.into(),
                ..Default::default()
            })
        }
    }
    for state in [session(None), session(Some(301))] {
        let client = XunleiClient::new(state, &ShareList).supervised();
        assert!(client.list_shares().unwrap().is_empty());
    }
}
