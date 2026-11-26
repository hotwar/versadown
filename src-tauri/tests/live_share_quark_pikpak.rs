//! Opt-in, redacted integration checks for the user-provided public share samples.
//!
//! The test uses the current user's system-credential-store sessions through the
//! production credential store. An opt-in PikPak account read may rotate its existing
//! refresh token before the share operation. The opt-in Aliyun acceptance test may save at
//! most two selected share files when the provider does not issue a direct share URL.
//! It never prints sessions, share URLs, access codes, identifiers, request headers, or
//! direct download URLs.

use versadown_lib::cloud::{
    commands::{
        apply_cloud_download_policy, parse_stable_cloud_source, resolve_download, restore_account,
    },
    share::{
        inspect_share_with_adapter, preflight_selected_share_download, provider_share_adapter,
        resolve_current_share_download, selected_share_download, selected_share_sources,
        CloudShareAdapter, CloudSharePlanStore, ShareConfirmationOutcome,
    },
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudProvider, QuarkError,
    ReqwestTransport, SystemCredentialStore,
};
use versadown_lib::{
    aria2::{Aria2Api, Aria2LaunchConfig, Aria2Process},
    Aria2Settings, DownloadAction, DownloadService, NewTask, TaskStore, UriSubmissionOptions,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

struct RedactedTransport {
    inner: ReqwestTransport,
    observations: Mutex<Vec<String>>,
}

impl RedactedTransport {
    fn new() -> Result<Self, QuarkError> {
        Ok(Self {
            inner: ReqwestTransport::new()?,
            observations: Mutex::new(Vec::new()),
        })
    }

    fn observations_since(&self, start: usize) -> String {
        let observations = self.observations.lock().unwrap();
        observations
            .get(start..)
            .unwrap_or_default()
            .iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(",")
    }

    fn observation_count(&self) -> usize {
        self.observations.lock().unwrap().len()
    }
}

impl CloudHttpTransport for RedactedTransport {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let mut fields = BTreeSet::new();
        if let Ok(url) = url::Url::parse(&request.url) {
            if let Some(host) = url.host_str() {
                fields.insert(format!("request-host:{host}"));
            }
            if url.path().starts_with("/1/clouddrive/") {
                fields.insert(format!("request-path:{}", url.path()));
            }
            if url.host_str() == Some("api.aliyundrive.com")
                && matches!(
                    url.path(),
                    "/v2/share_link/get_share_token"
                        | "/adrive/v3/file/list"
                        | "/v2/drive/get"
                        | "/v2/file/get_download_url"
                        | "/v2/file/copy"
                )
            {
                fields.insert(format!("request-path:{}", url.path()));
            }
        }
        let header_names = request
            .headers
            .keys()
            .map(|name| name.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(".");
        if !header_names.is_empty() {
            fields.insert(format!("request-headers:{header_names}"));
        }
        if let Some(device) = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-device-id"))
            .map(|(_, value)| value)
        {
            fields.insert(format!(
                "device-shape:len{}-hex{}-dot{}",
                device.len(),
                device.bytes().all(|byte| byte.is_ascii_hexdigit()),
                device.contains('.')
            ));
        }
        let response = self.inner.send(request)?;
        let value: serde_json::Value = serde_json::from_str(&response.body).unwrap_or_default();
        fields.insert(format!("response-http:{}", response.status));
        if !response.set_cookies.is_empty() {
            fields.insert("response-set-cookie-present".into());
        }
        if let Some(code) = value
            .get("code")
            .or_else(|| value.get("error_code"))
            .and_then(serde_json::Value::as_i64)
        {
            fields.insert(format!("code:{code}"));
        }
        if let Some(code) = value.get("code").and_then(serde_json::Value::as_str) {
            if code.len() <= 64
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            {
                fields.insert(format!("response-code:{code}"));
            }
        }
        let normalized_body = response.body.to_ascii_lowercase();
        for marker in [
            "captcha_sign",
            "device_id",
            "client_id",
            "timestamp",
            "action",
            "user_id",
        ] {
            if normalized_body.contains(marker) {
                fields.insert(format!("response-marker:{marker}"));
            }
        }
        if let Some(status) = value
            .get("share_status")
            .and_then(serde_json::Value::as_str)
        {
            if status.len() <= 48
                && status
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
            {
                fields.insert(format!("share_status:{status}"));
            }
        }
        if let Some(file) = value.get("file_info") {
            fields.insert(if file.is_object() {
                "file_info:object".into()
            } else {
                "file_info:missing-or-invalid".into()
            });
            if let Some(file) = file.as_object() {
                let safe_keys = file
                    .keys()
                    .filter(|key| {
                        key.len() <= 48
                            && key
                                .bytes()
                                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                    })
                    .take(24)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(".");
                if !safe_keys.is_empty() {
                    fields.insert(format!("file_info:keys:{safe_keys}"));
                }
                fields.insert(
                    if file
                        .get("web_content_link")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|url| !url.is_empty())
                    {
                        "file_info:direct-link-present".into()
                    } else {
                        "file_info:direct-link-missing".into()
                    },
                );
                if let Some(kind) = file.get("kind").and_then(serde_json::Value::as_str) {
                    if kind == "drive#file" || kind == "drive#folder" {
                        fields.insert(format!("file_info:kind:{kind}"));
                    }
                }
            }
        }
        if let Some(download_url) = value
            .pointer("/data/0/download_url")
            .and_then(serde_json::Value::as_str)
        {
            if let Ok(url) = url::Url::parse(download_url) {
                fields.insert(format!("direct-scheme:{}", url.scheme()));
                if let Some(host) = url.host_str() {
                    fields.insert(format!("direct-host:{host}"));
                }
            }
        }
        if let Some(message) = value.get("message").and_then(serde_json::Value::as_str) {
            let label = if message.contains("取消") {
                Some("remote:cancelled")
            } else if message.contains("封禁") || message.contains("受限") {
                Some("remote:blocked")
            } else if message.contains("提取码") || message.contains("访问码") {
                Some("remote:access-code")
            } else {
                None
            };
            if let Some(label) = label {
                fields.insert(label.into());
            }
        }
        self.observations
            .lock()
            .unwrap()
            .push(fields.into_iter().collect::<Vec<_>>().join("+"));
        Ok(response)
    }
}

fn app_session_store() -> Result<SystemCredentialStore, QuarkError> {
    let app_data = std::env::var_os("APPDATA").ok_or(QuarkError::Storage)?;
    SystemCredentialStore::new(
        PathBuf::from(app_data)
            .join("com.versadown.desktop")
            .join("cloud-sessions"),
    )
}

fn supplied_access_code(provider: CloudProvider) -> Option<String> {
    let variable = match provider {
        CloudProvider::Quark => "VERSADOWN_LIVE_QUARK_ACCESS_CODE",
        _ => return None,
    };
    std::env::var(variable)
        .ok()
        .filter(|value| !value.is_empty())
}

fn safe_error(error: &QuarkError) -> &'static str {
    match error {
        QuarkError::NotAuthorized => "not-authorized",
        QuarkError::AccountMismatch => "account-mismatch",
        QuarkError::AuthorizationExpired => "authorization-expired",
        QuarkError::WebAuthorizationRequired => "web-authorization-required",
        QuarkError::CaptchaRequired => "captcha-required",
        QuarkError::PikpakRegionalMembershipRestricted => "regional-membership-restricted",
        QuarkError::PikpakOperationVerificationRequired(_) => "operation-verification-required",
        QuarkError::AuthorizationPending => "authorization-pending",
        QuarkError::SourceFileMissing => "source-file-missing",
        QuarkError::DownloadCheckpointInvalid => "download-checkpoint-invalid",
        QuarkError::DownloadEngineUnavailable => "download-engine-unavailable",
        QuarkError::DownloadSubmissionFailed => "download-submission-failed",
        QuarkError::SessionRenewedRetryOperation => "session-renewed-retry-operation",
        QuarkError::InvalidResponse => "invalid-response",
        QuarkError::Storage => "credential-storage-error",
        QuarkError::Transport(_) => "network-error",
        QuarkError::Provider(_) => "provider-error",
        QuarkError::InvalidInput(_) => "invalid-input",
        QuarkError::UploadUnavailable | QuarkError::ProviderUnsupported => "provider-unsupported",
        QuarkError::XunleiVerificationUnavailable(_) => "xunlei-verification-unavailable",
        QuarkError::BaiduOperationVerificationRequired => "operation-verification-required",
        QuarkError::BaiduUploadAuthorizationRequired(_) => "baidu-upload-authorization-required",
        QuarkError::XunleiNativeDownload(_) => "xunlei-native-download-error",
        QuarkError::BaiduTransferPending => "baidu-transfer-pending",
        QuarkError::ShareTransferDownloadUnavailable => "transfer-saved-download-unavailable",
        QuarkError::Mega(_) => "mega-link-error",
        QuarkError::BaiduOperationVerificationRequired => "baidu-operation-verification-required",
        QuarkError::BaiduDriveVerificationRequired => "baidu-operation-verification-required",
        QuarkError::BaiduUploadPrecreateRejected { .. } => "baidu-upload-precreate-rejected",
        QuarkError::BaiduUploadWebSessionRequired => "baidu-upload-web-session-required",
    }
}

fn direct_range_probe(
    resolved: &versadown_lib::cloud::commands::ResolvedCloudDownload,
) -> Result<u16, QuarkError> {
    let mut request = reqwest::blocking::Client::builder()
        .build()
        .map_err(|_| QuarkError::Transport("direct-probe-client".into()))?
        .get(&resolved.url)
        .header("Range", "bytes=0-0");
    for (name, value) in &resolved.headers {
        request = request.header(name, value);
    }
    request
        .send()
        .map(|response| response.status().as_u16())
        .map_err(|_| QuarkError::Transport("direct-probe-request".into()))
}

struct IsolatedAria2 {
    process: Aria2Process,
    client: versadown_lib::Aria2RpcClient,
    downloads: PathBuf,
}

impl IsolatedAria2 {
    async fn start(root: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let mut aria2 = Aria2Settings::default();
        aria2.rpc_port = listener.local_addr()?.port();
        aria2.rpc_token = format!("live-share-test-{}", aria2.rpc_port);
        drop(listener);
        let downloads = root.join("downloads");
        let config = Aria2LaunchConfig {
            binary: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
            data_dir: root.join("aria2-data"),
            download_dir: downloads.clone(),
            parent_pid: std::process::id(),
            peer_discovery: false,
            aria2,
        };
        let (process, client) = Aria2Process::start(config).await?;
        Ok(Self {
            process,
            client,
            downloads,
        })
    }

    async fn shutdown(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.client.shutdown().await?;
        if !self.process.wait_for_exit(Duration::from_secs(5)).await? {
            self.process.terminate_fallback()?;
        }
        Ok(())
    }
}

async fn wait_for_download(
    client: &versadown_lib::Aria2RpcClient,
    service: &DownloadService,
    task_id: uuid::Uuid,
    over_limit: bool,
) -> Result<(String, u64, u64), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let sample_throughput =
        std::env::var("VERSADOWN_LIVE_THROUGHPUT_SAMPLE").as_deref() == Ok("1");
    let mut throughput_sample = None;
    let mut max_connections = 0_u32;
    let mut max_speed = 0_u64;
    loop {
        let task = service.reconcile_task(task_id).await?;
        let gid = task.aria_gid.as_deref().ok_or("aria2 GID missing")?;
        let status = client.tell_status(gid).await?;
        let completed = status.completed_length.parse::<u64>()?;
        let total = status.total_length.parse::<u64>()?;
        if sample_throughput {
            max_connections = max_connections.max(status.connections.parse()?);
            max_speed = max_speed.max(status.download_speed.parse()?);
        }
        match status.status.as_str() {
            "complete" => return Ok(("complete".into(), completed, total)),
            "error" | "removed" => {
                let code = status.error_code.as_deref().unwrap_or("unknown");
                return Ok((
                    format!("aria2-{}-{}", status.status, code),
                    completed,
                    total,
                ));
            }
            _ if over_limit && (completed > 0 || tokio::time::Instant::now() >= deadline) => {
                if sample_throughput && completed > 0 {
                    let (started_at, starting_bytes) = throughput_sample
                        .get_or_insert_with(|| (tokio::time::Instant::now(), completed));
                    if started_at.elapsed() < Duration::from_secs(2) {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    let paused = service.control(task_id, DownloadAction::Pause).await?;
                    let elapsed_millis = started_at.elapsed().as_millis().max(1);
                    let transferred = paused.completed_length.saturating_sub(*starting_bytes);
                    let sustained_speed = transferred.saturating_mul(1_000) / elapsed_millis as u64;
                    println!(
                        "LIVE_SHARE_THROUGHPUT max_connections={max_connections} max_speed={max_speed} sustained_speed={sustained_speed}"
                    );
                    assert!(
                        sustained_speed > 0,
                        "share throughput sample did not transfer bytes after it started"
                    );
                    return Ok((
                        "paused-over-20MiB".into(),
                        paused.completed_length,
                        paused.total_length.unwrap_or(total),
                    ));
                }
                let paused = service.control(task_id, DownloadAction::Pause).await?;
                return Ok((
                    "paused-over-20MiB".into(),
                    paused.completed_length,
                    paused.total_length.unwrap_or(total),
                ));
            }
            _ if tokio::time::Instant::now() >= deadline => {
                let paused = service.control(task_id, DownloadAction::Pause).await?;
                return Ok((
                    "paused-timeout".into(),
                    paused.completed_length,
                    paused.total_length.unwrap_or(total),
                ));
            }
            _ => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

async fn wait_for_first_download_bytes(
    client: &versadown_lib::Aria2RpcClient,
    service: &DownloadService,
    task_id: uuid::Uuid,
) -> Result<(String, u64, u64), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let task = service.reconcile_task(task_id).await?;
        let gid = task.aria_gid.as_deref().ok_or("aria2 GID missing")?;
        let status = client.tell_status(gid).await?;
        let completed = status.completed_length.parse::<u64>()?;
        let total = status.total_length.parse::<u64>()?;
        match status.status.as_str() {
            "complete" if completed > 0 => return Ok(("complete".into(), completed, total)),
            "error" | "removed" => {
                return Err(format!(
                    "aria2 {} {}: {}",
                    status.status,
                    status.error_code.as_deref().unwrap_or("unknown"),
                    status.error_message.as_deref().unwrap_or("unknown")
                )
                .into())
            }
            _ if completed > 0 => {
                let paused = service.control(task_id, DownloadAction::Pause).await?;
                return Ok((
                    "paused-after-bytes".into(),
                    paused.completed_length,
                    paused.total_length.unwrap_or(total),
                ));
            }
            _ if tokio::time::Instant::now() >= deadline => {
                let _ = service.control(task_id, DownloadAction::Pause).await;
                return Err("Aliyun share download produced no payload before timeout".into());
            }
            _ => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

const MIN_PERSONAL_QUARK_THROUGHPUT_BYTES: i64 = 8 * 1024 * 1024;

fn personal_quark_probe_connections() -> Result<u8, Box<dyn std::error::Error>> {
    match std::env::var("VERSADOWN_LIVE_QUARK_CONNECTIONS") {
        Ok(value) => {
            let connections = value.parse::<u8>()?;
            if !(1..=4).contains(&connections) {
                return Err("Quark throughput probe connections must be between 1 and 4".into());
            }
            Ok(connections)
        }
        Err(std::env::VarError::NotPresent) => Ok(4),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("Quark throughput probe connections is not Unicode".into())
        }
    }
}

fn largest_personal_quark_file_id() -> Result<String, Box<dyn std::error::Error>> {
    let app_data = PathBuf::from(std::env::var_os("APPDATA").ok_or("APPDATA missing")?)
        .join("com.versadown.desktop");
    let database = app_data.join("tasks.sqlite");
    let database_uri = format!(
        "file:{}?immutable=1",
        database.to_string_lossy().replace('\\', "/")
    );
    let connection = Connection::open_with_flags(
        database_uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let source = connection
        .query_row(
            "SELECT source FROM tasks WHERE source LIKE 'cloud://quark/%' AND total_length >= ?1 ORDER BY total_length DESC LIMIT 1",
            params![MIN_PERSONAL_QUARK_THROUGHPUT_BYTES],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or("no recorded Quark task is large enough for a throughput sample")?;
    let (provider, file_id) = parse_stable_cloud_source(&source)?;
    if provider != CloudProvider::Quark {
        return Err("recorded personal Quark task resolved to another provider".into());
    }
    Ok(file_id)
}

#[test]
#[ignore = "requires the current user's authorized Quark session and one recorded personal Quark task of at least 8 MiB"]
fn live_personal_quark_download_samples_configured_connection_throughput(
) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("VERSADOWN_LIVE_QUARK_PERSONAL").as_deref() != Ok("1") {
        return Ok(());
    }
    let configured_connections = personal_quark_probe_connections()?;
    let file_id = largest_personal_quark_file_id()?;
    let credentials = app_session_store()?;
    let transport = RedactedTransport::new()?;
    let resolved = resolve_download(CloudProvider::Quark, &file_id, &credentials, &transport)?;
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let engine = IsolatedAria2::start(temporary.path()).await?;
        let tasks = Arc::new(TaskStore::open_path(
            &temporary.path().join("tasks.sqlite"),
        )?);
        let service = DownloadService::new(Arc::new(engine.client.clone()), Arc::clone(&tasks));
        let output_name = resolved.name.clone();
        let direct_url = resolved.url.clone();
        std::fs::create_dir_all(&engine.downloads)?;
        std::fs::write(engine.downloads.join(&output_name), [0_u8])?;
        let prepared = tasks.create(NewTask {
            source: "cloud://quark/redacted-live-fixture".into(),
            name: output_name,
            kind: "cloud".into(),
            is_private: false,
        })?;
        let mut options = apply_cloud_download_policy(
            CloudProvider::Quark,
            configured_connections,
            resolved.into_uri_submission_options(),
        );
        options.directory = Some(engine.downloads.clone());
        let submitted = service
            .submit_uri_with_options(prepared.id, vec![direct_url], options)
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut max_connections = 0_u32;
        let mut max_speed = 0_u64;
        let mut throughput_sample = None;
        let outcome = loop {
            let task = service.reconcile_task(submitted.id).await?;
            let gid = task.aria_gid.as_deref().ok_or("aria2 GID missing")?;
            let status = engine.client.tell_status(gid).await?;
            let completed = status.completed_length.parse::<u64>()?;
            max_connections = max_connections.max(status.connections.parse()?);
            max_speed = max_speed.max(status.download_speed.parse()?);
            match status.status.as_str() {
                "complete" if completed > 0 => {
                    println!(
                        "LIVE_QUARK_PERSONAL configured_connections={configured_connections} max_connections={max_connections} max_speed={max_speed} sustained_speed=completed-before-sample"
                    );
                    break Ok(())
                }
                _ if completed > 0 => {
                    let (started_at, starting_bytes) = throughput_sample
                        .get_or_insert_with(|| (tokio::time::Instant::now(), completed));
                    if started_at.elapsed() >= Duration::from_secs(2) {
                        let paused = service.control(submitted.id, DownloadAction::Pause).await?;
                        let elapsed_millis = started_at.elapsed().as_millis().max(1);
                        let transferred = paused.completed_length.saturating_sub(*starting_bytes);
                        let sustained_speed = transferred.saturating_mul(1_000) / elapsed_millis as u64;
                        println!(
                            "LIVE_QUARK_PERSONAL configured_connections={configured_connections} max_connections={max_connections} max_speed={max_speed} sustained_speed={sustained_speed}"
                        );
                        assert!(
                            sustained_speed > 0,
                            "Quark throughput sample did not transfer bytes after it started"
                        );
                        break Ok(());
                    }
                }
                "error" | "removed" => {
                    break Err(format!(
                        "aria2 {} {}: {}",
                        status.status,
                        status.error_code.as_deref().unwrap_or("unknown"),
                        status.error_message.as_deref().unwrap_or("unknown")
                    ))
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    break Err("Quark download produced no payload before timeout".into())
                }
                _ => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        };
        engine.shutdown().await?;
        outcome.map_err(Into::into)
    })
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and the current user's authorized Quark and PikPak sessions"]
fn live_quark_and_pikpak_samples_report_redacted_inspection_outcomes() -> Result<(), QuarkError> {
    if std::env::var("VERSADOWN_LIVE_CLOUD").as_deref() != Ok("1") {
        return Ok(());
    }
    let sessions = app_session_store()?;
    let temporary = tempfile::tempdir().map_err(|_| QuarkError::Storage)?;
    let plans = CloudSharePlanStore::new(SystemCredentialStore::new(temporary.path().into())?);
    let transport = RedactedTransport::new()?;
    let samples = [
        (
            CloudProvider::Quark,
            "file",
            "https://pan.quark.cn/s/fa7a8b6f82e9",
        ),
        (
            CloudProvider::Quark,
            "directory",
            "https://pan.quark.cn/s/115465c7a2de",
        ),
        (
            CloudProvider::Quark,
            "cancelled",
            "https://pan.quark.cn/s/2ccaa376fc6c",
        ),
        (
            CloudProvider::Quark,
            "blocked",
            "https://pan.quark.cn/s/0168b134c3fb",
        ),
        (
            CloudProvider::Quark,
            "access-code",
            "https://pan.quark.cn/s/109face2d9a3",
        ),
        (
            CloudProvider::Pikpak,
            "file",
            "https://mypikpak.com/s/VP14Ww6ViZYWUlgPRn6G5N5Uo2",
        ),
        (
            CloudProvider::Pikpak,
            "directory",
            "https://mypikpak.com/s/VOND8tSX4JQ90Fb3S46Pr9Oqo1",
        ),
        (
            CloudProvider::Pikpak,
            "not-found",
            "https://mypikpak.com/s/VNU7HGPigvJhhyVV5",
        ),
        (
            CloudProvider::Pikpak,
            "deleted",
            "https://mypikpak.com/s/VNbk1SmKZaOKJt6xLzTarV6Yo1",
        ),
        (
            CloudProvider::Pikpak,
            "access-code",
            "https://mypikpak.com/s/VP0iOwkZWYRM19CWLpYKb36jo2",
        ),
    ];

    for (provider, expected, url) in samples {
        let access_code = supplied_access_code(provider);
        let candidate =
            versadown_lib::extract_cloud_share_candidate(url, access_code.as_deref())
                .map_err(|_| QuarkError::InvalidInput("sample URL invalid".into()))?
                .ok_or(QuarkError::InvalidResponse)?;
        assert_eq!(candidate.provider, provider);
        let before = transport.observation_count();
        let adapter = provider_share_adapter(provider, &sessions, &transport)?;
        let outcome = inspect_share_with_adapter(&plans, adapter.as_ref(), &candidate);
        let state = match outcome {
            Ok(ShareConfirmationOutcome::AccessCodeRequired) => "access-code-required".into(),
            Ok(ShareConfirmationOutcome::AutoSubmit { .. }) => "auto-submit".into(),
            Ok(ShareConfirmationOutcome::Confirmation(confirmation)) => {
                format!("confirmation:entries={}", confirmation.resources.len())
            }
            Err(error) => format!("error:{}", safe_error(&error)),
        };
        let remote = transport.observations_since(before);
        println!(
            "LIVE_SHARE provider={} sample={} outcome={} remote={}",
            provider.key(),
            expected,
            state,
            if remote.is_empty() { "none" } else { &remote }
        );
    }
    Ok(())
}

#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and submits valid single files plus one selected file from each directory into an isolated aria2 process"]
#[test]
fn live_quark_and_pikpak_valid_shares_submit_and_download_or_pause(
) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("VERSADOWN_LIVE_CLOUD").as_deref() != Ok("1") {
        return Ok(());
    }
    let sessions = app_session_store()?;
    let temporary = tempfile::tempdir()?;
    let plans =
        CloudSharePlanStore::new(SystemCredentialStore::new(temporary.path().join("plans"))?);
    let transport = RedactedTransport::new()?;
    // The production share adapters use reqwest's blocking client, so inspect
    // and resolve before entering the async aria2 runtime.
    let mut prepared_downloads = Vec::new();
    let mut preparation_failures = Vec::new();
    let provider_filter = std::env::var("VERSADOWN_LIVE_PROVIDER").ok();
    let sample_filter = std::env::var("VERSADOWN_LIVE_SAMPLE").ok();
    for (provider, sample, url) in [
        (
            CloudProvider::Quark,
            "file",
            "https://pan.quark.cn/s/fa7a8b6f82e9",
        ),
        (
            CloudProvider::Quark,
            "directory-first-entry",
            "https://pan.quark.cn/s/115465c7a2de",
        ),
        (
            CloudProvider::Pikpak,
            "file",
            "https://mypikpak.com/s/VP14Ww6ViZYWUlgPRn6G5N5Uo2",
        ),
        (
            CloudProvider::Pikpak,
            "directory-first-entry",
            "https://mypikpak.com/s/VOND8tSX4JQ90Fb3S46Pr9Oqo1",
        ),
        (
            CloudProvider::Quark,
            "access-code",
            "https://pan.quark.cn/s/109face2d9a3",
        ),
    ] {
        if provider_filter
            .as_deref()
            .is_some_and(|filter| filter != provider.key())
        {
            continue;
        }
        if sample_filter
            .as_deref()
            .is_some_and(|filter| filter != sample)
        {
            continue;
        }
        let access_code = supplied_access_code(provider);
        if sample == "access-code" && access_code.is_none() {
            continue;
        }
        // Only an explicitly supplied test access code may be used here; the
        // production share command never guesses or recovers a password.
        // A normal account read is nevertheless allowed to rotate an already
        // authorized PikPak refresh token before this separate share exercise.
        // Record only the typed outcome so the test remains secret-free.
        if provider == CloudProvider::Pikpak {
            match restore_account(provider, &sessions, &transport) {
                Ok(_) => println!(
                    "LIVE_SUBMIT provider={} sample={} state=session-ready",
                    provider.key(),
                    sample,
                ),
                Err(error) => println!(
                    "LIVE_SUBMIT provider={} sample={} state=session-refresh-{}",
                    provider.key(),
                    sample,
                    safe_error(&error),
                ),
            }
        }
        let candidate =
            versadown_lib::extract_cloud_share_candidate(url, access_code.as_deref())?
                .ok_or("sample URL was not recognized")?;
        let adapter = provider_share_adapter(provider, &sessions, &transport)?;
        let inspect_before = transport.observation_count();
        let source = match inspect_share_with_adapter(&plans, adapter.as_ref(), &candidate) {
            Ok(ShareConfirmationOutcome::AutoSubmit { source, .. }) => source,
            Ok(ShareConfirmationOutcome::Confirmation(confirmation)) => {
                let first = confirmation
                    .resources
                    .first()
                    .ok_or("directory sample had no selectable files")?;
                selected_share_sources(&plans, &confirmation.plan_source, &[first.id.clone()])?
                    .into_iter()
                    .next()
                    .ok_or("directory selection returned no source")?
            }
            Ok(_) => {
                let failure = "unexpected-inspection-outcome";
                println!(
                    "LIVE_SUBMIT provider={} sample={} state={failure}",
                    provider.key(),
                    sample
                );
                preparation_failures.push(format!("{}:{failure}", provider.key()));
                continue;
            }
            Err(error) => {
                let failure = safe_error(&error);
                let remote = transport.observations_since(inspect_before);
                println!(
                    "LIVE_SUBMIT provider={} sample={} state=inspect-{failure} remote={}",
                    provider.key(),
                    sample,
                    if remote.is_empty() { "none" } else { &remote },
                );
                preparation_failures.push(format!("{}:inspect-{failure}", provider.key()));
                continue;
            }
        };
        // This production resolver re-reads the encrypted plan and obtains a fresh direct
        // URL. The supplied samples are direct-capable; if a provider changes to requiring
        // transfer, the test reports the safe failure rather than moving a user cloud file.
        let resolve_before = transport.observation_count();
        let resolved = match resolve_current_share_download(&plans, adapter.as_ref(), &source) {
            Ok(resolved) => resolved,
            Err(error) => {
                let failure = safe_error(&error);
                let remote = transport.observations_since(resolve_before);
                println!(
                    "LIVE_SUBMIT provider={} sample={} state=resolve-{failure} remote={}",
                    provider.key(),
                    sample,
                    if remote.is_empty() { "none" } else { &remote },
                );
                preparation_failures.push(format!("{}:resolve-{failure}", provider.key()));
                continue;
            }
        };
        let remote = transport.observations_since(resolve_before);
        let direct =
            url::Url::parse(&resolved.url).map_err(|_| "resolved direct URL was malformed")?;
        let header_names = resolved
            .headers
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(".");
        println!(
            "LIVE_RESOLVE provider={} sample={} direct_scheme={} direct_host={} headers={} remote={}",
            provider.key(),
            sample,
            direct.scheme(),
            direct.host_str().unwrap_or("missing"),
            header_names,
            if remote.is_empty() { "none" } else { &remote },
        );
        // Some providers rotate the short-lived direct-download session after
        // any ranged request. The real app hands that URL straight to aria2,
        // so keep a range probe as a separate, explicitly requested
        // diagnostic instead of invalidating this submission test's URL.
        if std::env::var("VERSADOWN_LIVE_DIRECT_PROBE").as_deref() == Ok("1") {
            let probe = direct_range_probe(&resolved)
                .map(|status| format!("http-{status}"))
                .unwrap_or_else(|error| safe_error(&error).into());
            println!(
                "LIVE_DIRECT_PROBE provider={} sample={} range={probe}",
                provider.key(),
                sample,
            );
        }
        let entry = plans.resolve_entry(&source)?.entry;
        prepared_downloads.push((provider, sample, source, entry, resolved));
    }

    let runtime = tokio::runtime::Runtime::new()?;
    if prepared_downloads.is_empty() {
        return Err("no live share sample matched the requested filter".into());
    }
    runtime.block_on(async {
        let engine = IsolatedAria2::start(temporary.path()).await?;
        let tasks = Arc::new(TaskStore::open_path(&temporary.path().join("tasks.sqlite"))?);
        let service = DownloadService::new(Arc::new(engine.client.clone()), Arc::clone(&tasks));

        for (provider, sample, source, entry, resolved) in prepared_downloads {
            let prepared = tasks.create(NewTask {
                source,
                name: entry.name,
                kind: "cloud".into(),
                is_private: false,
            })?;
            let over_limit = entry.size.is_some_and(|size| size > 20 * 1024 * 1024);
            let expected_size = entry.size;
            let submitted = versadown_lib::cloud::share::submit_current_share_download(
                &service,
                &prepared,
                resolved,
                UriSubmissionOptions {
                    directory: Some(engine.downloads.clone()),
                    ..Default::default()
                },
            )
            .await?;
            let (state, completed, total) =
                wait_for_download(&engine.client, &service, submitted.id, over_limit).await?;
            println!(
                "LIVE_SUBMIT provider={} sample={} state={} completed={} total={} expected_size={} size_class={}",
                provider.key(),
                sample,
                state,
                completed,
                total,
                expected_size.map(|size| size.to_string()).as_deref().unwrap_or("unknown"),
                if over_limit { "over-20MiB" } else { "at-most-20MiB-or-unknown" },
            );
            if over_limit {
                assert_eq!(state, "paused-over-20MiB", "large sample must begin downloading before it is paused");
                assert!(completed > 0, "large sample did not receive content before pause");
            } else {
                assert_eq!(state, "complete", "small sample must fully download");
                assert_eq!(completed, total, "completed sample byte count must match total");
            }
            if let Some(expected_size) = expected_size {
                assert_eq!(total, expected_size, "remote total must match share metadata");
            }
        }
        engine.shutdown().await
    })?;
    assert!(
        preparation_failures.is_empty(),
        "live submission preparation failed: {}",
        preparation_failures.join(",")
    );
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1, an authorized Aliyun session, and two reusable current-account transfers from the supplied share"]
fn live_aliyun_share_parses_once_and_two_files_download() -> Result<(), Box<dyn std::error::Error>>
{
    if std::env::var("VERSADOWN_LIVE_CLOUD").as_deref() != Ok("1") {
        return Ok(());
    }
    let url = std::env::var("VERSADOWN_LIVE_ALIYUN_SHARE")
        .ok()
        .filter(|url| !url.is_empty())
        .ok_or("VERSADOWN_LIVE_ALIYUN_SHARE is required")?;
    let temporary = tempfile::tempdir()?;
    let sessions = app_session_store()?;
    let transport = RedactedTransport::new()?;
    restore_account(CloudProvider::Aliyun, &sessions, &transport)?;
    let plans = CloudSharePlanStore::new(app_session_store()?);
    let candidate = versadown_lib::extract_cloud_share_candidate(&url, None)?
        .ok_or("provided URL was not recognized as an Aliyun share")?;
    if candidate.provider != CloudProvider::Aliyun {
        return Err("provided URL was not recognized as an Aliyun share".into());
    }
    let adapter = provider_share_adapter(CloudProvider::Aliyun, &sessions, &transport)?;
    let (resource_count, sources) =
        match inspect_share_with_adapter(&plans, adapter.as_ref(), &candidate)? {
            ShareConfirmationOutcome::AutoSubmit { .. } => {
                return Err("expected the Aliyun sample to contain a directory".into())
            }
            ShareConfirmationOutcome::Confirmation(confirmation) => {
                assert_eq!(
                    confirmation.resources.len(),
                    2_427,
                    "the one-shot Aliyun inspection did not return the complete share"
                );
                let all_ids = confirmation
                    .resources
                    .iter()
                    .map(|resource| resource.id.clone())
                    .collect::<Vec<_>>();
                let full_selection =
                    selected_share_download(&plans, &confirmation.plan_source, &all_ids)?;
                let stored = plans.resolve_entry(&confirmation.plan_source)?.plan;
                let transferred = stored
                    .transfers
                    .iter()
                    .map(|transfer| transfer.entry_fingerprint.as_str())
                    .collect::<BTreeSet<_>>();
                let candidate_ids = confirmation
                    .resources
                    .iter()
                    .filter(|entry| transferred.contains(entry.id.as_str()))
                    .map(|entry| entry.id.clone())
                    .collect::<Vec<_>>();
                let candidate_sources =
                    selected_share_sources(&plans, &confirmation.plan_source, &candidate_ids)?;
                let mut sources = Vec::new();
                for source in candidate_sources {
                    let entry = plans.resolve_entry(&source)?;
                    let owner = adapter
                        .transfer_capability()
                        .ok_or("Aliyun transfer capability is unavailable")?
                        .transfer_owner_fingerprint(&entry)?
                        .ok_or("Aliyun transfer owner is unavailable")?;
                    if plans.reusable_transfer(&source, &owner)?.is_some() {
                        sources.push(source);
                    }
                    if sources.len() == 2 {
                        break;
                    }
                }
                assert_eq!(sources.len(), 2);
                println!(
                    "LIVE_ALIYUN_PARSE state=confirmation entries={} selected={}",
                    confirmation.resources.len(),
                    sources.len()
                );
                let before = transport.observation_count();
                let preflight =
                    preflight_selected_share_download(&plans, adapter.as_ref(), &full_selection);
                let remote = transport.observations_since(before);
                assert!(
                    remote.contains("request-path:/v2/drive/get"),
                    "the Aliyun capacity preflight did not query drive capacity"
                );
                assert!(
                    !remote.contains("request-path:/v2/file/copy"),
                    "the Aliyun capacity preflight attempted to copy a share file"
                );
                match preflight {
                    Err(error) if error.to_string().contains("转存空间不足") => {
                        assert!(error.to_string().contains("未建立下载任务"));
                        println!(
                            "LIVE_ALIYUN_PREFLIGHT state=insufficient no_task=true remote={remote}"
                        );
                    }
                    Ok(()) => {
                        println!(
                            "LIVE_ALIYUN_PREFLIGHT state=sufficient no_task=true remote={remote}"
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
                let reusable_ids = sources
                    .iter()
                    .map(|source| Ok(plans.resolve_entry(source)?.entry.entry_fingerprint))
                    .collect::<Result<Vec<_>, QuarkError>>()?;
                let reusable_selection =
                    selected_share_download(&plans, &confirmation.plan_source, &reusable_ids)?;
                let before = transport.observation_count();
                preflight_selected_share_download(&plans, adapter.as_ref(), &reusable_selection)?;
                let remote = transport.observations_since(before);
                assert!(
                    remote.is_empty(),
                    "reusable Aliyun samples unexpectedly made a remote preflight request: {remote}"
                );
                println!("LIVE_ALIYUN_PREFLIGHT state=reusable no_transfer=true remote=none");
                (confirmation.resources.len(), sources)
            }
            ShareConfirmationOutcome::AccessCodeRequired => {
                return Err("provided share requires an access code".into())
            }
        };
    assert_eq!(resource_count, 2_427);
    let mut prepared_downloads = Vec::new();
    for source in sources {
        let entry = plans.resolve_entry(&source)?.entry;
        let before = transport.observation_count();
        let resolved = match resolve_current_share_download(&plans, adapter.as_ref(), &source) {
            Ok(resolved) => resolved,
            Err(error) => {
                println!(
                    "LIVE_ALIYUN_RESOLVE state={} remote={}",
                    safe_error(&error),
                    transport.observations_since(before)
                );
                return Err(error.into());
            }
        };
        let remote = transport.observations_since(before);
        if remote.contains("request-path:/v2/file/copy") {
            // A recorded personal file may have been manually deleted or
            // moved to the provider recycle bin. In that case the production
            // path deliberately evicts only that stale journal entry and
            // creates one replacement instead of retrying its invalid URL.
            println!("LIVE_ALIYUN_REUSE state=stale-transfer-replaced");
        } else {
            println!("LIVE_ALIYUN_REUSE state=current-transfer-reused");
        }
        prepared_downloads.push((source, entry, resolved));
    }
    let runtime = tokio::runtime::Runtime::new()?;

    runtime.block_on(async {
        let engine = IsolatedAria2::start(temporary.path()).await?;
        let tasks = Arc::new(TaskStore::open_path(
            &temporary.path().join("tasks.sqlite"),
        )?);
        let service = DownloadService::new(Arc::new(engine.client.clone()), Arc::clone(&tasks));
        for (index, (source, entry, resolved)) in prepared_downloads.into_iter().enumerate() {
            let expected_size = entry.size;
            let prepared = tasks.create(NewTask {
                source,
                name: entry.name,
                kind: "cloud".into(),
                is_private: false,
            })?;
            let submitted = versadown_lib::cloud::share::submit_current_share_download(
                &service,
                &prepared,
                resolved,
                UriSubmissionOptions {
                    directory: Some(engine.downloads.clone()),
                    ..Default::default()
                },
            )
            .await?;
            let (state, completed, total) =
                wait_for_first_download_bytes(&engine.client, &service, submitted.id).await?;
            println!(
                "LIVE_ALIYUN_DOWNLOAD sample={} state={} completed={} total={} expected_size={}",
                index + 1,
                state,
                completed,
                total,
                expected_size
                    .map(|size| size.to_string())
                    .as_deref()
                    .unwrap_or("unknown"),
            );
            assert!(
                completed > 0,
                "the transferred sample did not receive content"
            );
            if let Some(expected_size) = expected_size {
                assert_eq!(total, expected_size);
            }
        }
        engine.shutdown().await
    })
}
