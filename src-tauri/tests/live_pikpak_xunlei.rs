//! Explicitly enabled live regressions for the two browser-authorized drives.
//!
//! They only ever create one uniquely named `VersaDown-IT-*` directory,
//! operate on descendants of that directory, and finally move that directory
//! to the provider's recycle bin.  They are ignored by default because they
//! use the current Windows user's encrypted provider sessions.

use versadown_lib::cloud::{
    xunlei::{XunleiClient, XunleiSession},
    CloudFile, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudUploadTransport,
    CredentialStore, PikPakClient, PikPakSession, QuarkError, ReqwestTransport,
    SystemCredentialStore, UploadHttpRequest, UploadHttpResponse,
};
use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type LiveResult<T> = Result<T, Box<dyn Error>>;

/// A test-only transport that records no secrets: it reports only the
/// provider hostname, HTTP method, and result class for a failed object-store
/// transfer.  That is enough to diagnose a transfer boundary without ever
/// printing the signed URL or STS credentials.
struct LiveTransport(ReqwestTransport);

impl LiveTransport {
    fn new() -> Result<Self, QuarkError> {
        Ok(Self(ReqwestTransport::new()?))
    }
}

fn safe_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| "invalid-host".into())
}

fn error_kind(error: &QuarkError) -> &'static str {
    match error {
        QuarkError::NotAuthorized => "not-authorized",
        QuarkError::AccountMismatch => "account-mismatch",
        QuarkError::AuthorizationExpired => "authorization-expired",
        QuarkError::WebAuthorizationRequired => "web-authorization-required",
        QuarkError::CaptchaRequired => "captcha-required",
        QuarkError::PikpakRegionalMembershipRestricted => "pikpak-regional-membership-restricted",
        QuarkError::PikpakOperationVerificationRequired(_) => "operation-verification-required",
        QuarkError::AuthorizationPending => "authorization-pending",
        QuarkError::SourceFileMissing => "source-file-missing",
        QuarkError::DownloadCheckpointInvalid => "download-checkpoint-invalid",
        QuarkError::DownloadEngineUnavailable => "download-engine-unavailable",
        QuarkError::DownloadSubmissionFailed => "download-submission-failed",
        QuarkError::SessionRenewedRetryOperation => "session-renewed-retry-operation",
        QuarkError::InvalidResponse => "invalid-response",
        QuarkError::Storage => "storage",
        QuarkError::Transport(_) => "transport",
        QuarkError::Provider(_) => "provider",
        QuarkError::InvalidInput(_) => "invalid-input",
        QuarkError::UploadUnavailable => "upload-unavailable",
        QuarkError::ProviderUnsupported => "provider-unsupported",
        QuarkError::XunleiVerificationUnavailable(_) => "xunlei-verification-unavailable",
        QuarkError::BaiduOperationVerificationRequired => "baidu-operation-verification-required",
        QuarkError::BaiduDriveVerificationRequired => "baidu-operation-verification-required",
        QuarkError::BaiduUploadAuthorizationRequired(_) => "baidu-upload-authorization-required",
        QuarkError::BaiduUploadPrecreateRejected { .. } => "baidu-upload-precreate-rejected",
        QuarkError::BaiduUploadWebSessionRequired => "baidu-upload-web-session-required",
        QuarkError::XunleiNativeDownload(_) => "xunlei-native-download-error",
        QuarkError::BaiduTransferPending => "baidu-transfer-pending",
        QuarkError::ShareTransferDownloadUnavailable => "transfer-saved-download-unavailable",
    }
}

#[test]
fn provider_failure_message_redacts_server_echoes() {
    let error = QuarkError::Provider(
        "HTTP 403 https://signed.example/file?token=credential&captcha=context".into(),
    );

    let message = safe_provider_failure("PikPak CACHE resolve download", &error);

    assert_eq!(message, "PikPak CACHE resolve download: provider");
    assert!(!message.contains("signed.example"));
    assert!(!message.contains("credential"));
    assert!(!message.contains("captcha"));
}

fn provider_step<T>(
    name: &str,
    operation: impl FnOnce() -> Result<T, QuarkError>,
) -> LiveResult<T> {
    operation().map_err(|error| failure(safe_provider_failure(name, &error)))
}

fn safe_provider_failure(name: &str, error: &QuarkError) -> String {
    // Server responses and transport errors can echo signed URLs, bearer
    // material, or challenge context. The enum class is sufficient for a
    // live-regression diagnosis and is the only safe failure detail to log.
    format!("{name}: {}", error_kind(error))
}

impl CloudHttpTransport for LiveTransport {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let cache_probe = request.url.contains("usage=CACHE");
        let xunlei_method =
            (safe_host(&request.url) == "api-pan.xunlei.com").then_some(request.method);
        let xunlei_detail = request.method == "GET"
            && url::Url::parse(&request.url).is_ok_and(|url| {
                matches!(
                    url.host_str(),
                    Some("api-pan.xunlei.com" | "x-api-pan.xunlei.com")
                ) && url.path().starts_with("/drive/v1/files/")
            });
        let response = self.0.send(request)?;
        if xunlei_detail {
            let value: serde_json::Value = serde_json::from_str(&response.body).unwrap_or_default();
            let file = value.get("data").unwrap_or(&value);
            let phase = match file["phase"].as_str() {
                Some("PHASE_TYPE_COMPLETE") => "complete",
                Some("PHASE_TYPE_PENDING") => "pending",
                Some("PHASE_TYPE_RUNNING") => "running",
                Some("PHASE_TYPE_ERROR") => "error",
                _ => "other",
            };
            let link_state = |key: &str| match file[key].as_str() {
                None => "missing",
                Some("") => "empty",
                Some(raw) => match url::Url::parse(raw).ok().as_ref().map(url::Url::scheme) {
                    Some("https") => "https",
                    Some("http") => "http",
                    _ => "other",
                },
            };
            eprintln!(
                "Xunlei detail: phase={phase}, web_link={}, download_link={}",
                link_state("web_content_link"),
                link_state("download_url")
            );
        }
        if let Some(method) = xunlei_method {
            let v: serde_json::Value = serde_json::from_str(&response.body).unwrap_or_default();
            let code = v["error_code"]
                .as_i64()
                .or_else(|| v["code"].as_i64())
                .unwrap_or_default();
            if response.status >= 400 || code != 0 {
                eprintln!(
                    "Xunlei API failure: method={method}, HTTP={}, code={code}, error={:?}",
                    response.status,
                    v["error"]
                        .as_str()
                        .filter(|s| s.len() < 80
                            && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
                );
            }
        }
        if cache_probe {
            let cache_details = serde_json::from_str::<serde_json::Value>(&response.body)
                .ok()
                .map(|value| {
                    let fields = value
                        .as_object()
                        .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
                        .unwrap_or_else(|| "non-object".into());
                    let link = match value["web_content_link"].as_str() {
                        Some(raw) => url::Url::parse(raw)
                            .ok()
                            .map(|url| {
                                format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""))
                            })
                            .unwrap_or_else(|| format!("non-url(len={})", raw.len())),
                        None => "missing".into(),
                    };
                    let media_link = value["medias"]
                        .as_array()
                        .and_then(|media| media.first())
                        .and_then(|media| media.pointer("/link/url"))
                        .and_then(serde_json::Value::as_str)
                        .and_then(|raw| url::Url::parse(raw).ok())
                        .map(|url| format!("{}://{}", url.scheme(), url.host_str().unwrap_or("")))
                        .unwrap_or_else(|| "missing-or-invalid".into());
                    format!("keys={fields}; web_content_link={link}; first_media={media_link}")
                })
                .unwrap_or_else(|| "non-json".into());
            // Keys only: the response can contain a signed link, which must
            // never appear in a test log.
            eprintln!(
                "PikPak CACHE response: HTTP {}; {cache_details}",
                response.status
            );
        }
        Ok(response)
    }
}

impl CloudUploadTransport for LiveTransport {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
        let method = request.method;
        let length = request.body.len();
        match self.0.send_upload(request) {
            Ok(response) => {
                eprintln!(
                    "live object transfer: {method} {length} bytes -> HTTP {}",
                    response.status
                );
                Ok(response)
            }
            Err(error) => {
                eprintln!("live object transfer: {method} -> {}", error_kind(&error));
                Err(error)
            }
        }
    }
}

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

fn require_live_authorization() -> LiveResult<()> {
    if std::env::var("VERSADOWN_LIVE_CLOUD").as_deref() != Ok("1") {
        return Err(failure(
            "live cloud regression requires VERSADOWN_LIVE_CLOUD=1",
        ));
    }
    Ok(())
}

fn session_store() -> LiveResult<SystemCredentialStore> {
    // An isolated test app can authorize without replacing the user's normal
    // account store. This is an explicit opt-in test-data path, not APPDATA.
    if let Some(data_dir) = std::env::var_os("VERSADOWN_TEST_DATA_DIR") {
        return Ok(SystemCredentialStore::new(
            PathBuf::from(data_dir).join("cloud-sessions"),
        )?);
    }
    let app_data = std::env::var_os("APPDATA")
        .ok_or_else(|| failure("APPDATA is required for the encrypted session store"))?;
    Ok(SystemCredentialStore::new(
        PathBuf::from(app_data)
            .join("com.versadown.desktop")
            .join("cloud-sessions"),
    )?)
}

fn test_name(provider: &str, suffix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("VersaDown-IT-{provider}-{millis}-{suffix}")
}

fn local_artifact(provider: &str) -> LiveResult<(PathBuf, PathBuf, Vec<u8>)> {
    let root = std::env::temp_dir().join(test_name(provider, "local"));
    fs::create_dir_all(&root)?;
    let contents = format!(
        "VersaDown live integration regression for {provider}.\nNo user content is used.\n"
    )
    .into_bytes();
    let file = root.join("payload.txt");
    fs::write(&file, &contents)?;
    Ok((root, file, contents))
}

fn contains_id(items: &[CloudFile], id: &str) -> bool {
    items.iter().any(|item| item.id == id)
}

fn find_named(directory: &[CloudFile], name: &str, kind: &str) -> LiveResult<CloudFile> {
    directory
        .iter()
        .find(|item| item.name == name && item.kind == kind)
        .cloned()
        .ok_or_else(|| failure(format!("newly created {kind} {name:?} was not listed")))
}

fn download_bytes(url: &str) -> LiveResult<Vec<u8>> {
    download_bytes_with_headers(url, &[])
}

fn download_bytes_with_native_engine(url: &str) -> LiveResult<Vec<u8>> {
    use versadown_lib::{
        Aria2Api, Aria2LaunchConfig, Aria2Options, Aria2Process, Aria2Settings,
    };
    let local = tempfile::tempdir()?;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = reservation.local_addr()?.port();
    drop(reservation);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let config = Aria2LaunchConfig {
            binary: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
            data_dir: local.path().join("state"),
            download_dir: local.path().join("downloads"),
            parent_pid: std::process::id(),
            peer_discovery: false,
            aria2: Aria2Settings {
                rpc_port: port,
                ..Default::default()
            },
        };
        let (_process, rpc) = Aria2Process::start(config)
            .await
            .map_err(|_| failure("test native engine startup failed"))?;
        let options = Aria2Options::from([
            ("pause".into(), "false".into()),
            ("out".into(), "roundtrip.bin".into()),
            ("follow-torrent".into(), "false".into()),
            ("all-proxy".into(), "".into()),
        ]);
        let gid = rpc
            .add_uri(vec![url.into()], options)
            .await
            .map_err(|_| failure("test native engine submission failed"))?;
        let started = std::time::Instant::now();
        loop {
            let status = rpc
                .tell_status(&gid)
                .await
                .map_err(|_| failure("test native engine status failed"))?;
            if status.status == "complete" {
                return fs::read(local.path().join("downloads/roundtrip.bin")).map_err(Into::into);
            }
            if status.status == "error" || started.elapsed() > Duration::from_secs(600) {
                return Err(failure(format!(
                    "test native transfer failed: code={:?}, completed={}",
                    status.error_code, status.completed_length
                )));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
}

fn download_bytes_with_headers(url: &str, headers: &[(&str, &str)]) -> LiveResult<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut request = client.get(url);
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    let mut response = request
        .send()
        .map_err(|_| failure("provider direct download transport failed"))?;
    if !response.status().is_success() {
        return Err(failure(format!(
            "provider direct download returned HTTP {}",
            response.status()
        )));
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut response, &mut bytes).map_err(|_| {
        failure(format!(
            "provider direct download body read failed after {} bytes",
            bytes.len()
        ))
    })?;
    Ok(bytes)
}

fn remove_local(root: &Path) {
    let _ = fs::remove_dir_all(root);
}

fn xunlei_browser_root_request(session: &XunleiSession, web_headers: bool) -> CloudHttpRequest {
    let mut url = url::Url::parse("https://x-api-pan.xunlei.com/drive/v1/files")
        .expect("fixed Xunlei browser endpoint is valid");
    url.query_pairs_mut()
        .append_pair("parent_id", "")
        .append_pair("page_token", "")
        .append_pair("space", "")
        .append_pair("filters", r#"{"trashed":{"eq":false}}"#)
        .append_pair("with", "url")
        .append_pair("with_audit", "true")
        .append_pair("thumbnail_size", "SIZE_LARGE");
    let mut headers = std::collections::BTreeMap::from([
        ("Authorization".into(), format!("Bearer {}", session.access_token)),
        ("Accept".into(), "application/json;charset=UTF-8".into()),
        (
            "User-Agent".into(),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36".into(),
        ),
    ]);
    if web_headers {
        headers.insert("Origin".into(), "https://pan.xunlei.com".into());
        headers.insert("Referer".into(), "https://pan.xunlei.com/".into());
    }
    CloudHttpRequest {
        method: "GET",
        url: url.into(),
        headers,
        body: None,
    }
}

fn xunlei_browser_root_probe(
    http: &impl CloudHttpTransport,
    session: &XunleiSession,
    web_headers: bool,
) -> LiveResult<()> {
    let response = http.send(xunlei_browser_root_request(session, web_headers))?;
    let value: serde_json::Value = serde_json::from_str(&response.body)?;
    let code = value
        .get("error_code")
        .or_else(|| value.get("code"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    if !(200..300).contains(&response.status) || code != 0 {
        return Err(failure(format!(
            "Xunlei browser endpoint probe with web_headers={web_headers} returned HTTP {} / code {code}",
            response.status
        )));
    }
    let count = value["files"].as_array().map_or(0, Vec::len);
    eprintln!(
        "Xunlei browser endpoint probe with web_headers={web_headers} succeeded: {count} root entries"
    );
    Ok(())
}

#[test]
#[ignore = "requires an explicitly authorized PikPak account and creates only VersaDown-IT-* test data"]
fn live_pikpak_unique_upload_download_roundtrip() -> LiveResult<()> {
    use sha2::{Digest, Sha256};
    require_live_authorization()?;
    let store = session_store()?;
    let http = LiveTransport::new()?;
    let client = PikPakClient::new(&http, &store);
    let local = tempfile::tempdir()?;
    let base_name = test_name("pikpak-roundtrip", "root");
    let base = provider_step("roundtrip mkdir", || client.mkdir("0", &base_name))?;
    let mut root_upload = None;
    let result = (|| -> LiveResult<()> {
        let nested = provider_step("roundtrip nested mkdir", || {
            client.mkdir(&base.id, "nested")
        })?;
        let nonce = uuid::Uuid::new_v4();
        for (label, parent, length) in [
            ("root-small", "0", 1024usize),
            ("multipart", base.id.as_str(), 12 * 1024 * 1024 + 17),
            ("nested", nested.id.as_str(), 4097),
        ] {
            let mut expected = Vec::with_capacity(length);
            for block in 0..length.div_ceil(32) {
                expected.extend_from_slice(&Sha256::digest(
                    format!("{nonce}/{label}/{block}").as_bytes(),
                ));
            }
            expected.truncate(length);
            let name = format!("{base_name}-{label}.bin");
            let payload = local.path().join(&name);
            fs::write(&payload, &expected)?;
            eprintln!("roundtrip {label}: uploading {length} bytes");
            let uploaded =
                provider_step("roundtrip upload", || client.upload_file(parent, &payload))?;
            if parent == "0" {
                root_upload = Some(uploaded.id.clone());
            }
            if uploaded.name != name {
                return Err(failure("uploaded filename mismatch"));
            }
            let mut confirmed = false;
            for _ in 0..8 {
                let listing = provider_step("roundtrip confirm actual parent", || {
                    client.list(&uploaded.parent_id)
                })?;
                if listing
                    .items
                    .iter()
                    .any(|file| file.id == uploaded.id && file.size == Some(length as u64))
                {
                    confirmed = true;
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }
            if !confirmed {
                return Err(failure(format!(
                    "{label}: actual destination did not confirm size"
                )));
            }
            eprintln!(
                "roundtrip {label}: actual directory and size confirmed (redirected={})",
                uploaded.parent_id != parent
            );
            let (url, _) = provider_step("roundtrip resolve", || client.download(&uploaded.id))?;
            let actual = download_bytes(&url)?;
            if actual.len() != length || Sha256::digest(&actual) != Sha256::digest(&expected) {
                return Err(failure(format!("{label}: size/SHA-256 mismatch")));
            }
            eprintln!("roundtrip {label}: downloaded {length} bytes; SHA-256 matched");
            let native = download_bytes_with_native_engine(&url)?;
            if native.len() != length || Sha256::digest(&native) != Sha256::digest(&expected) {
                return Err(failure(format!(
                    "{label}: native engine size/SHA-256 mismatch"
                )));
            }
            eprintln!(
                "roundtrip {label}: bundled aria2-next downloaded {length} bytes; SHA-256 matched"
            );
        }
        Ok(())
    })();
    // IDs come exclusively from mutations performed by this test. Recycle,
    // never permanently delete, and do not touch any pre-existing user item.
    let mut created = vec![base.id];
    if let Some(id) = root_upload {
        created.push(id);
    }
    let cleanup = provider_step("roundtrip recycle only created items", || {
        client.trash(&created)
    });
    if cleanup.is_ok() {
        eprintln!("roundtrip cleanup: only this test's created items moved to recycle bin");
    }
    result.and(cleanup)
}

#[test]
#[ignore = "requires explicit existing PikPak task ID; reads remote file without modifying tasks or user files"]
fn live_pikpak_existing_task_native_download() -> LiveResult<()> {
    use versadown_lib::cloud::commands::parse_stable_cloud_source;
    use rusqlite::{Connection, OpenFlags};
    use sha2::{Digest, Sha256};
    require_live_authorization()?;
    let task_id = std::env::var("VERSADOWN_TEST_TASK_ID")?;
    let data = PathBuf::from(std::env::var("APPDATA")?).join("com.versadown.desktop");
    let db =
        Connection::open_with_flags(data.join("tasks.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (source, expected): (String, u64) = db.query_row(
        "SELECT source,total_length FROM tasks WHERE id=?1",
        [&task_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (provider, id) = parse_stable_cloud_source(&source)?;
    if provider.key() != "pikpak" {
        return Err(failure("existing task is not PikPak"));
    }
    let store = session_store()?;
    let http = LiveTransport::new()?;
    let (url, _) = provider_step("existing task resolve", || {
        PikPakClient::new(&http, &store).download(&id)
    })?;
    let bytes = download_bytes_with_native_engine(&url)?;
    if bytes.len() as u64 != expected {
        return Err(failure("existing task download length mismatch"));
    }
    eprintln!(
        "existing task native download: {} bytes; SHA-256={:x}; original task and files unchanged",
        bytes.len(),
        Sha256::digest(&bytes)
    );
    Ok(())
}

#[test]
#[ignore = "requires an explicitly authorized PikPak account and creates only VersaDown-IT-* test data"]
fn live_pikpak_directory_upload_rename_move_download_and_trash() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let session: PikPakSession = serde_json::from_slice(
        &store
            .load("pikpak")?
            .ok_or_else(|| failure("PikPak is not currently authorized"))?,
    )?;
    eprintln!(
        "PikPak live session: auth={:?}, access_expired={}, browser_client={}, client_is_android={}, device_present={}, captcha_present={}",
        session.auth_method,
        session.expires_at <= SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        session.client_id.is_some(),
        session.client_id.as_deref() == Some("YNxT9w7GMdWvEOKa"),
        !session.device_id.is_empty(),
        !session.captcha_token.is_empty(),
    );
    let http = LiveTransport::new()?;
    let client = PikPakClient::new(&http, &store);
    let (local_root, payload, expected) = local_artifact("pikpak")?;
    let base_name = test_name("pikpak", "root");
    let target_name = test_name("pikpak", "move-target");
    let renamed_name = test_name("pikpak", "renamed.txt");

    // Root listing is deliberately performed before any mutation.
    let initial_root = client.list("0")?;
    if initial_root.items.iter().any(|item| item.name == base_name) {
        remove_local(&local_root);
        return Err(failure("unexpected test-directory name collision"));
    }
    let base = client.mkdir("0", &base_name)?;
    let result = (|| -> LiveResult<()> {
        let listed = provider_step("PikPak list after mkdir", || client.list("0"))?;
        if !contains_id(&listed.items, &base.id) {
            return Err(failure(
                "PikPak did not list the newly created test directory",
            ));
        }
        let target = provider_step("PikPak mkdir move target", || {
            client.mkdir(&base.id, &target_name)
        })?;
        let uploaded = provider_step("PikPak upload", || client.upload_file(&base.id, &payload))?;
        provider_step("PikPak rename", || {
            client.rename(&uploaded.id, &renamed_name)
        })?;
        let after_rename = provider_step("PikPak list after rename", || client.list(&base.id))?;
        let renamed = find_named(&after_rename.items, &renamed_name, "file")?;
        provider_step("PikPak move", || {
            client.move_files(&[renamed.id.clone()], &target.id)
        })?;
        let moved = provider_step("PikPak list after move", || client.list(&target.id))?;
        let moved_file = find_named(&moved.items, &renamed_name, "file")?;
        let (url, download_name) = provider_step("PikPak resolve download", || {
            client.download(&moved_file.id)
        })?;
        if download_name != renamed_name {
            return Err(failure(
                "PikPak download metadata did not preserve the renamed file",
            ));
        }
        if download_bytes(&url)? != expected {
            return Err(failure(
                "PikPak downloaded bytes differ from the uploaded test file",
            ));
        }
        Ok(())
    })();

    // Only the directory created above is sent to the recycle bin, never a
    // pre-existing root item.  This also removes every test descendant.
    let cleanup = client.trash(&[base.id.clone()]);
    remove_local(&local_root);
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            if contains_id(&client.list("0")?.items, &base.id) {
                Err(failure(
                    "PikPak test directory remained visible after trash",
                ))
            } else {
                Ok(())
            }
        }
        (Err(test), Ok(())) => Err(test),
        (Ok(()), Err(cleanup)) => Err(Box::new(cleanup)),
        (Err(test), Err(cleanup)) => Err(failure(format!(
            "test failed ({test}); cleanup of only its test directory also failed ({cleanup})"
        ))),
    }
}

#[test]
#[ignore = "requires an explicitly authorized PikPak account and creates only VersaDown-IT-* test data"]
fn live_pikpak_cache_downloads_uploaded_bytes_and_trashes_only_its_directory() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let http = LiveTransport::new()?;
    let client = PikPakClient::new(&http, &store);
    let (local_root, payload, expected) = local_artifact("pikpak-cache")?;
    let base_name = test_name("pikpak-cache", "root");

    let initial_root = client.list("0")?;
    if initial_root.items.iter().any(|item| item.name == base_name) {
        remove_local(&local_root);
        return Err(failure("unexpected test-directory name collision"));
    }
    let base = client.mkdir("0", &base_name)?;
    let result = (|| -> LiveResult<()> {
        let uploaded = provider_step("PikPak cache-test upload", || {
            client.upload_file(&base.id, &payload)
        })?;
        let (url, download_name) = provider_step("PikPak CACHE resolve download", || {
            client.download(&uploaded.id)
        })?;
        if download_name != "payload.txt" {
            return Err(failure(
                "PikPak CACHE download metadata did not preserve payload name",
            ));
        }
        if download_bytes(&url)? != expected {
            return Err(failure(
                "PikPak CACHE download bytes differ from uploaded payload",
            ));
        }
        Ok(())
    })();
    let cleanup = client.trash(&[base.id.clone()]);
    remove_local(&local_root);
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(test), Ok(())) => Err(test),
        (Ok(()), Err(cleanup)) => Err(Box::new(cleanup)),
        (Err(test), Err(cleanup)) => Err(failure(format!(
            "test failed ({test}); cleanup of only its test directory also failed ({cleanup})"
        ))),
    }
}

#[test]
#[ignore = "requires an explicitly authorized Xunlei account and creates only VersaDown-IT-* test data"]
fn live_xunlei_directory_upload_rename_move_download_and_trash() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let session: XunleiSession = serde_json::from_slice(
        &store
            .load("xunlei")?
            .ok_or_else(|| failure("Xunlei is not currently authorized"))?,
    )?;
    eprintln!(
        "Xunlei live session: web_auth={}, user_id_present={}, browser_client={}, browser_device={}, browser_version={}, browser_package={}, browser_user_agent={}, captcha_present={}",
        matches!(session.auth_method, versadown_lib::cloud::CloudAuthMethod::Web),
        !session.user_id.is_empty(),
        !session.profile.client_id.is_empty(),
        !session.profile.device_id.is_empty(),
        !session.profile.client_version.is_empty(),
        !session.profile.package_name.is_empty(),
        !session.profile.user_agent.is_empty(),
        !session.security_token.is_empty() || !session.web_security.captcha_token.is_empty(),
    );
    let http = LiveTransport::new()?;
    let client = XunleiClient::new(session, &http);
    let (local_root, payload, mut expected) = local_artifact("xunlei")?;
    // Avoid reusing a previously indexed GCID when investigating readiness
    // immediately after a genuinely new object-store upload.
    expected.resize(32768, 0);
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut expected);
    fs::write(&payload, &expected)?;
    let base_name = test_name("xunlei", "root");
    let target_name = test_name("xunlei", "move-target");
    let renamed_name = test_name("xunlei", "renamed.txt");

    // This first request is intentionally mutation-free and is also the
    // real-world probe for headers carried by browser-bearer authorization.
    let initial_root = provider_step("Xunlei root", || client.directory("0"))?;
    if initial_root.items.iter().any(|item| item.name == base_name) {
        remove_local(&local_root);
        return Err(failure("unexpected test-directory name collision"));
    }
    provider_step("Xunlei create test root", || client.mkdir("0", &base_name))?;
    let base = find_named(
        &provider_step("Xunlei locate test root", || client.directory("0"))?.items,
        &base_name,
        "folder",
    )?;
    let result = (|| -> LiveResult<()> {
        provider_step("Xunlei create destination", || {
            client.mkdir(&base.id, &target_name)
        })?;
        let target = find_named(
            &provider_step("Xunlei locate destination", || client.directory(&base.id))?.items,
            &target_name,
            "folder",
        )?;
        let uploaded = provider_step("Xunlei upload", || client.upload_file(&base.id, &payload))?;
        // OSS transfer completion precedes the drive's asynchronous file
        // finalization; renaming a pending object returns file_rename_uncompleted.
        let mut upload_ready = false;
        for _ in 0..30 {
            if provider_step("Xunlei wait upload completion", || {
                client.directory(&base.id)
            })?
            .items
            .iter()
            .any(|file| file.id == uploaded.id)
            {
                upload_ready = true;
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
        if !upload_ready {
            return Err(failure(
                "Xunlei uploaded object was not finalized within 30 seconds",
            ));
        }
        provider_step("Xunlei rename", || {
            client.rename(&uploaded.id, &renamed_name)
        })?;

        // Xunlei hides non-complete files from ordinary directory results;
        // wait for the provider's asynchronous completion before moving it.
        let mut renamed = None;
        for _ in 0..30 {
            let files = provider_step("Xunlei verify rename", || client.directory(&base.id))?;
            if let Some(file) = files
                .items
                .into_iter()
                .find(|item| item.id == uploaded.id && item.name == renamed_name)
            {
                renamed = Some(file);
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
        let renamed =
            renamed.ok_or_else(|| failure("Xunlei upload did not reach complete state"))?;
        provider_step("Xunlei move", || {
            client.move_files(&[renamed.id.clone()], &target.id)
        })?;
        let moved = find_named(
            &provider_step("Xunlei verify move", || client.directory(&target.id))?.items,
            &renamed_name,
            "file",
        )?;
        provider_step("Xunlei copy", || {
            client.copy_files(&[moved.id.clone()], &base.id)
        })?;
        let copied = find_named(
            &provider_step("Xunlei verify copy", || client.directory(&base.id))?.items,
            &renamed_name,
            "file",
        )?;
        provider_step("Xunlei resolve immediate copied download", || {
            client.download(&copied.id)
        })?;
        let (url, download_name) =
            provider_step("Xunlei resolve download", || client.download(&moved.id))?;
        if download_name != renamed_name {
            return Err(failure(
                "Xunlei download metadata did not preserve the renamed file",
            ));
        }
        let session = client.session();
        if download_bytes_with_headers(
            &url,
            &[
                ("User-Agent", session.profile.user_agent.as_str()),
                ("Referer", "https://pan.xunlei.com/"),
            ],
        )? != expected
        {
            return Err(failure(
                "Xunlei downloaded bytes differ from the uploaded test file",
            ));
        }
        if copied.id == moved.id {
            return Err(failure("Xunlei copy returned the source object"));
        }
        for _ in 0..2 {
            provider_step("Xunlei resolve newly copied download", || {
                client.download(&copied.id)
            })?;
            provider_step("Xunlei resolve moved download again", || {
                client.download(&moved.id)
            })?;
        }
        provider_step("Xunlei trash copied fixture", || {
            client.trash(&[copied.id.clone()])
        })?;
        Ok(())
    })();

    // `trash` is a recycle-bin move.  The id was obtained by listing the
    // just-created uniquely named directory, so no user file can be targeted.
    let cleanup = client.trash(&[base.id.clone()]);
    if let Ok(()) = cleanup {
        // A token refresh is persisted only after all operations and cleanup.
        // Browser-only sessions simply write the equivalent current session.
        let session = client.session();
        store.save("xunlei", &serde_json::to_vec(&session)?)?;
    }
    remove_local(&local_root);
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            if contains_id(
                &provider_step("Xunlei verify trash", || client.directory("0"))?.items,
                &base.id,
            ) {
                Err(failure(
                    "Xunlei test directory remained visible after trash",
                ))
            } else {
                Ok(())
            }
        }
        (Err(test), Ok(())) => Err(test),
        (Ok(()), Err(cleanup)) => Err(failure(safe_provider_failure("Xunlei cleanup", &cleanup))),
        (Err(test), Err(cleanup)) => Err(failure(format!(
            "test failed ({test}); {}",
            safe_provider_failure("Xunlei cleanup of its test directory", &cleanup)
        ))),
    }
}

#[test]
#[ignore = "requires an explicitly authorized Xunlei account; performs only two read-only browser-contract probes"]
fn live_xunlei_browser_contract_probes_origin_and_referer_separately() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let session: XunleiSession = serde_json::from_slice(
        &store
            .load("xunlei")?
            .ok_or_else(|| failure("Xunlei is not currently authorized"))?,
    )?;
    eprintln!(
        "Xunlei browser probe session: client={}, device={}, version={}, package={}, user_agent={}, captcha={}",
        !session.profile.client_id.is_empty(),
        !session.profile.device_id.is_empty(),
        !session.profile.client_version.is_empty(),
        !session.profile.package_name.is_empty(),
        !session.profile.user_agent.is_empty(),
        !session.security_token.is_empty(),
    );
    let http = LiveTransport::new()?;

    // Same endpoint and request shape; Origin/Referer are the only difference
    // between the two probes. This identifies whether these browser headers
    // are actually required without guessing or exposing credentials.
    let without_web_headers = xunlei_browser_root_probe(&http, &session, false);
    let with_web_headers = xunlei_browser_root_probe(&http, &session, true);
    match (without_web_headers, with_web_headers) {
        (Ok(()), Ok(())) => Ok(()),
        (without, with) => Err(failure(format!(
            "Xunlei browser probe outcomes: no Origin/Referer={}; with Origin/Referer={}",
            without.err().map_or("ok".into(), |error| error.to_string()),
            with.err().map_or("ok".into(), |error| error.to_string()),
        ))),
    }
}
