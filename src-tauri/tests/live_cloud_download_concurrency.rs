//! Explicit, opt-in, read-only probe for provider direct-download Range support.
//! It uses the configured app proxy when enabled, and otherwise uses the same
//! direct path a normal cloud download uses.
//!
//! The probe never prints cloud item metadata, resolved URLs, request headers,
//! session material, or proxy configuration.

use versadown_lib::{
    cloud::{
        commands::CloudState, CloudError, CloudFile, CloudFileKind, CloudProvider, CredentialStore,
        ProviderRegistry, ReqwestTransport, ResolvedCloudDownload,
    },
    AppSettings, ProxySettings,
};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::PathBuf,
    thread,
    time::Duration,
};

const RANGE_BYTES: u64 = 4 * 1024;
const RANGE_COUNT: u64 = 4;
const MIN_FILE_BYTES: u64 = RANGE_BYTES * RANGE_COUNT;
const MAX_DIRECTORIES: usize = 32;

fn app_data_root() -> Result<PathBuf, &'static str> {
    let app_data = std::env::var_os("APPDATA").ok_or("app-data-unavailable")?;
    Ok(PathBuf::from(app_data).join("com.versadown.desktop"))
}

fn direct_or_configured_proxy_client(
    root: &PathBuf,
) -> Result<(reqwest::blocking::Client, &'static str), &'static str> {
    let proxy_url = match std::env::var("VERSADOWN_LIVE_PROXY") {
        Ok(value) => Some(live_proxy_url(&value)?),
        Err(std::env::VarError::NotPresent) => {
            let bytes = fs::read(root.join("settings.json")).map_err(|_| "settings-unavailable")?;
            let settings: AppSettings =
                serde_json::from_slice(&bytes).map_err(|_| "settings-invalid")?;
            settings
                .proxy
                .enabled
                .then(|| proxy_url(&settings.proxy))
                .transpose()?
        }
        Err(std::env::VarError::NotUnicode(_)) => return Err("proxy-invalid"),
    };
    let transport = if proxy_url.is_some() {
        "configured-proxy"
    } else {
        "direct"
    };
    let mut client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(45));
    if let Some(proxy_url) = proxy_url {
        let proxy = reqwest::Proxy::all(proxy_url.as_str()).map_err(|_| "proxy-invalid")?;
        client = client.proxy(proxy);
    }
    client
        .build()
        .map(|client| (client, transport))
        .map_err(|_| "download-client-unavailable")
}

fn live_proxy_url(value: &str) -> Result<url::Url, &'static str> {
    let value = url::Url::parse(value).map_err(|_| "proxy-invalid")?;
    if !matches!(value.scheme(), "http" | "https")
        || value.host_str().is_none()
        || value.port_or_known_default().is_none()
    {
        return Err("proxy-invalid");
    }
    Ok(value)
}

fn proxy_url(proxy: &ProxySettings) -> Result<url::Url, &'static str> {
    if !proxy.enabled {
        return Err("proxy-disabled");
    }
    if !matches!(proxy.protocol.as_str(), "http" | "https")
        || proxy.host.trim().is_empty()
        || proxy.host.contains("://")
        || proxy.port == 0
    {
        return Err("proxy-invalid");
    }
    let host = proxy.host.trim();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let mut value = url::Url::parse(&format!("{}://{}:{}", proxy.protocol, host, proxy.port))
        .map_err(|_| "proxy-invalid")?;
    if !proxy.username.is_empty() {
        value
            .set_username(&proxy.username)
            .map_err(|_| "proxy-invalid")?;
    }
    if !proxy.password.is_empty() {
        value
            .set_password(Some(&proxy.password))
            .map_err(|_| "proxy-invalid")?;
    }
    Ok(value)
}

fn cloud_error_kind(error: &CloudError) -> &'static str {
    match error {
        CloudError::NotAuthorized => "session-missing",
        CloudError::AccountMismatch => "account-mismatch",
        CloudError::AuthorizationExpired => "session-expired",
        CloudError::SessionRenewedRetryOperation => "session-renewed-retry-needed",
        CloudError::WebAuthorizationRequired => "web-authorization-required",
        CloudError::CaptchaRequired => "verification-required",
        CloudError::PikpakOperationVerificationRequired(_) => "verification-required",
        CloudError::PikpakRegionalMembershipRestricted => "regional-membership-restricted",
        CloudError::AuthorizationPending => "authorization-pending",
        CloudError::SourceFileMissing => "source-file-missing",
        CloudError::DownloadCheckpointInvalid => "download-checkpoint-invalid",
        CloudError::DownloadEngineUnavailable => "download-engine-unavailable",
        CloudError::DownloadSubmissionFailed => "download-submission-failed",
        CloudError::InvalidResponse => "provider-response-invalid",
        CloudError::Storage => "session-store-unavailable",
        CloudError::Transport(_) => "provider-transport-error",
        CloudError::Provider(_) => "provider-error",
        CloudError::InvalidInput(_) => "provider-input-invalid",
        CloudError::UploadUnavailable | CloudError::ProviderUnsupported => "provider-unsupported",
        CloudError::XunleiVerificationUnavailable(_) => "xunlei-verification-unavailable",
        CloudError::BaiduOperationVerificationRequired => "operation-verification-required",
        CloudError::BaiduUploadAuthorizationRequired(_) => "baidu-upload-authorization-required",
        CloudError::XunleiNativeDownload(_) => "xunlei-native-download-error",
        CloudError::BaiduTransferPending => "baidu-transfer-pending",
        CloudError::ShareTransferDownloadUnavailable => "transfer-saved-download-unavailable",
        CloudError::Mega(_) => "mega-link-error",
        CloudError::BaiduOperationVerificationRequired => "baidu-operation-verification-required",
        CloudError::BaiduDriveVerificationRequired => "baidu-operation-verification-required",
        CloudError::BaiduUploadPrecreateRejected { .. } => "baidu-upload-precreate-rejected",
        CloudError::BaiduUploadWebSessionRequired => "baidu-upload-web-session-required",
    }
}

fn find_large_file(
    state: &CloudState,
    http: &ReqwestTransport,
    provider: CloudProvider,
) -> Result<Option<CloudFile>, CloudError> {
    let mut directories = VecDeque::from([ProviderRegistry::global()
        .get(provider)
        .root_directory_id
        .to_owned()]);
    let mut visited = HashSet::new();
    for _ in 0..MAX_DIRECTORIES {
        let Some(parent) = directories.pop_front() else {
            break;
        };
        if !visited.insert(parent.clone()) {
            continue;
        }
        let listing = ProviderRegistry::global()
            .driver(provider, &state.store, http)
            .list_directory(&parent)?;
        for item in listing.items {
            match item.kind {
                CloudFileKind::File if item.size.is_some_and(|size| size >= MIN_FILE_BYTES) => {
                    return Ok(Some(item));
                }
                CloudFileKind::Folder if visited.len() + directories.len() < MAX_DIRECTORIES => {
                    directories.push_back(item.id);
                }
                _ => {}
            }
        }
    }
    Ok(None)
}

fn resolve_download(
    state: &CloudState,
    http: &ReqwestTransport,
    provider: CloudProvider,
    file: &CloudFile,
) -> Result<ResolvedCloudDownload, CloudError> {
    ProviderRegistry::global()
        .driver(provider, &state.store, http)
        .resolve_download(&file.id)
}

enum RangeResult {
    Pass,
    Transport,
    Status(u16),
    ContentRange,
    ByteCount,
    Worker,
}

fn request_range(
    client: &reqwest::blocking::Client,
    download: &ResolvedCloudDownload,
    start: u64,
    end: u64,
    total: u64,
) -> RangeResult {
    let mut request = client
        .get(&download.url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
    for (name, value) in &download.headers {
        if !name.eq_ignore_ascii_case("range") {
            request = request.header(name, value);
        }
    }
    let response = match request.send() {
        Ok(response) => response,
        Err(_) => return RangeResult::Transport,
    };
    let status = response.status().as_u16();
    if status != 206 {
        return RangeResult::Status(status);
    }
    let expected = format!("bytes {start}-{end}/{total}");
    if response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        != Some(expected.as_str())
    {
        return RangeResult::ContentRange;
    }
    match response.bytes() {
        Ok(bytes) if bytes.len() as u64 == end - start + 1 => RangeResult::Pass,
        Ok(_) => RangeResult::ByteCount,
        Err(_) => RangeResult::Transport,
    }
}

fn probe_ranges(
    client: &reqwest::blocking::Client,
    download: &ResolvedCloudDownload,
    total: u64,
) -> Vec<RangeResult> {
    thread::scope(|scope| {
        let handles = (0..RANGE_COUNT)
            .map(|index| {
                let start = index * RANGE_BYTES;
                let end = start + RANGE_BYTES - 1;
                scope.spawn(move || request_range(client, download, start, end, total))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or(RangeResult::Worker))
            .collect()
    })
}

fn print_skip(provider: CloudProvider, reason: &str) {
    eprintln!("{} SKIP {reason}", provider.key());
}

fn print_fail(provider: CloudProvider, reason: &str) {
    eprintln!("{} FAIL {reason}", provider.key());
}

fn selected_providers() -> Result<Vec<CloudProvider>, &'static str> {
    let filter = match std::env::var("VERSADOWN_LIVE_PROVIDER") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => return Err("provider-filter-invalid"),
    };
    let providers = CloudProvider::ALL
        .into_iter()
        .filter(|provider| provider.is_enabled())
        .filter(|provider| filter.as_deref().is_none_or(|key| key == provider.key()))
        .collect::<Vec<_>>();
    if providers.is_empty() {
        return Err("provider-filter-invalid");
    }
    Ok(providers)
}

#[test]
#[ignore = "requires saved cloud sessions and VERSADOWN_LIVE_CLOUD_CONCURRENCY=1"]
fn enabled_providers_support_four_concurrent_disjoint_ranges() {
    let providers = match selected_providers() {
        Ok(providers) => providers,
        Err(reason) => {
            eprintln!("cloud-download-concurrency SKIP {reason}");
            return;
        }
    };
    if std::env::var("VERSADOWN_LIVE_CLOUD_CONCURRENCY").as_deref() != Ok("1") {
        for provider in providers {
            print_skip(provider, "opt-in-disabled");
        }
        return;
    }

    let root = match app_data_root() {
        Ok(root) => root,
        Err(reason) => {
            for provider in providers {
                print_skip(provider, reason);
            }
            return;
        }
    };
    let (client, transport) = match direct_or_configured_proxy_client(&root) {
        Ok(client) => client,
        Err(reason) => {
            for provider in providers {
                print_skip(provider, reason);
            }
            return;
        }
    };
    let state = match CloudState::new(root.join("cloud-sessions")) {
        Ok(state) => state,
        Err(error) => {
            let reason = cloud_error_kind(&error);
            for provider in providers {
                print_skip(provider, reason);
            }
            return;
        }
    };
    let http = match ReqwestTransport::new() {
        Ok(http) => http,
        Err(error) => {
            let reason = cloud_error_kind(&error);
            for provider in providers {
                print_skip(provider, reason);
            }
            return;
        }
    };

    let mut failures = 0usize;
    for provider in providers {
        eprintln!("{} TRANSPORT {transport}", provider.key());
        match state.store.load(provider.key()) {
            Ok(Some(_)) => {}
            Ok(None) => {
                print_skip(provider, "session-missing");
                continue;
            }
            Err(error) => {
                print_skip(provider, cloud_error_kind(&error));
                continue;
            }
        }
        if let Err(error) = ProviderRegistry::global()
            .driver(provider, &state.store, &http)
            .account()
        {
            print_skip(provider, cloud_error_kind(&error));
            continue;
        }
        let file = match find_large_file(&state, &http, provider) {
            Ok(Some(file)) => file,
            Ok(None) => {
                print_skip(provider, "no-suitable-file");
                continue;
            }
            Err(error) => {
                print_skip(provider, cloud_error_kind(&error));
                continue;
            }
        };
        let total = file.size.expect("large-file selection requires a size");
        let download = match resolve_download(&state, &http, provider, &file) {
            Ok(download) => download,
            Err(error) => {
                print_skip(provider, cloud_error_kind(&error));
                continue;
            }
        };
        let results = probe_ranges(&client, &download, total);
        if results
            .iter()
            .all(|result| matches!(result, RangeResult::Pass))
        {
            eprintln!(
                "{} PASS HTTP 206 ranges 0-4095,4096-8191,8192-12287,12288-16383 validated",
                provider.key()
            );
            continue;
        }
        failures += 1;
        let (index, result) = results
            .iter()
            .enumerate()
            .find(|(_, result)| !matches!(result, RangeResult::Pass))
            .expect("a failed result must be present");
        match result {
            RangeResult::Transport => {
                print_fail(provider, &format!("range-{index} transport-error"))
            }
            RangeResult::Status(status) => {
                print_fail(provider, &format!("range-{index} HTTP-{status}"))
            }
            RangeResult::ContentRange => print_fail(
                provider,
                &format!("range-{index} HTTP-206 content-range-mismatch"),
            ),
            RangeResult::ByteCount => print_fail(
                provider,
                &format!("range-{index} HTTP-206 byte-count-mismatch"),
            ),
            RangeResult::Worker => print_fail(provider, &format!("range-{index} worker-error")),
            RangeResult::Pass => unreachable!(),
        }
    }
    assert_eq!(failures, 0, "one or more providers failed Range validation");
}
