#[path = "support/upload.rs"]
mod upload_support;
use base64::Engine;
use std::{collections::VecDeque, io::Write, sync::Mutex};
use versadown_lib::cloud::{
    parse_quark_download_url, parse_quark_files,
    share::{
        provider_share_adapter, CloudShareEntry, CloudSharePlan, CloudShareResolvedEntry,
        RecordedTransferDownload, ShareResolution, ShareResolve,
    },
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudProvider, CloudUploadTransport,
    CredentialStore, MemoryCredentialStore, ProviderRegistry, QuarkClient, QuarkError,
    UploadHttpRequest, UploadHttpResponse,
};

struct RecordingHttp(Mutex<Vec<CloudHttpRequest>>);
impl RecordingHttp {
    fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }
}
impl CloudHttpTransport for RecordingHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        self.0.lock().unwrap().push(request);
        Ok(CloudHttpResponse {
            status: 401,
            body: r#"{\"status\":401,\"message\":\"login required\"}"#.into(),
            ..Default::default()
        })
    }
}

#[test]
fn quark_list_uses_cookie_auth_and_maps_401_to_reauthorization() {
    let http = RecordingHttp::new();
    let client = QuarkClient::new("sid=local-session".into(), &http);
    assert!(matches!(
        client.list("0"),
        Err(QuarkError::AuthorizationExpired)
    ));
    let request = http.0.lock().unwrap().pop().unwrap();
    assert!(request.url.contains("/1/clouddrive/file/sort"));
    assert!(request.url.contains("pdir_fid=0"));
    assert_eq!(request.headers.get("Cookie").unwrap(), "sid=local-session");
}

#[test]
fn quark_copy_uses_the_native_copy_endpoint_and_destination() {
    let http = RecordingHttp::new();
    let client = QuarkClient::new("sid=local-session".into(), &http);
    let ids = vec!["file-1".to_owned(), "folder-2".to_owned()];

    assert!(matches!(
        client.copy_files(&ids, "target-folder"),
        Err(QuarkError::AuthorizationExpired)
    ));
    let request = http.0.lock().unwrap().pop().unwrap();
    assert!(request.url.contains("/1/clouddrive/file/copy"));
    let body: serde_json::Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["filelist"], serde_json::json!(["file-1", "folder-2"]));
    assert_eq!(body["to_pdir_fid"], "target-folder");
}

#[test]
fn quark_file_download_uses_the_desktop_file_transfer_profile() {
    let http = RecordingHttp::new();
    let client = QuarkClient::new("sid=local-session".into(), &http);

    assert!(matches!(
        client.download_url("file-1"),
        Err(QuarkError::AuthorizationExpired)
    ));

    let request = http.0.lock().unwrap().pop().unwrap();
    let url = url::Url::parse(&request.url).unwrap();
    assert_eq!(url.path(), "/1/clouddrive/file/download");
    let query = url
        .query_pairs()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(query.get("entry"), Some(&"ft".into()));
    assert_eq!(query.get("fr"), Some(&"pc".into()));
    assert_eq!(query.get("pr"), Some(&"ucpro".into()));
    assert_eq!(query.get("sys"), Some(&"win32".into()));
    assert_eq!(query.get("ve"), Some(&"3.20.0".into()));
    assert!(query.contains_key("__t"));
    assert_eq!(
        request.headers.get("Accept").unwrap(),
        "application/json, text/plain, */*"
    );
    assert_eq!(
        request.headers.get("Accept-Language").unwrap(),
        "zh-CN,zh;q=0.9"
    );
    assert_eq!(
        request.headers.get("Content-Type").unwrap(),
        "application/json"
    );
}

#[test]
fn quark_personal_download_forwards_the_desktop_profile_to_the_cdn() {
    let store = MemoryCredentialStore::default();
    store.save("quark", b"sid=local-session").unwrap();
    let http = SequencedHttp::new([
        r#"{"code":0,"data":[{"fid":"file-1","file_name":"movie.mkv","download_url":"https://download.quark.cn/movie?sign=secret"}]}"#,
    ]);

    let download = ProviderRegistry::global()
        .driver(CloudProvider::Quark, &store, &http)
        .resolve_download("file-1")
        .unwrap();

    assert!(download
        .headers
        .contains(&("Cookie".into(), "sid=local-session".into())));
    assert!(download
        .headers
        .contains(&("Origin".into(), "https://pan.quark.cn".into())));
    assert!(download
        .headers
        .contains(&("Accept".into(), "application/json, text/plain, */*".into())));
    let request = http.requests().pop().unwrap();
    let url = url::Url::parse(&request.url).unwrap();
    let query = url
        .query_pairs()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(query.get("entry"), Some(&"ft".into()));
}

struct DownloadCookieHttp {
    response_cookies: Vec<String>,
}

impl CloudHttpTransport for DownloadCookieHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        assert!(request.url.contains("/file/download?"));
        assert_eq!(request.headers["Cookie"], "sid=stored; __puus=stored");
        Ok(CloudHttpResponse {
            status: 200,
            body: r#"{"code":0,"data":[{"fid":"file-1","file_name":"movie.mkv","download_url":"https://download.quark.cn/movie?sign=fixture"}]}"#.into(),
            set_cookies: self.response_cookies.clone(),
        })
    }

    fn send_with_cookie_snapshot(
        &self,
        request: CloudHttpRequest,
    ) -> Result<(CloudHttpResponse, Option<String>), QuarkError> {
        Ok((self.send(request)?, Some("sid=sent; __puus=sent".into())))
    }
}

fn cookie_bound_share_entry() -> CloudShareResolvedEntry {
    let plan = CloudSharePlan::new(
        CloudProvider::Quark,
        "share-fixture".into(),
        None,
        vec![CloudShareEntry::new(
            "share-fixture",
            "file-1".into(),
            "movie.mkv".into(),
            vec![],
            Some(12),
        )],
        serde_json::json!({"stoken":"fixture-share-token","files":{"file-1":{"token":"fixture-file-token","parent":"0"}}}),
    );
    CloudShareResolvedEntry {
        provider: plan.provider,
        entry: plan.entries[0].clone(),
        plan,
    }
}

fn assert_bound_download_cookie(
    headers: &[(String, String)],
    store: &MemoryCredentialStore,
    expected: &str,
) {
    fn pairs(cookie: &str) -> std::collections::BTreeMap<&str, &str> {
        cookie
            .split(';')
            .map(str::trim)
            .map(|pair| pair.split_once('=').unwrap())
            .collect()
    }
    let sent = headers
        .iter()
        .find_map(|(name, value)| name.eq_ignore_ascii_case("Cookie").then_some(value))
        .expect("download Cookie header");
    let stored = String::from_utf8(store.load("quark").unwrap().unwrap()).unwrap();
    assert_eq!(pairs(sent), pairs(expected));
    assert_eq!(pairs(&stored), pairs(expected));
}

#[test]
fn quark_personal_download_persists_the_sent_cookie_without_a_new_set_cookie() {
    let store = MemoryCredentialStore::default();
    store.save("quark", b"sid=stored; __puus=stored").unwrap();
    let http = DownloadCookieHttp {
        response_cookies: vec![],
    };

    let download = ProviderRegistry::global()
        .driver(CloudProvider::Quark, &store, &http)
        .resolve_download("file-1")
        .unwrap();

    assert_bound_download_cookie(&download.headers, &store, "sid=sent; __puus=sent");
}

#[test]
fn quark_share_direct_download_merges_response_rotation_into_the_sent_cookie() {
    let store = MemoryCredentialStore::default();
    store.save("quark", b"sid=stored; __puus=stored").unwrap();
    let http = DownloadCookieHttp {
        response_cookies: vec!["__puus=response; Path=/; Secure".into()],
    };
    let adapter = provider_share_adapter(CloudProvider::Quark, &store, &http).unwrap();

    let ShareResolution::Direct(download) = adapter.resolve(&cookie_bound_share_entry()).unwrap()
    else {
        panic!("direct download required")
    };

    assert_bound_download_cookie(&download.headers, &store, "sid=sent; __puus=response");
}

#[test]
fn quark_recorded_transfer_download_persists_the_sent_cookie_without_a_new_set_cookie() {
    let store = MemoryCredentialStore::default();
    store.save("quark", b"sid=stored; __puus=stored").unwrap();
    let http = DownloadCookieHttp {
        response_cookies: vec![],
    };
    let adapter = provider_share_adapter(CloudProvider::Quark, &store, &http).unwrap();

    let refreshed = adapter
        .refresh_recorded_transfer(&cookie_bound_share_entry(), "personal-file-1")
        .unwrap();
    let RecordedTransferDownload::Available(download) = refreshed.result.unwrap();

    assert_bound_download_cookie(&download.headers, &store, "sid=sent; __puus=sent");
}

#[test]
fn memory_credential_store_never_persists_plaintext_to_a_file() {
    let store = MemoryCredentialStore::default();
    store.save("quark", b"sid=secret").unwrap();
    assert_eq!(store.load("quark").unwrap().unwrap(), b"sid=secret");
}

#[test]
fn maps_provider_list_and_temporary_download_url_without_inventing_values() {
    let files = parse_quark_files(
        r#"{"data":{"list":[{"fid":"a","file_name":"movie.mkv","dir":false,"size":12}]}}"#,
    )
    .unwrap();
    assert_eq!(files[0].name, "movie.mkv");
    assert_eq!(
        parse_quark_download_url(r#"{"data":[{"download_url":"https://signed.example/file"}]}"#)
            .unwrap(),
        "https://signed.example/file"
    );
    assert!(parse_quark_download_url(r#"{"data":[]}"#).is_err());
}

#[test]
fn quark_directory_preserves_raw_updated_at_for_files_and_folders() {
    // Break caught: dropping Quark's `updated_at` loses the modified-time
    // display for ordinary files and folders in a directory response.
    let body = r#"{"code":0,"data":{"list":[{"fid":"file-1","file_name":"movie.mkv","dir":false,"size":12,"updated_at":"2026-09-10T09:08:07Z"},{"fid":"folder-1","file_name":"Shows","dir":true,"size":0,"updated_at":1789031287}]},"metadata":{"_total":2}}"#;
    let parsed = parse_quark_files(body).unwrap();
    assert_eq!(
        parsed[0].updated_at.as_deref(),
        Some("2026-09-10T09:08:07Z")
    );
    assert_eq!(parsed[1].updated_at.as_deref(), Some("1789031287"));

    let http = SequencedHttp::new([body]);
    let directory = QuarkClient::new("sid=local-session".into(), &http)
        .directory("0")
        .unwrap();
    let file = directory
        .items
        .iter()
        .find(|item| item.id == "file-1")
        .unwrap();
    let folder = directory
        .items
        .iter()
        .find(|item| item.id == "folder-1")
        .unwrap();
    assert_eq!(file.modified_at.as_deref(), Some("2026-09-10T09:08:07Z"));
    assert_eq!(folder.modified_at.as_deref(), Some("1789031287"));
}

#[test]
fn provider_body_expiry_is_not_treated_as_success() {
    struct Expired;
    impl CloudHttpTransport for Expired {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            Ok(CloudHttpResponse {
                status: 200,
                body: r#"{"code":31001,"status":401,"message":"login required"}"#.into(),
                ..Default::default()
            })
        }
    }
    assert!(matches!(
        QuarkClient::new("sid=x".into(), &Expired).list("0"),
        Err(QuarkError::AuthorizationExpired)
    ));
}

#[test]
fn authenticated_account_uses_the_uid_cookie_when_account_info_omits_an_id() {
    struct AccountInfo;
    impl CloudHttpTransport for AccountInfo {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            Ok(CloudHttpResponse {
                status: 200,
                body: r#"{"status":200,"data":{"nickname":"alice"}}"#.into(),
                ..Default::default()
            })
        }
    }

    let account = QuarkClient::new("__uid=user-42; __pus=session".into(), &AccountInfo)
        .account()
        .unwrap();
    assert_eq!(account.id, "user-42");
    assert_eq!(account.display_name, "alice");
}

struct SequencedHttp {
    responses: Mutex<VecDeque<CloudHttpResponse>>,
    requests: Mutex<Vec<CloudHttpRequest>>,
}

impl SequencedHttp {
    fn new<T: Into<String>>(bodies: impl IntoIterator<Item = T>) -> Self {
        Self {
            responses: Mutex::new(
                bodies
                    .into_iter()
                    .map(|body| CloudHttpResponse {
                        status: 200,
                        body: body.into(),
                        ..Default::default()
                    })
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<CloudHttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl CloudHttpTransport for SequencedHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(QuarkError::InvalidResponse)
    }
}

struct MutatingHttp {
    inner: SequencedHttp,
    replacement_path: std::path::PathBuf,
}

impl MutatingHttp {
    fn new<T: Into<String>>(path: std::path::PathBuf, bodies: impl IntoIterator<Item = T>) -> Self {
        Self {
            inner: SequencedHttp::new(bodies),
            replacement_path: path,
        }
    }

    fn requests(&self) -> Vec<CloudHttpRequest> {
        self.inner.requests()
    }
}

impl CloudHttpTransport for MutatingHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let replace_after_hash_registration = request.url.contains("file/update/hash");
        let response = self.inner.send(request)?;
        if replace_after_hash_registration {
            let modified = std::fs::metadata(&self.replacement_path)
                .unwrap()
                .modified()
                .unwrap();
            std::fs::write(&self.replacement_path, b"mutated upload data!").unwrap();
            // Exercise metadata rejection deterministically even when both
            // writes fall in the same filesystem timestamp tick. The restored
            // mtime test below separately covers streaming hash rejection.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&self.replacement_path)
                .unwrap()
                .set_modified(modified + std::time::Duration::from_secs(5))
                .unwrap();
        }
        Ok(response)
    }
}

struct RecordingUpload {
    responses: Mutex<VecDeque<UploadHttpResponse>>,
    requests: Mutex<Vec<UploadHttpRequest>>,
}

impl RecordingUpload {
    fn new(responses: impl IntoIterator<Item = UploadHttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<UploadHttpRequest> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}

impl CloudUploadTransport for RecordingUpload {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
        let request = upload_support::consume(request)?;
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(QuarkError::InvalidResponse)
    }
}

fn upload_fixture() -> tempfile::NamedTempFile {
    let mut fixture = tempfile::Builder::new().suffix(".txt").tempfile().unwrap();
    fixture.write_all(b"cloud upload fixture").unwrap();
    fixture
}

fn multipart_upload_fixture() -> tempfile::NamedTempFile {
    let fixture = tempfile::Builder::new().suffix(".bin").tempfile().unwrap();
    fixture.as_file().set_len(1024 * 1024 + 1).unwrap();
    fixture
}

#[test]
fn quark_upload_runs_pre_hash_authorize_multipart_and_finish() {
    // Break caught: skipping a CloudDrive stage can leave an uploaded object
    // unreachable, while copying a temporary authorization into durable state
    // would leak credentials outside this request.
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"ul-zb","upload_url":"https://pds.quark.cn","callback":"callback"}}"#,
        r#"{"code":0,"data":{"finish":false}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-put"}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-complete"}}"#,
        r#"{"code":0,"data":{"fid":"uploaded-1","file_name":"upload.txt","size":20}}"#,
    ]);
    let upload = RecordingUpload::new([
        UploadHttpResponse {
            status: 200,
            headers: [("etag".into(), "\"part-1\"".into())].into(),
            body: String::new(),
        },
        UploadHttpResponse {
            status: 200,
            headers: Default::default(),
            body: "<CompleteMultipartUploadResult/>".into(),
        },
    ]);
    let fixture = upload_fixture();
    let client = QuarkClient::new("sid=local-session".into(), &http);

    let uploaded = client.upload_file("0", fixture.path(), &upload).unwrap();

    assert_eq!(uploaded.name, "upload.txt");
    assert_eq!(uploaded.id, "uploaded-1");
    let requests = http.requests();
    assert!(requests
        .iter()
        .any(|request| request.url.contains("file/upload/pre")));
    assert!(requests
        .iter()
        .any(|request| request.url.contains("file/update/hash")));
    assert!(requests
        .iter()
        .any(|request| request.url.contains("file/upload/auth")));
    assert!(requests
        .iter()
        .any(|request| request.url.contains("file/upload/finish")));
    assert!(requests
        .iter()
        .all(|request| request.headers["Cookie"] == "sid=local-session"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.contains("file/upload/auth"))
            .count(),
        2
    );
    let upload_requests = upload.requests();
    assert!(upload_requests
        .iter()
        .all(|request| request.url.starts_with("https://ul-zb.pds.quark.cn/")));
    assert_eq!(upload_requests.len(), 2);
    assert_eq!(
        upload_requests[0]
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("OSS temporary-put")
    );
    assert_eq!(
        upload_requests[1]
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("OSS temporary-complete")
    );
}

#[test]
fn quark_upload_signs_and_sends_the_hash_context_for_later_parts() {
    // Break caught: Quark rejects later OSS parts unless their cumulative SHA-1
    // context is both signed in `auth_meta` and sent with the upload request.
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.bin","bucket":"ul-zb","upload_url":"https://pds.quark.cn","callback":"callback"},"metadata":{"part_size":1048576}}"#,
        r#"{"code":0,"data":{"finish":false}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-part-1"}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-part-2"}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-complete"}}"#,
        r#"{"code":0,"data":{"fid":"uploaded-1","file_name":"upload.bin","size":1048577}}"#,
    ]);
    let upload = RecordingUpload::new([
        UploadHttpResponse {
            status: 200,
            headers: [("etag".into(), "\"part-1\"".into())].into(),
            body: String::new(),
        },
        UploadHttpResponse {
            status: 200,
            headers: [("etag".into(), "\"part-2\"".into())].into(),
            body: String::new(),
        },
        UploadHttpResponse {
            status: 200,
            headers: Default::default(),
            body: "<CompleteMultipartUploadResult/>".into(),
        },
    ]);
    let fixture = multipart_upload_fixture();

    QuarkClient::new("sid=local-session".into(), &http)
        .upload_file("0", fixture.path(), &upload)
        .unwrap();

    let upload_requests = upload.requests();
    let context = upload_requests[1]
        .headers
        .get("x-oss-hash-ctx")
        .expect("second part must send a cumulative hash context");
    let context_json: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(context)
            .expect("hash context must be base64 JSON"),
    )
    .unwrap();
    assert_eq!(context_json["hash_type"], "sha1");
    assert_eq!(context_json["Nl"], "8388608");
    assert_eq!(context_json["Nh"], "0");
    assert_eq!(context_json["data"], "");
    assert_eq!(context_json["num"], "0");

    let authorizations: Vec<_> = http
        .requests()
        .into_iter()
        .filter(|request| request.url.contains("file/upload/auth"))
        .collect();
    let part_two_auth: serde_json::Value =
        serde_json::from_str(authorizations[1].body.as_deref().unwrap()).unwrap();
    assert!(part_two_auth["auth_meta"]
        .as_str()
        .unwrap()
        .contains(&format!("x-oss-hash-ctx:{context}\n")));
}

#[test]
fn quark_upload_upgrades_the_provider_http_object_endpoint_to_https() {
    // The current Quark pre-upload API returns the fixed pds.quark.cn endpoint
    // with an http scheme. Bytes and one-shot authorization must still be sent
    // only over HTTPS, after validating the exact provider host.
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"ul-zb","upload_url":"http://pds.quark.cn","callback":"callback"}}"#,
        r#"{"code":0,"data":{"finish":false}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-put"}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-complete"}}"#,
        r#"{"code":0,"data":{"fid":"uploaded-1","file_name":"upload.txt","size":20}}"#,
    ]);
    let upload = RecordingUpload::new([
        UploadHttpResponse {
            status: 200,
            headers: [("etag".into(), "\"part-1\"".into())].into(),
            body: String::new(),
        },
        UploadHttpResponse {
            status: 200,
            headers: Default::default(),
            body: "<CompleteMultipartUploadResult/>".into(),
        },
    ]);
    let fixture = upload_fixture();

    QuarkClient::new("sid=local-session".into(), &http)
        .upload_file("0", fixture.path(), &upload)
        .unwrap();

    assert!(upload
        .requests()
        .iter()
        .all(|request| request.url.starts_with("https://ul-zb.pds.quark.cn/")));
}

#[test]
fn quark_upload_accepts_current_json_multipart_completion() {
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"ul-zb","upload_url":"https://pds.quark.cn","callback":"callback"}}"#,
        r#"{"code":0,"data":{"finish":false}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-put"}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-complete"}}"#,
        r#"{"code":0,"data":{"fid":"uploaded-1","file_name":"upload.txt","size":20}}"#,
    ]);
    let upload = RecordingUpload::new([
        UploadHttpResponse {
            status: 200,
            headers: [("etag".into(), "\"part-1\"".into())].into(),
            body: String::new(),
        },
        UploadHttpResponse {
            status: 200,
            headers: Default::default(),
            body: r#"{"Status":"OK"}"#.into(),
        },
    ]);
    let fixture = upload_fixture();

    let uploaded = QuarkClient::new("sid=local-session".into(), &http)
        .upload_file("0", fixture.path(), &upload)
        .unwrap();

    assert_eq!(uploaded.id, "uploaded-1");
}

#[test]
fn quark_upload_returns_hash_completed_file_without_object_transfer() {
    // Break caught: treating a hash-complete response as an incomplete upload
    // unnecessarily sends file bytes with a temporary object-store credential.
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"ul-zb"}}"#,
        r#"{"code":0,"data":{"finish":true,"fid":"instant-1","file_name":"upload.txt","size":20}}"#,
    ]);
    let upload = RecordingUpload::new([]);
    let fixture = upload_fixture();
    let client = QuarkClient::new("sid=local-session".into(), &http);

    let uploaded = client.upload_file("0", fixture.path(), &upload).unwrap();

    assert_eq!(uploaded.id, "instant-1");
    assert!(upload.requests().is_empty());
    assert_eq!(http.requests().len(), 2);
}

#[test]
fn quark_upload_rejects_same_size_replacement_after_hash_registration() {
    // Break caught: the bytes transferred after hash registration must still
    // match the MD5/SHA-1 provided to CloudDrive, even when their length is
    // unchanged.
    let fixture = upload_fixture();
    let http = MutatingHttp::new(
        fixture.path().to_path_buf(),
        [
            r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"ul-zb","upload_url":"https://pds.quark.cn"}}"#,
            r#"{"code":0,"data":{"finish":false}}"#,
            r#"{"code":0,"data":{"auth_key":"OSS temporary-put"}}"#,
        ],
    );
    let upload = RecordingUpload::new([UploadHttpResponse {
        status: 200,
        headers: [("etag".into(), "\"part-1\"".into())].into(),
        body: String::new(),
    }]);
    let client = QuarkClient::new("sid=local-session".into(), &http);

    assert!(matches!(
        client.upload_file("0", fixture.path(), &upload),
        Err(QuarkError::InvalidInput(message)) if message.contains("上传过程中发生变化")
    ));
    assert!(upload.requests().is_empty());
    assert!(!http
        .requests()
        .iter()
        .any(|request| request.url.contains("file/upload/finish")));
}

#[test]
fn quark_streaming_preserves_uploaded_hash_verification_when_mtime_is_restored() {
    struct Rewrite<'a> {
        inner: SequencedHttp,
        path: &'a std::path::Path,
        modified: std::time::SystemTime,
    }
    impl CloudHttpTransport for Rewrite<'_> {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            if request.url.contains("file/update/hash") {
                std::fs::write(self.path, b"changed upload bytes").unwrap();
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(self.path)
                    .unwrap()
                    .set_modified(self.modified)
                    .unwrap();
            }
            self.inner.send(request)
        }
    }
    let fixture = upload_fixture();
    let http = Rewrite {
        path: fixture.path(),
        modified: fixture.as_file().metadata().unwrap().modified().unwrap(),
        inner: SequencedHttp::new([
            r#"{"code":0,"data":{"task_id":"task","auth_info":"info","upload_id":"upload","obj_key":"objects/file","bucket":"ul-zb","upload_url":"https://pds.quark.cn"}}"#,
            r#"{"code":0,"data":{"finish":false}}"#,
            r#"{"code":0,"data":{"auth_key":"OSS put"}}"#,
            r#"{"code":0,"data":{"auth_key":"OSS complete"}}"#,
            r#"{"code":0,"data":{"fid":"file"}}"#,
        ]),
    };
    let upload = RecordingUpload::new([
        UploadHttpResponse {
            status: 200,
            headers: [("etag".into(), "part".into())].into(),
            body: String::new(),
        },
        UploadHttpResponse {
            status: 200,
            headers: Default::default(),
            body: "<CompleteMultipartUploadResult/>".into(),
        },
    ]);
    let result =
        QuarkClient::new("sid=session".into(), &http).upload_file("0", fixture.path(), &upload);
    assert!(
        matches!(result, Err(QuarkError::InvalidInput(_))),
        "stream must still match its registered hash"
    );
    assert_eq!(upload.requests().len(), 1);
}

#[test]
fn quark_upload_rejects_a_malformed_object_authorization_before_sending_bytes() {
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"ul-zb","upload_url":"https://pds.quark.cn"}}"#,
        r#"{"code":0,"data":{"finish":false}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary\r\ninjected"}}"#,
    ]);
    let upload = RecordingUpload::new([]);
    let fixture = upload_fixture();
    let client = QuarkClient::new("sid=local-session".into(), &http);

    assert!(matches!(
        client.upload_file("0", fixture.path(), &upload),
        Err(QuarkError::InvalidResponse)
    ));
    assert!(upload.requests().is_empty());
    assert_eq!(http.requests().len(), 3);
}

#[test]
fn quark_upload_rejects_a_malformed_etag_before_multipart_completion() {
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"ul-zb","upload_url":"https://pds.quark.cn"}}"#,
        r#"{"code":0,"data":{"finish":false}}"#,
        r#"{"code":0,"data":{"auth_key":"OSS temporary-put"}}"#,
    ]);
    let upload = RecordingUpload::new([UploadHttpResponse {
        status: 200,
        headers: [("etag".into(), "part-1\r\ninjected".into())].into(),
        body: String::new(),
    }]);
    let fixture = upload_fixture();
    let client = QuarkClient::new("sid=local-session".into(), &http);

    assert!(matches!(
        client.upload_file("0", fixture.path(), &upload),
        Err(QuarkError::InvalidResponse)
    ));
    assert_eq!(upload.requests().len(), 1);
    assert_eq!(http.requests().len(), 3);
}

#[test]
fn quark_upload_rejects_an_oversized_callback_before_authorizing_object_store() {
    let pre = serde_json::json!({
        "code": 0,
        "data": {
            "task_id": "task-1",
            "auth_info": "one-shot",
            "upload_id": "upload-1",
            "obj_key": "objects/upload.txt",
            "bucket": "ul-zb",
            "upload_url": "https://pds.quark.cn",
            "callback": "x".repeat(16 * 1024),
        }
    })
    .to_string();
    let http = SequencedHttp::new(vec![pre, r#"{"code":0,"data":{"finish":false}}"#.into()]);
    let upload = RecordingUpload::new([]);
    let fixture = upload_fixture();
    let client = QuarkClient::new("sid=local-session".into(), &http);

    assert!(matches!(
        client.upload_file("0", fixture.path(), &upload),
        Err(QuarkError::InvalidResponse)
    ));
    assert!(upload.requests().is_empty());
    assert_eq!(http.requests().len(), 2);
}

#[test]
fn quark_recycle_records_restore_and_permanent_delete_use_recycle_record_id() {
    // Break caught: using the live file id instead of the recycle record id
    // restores or permanently deletes a different item after the file changed.
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"list":[{"record_id":"recycle-1","fid":"live-file-1","file_name":"old.txt","dir":false,"size":12}]},"metadata":{"_total":1}}"#,
        r#"{"code":0,"data":{}}"#,
        r#"{"code":0,"data":{}}"#,
    ]);
    let client = QuarkClient::new("sid=local-session".into(), &http);

    let directory = client.list_recycle_bin().unwrap();
    client
        .restore_recycle_entries(&[directory.items[0].id.clone()])
        .unwrap();
    client
        .delete_recycle_entries(&[directory.items[0].id.clone()])
        .unwrap();

    assert_eq!(directory.items[0].id, "recycle-1");
    assert_eq!(
        directory.items[0].parent_id,
        "__versa_down_quark_recycle_bin__"
    );
    let requests = http.requests();
    assert!(requests[0].url.contains("file/recycle/list"));
    assert!(requests[1].url.contains("file/recycle/restore"));
    assert!(requests[2].url.contains("file/recycle/remove"));
    for request in &requests[1..] {
        let body: serde_json::Value =
            serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["record_list"], serde_json::json!(["recycle-1"]));
    }
}

#[test]
fn quark_root_directory_exposes_the_synthetic_recycle_bin() {
    // Break caught: exposing the provider's recycle API as a real folder ID
    // would let normal file operations target a synthetic directory.
    let http = SequencedHttp::new([r#"{"code":0,"data":{"list":[]},"metadata":{"_total":0}}"#]);
    let client = QuarkClient::new("sid=local-session".into(), &http);

    let root = client.directory("0").unwrap();

    assert!(root.items.iter().any(|file| {
        file.id == "__versa_down_quark_recycle_bin__"
            && file.name == "回收站"
            && file.kind == "folder"
    }));
}

#[test]
fn quark_upload_rejects_an_invalid_object_store_endpoint_before_sending_bytes() {
    // Break caught: an unvalidated server-supplied bucket can direct local
    // bytes and a short-lived authorization to an arbitrary network origin.
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"bad.example/evil","upload_url":"https://pds.quark.cn"}}"#,
        r#"{"code":0,"data":{"finish":false}}"#,
    ]);
    let upload = RecordingUpload::new([]);
    let fixture = upload_fixture();
    let client = QuarkClient::new("sid=local-session".into(), &http);

    assert!(matches!(
        client.upload_file("0", fixture.path(), &upload),
        Err(QuarkError::InvalidResponse)
    ));
    assert!(upload.requests().is_empty());
}

#[test]
fn quark_upload_rejects_an_untrusted_http_object_host_before_sending_bytes() {
    let http = SequencedHttp::new([
        r#"{"code":0,"data":{"task_id":"task-1","auth_info":"one-shot","upload_id":"upload-1","obj_key":"objects/upload.txt","bucket":"ul-zb","upload_url":"http://evil.example","callback":"callback"}}"#,
        r#"{"code":0,"data":{"finish":false}}"#,
    ]);
    let upload = RecordingUpload::new([]);
    let fixture = upload_fixture();

    assert!(matches!(
        QuarkClient::new("sid=local-session".into(), &http).upload_file(
            "0",
            fixture.path(),
            &upload
        ),
        Err(QuarkError::InvalidResponse)
    ));
    assert!(upload.requests().is_empty());
    assert_eq!(http.requests().len(), 2);
}
