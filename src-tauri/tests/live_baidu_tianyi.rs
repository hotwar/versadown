//! Explicit, opt-in live regressions for the browser-authorized Baidu and
//! Tianyi drives.
//!
//! The tests deliberately create only one freshly generated
//! `VersaDown-E2E-*` directory per provider.  It is the sole remote item
//! moved to the provider recycle bin during cleanup.  Session bytes, cookies,
//! signed URLs and account details are never printed.

use versadown_lib::cloud::{
    auth_supervisor::{recover_in_order, AuthSupervisor},
    baidu::{BaiduClient, BaiduSession},
    share::{
        CloudShareResolvedEntry, ShareInspect, ShareInspection, ShareResolution, ShareResolve,
    },
    tianyi::{TianyiClient, TianyiSession},
    CloudFile, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudProvider,
    CloudUploadTransport, CredentialStore, ProviderRegistry, QuarkError, ReqwestTransport,
    SessionWorkKind, SystemCredentialStore, UploadHttpRequest, UploadHttpResponse,
};
use versadown_lib::extract_cloud_share_candidate;
use std::{
    error::Error,
    fs, io,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type LiveResult<T> = Result<T, Box<dyn Error>>;

/// Delegates to the real HTTPS transport without logging a request URL or
/// headers.  Both can contain sensitive signed credentials.
struct LiveTransport {
    inner: ReqwestTransport,
    /// Diagnostic-only metadata.  It intentionally excludes URLs, header
    /// values, response bodies, account data and all credential material.
    observations: Mutex<Vec<String>>,
}

impl LiveTransport {
    fn new() -> Result<Self, QuarkError> {
        Ok(Self {
            inner: ReqwestTransport::new()?,
            observations: Mutex::new(Vec::new()),
        })
    }

    fn report(&self) {
        for observation in self.observations.lock().unwrap().iter() {
            eprintln!("live cloud request: {observation}");
        }
    }

    fn observe_request(request: &CloudHttpRequest) -> String {
        let host = url::Url::parse(&request.url)
            .ok()
            .and_then(|value| value.host_str().map(str::to_owned))
            .unwrap_or_else(|| "invalid-host".into());
        let presence = |name: &str| request.headers.contains_key(name);
        format!(
            "{} {host}; cookie={}, origin={}, referer={}, user_agent={}",
            request.method,
            presence("Cookie"),
            presence("Origin"),
            presence("Referer"),
            presence("User-Agent"),
        )
    }
}

/// Reports only response schema and public protocol flags. It must never
/// include filenames, share identifiers, cookies, URLs, access codes, or
/// server text, all of which may be private share context.
fn public_value_shape(value: Option<&serde_json::Value>) -> String {
    match value {
        None => "absent".into(),
        Some(serde_json::Value::Null) => "null".into(),
        Some(serde_json::Value::Bool(value)) => format!("bool({value})"),
        Some(serde_json::Value::Number(value)) => format!("number({value})"),
        Some(serde_json::Value::String(value)) => {
            let safe = value.len() <= 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte));
            if safe {
                format!("code({value})")
            } else {
                format!("string(len={})", value.len())
            }
        }
        Some(serde_json::Value::Array(values)) => format!("array({})", values.len()),
        Some(serde_json::Value::Object(_)) => "object".into(),
    }
}

fn share_response_profile(value: Option<&serde_json::Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let mut fields = Vec::new();
    for field in [
        "errno",
        "errtype",
        "errorCode",
        "needAccessCode",
        "isFolder",
        "shareMode",
    ] {
        if value.get(field).is_some() {
            fields.push(format!("{field}={}", public_value_shape(value.get(field))));
        }
    }
    if let Some(list) = value.get("fileListAO") {
        fields.push(format!(
            "list.count={}",
            public_value_shape(list.get("count"))
        ));
        fields.push(format!(
            "list.files={}",
            public_value_shape(list.get("fileList"))
        ));
        fields.push(format!(
            "list.folders={}",
            public_value_shape(list.get("folderList"))
        ));
    }
    if let Some(data) = value.get("data") {
        fields.push(format!("data={}", public_value_shape(Some(data))));
        if let Some(data) = data.as_object() {
            for field in ["shareid", "uk", "has_more", "list"] {
                if let Some(value) = data.get(field) {
                    fields.push(format!("data.{field}={}", public_value_shape(Some(value))));
                }
            }
            if data.contains_key("seckey") {
                fields.push("data.seckey=present".into());
            }
        }
    }
    if let Some(list) = value.get("list") {
        fields.push(format!("list={}", public_value_shape(Some(list))));
    }
    if fields.is_empty() {
        String::new()
    } else {
        format!("; profile({})", fields.join(","))
    }
}

impl CloudHttpTransport for LiveTransport {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let request_description = Self::observe_request(&request);
        match self.inner.send(request) {
            Ok(response) => {
                let parsed = serde_json::from_str::<serde_json::Value>(&response.body).ok();
                let code = parsed.as_ref().and_then(|value| {
                    value
                        .get("errno")
                        .or_else(|| value.get("res_code"))
                        .or_else(|| value.get("result"))
                        .or_else(|| value.get("code"))
                        .or_else(|| value.get("errorCode"))
                        .and_then(serde_json::Value::as_i64)
                });
                let markup_code = (!response.body.trim_start().starts_with('{'))
                    .then(|| {
                        regex::Regex::new(r"<(?:errno|res_code|result|code)>\s*(-?\d+)")
                            .ok()
                            .and_then(|pattern| pattern.captures(&response.body))
                            .and_then(|capture| capture.get(1))
                            .map(|value| value.as_str().to_owned())
                    })
                    .flatten();
                let shape = match parsed.as_ref() {
                    Some(serde_json::Value::Object(object)) => {
                        let mut keys = object.keys().cloned().collect::<Vec<_>>();
                        keys.sort();
                        format!("json-object({})", keys.join(","))
                    }
                    Some(serde_json::Value::Array(_)) => "json-array".into(),
                    Some(_) => "json-scalar".into(),
                    None if response.body.trim_start().starts_with('<') => {
                        let root = regex::Regex::new(
                            r"<\??[A-Za-z0-9_.:-]+[^>]*>\s*<([A-Za-z][A-Za-z0-9_-]*)",
                        )
                        .ok()
                        .and_then(|pattern| pattern.captures(&response.body))
                        .and_then(|capture| capture.get(1))
                        .map(|value| value.as_str())
                        .unwrap_or("unknown");
                        let tags = regex::Regex::new(r"</?([A-Za-z][A-Za-z0-9_-]*)")
                            .ok()
                            .map(|pattern| {
                                let mut tags = pattern
                                    .captures_iter(&response.body)
                                    .filter_map(|capture| {
                                        capture.get(1).map(|value| value.as_str())
                                    })
                                    .collect::<Vec<_>>();
                                tags.sort_unstable();
                                tags.dedup();
                                tags.into_iter().take(8).collect::<Vec<_>>().join(",")
                            })
                            .unwrap_or_default();
                        format!("markup(root={root};{tags})")
                    }
                    None => "non-json".into(),
                };
                self.observations.lock().unwrap().push(format!(
                    "{request_description}; HTTP {}; provider_code={}; response={shape}{}",
                    response.status,
                    code.map(|value| value.to_string())
                        .or(markup_code)
                        .unwrap_or_else(|| "none".into()),
                    share_response_profile(parsed.as_ref())
                ));
                Ok(response)
            }
            Err(error) => {
                self.observations.lock().unwrap().push(format!(
                    "{request_description}; transport={}",
                    provider_error_kind(&error)
                ));
                Err(error)
            }
        }
    }
}

impl CloudUploadTransport for LiveTransport {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
        self.inner.send_upload(request)
    }
}

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

fn provider_error_kind(error: &QuarkError) -> &'static str {
    match error {
        QuarkError::NotAuthorized => "not-authorized",
        QuarkError::AccountMismatch => "account-mismatch",
        QuarkError::AuthorizationExpired => "authorization-expired",
        QuarkError::WebAuthorizationRequired => "web-authorization-required",
        QuarkError::CaptchaRequired => "captcha-required",
        QuarkError::PikpakOperationVerificationRequired(_) => {
            "pikpak-operation-verification-required"
        }
        QuarkError::PikpakRegionalMembershipRestricted => "pikpak-regional-membership-restricted",
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
        QuarkError::BaiduOperationVerificationRequired => "operation-verification-required",
        QuarkError::BaiduUploadAuthorizationRequired(_) => "baidu-upload-authorization-required",
        QuarkError::XunleiNativeDownload(_) => "xunlei-native-download-error",
        QuarkError::BaiduTransferPending => "baidu-transfer-pending",
        QuarkError::ShareTransferDownloadUnavailable => "transfer-saved-download-unavailable",
        QuarkError::Mega(_) => "mega-link-error",
    }
}

fn provider_step<T>(
    provider: &str,
    operation: &str,
    action: impl FnOnce() -> Result<T, QuarkError>,
) -> LiveResult<T> {
    action().map_err(|error| {
        failure(format!(
            "{provider} {operation}: {}",
            provider_error_kind(&error)
        ))
    })
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
    let app_data = std::env::var_os("APPDATA")
        .ok_or_else(|| failure("APPDATA is required for the encrypted session store"))?;
    Ok(SystemCredentialStore::new(
        PathBuf::from(app_data)
            .join("com.versadown.desktop")
            .join("cloud-sessions"),
    )?)
}

#[test]
#[ignore = "requires saved Baidu authorization; read-only file metadata and 1 KiB download probe"]
fn live_baidu_personal_download_bytes() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let session: BaiduSession = serde_json::from_slice(
        &store
            .load("baidu")?
            .ok_or_else(|| failure("Baidu session missing"))?,
    )?;
    let http = LiveTransport::new()?;
    let client = BaiduClient::new(session, &http);
    let result = (|| -> LiveResult<()> {
        client.account()?;
        let root = client.directory("/")?;
        let file = root
            .items
            .iter()
            .find(|file| file.kind == "file" && file.size.unwrap_or(0) > 0)
            .ok_or_else(|| failure("No nonempty file in root for read-only probe"))?;
        let download = client.download(&file.id)?;
        let network = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let mut request = network.get(&download.url).header("Range", "bytes=0-1023");
        for (name, value) in &download.headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .map_err(|_| failure("Download network failure"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(failure(format!("Download HTTP {status}")));
        }
        let mut bytes = Vec::new();
        response.take(1024).read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.starts_with(b"{\"error") || bytes.starts_with(b"<!DOCTYPE") {
            return Err(failure("Download returned error/empty payload"));
        }
        eprintln!(
            "Baidu personal file: HTTP {status}, {} bytes received",
            bytes.len()
        );
        Ok(())
    })();
    http.report();
    result
}

fn supervised_directory(provider: CloudProvider, root: &str) -> Result<(), QuarkError> {
    let store = session_store().map_err(|_| QuarkError::Storage)?;
    let http = ReqwestTransport::new()?;
    let supervisor = AuthSupervisor::default();
    supervisor.restore(&store, provider)?;
    supervisor.run_checked(
        &store,
        provider,
        SessionWorkKind::Read,
        |snapshot| {
            ProviderRegistry::global()
                .driver(provider, snapshot, &http)
                .list_directory(root)
                .map(|_| ())
        },
        |phase| {
            let mut driver = ProviderRegistry::global().driver(provider, &store, &http);
            recover_in_order(provider, &store, driver.auth(), phase, || Ok(false))
        },
        || Ok(()),
    )
}

fn test_name(provider: &str, suffix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("VersaDown-E2E-{provider}-{millis}-{suffix}")
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

fn find_named(directory: &[CloudFile], name: &str, kind: &str) -> LiveResult<CloudFile> {
    directory
        .iter()
        .find(|item| item.name == name && item.kind == kind)
        .cloned()
        .ok_or_else(|| failure(format!("newly created {kind} {name:?} was not listed")))
}

fn download_bytes(
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> LiveResult<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut request = client.get(url);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request.send()?;
    if !response.status().is_success() {
        return Err(failure(format!(
            "provider direct download returned HTTP {}",
            response.status()
        )));
    }
    Ok(response.bytes()?.to_vec())
}

fn remove_local(root: &Path) {
    let _ = fs::remove_dir_all(root);
}

fn cookie_names(cookie: &str) -> String {
    let mut names = cookie
        .split(';')
        .filter_map(|pair| pair.trim().split_once('=').map(|(name, _)| name.trim()))
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    names.join(",")
}

fn without_cookie_name(cookie: &str, rejected: &str) -> String {
    cookie
        .split(';')
        .map(str::trim)
        .filter(|pair| {
            pair.split_once('=')
                .is_some_and(|(name, _)| name != rejected)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Opens a real direct-download response but consumes at most the first KiB.
/// This verifies the transfer boundary while keeping the 33 MB Tianyi sample
/// and any other large public sample out of the local Downloads directory.
/// The signed URL and all request headers deliberately remain unreported.
fn probe_direct_download(
    url: &str,
    headers: &[(String, String)],
) -> LiveResult<(u16, usize, Option<u64>)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut request = client.get(url).header("Range", "bytes=0-1023");
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let mut response = request.send()?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(failure(format!(
            "provider direct download returned HTTP {status}"
        )));
    }
    let total = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit('/').next())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| response.content_length());
    let mut bytes = [0_u8; 1024];
    let read = response.read(&mut bytes)?;
    if read == 0 {
        return Err(failure("provider direct download returned an empty body"));
    }
    Ok((status, read, total))
}

fn require_live_share_samples() -> LiveResult<()> {
    if std::env::var("VERSADOWN_LIVE_SHARE_SAMPLES").as_deref() != Ok("1") {
        return Err(failure(
            "live share-sample regression requires VERSADOWN_LIVE_SHARE_SAMPLES=1",
        ));
    }
    Ok(())
}

fn app_session_store() -> LiveResult<SystemCredentialStore> {
    let app_data = std::env::var_os("APPDATA")
        .ok_or_else(|| failure("APPDATA is required for the encrypted session store"))?;
    Ok(SystemCredentialStore::new(
        PathBuf::from(app_data)
            .join("com.versadown.desktop")
            .join("cloud-sessions"),
    )?)
}

fn supplied_access_code(provider: &str, category: &str) -> Option<String> {
    let variable = match (provider, category) {
        ("百度", "目录（不提供访问码）") => "VERSADOWN_LIVE_BAIDU_ACCESS_CODE",
        _ => return None,
    };
    std::env::var(variable)
        .ok()
        .filter(|value| !value.is_empty())
}

fn report_share_error(provider: &str, category: &str, error: &QuarkError) {
    // `Display` contains the app's localized user-facing error, never a
    // request URL, cookie, signed link, or access code.
    eprintln!("LIVE-SHARE {provider} {category}: error={error}");
}

/// Isolates which official web-context headers Baidu currently requires for
/// the read-only share-list endpoint. Each request only lists the same public
/// root and logs the numeric result code, never the response or session data.
fn probe_baidu_share_header_profiles(
    credentials: &SystemCredentialStore,
    http: &LiveTransport,
) -> LiveResult<()> {
    let session: BaiduSession = serde_json::from_slice(
        &credentials
            .load("baidu")?
            .ok_or_else(|| failure("百度测试会话不存在"))?,
    )?;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("shorturl", "dG1NCeH")
        .append_pair("pwd", "")
        .append_pair("root", "1")
        .append_pair("dir", "/")
        .append_pair("num", "100")
        .append_pair("page", "1")
        .append_pair("order", "time")
        .finish();
    let native = "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;";
    let browser = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36";
    for (profile, user_agent, origin, referer) in [
        ("native", native, None, None),
        (
            "native-referer",
            native,
            None,
            Some("https://pan.baidu.com/s/1dG1NCeH"),
        ),
        (
            "browser-referer",
            browser,
            None,
            Some("https://pan.baidu.com/s/1dG1NCeH"),
        ),
        (
            "browser-origin-referer",
            browser,
            Some("https://pan.baidu.com"),
            Some("https://pan.baidu.com/s/1dG1NCeH"),
        ),
    ] {
        let mut headers = std::collections::BTreeMap::from([
            ("Cookie".into(), session.cookie.clone()),
            ("User-Agent".into(), user_agent.into()),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
        ]);
        if let Some(origin) = origin {
            headers.insert("Origin".into(), origin.into());
        }
        if let Some(referer) = referer {
            headers.insert("Referer".into(), referer.into());
        }
        let response = http.send(CloudHttpRequest {
            method: "POST",
            url: "https://pan.baidu.com/share/wxlist?channel=weixin&version=2.2.2&clienttype=25&web=1".into(),
            headers,
            body: Some(form.clone()),
        })?;
        let value: serde_json::Value = serde_json::from_str(&response.body)
            .map_err(|_| failure("百度分享端点返回了无效 JSON"))?;
        let errno = value
            .get("errno")
            .and_then(serde_json::Value::as_i64)
            .map_or_else(|| "absent".into(), |value| value.to_string());
        let errtype = value
            .get("errtype")
            .and_then(serde_json::Value::as_i64)
            .map_or_else(|| "absent".into(), |value| value.to_string());
        eprintln!("LIVE-SHARE 百度 header-profile={profile}: errno={errno}, errtype={errtype}");
    }
    let query = "shorturl=dG1NCeH&root=1&dir=%2F&num=100&page=1&order=time&desc=1";
    for (method, body, mode) in [
        (
            "GET",
            None,
            "https://pan.baidu.com/share/list?web=1&channel=chunlei&clienttype=0&app_id=250528",
        ),
        (
            "POST",
            Some(form.clone()),
            "https://pan.baidu.com/share/list?web=1&channel=chunlei&clienttype=0&app_id=250528",
        ),
    ] {
        for (profile, user_agent, referer) in [
            ("native", native, None),
            (
                "native-referer",
                native,
                Some("https://pan.baidu.com/s/1dG1NCeH"),
            ),
            (
                "browser-referer",
                browser,
                Some("https://pan.baidu.com/s/1dG1NCeH"),
            ),
        ] {
            let mut headers = std::collections::BTreeMap::from([
                ("Cookie".into(), session.cookie.clone()),
                ("User-Agent".into(), user_agent.into()),
            ]);
            if let Some(referer) = referer {
                headers.insert("Referer".into(), referer.into());
            }
            if method == "POST" {
                headers.insert(
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                );
            }
            let response = http.send(CloudHttpRequest {
                method,
                url: format!("{mode}&{query}"),
                headers,
                body: body.clone(),
            })?;
            let value: serde_json::Value = serde_json::from_str(&response.body)
                .map_err(|_| failure("百度候选分享端点返回了无效 JSON"))?;
            let errno = value
                .get("errno")
                .and_then(serde_json::Value::as_i64)
                .map_or_else(|| "absent".into(), |value| value.to_string());
            eprintln!(
                "LIVE-SHARE 百度 endpoint-profile=share-list-{method}-{profile}: errno={errno}{}",
                share_response_profile(Some(&value))
            );
        }
    }
    let adapter_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("shorturl", "dG1NCeH")
        .append_pair("pwd", "")
        .append_pair("root", "1")
        .append_pair("dir", "/")
        .append_pair("num", "100")
        .append_pair("page", "1")
        .append_pair("order", "time")
        .finish();
    let response = http.send(CloudHttpRequest {
        method: "GET",
        url: format!(
            "https://pan.baidu.com/share/list?web=1&channel=chunlei&clienttype=0&app_id=250528&{adapter_query}"
        ),
        headers: std::collections::BTreeMap::from([
            ("Cookie".into(), session.cookie),
            ("User-Agent".into(), native.into()),
        ]),
        body: None,
    })?;
    let value: serde_json::Value = serde_json::from_str(&response.body)
        .map_err(|_| failure("百度当前分享请求返回了无效 JSON"))?;
    let errno = value
        .get("errno")
        .and_then(serde_json::Value::as_i64)
        .map_or_else(|| "absent".into(), |value| value.to_string());
    eprintln!(
        "LIVE-SHARE 百度 endpoint-profile=adapter-current: errno={errno}{}",
        share_response_profile(Some(&value))
    );
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_SHARE_SAMPLES=1 and an existing local Baidu session"]
fn live_baidu_share_header_profile_probe() -> LiveResult<()> {
    require_live_share_samples()?;
    let credentials = app_session_store()?;
    let http = LiveTransport::new()?;
    probe_baidu_share_header_profiles(&credentials, &http)?;
    http.report();
    Ok(())
}

#[test]
#[ignore = "diagnostic: requires VERSADOWN_LIVE_SHARE_SAMPLES=1 and an existing local Baidu session"]
fn live_baidu_single_download_response_probe() -> LiveResult<()> {
    require_live_share_samples()?;
    let credentials = app_session_store()?;
    let http = LiveTransport::new()?;
    let candidate = extract_cloud_share_candidate("https://pan.baidu.com/s/1dG1NCeH", None)?
        .ok_or_else(|| failure("百度单文件样本未被识别"))?;
    let adapter = versadown_lib::cloud::share::provider_share_adapter(
        candidate.provider,
        &credentials,
        &http,
    )?;
    let ShareInspection::Plan(plan) = adapter.inspect(&candidate)? else {
        return Err(failure("百度单文件样本意外要求访问码"));
    };
    let share_id = plan.provider_context["shareid"]
        .as_str()
        .ok_or_else(|| failure("百度单文件样本缺少 shareid"))?
        .to_owned();
    let uk = plan.provider_context["uk"]
        .as_str()
        .ok_or_else(|| failure("百度单文件样本缺少 uk"))?
        .to_owned();
    let shorturl = plan.provider_context["shorturl"]
        .as_str()
        .ok_or_else(|| failure("百度单文件样本缺少 shorturl"))?
        .to_owned();
    let session: BaiduSession = serde_json::from_slice(
        &credentials
            .load("baidu")?
            .ok_or_else(|| failure("百度测试会话不存在"))?,
    )?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("surl", &shorturl)
        .append_pair("fields", "sign,timestamp")
        .append_pair("channel", "chunlei")
        .append_pair("web", "1")
        .append_pair("app_id", "250528")
        .append_pair("clienttype", "0")
        .append_pair("share_id", &share_id)
        .append_pair("uk", &uk)
        .finish();
    for (profile, user_agent, referer) in [
        (
            "native",
            "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;",
            None,
        ),
        (
            "native-referer",
            "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;",
            Some("https://pan.baidu.com/s/1dG1NCeH"),
        ),
        (
            "browser-referer",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36",
            Some("https://pan.baidu.com/s/1dG1NCeH"),
        ),
    ] {
        let mut headers = std::collections::BTreeMap::from([
            ("Cookie".into(), session.cookie.clone()),
            ("User-Agent".into(), user_agent.into()),
        ]);
        if let Some(referer) = referer {
            headers.insert("Referer".into(), referer.into());
        }
        let response = http.send(CloudHttpRequest {
            method: "GET",
            url: format!("https://pan.baidu.com/share/tplconfig?{query}"),
            headers,
            body: None,
        })?;
        let value: serde_json::Value = serde_json::from_str(&response.body)
            .map_err(|_| failure("百度签名配置端点返回了无效 JSON"))?;
        let errno = value
            .get("errno")
            .and_then(serde_json::Value::as_i64)
            .map_or_else(|| "absent".into(), |value| value.to_string());
        eprintln!("LIVE-SHARE 百度 tplconfig-profile={profile}: errno={errno}");
    }
    let leading_surl_query =
        query.replacen(&format!("surl={shorturl}"), &format!("surl=1{shorturl}"), 1);
    for (profile, user_agent, referer) in [
        (
            "native-leading-surl",
            "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;",
            None,
        ),
        (
            "browser-referer-leading-surl",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36",
            Some("https://pan.baidu.com/s/1dG1NCeH"),
        ),
    ] {
        let mut headers = std::collections::BTreeMap::from([
            ("Cookie".into(), session.cookie.clone()),
            ("User-Agent".into(), user_agent.into()),
        ]);
        if let Some(referer) = referer {
            headers.insert("Referer".into(), referer.into());
        }
        let response = http.send(CloudHttpRequest {
            method: "GET",
            url: format!("https://pan.baidu.com/share/tplconfig?{leading_surl_query}"),
            headers,
            body: None,
        })?;
        let value: serde_json::Value = serde_json::from_str(&response.body)
            .map_err(|_| failure("百度带前缀签名配置端点返回了无效 JSON"))?;
        let errno = value
            .get("errno")
            .and_then(serde_json::Value::as_i64)
            .map_or_else(|| "absent".into(), |value| value.to_string());
        eprintln!("LIVE-SHARE 百度 tplconfig-profile={profile}: errno={errno}");
    }
    let entry = CloudShareResolvedEntry {
        provider: plan.provider,
        entry: plan
            .entries
            .first()
            .cloned()
            .ok_or_else(|| failure("百度单文件样本缺少文件项"))?,
        plan,
    };
    match adapter.resolve(&entry) {
        Ok(ShareResolution::Direct(_)) => eprintln!("LIVE-SHARE 百度 单文件: direct-resolution=ok"),
        Ok(ShareResolution::TransferRequired(_)) => {
            eprintln!("LIVE-SHARE 百度 单文件: transfer-required")
        }
        Err(error) => report_share_error("百度", "单文件 download-resolution", &error),
    }
    http.report();
    Ok(())
}

/// Exercised against the user-supplied current samples only when explicitly
/// enabled. It performs read-only share inspection plus a 1 KiB range probe
/// for directly downloadable single files; it never stores a share plan,
/// submits an aria2 task, copies share content to a personal cloud drive, or
/// guesses a password. This keeps the user's task list and cloud data intact.
#[test]
#[ignore = "requires VERSADOWN_LIVE_SHARE_SAMPLES=1 and the current local Baidu/Tianyi sessions"]
fn live_baidu_and_tianyi_share_samples() -> LiveResult<()> {
    require_live_share_samples()?;
    let credentials = app_session_store()?;
    let http = LiveTransport::new()?;
    for (provider, session) in [
        (
            "百度",
            credentials
                .load("baidu")?
                .ok_or_else(|| failure("百度测试会话不存在"))?,
        ),
        (
            "天翼",
            credentials
                .load("tianyi")?
                .ok_or_else(|| failure("天翼测试会话不存在"))?,
        ),
    ] {
        let result = if provider == "百度" {
            let session: BaiduSession = serde_json::from_slice(&session)?;
            BaiduClient::new(session, &http).account().map(|_| ())
        } else {
            let session: TianyiSession = serde_json::from_slice(&session)?;
            TianyiClient::new(session, &http).account().map(|_| ())
        };
        match result {
            Ok(()) => eprintln!("LIVE-SHARE {provider} account-probe=ok"),
            Err(error) => report_share_error(provider, "account-probe", &error),
        }
    }
    let samples = [
        (
            "百度",
            "单文件",
            "https://pan.baidu.com/s/1dG1NCeH",
            false,
            true,
        ),
        (
            "百度",
            "目录（不提供访问码）",
            "https://pan.baidu.com/s/1_49elQi_54BPJASvAxAB6w",
            true,
            false,
        ),
        (
            "百度",
            "已取消",
            "https://pan.baidu.com/s/1hIm_wG-LtGPYQ3lY2ANvxQ",
            false,
            false,
        ),
        (
            "百度",
            "带密码（不提供访问码）",
            "https://pan.baidu.com/s/1ekPm-ooS0uvVA_J7ZqVGDQ",
            true,
            false,
        ),
        (
            "天翼",
            "单文件",
            "https://cloud.189.cn/t/MjaIzmMJfeqy",
            false,
            true,
        ),
        (
            "天翼",
            "目录",
            "https://cloud.189.cn/t/F7jUJnM7rINv",
            false,
            false,
        ),
        (
            "天翼",
            "已失效",
            "https://cloud.189.cn/t/EvmUnymYBzyi",
            false,
            false,
        ),
        (
            "天翼",
            "带密码（不提供访问码）",
            "https://cloud.189.cn/t/R7ZR7f73aEBb",
            true,
            false,
        ),
    ];

    let mut failures = Vec::new();
    let provider_filter = std::env::var("VERSADOWN_LIVE_PROVIDER").ok();
    let sample_filter = std::env::var("VERSADOWN_LIVE_SAMPLE").ok();
    for (provider, category, url, expect_access_code, probe_download) in samples {
        if provider_filter
            .as_deref()
            .is_some_and(|filter| filter != provider)
            || sample_filter
                .as_deref()
                .is_some_and(|filter| filter != category)
        {
            continue;
        }
        let access_code = supplied_access_code(provider, category);
        let candidate = extract_cloud_share_candidate(url, access_code.as_deref())
            .map_err(|error| {
                failure(format!(
                    "{provider} {category}: parser rejected URL: {error}"
                ))
            })?
            .ok_or_else(|| {
                failure(format!(
                    "{provider} {category}: parser did not recognize URL"
                ))
            })?;
        let adapter = versadown_lib::cloud::share::provider_share_adapter(
            candidate.provider,
            &credentials,
            &http,
        )?;
        match adapter.inspect(&candidate) {
            Ok(ShareInspection::AccessCodeRequired)
                if expect_access_code && access_code.is_none() =>
            {
                eprintln!("LIVE-SHARE {provider} {category}: access-code-required");
            }
            Ok(ShareInspection::AccessCodeRequired) => failures.push(format!(
                "{provider} {category}: unexpectedly requested an access code"
            )),
            Ok(ShareInspection::Plan(plan)) if expect_access_code && access_code.is_none() => {
                failures.push(format!(
                "{provider} {category}: unexpectedly inspected without an access code ({} entries)",
                plan.entries.len()
            ))
            }
            Ok(ShareInspection::Plan(plan)) => {
                let root = match &plan.root_kind {
                    versadown_lib::cloud::share::CloudShareRootKind::File { .. } => "file",
                    versadown_lib::cloud::share::CloudShareRootKind::Directory => "directory",
                    versadown_lib::cloud::share::CloudShareRootKind::Collection => "collection",
                    versadown_lib::cloud::share::CloudShareRootKind::Unknown => "unknown",
                };
                eprintln!(
                    "LIVE-SHARE {provider} {category}: inspected root={root}, entries={}",
                    plan.entries.len()
                );
                if probe_download {
                    let entry = plan.entries.first().cloned().ok_or_else(|| {
                        failure(format!(
                            "{provider} {category}: share has no downloadable entry"
                        ))
                    })?;
                    let entry = CloudShareResolvedEntry {
                        provider: plan.provider,
                        plan,
                        entry,
                    };
                    match adapter.resolve(&entry) {
                        Ok(ShareResolution::Direct(download)) => {
                            let (status, bytes, total) =
                                probe_direct_download(&download.url, &download.headers)?;
                            eprintln!(
                                "LIVE-SHARE {provider} {category}: direct-transfer-started HTTP {status}, sampled={bytes}B, total={}",
                                total.map_or_else(|| "unknown".into(), |size| size.to_string())
                            );
                        }
                        Ok(ShareResolution::TransferRequired(_)) => failures.push(format!(
                            "{provider} {category}: requires a personal-cloud transfer before download; probe intentionally did not mutate cloud data"
                        )),
                        Err(error) => {
                            report_share_error(provider, category, &error);
                            if provider == "百度" && matches!(&error, QuarkError::CaptchaRequired) {
                                eprintln!(
                                    "LIVE-SHARE {provider} {category}: download blocked by the provider captcha; no bypass attempted"
                                );
                            } else if provider == "天翼"
                                && matches!(&error, QuarkError::AuthorizationExpired)
                            {
                                eprintln!(
                                    "LIVE-SHARE {provider} {category}: download blocked because the saved authorized session has expired"
                                );
                            } else {
                                failures.push(format!("{provider} {category}: {error}"));
                            }
                        }
                    }
                }
            }
            Err(error) => {
                report_share_error(provider, category, &error);
                if expect_access_code
                    && access_code.is_some()
                    && matches!(error, QuarkError::InvalidInput(_))
                {
                    eprintln!(
                        "LIVE-SHARE {provider} {category}: remote rejected the explicitly supplied access code"
                    );
                } else if expect_access_code {
                    failures.push(format!(
                        "{provider} {category}: expected access-code-required, got {error}"
                    ));
                } else if category.contains("已取消") || category.contains("已失效") {
                    if !matches!(error, QuarkError::Provider(_)) {
                        failures.push(format!(
                            "{provider} {category}: expected readable provider error, got {error}"
                        ));
                    }
                } else {
                    failures.push(format!("{provider} {category}: {error}"));
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failure(format!(
            "live share sample failures: {}",
            failures.join(" | ")
        )))
    }
}

fn tianyi_unauthenticated_response_shape(cookie: &str) -> LiveResult<String> {
    // No redirects are followed: this records only a status and a sanitized
    // destination host/path, allowing us to distinguish an API-shape issue
    // from a server-side login redirect without retaining a URL query.
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .get("https://cloud.189.cn/api/open/file/listFiles.action?folderId=-11&pageNum=1&pageSize=1&orderBy=1&order=ASC")
        .header("Cookie", cookie)
        .header("Origin", "https://cloud.189.cn")
        .header("Referer", "https://cloud.189.cn/")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36")
        .send()?;
    let destination = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| url::Url::parse(value).ok())
        .map(|url| format!("{}{}", url.host_str().unwrap_or("invalid-host"), url.path()))
        .unwrap_or_else(|| "none".into());
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or("none");
    Ok(format!(
        "HTTP {}; location={destination}; content_type={content_type}",
        response.status()
    ))
}

/// This validates the current browser session against Baidu's official Pan
/// API.  Baidu is intentionally the only provider that uses its `netdisk`
/// client identity: the client implementation owns that distinction.
#[test]
#[ignore = "requires saved Baidu credentials and performs only a production-supervised root read"]
fn live_baidu_production_supervised_session_recovery() -> LiveResult<()> {
    require_live_authorization()?;
    provider_step("Baidu", "production supervised root read", || {
        supervised_directory(CloudProvider::Baidu, "/")
    })
}

#[test]
#[ignore = "requires saved Baidu credentials and performs a read-only BFESS compatibility probe"]
fn live_baidu_bfess_compatibility_probe() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let mut session: BaiduSession = serde_json::from_slice(
        &store
            .load("baidu")?
            .ok_or_else(|| failure("Baidu is not currently authorized"))?,
    )?;
    session.cookie = without_cookie_name(&session.cookie, "BDUSS");
    let http = LiveTransport::new()?;
    let result = provider_step("Baidu", "BFESS compatibility root read", || {
        BaiduClient::new(session, &http).directory("/").map(|_| ())
    });
    http.report();
    result
}

#[test]
#[ignore = "requires an explicitly authorized Baidu account and creates only VersaDown-E2E-* test data"]
fn live_baidu_directory_upload_rename_move_download_and_trash() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let session: BaiduSession = serde_json::from_slice(
        &store
            .load("baidu")?
            .ok_or_else(|| failure("Baidu is not currently authorized"))?,
    )?;
    eprintln!(
        "Baidu live session: web_auth={}, cookie_present={}, cookie_names={}",
        matches!(
            session.auth_method,
            versadown_lib::cloud::CloudAuthMethod::Web
        ),
        !session.cookie.is_empty(),
        cookie_names(&session.cookie),
    );
    let http = LiveTransport::new()?;
    let client = BaiduClient::new(session, &http);
    let (local_root, payload, expected) = local_artifact("baidu")?;
    let base_name = test_name("baidu", "root");
    let target_name = test_name("baidu", "move-target");
    let renamed_name = test_name("baidu", "renamed.txt");

    let initial_root = match provider_step("Baidu", "list root", || client.directory("/")) {
        Ok(directory) => directory,
        Err(error) => {
            http.report();
            remove_local(&local_root);
            return Err(error);
        }
    };
    if initial_root.items.iter().any(|item| item.name == base_name) {
        remove_local(&local_root);
        return Err(failure("unexpected Baidu test-directory name collision"));
    }
    if let Err(error) = provider_step("Baidu", "mkdir root", || client.mkdir("/", &base_name)) {
        http.report();
        remove_local(&local_root);
        return Err(error);
    }
    let base = find_named(
        &provider_step("Baidu", "list after mkdir", || client.directory("/"))?.items,
        &base_name,
        "folder",
    )?;
    let result = (|| -> LiveResult<()> {
        provider_step("Baidu", "mkdir move target", || {
            client.mkdir(&base.id, &target_name)
        })?;
        let target = find_named(
            &provider_step("Baidu", "list move target", || client.directory(&base.id))?.items,
            &target_name,
            "folder",
        )?;
        let uploaded = provider_step("Baidu", "upload", || {
            client.upload_file(&base.id, &payload, &http)
        })?;
        provider_step("Baidu", "rename", || {
            client.rename(&uploaded.id, &renamed_name)
        })?;
        let renamed = find_named(
            &provider_step("Baidu", "list after rename", || client.directory(&base.id))?.items,
            &renamed_name,
            "file",
        )?;
        provider_step("Baidu", "move", || {
            client.move_files(&[renamed.id.clone()], &target.id)
        })?;
        let moved = find_named(
            &provider_step("Baidu", "list after move", || client.directory(&target.id))?.items,
            &renamed_name,
            "file",
        )?;
        let download = provider_step("Baidu", "resolve download", || client.download(&moved.id))?;
        if download.name != renamed_name {
            return Err(failure(
                "Baidu download metadata did not preserve renamed file",
            ));
        }
        if download_bytes(&download.url, &download.headers)? != expected {
            return Err(failure(
                "Baidu downloaded bytes differ from uploaded test file",
            ));
        }
        Ok(())
    })();

    // The id was produced by listing the unique directory we just created;
    // Baidu's delete operation moves it to the recycle bin.
    let cleanup = client.trash(&[base.id.clone()]);
    remove_local(&local_root);
    http.report();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            if client
                .directory("/")?
                .items
                .iter()
                .any(|item| item.id == base.id)
            {
                Err(failure("Baidu test directory remained visible after trash"))
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

/// Tianyi uses its own authenticated browser profile (Cookie, Origin,
/// Referer and a normal Chrome UA).  This regression verifies that exact
/// configured profile end-to-end; it deliberately never substitutes Baidu's
/// `netdisk` identity into a different provider's API.
#[test]
#[ignore = "requires saved Tianyi credentials and performs only a production-supervised root read"]
fn live_tianyi_production_supervised_session_recovery() -> LiveResult<()> {
    require_live_authorization()?;
    provider_step("Tianyi", "production supervised root read", || {
        supervised_directory(CloudProvider::Tianyi, "-11")
    })
}

#[test]
#[ignore = "requires an explicitly authorized Tianyi account and creates only VersaDown-E2E-* test data"]
fn live_tianyi_directory_upload_rename_move_download_and_trash() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let session: TianyiSession = serde_json::from_slice(
        &store
            .load("tianyi")?
            .ok_or_else(|| failure("Tianyi is not currently authorized"))?,
    )?;
    eprintln!(
        "Tianyi live session: web_auth={}, cookie_present={}, browser_context={}, cookie_names={}",
        matches!(
            session.auth_method,
            versadown_lib::cloud::CloudAuthMethod::Web
        ),
        !session.cookie.is_empty(),
        session.has_browser_context(),
        cookie_names(&session.cookie),
    );
    let http = LiveTransport::new()?;
    let client = TianyiClient::new(session.clone(), &http);
    let (local_root, payload, expected) = local_artifact("tianyi")?;
    let base_name = test_name("tianyi", "root");
    let target_name = test_name("tianyi", "move-target");
    let renamed_name = test_name("tianyi", "renamed.txt");

    let initial_root = match provider_step("Tianyi", "list root", || client.directory("-11")) {
        Ok(directory) => directory,
        Err(error) => {
            http.report();
            eprintln!(
                "Tianyi no-redirect probe: {}",
                tianyi_unauthenticated_response_shape(&session.cookie)
                    .unwrap_or_else(|_| "transport-failed".into())
            );
            remove_local(&local_root);
            return Err(error);
        }
    };
    if initial_root.items.iter().any(|item| item.name == base_name) {
        remove_local(&local_root);
        return Err(failure("unexpected Tianyi test-directory name collision"));
    }
    if let Err(error) = provider_step("Tianyi", "mkdir root", || client.mkdir("-11", &base_name)) {
        http.report();
        remove_local(&local_root);
        return Err(error);
    }
    let base = find_named(
        &provider_step("Tianyi", "list after mkdir", || client.directory("-11"))?.items,
        &base_name,
        "folder",
    )?;
    let result = (|| -> LiveResult<()> {
        provider_step("Tianyi", "mkdir move target", || {
            client.mkdir(&base.id, &target_name)
        })?;
        let target = find_named(
            &provider_step("Tianyi", "list move target", || client.directory(&base.id))?.items,
            &target_name,
            "folder",
        )?;
        let uploaded = provider_step("Tianyi", "upload", || {
            client.upload_file(&base.id, &payload, &http)
        })?;
        provider_step("Tianyi", "rename", || {
            client.rename(&uploaded.id, &renamed_name)
        })?;
        let renamed = find_named(
            &provider_step("Tianyi", "list after rename", || client.directory(&base.id))?.items,
            &renamed_name,
            "file",
        )?;
        provider_step("Tianyi", "move", || {
            client.move_files(&[renamed.id.clone()], &target.id)
        })?;
        let moved = find_named(
            &provider_step("Tianyi", "list after move", || client.directory(&target.id))?.items,
            &renamed_name,
            "file",
        )?;
        let download = provider_step("Tianyi", "resolve download", || client.download(&moved.id))?;
        if download.name != renamed_name {
            return Err(failure(
                "Tianyi download metadata did not preserve renamed file",
            ));
        }
        if download_bytes(&download.url, &download.headers)? != expected {
            return Err(failure(
                "Tianyi downloaded bytes differ from uploaded test file",
            ));
        }
        Ok(())
    })();

    // The id was produced by listing the unique directory we just created;
    // Tianyi's delete operation is explicitly requested with `toRecycle`.
    let cleanup = client.trash(&[base.id.clone()]);
    remove_local(&local_root);
    http.report();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            if client
                .directory("-11")?
                .items
                .iter()
                .any(|item| item.id == base.id)
            {
                Err(failure(
                    "Tianyi test directory remained visible after trash",
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

fn wait_for_named(
    mut directory: impl FnMut() -> Result<versadown_lib::cloud::CloudDirectory, QuarkError>,
    name: &str,
    kind: &str,
) -> LiveResult<CloudFile> {
    let mut last_error = None;
    for _ in 0..10 {
        match directory() {
            Ok(listing) => match find_named(&listing.items, name, kind) {
                Ok(item) => return Ok(item),
                Err(error) => last_error = Some(error),
            },
            Err(error) => last_error = Some(Box::new(error)),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(last_error.unwrap_or_else(|| failure("provider did not expose the expected item")))
}

#[test]
#[ignore = "requires an explicitly authorized Tianyi account and creates only VersaDown-E2E-* test data"]
fn live_tianyi_folder_rename_move_and_trash() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let session: TianyiSession = serde_json::from_slice(
        &store
            .load("tianyi")?
            .ok_or_else(|| failure("Tianyi is not currently authorized"))?,
    )?;
    let http = LiveTransport::new()?;
    let client = TianyiClient::new(session, &http);
    let base_name = test_name("tianyi", "mutations-root");
    let source_name = test_name("tianyi", "rename-source");
    let renamed_name = test_name("tianyi", "renamed-folder");
    let destination_name = test_name("tianyi", "move-target");

    if let Err(error) = client.mkdir("-11", &base_name) {
        http.report();
        return Err(Box::new(error));
    }
    let base = find_named(&client.directory("-11")?.items, &base_name, "folder")?;
    let result = (|| -> LiveResult<()> {
        client.mkdir(&base.id, &source_name)?;
        client.mkdir(&base.id, &destination_name)?;
        let children = client.directory(&base.id)?;
        let source = find_named(&children.items, &source_name, "folder")?;
        let destination = find_named(&children.items, &destination_name, "folder")?;
        client.rename(&source.id, &renamed_name)?;
        let renamed = wait_for_named(|| client.directory(&base.id), &renamed_name, "folder")?;
        client.move_files(&[renamed.id.clone()], &destination.id)?;
        wait_for_named(
            || client.directory(&destination.id),
            &renamed_name,
            "folder",
        )?;
        Ok(())
    })();
    let cleanup = client.trash(&[base.id.clone()]);
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            // The batch delete operation is asynchronous.  A successful task
            // id may take a few seconds before the listing hides the folder.
            for _ in 0..10 {
                if !client
                    .directory("-11")?
                    .items
                    .iter()
                    .any(|item| item.id == base.id)
                {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(failure(
                "Tianyi mutation test root remained visible after trash",
            ))
        }
        (Err(test), Ok(())) => Err(test),
        (Ok(()), Err(cleanup)) => Err(Box::new(cleanup)),
        (Err(test), Err(cleanup)) => Err(failure(format!(
            "test failed ({test}); cleanup of only its test directory also failed ({cleanup})"
        ))),
    }
}
