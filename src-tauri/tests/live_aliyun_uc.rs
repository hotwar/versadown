//! Explicit, opt-in end-to-end regression for the production Aliyun and UC
//! provider drivers.
//!
//! Every mutation is contained below one freshly generated
//! `VersaDown-E2E-*` directory. Existing user items are never selected or
//! mutated. Credential bytes, cookies and signed download URLs are never
//! printed.

use versadown_lib::{
    aria2::{Aria2LaunchConfig, Aria2Process},
    cloud::{
        commands::{parse_stable_cloud_source, CloudState},
        uc::{UcClient, UcSession},
        CloudDirectory, CloudError, CloudFile, CloudFileKind, CloudHttpRequest, CloudHttpResponse,
        CloudHttpTransport, CloudProvider, CloudShareOptions, CloudUploadTransport,
        CredentialStore, ProviderRegistry, ReqwestTransport, ResolvedCloudDownload,
        SystemCredentialStore, UploadHttpRequest, UploadHttpResponse,
    },
    Aria2Settings, DownloadService, NewTask, TaskStatus, TaskStore,
};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::BTreeSet,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type LiveResult<T> = Result<T, Box<dyn Error>>;

struct LiveTransport {
    inner: ReqwestTransport,
    observations: Mutex<Vec<String>>,
}

impl LiveTransport {
    fn new() -> Result<Self, CloudError> {
        Ok(Self {
            inner: ReqwestTransport::new()?,
            observations: Mutex::new(Vec::new()),
        })
    }

    fn report(&self) {
        for observation in self.observations.lock().unwrap().iter() {
            eprintln!("LIVE-E2E request: {observation}");
        }
    }
}

impl CloudHttpTransport for LiveTransport {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
        let method = request.method;
        let endpoint = url::Url::parse(&request.url)
            .ok()
            .map(|url| format!("{}{}", url.host_str().unwrap_or("invalid-host"), url.path()))
            .unwrap_or_else(|| "invalid-endpoint".into());
        match self.inner.send(request) {
            Ok(response) => {
                let value = serde_json::from_str::<serde_json::Value>(&response.body).ok();
                let keys = value
                    .as_ref()
                    .and_then(serde_json::Value::as_object)
                    .map(|object| {
                        let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
                        keys.sort_unstable();
                        keys.join(",")
                    })
                    .unwrap_or_else(|| "non-object".into());
                let code = value
                    .as_ref()
                    .and_then(|value| value.get("code"))
                    .map(|value| match value {
                        serde_json::Value::Number(value) => value.to_string(),
                        serde_json::Value::String(value)
                            if value.len() <= 64
                                && value.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric() || b"._-".contains(&byte)
                                }) =>
                        {
                            value.clone()
                        }
                        _ => "redacted".into(),
                    })
                    .unwrap_or_else(|| "absent".into());
                let status = value
                    .as_ref()
                    .and_then(|value| value.get("status"))
                    .and_then(serde_json::Value::as_i64)
                    .map_or_else(|| "absent".into(), |value| value.to_string());
                let message = value
                    .as_ref()
                    .and_then(|value| value.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|_| code != "0" || status != "200")
                    .filter(|value| {
                        value.len() <= 256
                            && !value.contains(['\r', '\n'])
                            && !value.chars().any(char::is_control)
                    })
                    .unwrap_or("redacted");
                let data_shape = value
                    .as_ref()
                    .and_then(|value| value.get("data"))
                    .map(|value| match value {
                        serde_json::Value::Null => "null",
                        serde_json::Value::Bool(_) => "bool",
                        serde_json::Value::Number(_) => "number",
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::Object(_) => "object",
                    })
                    .unwrap_or("absent");
                let data_keys = value
                    .as_ref()
                    .and_then(|value| value.get("data"))
                    .and_then(serde_json::Value::as_object)
                    .map(|object| {
                        let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
                        keys.sort_unstable();
                        keys.join(",")
                    })
                    .unwrap_or_else(|| "none".into());
                let upload_contract = if endpoint.ends_with("/file/upload/pre") {
                    value
                        .as_ref()
                        .and_then(|value| value.get("data"))
                        .and_then(serde_json::Value::as_object)
                        .map(|data| {
                            let string_field = |name: &str| {
                                data.get(name)
                                    .and_then(serde_json::Value::as_str)
                                    .map(|value| {
                                        if name == "upload_url" {
                                            url::Url::parse(value)
                                                .ok()
                                                .map(|url| {
                                                    format!(
                                                        "{}://{}{}",
                                                        url.scheme(),
                                                        url.host_str().unwrap_or("invalid-host"),
                                                        url.path()
                                                    )
                                                })
                                                .unwrap_or_else(|| "invalid-url".into())
                                        } else {
                                            format!("string(len={})", value.len())
                                        }
                                    })
                                    .unwrap_or_else(|| {
                                        data.get(name)
                                            .map(|value| format!("{}", value_type(value)))
                                            .unwrap_or_else(|| "absent".into())
                                    })
                            };
                            format!(
                                "; upload-contract bucket={:?}, upload_url={}, obj_key={}, auth_info={}, upload_id={}, callback={}",
                                data.get("bucket").and_then(serde_json::Value::as_str),
                                string_field("upload_url"),
                                string_field("obj_key"),
                                string_field("auth_info"),
                                string_field("upload_id"),
                                data.get("callback")
                                    .map(value_type)
                                    .unwrap_or("absent")
                            )
                        })
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let share_list_contract = if endpoint.ends_with("/share/mypage/detail") {
                    let entries = value
                        .as_ref()
                        .and_then(|value| value.pointer("/data/list"))
                        .and_then(serde_json::Value::as_array);
                    let count = entries.map_or(0, Vec::len);
                    let fields = entries
                        .into_iter()
                        .flatten()
                        .filter_map(serde_json::Value::as_object)
                        .flat_map(|entry| entry.keys().map(String::as_str))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("; share-list count={count}; item-fields={fields}")
                } else {
                    String::new()
                };
                let download_url_contract = if endpoint.ends_with("/file/get_download_url") {
                    let describe_url = |field: &str| {
                        value
                            .as_ref()
                            .and_then(|value| value.get(field))
                            .and_then(serde_json::Value::as_str)
                            .map(|url| {
                                if url.is_empty() {
                                    "empty".into()
                                } else {
                                    url::Url::parse(url)
                                        .ok()
                                        .map(|url| format!("scheme={}", url.scheme()))
                                        .unwrap_or_else(|| "invalid".into())
                                }
                            })
                            .unwrap_or_else(|| "absent-or-not-string".into())
                    };
                    format!(
                        "; download-url url={}, cdn_url={}",
                        describe_url("url"),
                        describe_url("cdn_url")
                    )
                } else {
                    String::new()
                };
                let metadata_download_url_contract = if endpoint.ends_with("/v2/file/get") {
                    match value
                        .as_ref()
                        .and_then(|value| value.get("download_url"))
                        .and_then(serde_json::Value::as_str)
                    {
                        Some(url) if url.is_empty() => "; metadata-download-url empty".into(),
                        Some(url) => url::Url::parse(url)
                            .ok()
                            .map(|url| format!("; metadata-download-url scheme={}", url.scheme()))
                            .unwrap_or_else(|| "; metadata-download-url invalid".into()),
                        None => "; metadata-download-url absent-or-not-string".into(),
                    }
                } else {
                    String::new()
                };
                let mut set_cookie_names = response
                    .set_cookies
                    .iter()
                    .filter_map(|cookie| cookie.split_once('=').map(|(name, _)| name.trim()))
                    .filter(|name| !name.is_empty())
                    .collect::<Vec<_>>();
                set_cookie_names.sort_unstable();
                set_cookie_names.dedup();
                let set_cookie_contract = if set_cookie_names.is_empty() {
                    String::new()
                } else {
                    format!("; set-cookie-names={}", set_cookie_names.join(","))
                };
                self.observations.lock().unwrap().push(format!(
                    "{method} {endpoint}; HTTP {}; code={code}; status={status}; message={message:?}; data={data_shape}({data_keys}); keys={keys}{upload_contract}{share_list_contract}{download_url_contract}{metadata_download_url_contract}{set_cookie_contract}",
                    response.status,
                ));
                Ok(response)
            }
            Err(error) => {
                self.observations.lock().unwrap().push(format!(
                    "{method} {endpoint}; transport={}",
                    error_kind(&error)
                ));
                Err(error)
            }
        }
    }
}

fn value_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

impl CloudUploadTransport for LiveTransport {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, CloudError> {
        self.inner.send_upload(request)
    }
}

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

fn error_kind(error: &CloudError) -> &'static str {
    match error {
        CloudError::NotAuthorized => "not-authorized",
        CloudError::AccountMismatch => "account-mismatch",
        CloudError::AuthorizationExpired => "authorization-expired",
        CloudError::SessionRenewedRetryOperation => "session-renewed-retry-operation",
        CloudError::WebAuthorizationRequired => "web-authorization-required",
        CloudError::CaptchaRequired => "captcha-required",
        CloudError::PikpakRegionalMembershipRestricted => "regional-membership-restricted",
        CloudError::PikpakOperationVerificationRequired(_) => "operation-verification-required",
        CloudError::AuthorizationPending => "authorization-pending",
        CloudError::SourceFileMissing => "source-file-missing",
        CloudError::DownloadCheckpointInvalid => "download-checkpoint-invalid",
        CloudError::DownloadEngineUnavailable => "download-engine-unavailable",
        CloudError::DownloadSubmissionFailed => "download-submission-failed",
        CloudError::InvalidResponse => "invalid-response",
        CloudError::Storage => "storage",
        CloudError::Transport(_) => "transport",
        CloudError::Provider(message) if message.contains("UC 网盘账号未实名") => {
            "uc-real-name-verification-required"
        }
        CloudError::Provider(_) => "provider",
        CloudError::InvalidInput(_) => "invalid-input",
        CloudError::UploadUnavailable => "upload-unavailable",
        CloudError::ProviderUnsupported => "provider-unsupported",
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

fn require_live_authorization() -> LiveResult<()> {
    if std::env::var("VERSADOWN_LIVE_CLOUD").as_deref() != Ok("1") {
        return Err(failure(
            "live cloud regression requires VERSADOWN_LIVE_CLOUD=1",
        ));
    }
    Ok(())
}

fn session_root() -> LiveResult<PathBuf> {
    let app_data = std::env::var_os("APPDATA")
        .ok_or_else(|| failure("APPDATA is required for the encrypted session store"))?;
    Ok(PathBuf::from(app_data)
        .join("com.versadown.desktop")
        .join("cloud-sessions"))
}

fn test_root_name(provider: CloudProvider) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(
        "VersaDown-E2E-{}-{millis}-{}",
        provider.key(),
        uuid::Uuid::new_v4().simple()
    )
}

fn step<T>(
    provider: CloudProvider,
    operation: &str,
    action: impl FnOnce() -> Result<T, CloudError>,
) -> LiveResult<T> {
    match action() {
        Ok(value) => {
            eprintln!("LIVE-E2E {} {operation}: PASS", provider.key());
            Ok(value)
        }
        Err(error) => {
            eprintln!(
                "LIVE-E2E {} {operation}: FAIL ({})",
                provider.key(),
                error_kind(&error)
            );
            Err(failure(format!(
                "{} {operation} failed: {}",
                provider.key(),
                error_kind(&error)
            )))
        }
    }
}

fn driver_list(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    parent: &str,
) -> Result<CloudDirectory, CloudError> {
    ProviderRegistry::global()
        .driver(provider, &state.store, http)
        .list_directory(parent)
}

fn create_directory(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    parent: &str,
    name: &str,
) -> Result<CloudFile, CloudError> {
    ProviderRegistry::global()
        .driver(provider, &state.store, http)
        .create_directory(parent, name)
}

fn find_named(items: &[CloudFile], name: &str, kind: CloudFileKind) -> LiveResult<CloudFile> {
    items
        .iter()
        .find(|item| item.name == name && item.kind == kind)
        .cloned()
        .ok_or_else(|| {
            failure(format!(
                "expected isolated {kind:?} {name:?} was not listed"
            ))
        })
}

fn wait_for_named(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    parent: &str,
    name: &str,
    kind: CloudFileKind,
) -> LiveResult<CloudFile> {
    let mut last = None;
    for _ in 0..20 {
        match driver_list(state, http, provider, parent) {
            Ok(directory) => match find_named(&directory.items, name, kind) {
                Ok(file) => return Ok(file),
                Err(error) => last = Some(error),
            },
            Err(error) => {
                last = Some(failure(format!(
                    "{} listing failed while waiting: {}",
                    provider.key(),
                    error_kind(&error)
                )))
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(last.unwrap_or_else(|| failure("provider did not expose the expected item")))
}

fn wait_until_absent(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    parent: &str,
    id: &str,
) -> LiveResult<()> {
    for _ in 0..20 {
        let directory = driver_list(state, http, provider, parent).map_err(|error| {
            failure(format!(
                "{} listing failed while waiting for deletion: {}",
                provider.key(),
                error_kind(&error)
            ))
        })?;
        if !directory.items.iter().any(|item| item.id == id) {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(failure("isolated item remained visible after deletion"))
}

fn cleanup_abandoned_uc_e2e_shares(state: &CloudState, http: &LiveTransport) -> LiveResult<()> {
    let provider = CloudProvider::Uc;
    let shares = ProviderRegistry::global()
        .driver(provider, &state.store, http)
        .list_shares()
        .map_err(|error| {
            failure(format!(
                "UC test-share cleanup listing failed: {}",
                error_kind(&error)
            ))
        })?;
    let ids = shares
        .into_iter()
        .filter(|share| share.name.starts_with("VersaDown-E2E-uc-"))
        .map(|share| share.id)
        .collect::<Vec<_>>();
    for (index, share_id) in ids.into_iter().enumerate() {
        ProviderRegistry::global()
            .driver(provider, &state.store, http)
            .cancel_shares(std::slice::from_ref(&share_id))
            .map_err(|error| {
                failure(format!(
                    "UC abandoned test-share cleanup item #{index} failed: {error}",
                ))
            })?;
    }
    Ok(())
}

fn upload_file(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    parent: &str,
    path: &Path,
) -> Result<CloudFile, CloudError> {
    state.upload_file(provider, http, parent, path)
}

fn resolve_download(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    file: &str,
) -> Result<ResolvedCloudDownload, CloudError> {
    ProviderRegistry::global()
        .driver(provider, &state.store, http)
        .resolve_download(file)
}

fn download_bytes(download: ResolvedCloudDownload) -> LiveResult<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut request = client.get(&download.url);
    for (name, value) in download.headers {
        request = request.header(name, value);
    }
    let response = request.send()?;
    if !response.status().is_success() {
        return Err(failure(format!(
            "direct download returned HTTP {}",
            response.status()
        )));
    }
    Ok(response.bytes()?.to_vec())
}

fn rename(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    id: &str,
    name: &str,
) -> Result<(), CloudError> {
    ProviderRegistry::global()
        .driver(provider, &state.store, http)
        .rename(id, name)
}

fn move_item(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    id: &str,
    parent: &str,
) -> Result<(), CloudError> {
    ProviderRegistry::global()
        .driver(provider, &state.store, http)
        .move_files(&[id.to_owned()], parent)
}

fn trash(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    id: &str,
) -> Result<(), CloudError> {
    ProviderRegistry::global()
        .driver(provider, &state.store, http)
        .trash(&[id.to_owned()])
}

fn purge_named_from_recycle(
    state: &CloudState,
    http: &LiveTransport,
    provider: CloudProvider,
    names: &[&str],
) -> LiveResult<()> {
    let recycle_id = ProviderRegistry::global()
        .get(provider)
        .recycle_bin_directory_id;
    let required_root_name = names
        .first()
        .ok_or_else(|| failure("cleanup requires the isolated root name"))?;
    let mut found_root = false;
    for attempt in 0..20 {
        let recycle = driver_list(state, http, provider, recycle_id).map_err(|error| {
            failure(format!(
                "{} recycle listing failed during cleanup: {}",
                provider.key(),
                error_kind(&error)
            ))
        })?;
        if recycle
            .items
            .iter()
            .any(|item| item.name == *required_root_name)
        {
            found_root = true;
            let ids = recycle
                .items
                .into_iter()
                .filter(|item| names.contains(&item.name.as_str()))
                .map(|item| item.id)
                .collect::<Vec<_>>();
            ProviderRegistry::global()
                .driver(provider, &state.store, http)
                .delete(&ids)
                .map_err(|error| {
                    failure(format!(
                        "{} permanent cleanup failed: {}",
                        provider.key(),
                        error_kind(&error)
                    ))
                })?;
            break;
        }
        if attempt < 19 {
            thread::sleep(Duration::from_millis(250));
        }
    }
    if !found_root {
        return Err(failure("isolated root did not appear in the recycle bin"));
    }
    for attempt in 0..20 {
        let recycle = driver_list(state, http, provider, recycle_id)?;
        if !recycle
            .items
            .iter()
            .any(|item| names.contains(&item.name.as_str()))
        {
            return Ok(());
        }
        if attempt < 19 {
            thread::sleep(Duration::from_millis(250));
        }
    }
    Err(failure(
        "isolated entries remained in recycle after cleanup",
    ))
}

fn write_local_fixture(
    provider: CloudProvider,
) -> LiveResult<(tempfile::TempDir, PathBuf, Vec<u8>, PathBuf, Vec<u8>)> {
    let root = tempfile::Builder::new()
        .prefix(&format!("VersaDown-E2E-{}-", provider.key()))
        .tempdir()?;
    let single_bytes = format!(
        "VersaDown {} single-file live fixture\n",
        provider.key()
    )
    .into_bytes();
    let single = root.path().join("single.txt");
    fs::write(&single, &single_bytes)?;
    let nested_dir = root.path().join("upload-directory").join("child");
    fs::create_dir_all(&nested_dir)?;
    let nested_bytes =
        format!("VersaDown {} nested live fixture\n", provider.key()).into_bytes();
    let nested = nested_dir.join("nested.txt");
    fs::write(&nested, &nested_bytes)?;
    Ok((root, single, single_bytes, nested, nested_bytes))
}

fn run_provider(provider: CloudProvider) -> LiveResult<()> {
    require_live_authorization()?;
    let root_id = ProviderRegistry::global().get(provider).root_directory_id;
    let credentials_root = session_root()?;
    let direct_store = SystemCredentialStore::new(credentials_root.clone())?;
    direct_store.load(provider.key()).map_err(|error| {
        failure(format!(
            "{} credential decrypt failed: {}",
            provider.key(),
            error_kind(&error)
        ))
    })?;
    eprintln!("LIVE-E2E {} credential decrypt: PASS", provider.key());
    let state = CloudState::new(credentials_root).map_err(|error| {
        failure(format!(
            "{} CloudState initialization failed: {}",
            provider.key(),
            error_kind(&error)
        ))
    })?;
    if state.store.load(provider.key())?.is_none() {
        return Err(failure(format!(
            "{} is not currently authorized",
            provider.key()
        )));
    }
    let http = LiveTransport::new()?;
    let (_local, single_path, single_bytes, nested_path, nested_bytes) =
        write_local_fixture(provider)?;
    let test_name = test_root_name(provider);
    let delete_file_name = "delete-me.txt";
    let delete_directory_name = "delete-directory";

    step(provider, "account", || {
        ProviderRegistry::global()
            .driver(provider, &state.store, &http)
            .account()
    })?;
    let initial = step(provider, "list root", || {
        driver_list(&state, &http, provider, root_id)
    })?;
    if initial.items.iter().any(|item| item.name == test_name) {
        return Err(failure("unexpected isolated test-root name collision"));
    }

    let test_root = match step(provider, "create directory", || {
        create_directory(&state, &http, provider, root_id, &test_name)
    }) {
        Ok(root) => root,
        Err(error) => {
            http.report();
            if let Ok(created) = wait_for_named(
                &state,
                &http,
                provider,
                root_id,
                &test_name,
                CloudFileKind::Folder,
            ) {
                let _ = trash(&state, &http, provider, &created.id);
                let _ = purge_named_from_recycle(&state, &http, provider, &[&test_name]);
            }
            return Err(error);
        }
    };
    let result = (|| -> LiveResult<()> {
        let move_target = step(provider, "create move target", || {
            create_directory(&state, &http, provider, &test_root.id, "move-target")
        })?;

        let uploaded = step(provider, "upload single file", || {
            upload_file(&state, &http, provider, &test_root.id, &single_path)
        })?;
        let renamed_file_name = "single-renamed.txt";
        step(provider, "rename file", || {
            rename(&state, &http, provider, &uploaded.id, renamed_file_name)
        })?;
        let renamed_file = wait_for_named(
            &state,
            &http,
            provider,
            &test_root.id,
            renamed_file_name,
            CloudFileKind::File,
        )?;
        step(provider, "move file", || {
            move_item(&state, &http, provider, &renamed_file.id, &move_target.id)
        })?;
        let moved_file = wait_for_named(
            &state,
            &http,
            provider,
            &move_target.id,
            renamed_file_name,
            CloudFileKind::File,
        )?;
        let single_download = step(provider, "resolve single-file download", || {
            resolve_download(&state, &http, provider, &moved_file.id)
        })?;
        if download_bytes(single_download)? != single_bytes {
            return Err(failure("single-file download bytes differ from upload"));
        }
        eprintln!("LIVE-E2E {} download single file: PASS", provider.key());

        let upload_directory = step(provider, "create upload directory", || {
            create_directory(&state, &http, provider, &test_root.id, "uploaded-directory")
        })?;
        let upload_child = step(provider, "create upload subdirectory", || {
            create_directory(&state, &http, provider, &upload_directory.id, "child")
        })?;
        let nested_file = step(provider, "upload nested directory file", || {
            upload_file(&state, &http, provider, &upload_child.id, &nested_path)
        })?;
        eprintln!("LIVE-E2E {} upload directory: PASS", provider.key());
        let nested_listing = step(provider, "list downloaded directory recursively", || {
            driver_list(&state, &http, provider, &upload_child.id)
        })?;
        let nested_file = nested_listing
            .items
            .into_iter()
            .find(|item| item.id == nested_file.id)
            .ok_or_else(|| failure("nested uploaded file was not listed"))?;
        let nested_download = step(provider, "resolve nested directory download", || {
            resolve_download(&state, &http, provider, &nested_file.id)
        })?;
        if download_bytes(nested_download)? != nested_bytes {
            return Err(failure("directory download bytes differ from upload"));
        }
        eprintln!("LIVE-E2E {} download directory: PASS", provider.key());

        let rename_folder = step(provider, "create folder to rename", || {
            create_directory(
                &state,
                &http,
                provider,
                &test_root.id,
                "folder-before-rename",
            )
        })?;
        step(provider, "rename folder", || {
            rename(
                &state,
                &http,
                provider,
                &rename_folder.id,
                "folder-after-rename",
            )
        })?;
        let renamed_folder = wait_for_named(
            &state,
            &http,
            provider,
            &test_root.id,
            "folder-after-rename",
            CloudFileKind::Folder,
        )?;
        step(provider, "move directory", || {
            move_item(&state, &http, provider, &renamed_folder.id, &move_target.id)
        })?;
        wait_for_named(
            &state,
            &http,
            provider,
            &move_target.id,
            "folder-after-rename",
            CloudFileKind::Folder,
        )?;

        let delete_file = step(provider, "upload file for deletion", || {
            upload_file(&state, &http, provider, &test_root.id, &single_path)
        })?;
        step(provider, "rename deletion fixture", || {
            rename(&state, &http, provider, &delete_file.id, delete_file_name)
        })?;
        let delete_file = wait_for_named(
            &state,
            &http,
            provider,
            &test_root.id,
            delete_file_name,
            CloudFileKind::File,
        )?;
        step(provider, "delete file", || {
            trash(&state, &http, provider, &delete_file.id)
        })?;
        wait_until_absent(&state, &http, provider, &test_root.id, &delete_file.id)?;

        let delete_directory = step(provider, "create recursive-delete directory", || {
            create_directory(
                &state,
                &http,
                provider,
                &test_root.id,
                delete_directory_name,
            )
        })?;
        let delete_child = step(provider, "create recursive-delete child", || {
            create_directory(&state, &http, provider, &delete_directory.id, "child")
        })?;
        step(provider, "upload recursive-delete descendant", || {
            upload_file(&state, &http, provider, &delete_child.id, &nested_path)
        })?;
        step(provider, "delete directory recursively", || {
            trash(&state, &http, provider, &delete_directory.id)
        })?;
        wait_until_absent(&state, &http, provider, &test_root.id, &delete_directory.id)?;
        Ok(())
    })();

    let cleanup = trash(&state, &http, provider, &test_root.id)
        .map_err(|error| {
            failure(format!(
                "isolated root cleanup failed: {}",
                error_kind(&error)
            ))
        })
        .and_then(|()| wait_until_absent(&state, &http, provider, root_id, &test_root.id))
        .and_then(|()| {
            purge_named_from_recycle(
                &state,
                &http,
                provider,
                &[&test_name, delete_file_name, delete_directory_name],
            )
        });

    http.report();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            eprintln!("LIVE-E2E {} cleanup: PASS", provider.key());
            Ok(())
        }
        (Err(test), Ok(())) => Err(test),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(test), Err(cleanup)) => Err(failure(format!(
            "test failed ({test}); cleanup also failed ({cleanup})"
        ))),
    }
}

fn run_uc_folder_matrix() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let root_id = ProviderRegistry::global().get(provider).root_directory_id;
    let state = CloudState::new(session_root()?)?;
    let http = LiveTransport::new()?;
    let test_name = test_root_name(provider);
    let test_root = step(provider, "folder-matrix create root", || {
        create_directory(&state, &http, provider, root_id, &test_name)
    })?;
    let result = (|| -> LiveResult<()> {
        let target = step(provider, "folder-matrix create move target", || {
            create_directory(&state, &http, provider, &test_root.id, "move-target")
        })?;
        let source = step(provider, "folder-matrix create rename source", || {
            create_directory(
                &state,
                &http,
                provider,
                &test_root.id,
                "folder-before-rename",
            )
        })?;
        step(provider, "folder-matrix create nested child", || {
            create_directory(&state, &http, provider, &source.id, "nested-child")
        })?;
        step(provider, "folder-matrix rename folder", || {
            rename(&state, &http, provider, &source.id, "folder-after-rename")
        })?;
        let renamed = wait_for_named(
            &state,
            &http,
            provider,
            &test_root.id,
            "folder-after-rename",
            CloudFileKind::Folder,
        )?;
        step(provider, "folder-matrix move directory", || {
            move_item(&state, &http, provider, &renamed.id, &target.id)
        })?;
        let moved = wait_for_named(
            &state,
            &http,
            provider,
            &target.id,
            "folder-after-rename",
            CloudFileKind::Folder,
        )?;
        let nested = driver_list(&state, &http, provider, &moved.id)?;
        find_named(&nested.items, "nested-child", CloudFileKind::Folder)?;
        eprintln!("LIVE-E2E uc move directory with child: PASS");

        let delete_directory = step(
            provider,
            "folder-matrix create recursive-delete root",
            || create_directory(&state, &http, provider, &test_root.id, "delete-directory"),
        )?;
        step(
            provider,
            "folder-matrix create recursive-delete child",
            || create_directory(&state, &http, provider, &delete_directory.id, "child"),
        )?;
        step(
            provider,
            "folder-matrix delete directory recursively",
            || trash(&state, &http, provider, &delete_directory.id),
        )?;
        wait_until_absent(&state, &http, provider, &test_root.id, &delete_directory.id)?;
        Ok(())
    })();
    let cleanup = trash(&state, &http, provider, &test_root.id)
        .map_err(|error| {
            failure(format!(
                "UC folder root cleanup failed: {}",
                error_kind(&error)
            ))
        })
        .and_then(|()| wait_until_absent(&state, &http, provider, root_id, &test_root.id))
        .and_then(|()| {
            purge_named_from_recycle(&state, &http, provider, &[&test_name, "delete-directory"])
        });
    http.report();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(test), Ok(())) => Err(test),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(test), Err(cleanup)) => Err(failure(format!(
            "folder test failed ({test}); cleanup also failed ({cleanup})"
        ))),
    }
}

fn run_uc_requested_upload() -> LiveResult<()> {
    require_live_authorization()?;
    let path = std::env::var_os("VERSADOWN_LIVE_UC_UPLOAD_PATH")
        .map(PathBuf::from)
        .ok_or_else(|| failure("VERSADOWN_LIVE_UC_UPLOAD_PATH is required"))?;
    let metadata = fs::metadata(&path)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| failure("requested upload filename is invalid"))?;
    let provider = CloudProvider::Uc;
    let root_id = ProviderRegistry::global().get(provider).root_directory_id;
    let state = CloudState::new(session_root()?)?;
    let http = LiveTransport::new()?;
    let test_name = test_root_name(provider);
    let test_root = step(provider, "requested-upload create root", || {
        create_directory(&state, &http, provider, root_id, &test_name)
    })?;
    let result = (|| -> LiveResult<()> {
        let uploaded = step(provider, "requested file upload", || {
            upload_file(&state, &http, provider, &test_root.id, &path)
        })?;
        if uploaded.name != name || uploaded.size != Some(metadata.len()) {
            return Err(failure("requested upload returned mismatched metadata"));
        }
        let listed = wait_for_named(
            &state,
            &http,
            provider,
            &test_root.id,
            name,
            CloudFileKind::File,
        )?;
        if listed.size != Some(metadata.len()) {
            return Err(failure("requested upload listing returned mismatched size"));
        }
        Ok(())
    })();
    let cleanup = trash(&state, &http, provider, &test_root.id)
        .map_err(|error| {
            failure(format!(
                "requested upload cleanup failed: {}",
                error_kind(&error)
            ))
        })
        .and_then(|()| wait_until_absent(&state, &http, provider, root_id, &test_root.id))
        .and_then(|()| purge_named_from_recycle(&state, &http, provider, &[&test_name]));
    http.report();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(test), Ok(())) => Err(test),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(test), Err(cleanup)) => Err(failure(format!(
            "requested upload failed ({test}); cleanup also failed ({cleanup})"
        ))),
    }
}

fn run_uc_personal_share_lifecycle() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let root_id = ProviderRegistry::global().get(provider).root_directory_id;
    let credentials_root = session_root()?;
    let direct_store = SystemCredentialStore::new(credentials_root.clone())?;
    direct_store.load(provider.key()).map_err(|error| {
        failure(format!(
            "{} credential decrypt failed: {}",
            provider.key(),
            error_kind(&error)
        ))
    })?;
    eprintln!("LIVE-E2E {} credential decrypt: PASS", provider.key());
    let state = CloudState::new(credentials_root)?;
    if state.store.load(provider.key())?.is_none() {
        return Err(failure(format!(
            "{} is not currently authorized",
            provider.key()
        )));
    }
    let http = LiveTransport::new()?;
    if let Err(error) = cleanup_abandoned_uc_e2e_shares(&state, &http) {
        http.report();
        return Err(error);
    }
    let test_name = test_root_name(provider);
    let test_root = match step(provider, "personal-share create root", || {
        create_directory(&state, &http, provider, root_id, &test_name)
    }) {
        Ok(root) => root,
        Err(error) => {
            http.report();
            if let Ok(created) = wait_for_named(
                &state,
                &http,
                provider,
                root_id,
                &test_name,
                CloudFileKind::Folder,
            ) {
                let _ = trash(&state, &http, provider, &created.id);
                let _ = purge_named_from_recycle(&state, &http, provider, &[&test_name]);
            }
            return Err(error);
        }
    };
    let mut created_share_id = None;
    let result = (|| -> LiveResult<()> {
        step(provider, "personal-share create child", || {
            create_directory(&state, &http, provider, &test_root.id, "share-source")
        })?;
        let share = step(provider, "personal-share create", || {
            ProviderRegistry::global()
                .driver(provider, &state.store, &http)
                .create_share(
                    std::slice::from_ref(&test_root.id),
                    &CloudShareOptions {
                        name: test_name.clone(),
                        pass_code: None,
                        generate_pass_code: false,
                        expires_in_days: None,
                    },
                )
        })?;
        if share.id.is_empty() {
            return Err(failure("UC personal-share create returned an empty ID"));
        }
        let share_url = url::Url::parse(&share.url)
            .map_err(|_| failure("UC personal-share create returned an invalid URL"))?;
        if share_url.scheme() != "https" || share_url.host_str().is_none() {
            return Err(failure(
                "UC personal-share create did not return an HTTPS URL",
            ));
        }
        created_share_id = Some(share.id.clone());
        let listed = step(provider, "personal-share list", || {
            ProviderRegistry::global()
                .driver(provider, &state.store, &http)
                .list_shares()
        })?;
        if !listed.iter().any(|candidate| candidate.id == share.id) {
            return Err(failure(
                "UC personal-share list did not contain the created share",
            ));
        }
        step(provider, "personal-share cancel", || {
            ProviderRegistry::global()
                .driver(provider, &state.store, &http)
                .cancel_shares(std::slice::from_ref(&share.id))
        })?;
        let listed_after_cancellation = step(provider, "personal-share list after cancel", || {
            ProviderRegistry::global()
                .driver(provider, &state.store, &http)
                .list_shares()
        })?;
        if listed_after_cancellation
            .iter()
            .any(|candidate| candidate.id == share.id)
        {
            return Err(failure(
                "UC personal-share list still contained the cancelled share",
            ));
        }
        created_share_id = None;
        eprintln!("LIVE-E2E uc personal-share cancellation list confirmation: PASS");
        Ok(())
    })();
    let share_cleanup = match created_share_id.take() {
        Some(share_id) => ProviderRegistry::global()
            .driver(provider, &state.store, &http)
            .cancel_shares(std::slice::from_ref(&share_id))
            .map_err(|error| {
                failure(format!(
                    "UC personal-share cleanup failed: {}",
                    error_kind(&error)
                ))
            }),
        None => Ok(()),
    };
    let directory_cleanup = trash(&state, &http, provider, &test_root.id)
        .map_err(|error| {
            failure(format!(
                "UC personal-share root cleanup failed: {}",
                error_kind(&error)
            ))
        })
        .and_then(|()| wait_until_absent(&state, &http, provider, root_id, &test_root.id))
        .and_then(|()| purge_named_from_recycle(&state, &http, provider, &[&test_name]));
    http.report();
    match (result, share_cleanup, directory_cleanup) {
        (Ok(()), Ok(()), Ok(())) => {
            eprintln!("LIVE-E2E uc personal-share cleanup: PASS");
            Ok(())
        }
        (Err(test), Ok(()), Ok(())) => Err(test),
        (Ok(()), Err(share_cleanup), Ok(())) => Err(share_cleanup),
        (Ok(()), Ok(()), Err(directory_cleanup)) => Err(directory_cleanup),
        (Err(test), Err(share_cleanup), Ok(())) => Err(failure(format!(
            "personal-share test failed ({test}); share cleanup also failed ({share_cleanup})"
        ))),
        (Err(test), Ok(()), Err(directory_cleanup)) => Err(failure(format!(
            "personal-share test failed ({test}); directory cleanup also failed ({directory_cleanup})"
        ))),
        (Ok(()), Err(share_cleanup), Err(directory_cleanup)) => Err(failure(format!(
            "personal-share share cleanup failed ({share_cleanup}); directory cleanup also failed ({directory_cleanup})"
        ))),
        (Err(test), Err(share_cleanup), Err(directory_cleanup)) => Err(failure(format!(
            "personal-share test failed ({test}); share cleanup failed ({share_cleanup}); directory cleanup failed ({directory_cleanup})"
        ))),
    }
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and the current user's authorized Aliyun session"]
fn live_aliyun_complete_crud_and_transfer_matrix() -> LiveResult<()> {
    run_provider(CloudProvider::Aliyun)
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and the current user's authorized Aliyun session; read-only personal-share listing"]
fn live_aliyun_personal_share_list() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Aliyun;
    let state = CloudState::new(session_root()?)?;
    let http = LiveTransport::new()?;
    let result = step(provider, "personal-share list", || {
        ProviderRegistry::global()
            .driver(provider, &state.store, &http)
            .list_shares()
    });
    http.report();
    result.map(|shares| {
        eprintln!(
            "LIVE-E2E aliyun personal-share list returned {} item(s): PASS",
            shares.len()
        );
    })
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and an existing file in the current user's Aliyun root directory; read-only download URL resolution"]
fn live_aliyun_existing_file_download_resolution() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Aliyun;
    let state = CloudState::new(session_root()?)?;
    let http = LiveTransport::new()?;
    let directory = step(provider, "list root for existing-file download", || {
        driver_list(
            &state,
            &http,
            provider,
            ProviderRegistry::global().get(provider).root_directory_id,
        )
    })?;
    let file = directory
        .items
        .into_iter()
        .find(|item| item.kind == CloudFileKind::File)
        .ok_or_else(|| {
            failure("Aliyun root has no file available for read-only download testing")
        })?;
    let result = step(provider, "resolve existing-file download", || {
        resolve_download(&state, &http, provider, &file.id)
    });
    http.report();
    result.map(|_| ())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and an existing file in the current user's Aliyun root directory; creates then cancels one temporary personal share"]
fn live_aliyun_existing_file_personal_share_lifecycle() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Aliyun;
    let state = CloudState::new(session_root()?)?;
    let http = LiveTransport::new()?;
    let directory = step(provider, "list root for existing-file share", || {
        driver_list(
            &state,
            &http,
            provider,
            ProviderRegistry::global().get(provider).root_directory_id,
        )
    })?;
    let file = directory
        .items
        .into_iter()
        .find(|item| item.kind == CloudFileKind::File)
        .ok_or_else(|| failure("Aliyun root has no file available for temporary share testing"))?;
    let share_name = format!(
        "VersaDown temporary verification {}",
        uuid::Uuid::new_v4()
    );
    let mut created_share_id = None;
    let result = (|| -> LiveResult<()> {
        let share = step(provider, "temporary personal-share create", || {
            ProviderRegistry::global()
                .driver(provider, &state.store, &http)
                .create_share(
                    std::slice::from_ref(&file.id),
                    &CloudShareOptions {
                        name: share_name,
                        pass_code: None,
                        generate_pass_code: false,
                        expires_in_days: None,
                    },
                )
        })?;
        if share.id.is_empty() {
            return Err(failure(
                "Aliyun temporary personal-share create returned an empty ID",
            ));
        }
        created_share_id = Some(share.id.clone());
        let share_url = url::Url::parse(&share.url).map_err(|_| {
            failure("Aliyun temporary personal-share create returned an invalid URL")
        })?;
        if share_url.scheme() != "https" || share_url.host_str().is_none() {
            return Err(failure(
                "Aliyun temporary personal-share create did not return an HTTPS URL",
            ));
        }
        step(provider, "temporary personal-share cancel", || {
            ProviderRegistry::global()
                .driver(provider, &state.store, &http)
                .cancel_shares(std::slice::from_ref(&share.id))
        })?;
        let listed_after_cancellation = step(
            provider,
            "personal-share list after temporary cancel",
            || {
                ProviderRegistry::global()
                    .driver(provider, &state.store, &http)
                    .list_shares()
            },
        )?;
        if listed_after_cancellation
            .iter()
            .any(|candidate| candidate.id == share.id)
        {
            return Err(failure(
                "Aliyun personal-share list still contained the cancelled temporary share",
            ));
        }
        created_share_id = None;
        Ok(())
    })();
    let share_cleanup = match created_share_id.take() {
        Some(share_id) => ProviderRegistry::global()
            .driver(provider, &state.store, &http)
            .cancel_shares(std::slice::from_ref(&share_id))
            .map_err(|error| {
                failure(format!(
                    "Aliyun temporary personal-share cleanup failed: {}",
                    error_kind(&error)
                ))
            }),
        None => Ok(()),
    };
    http.report();
    match (result, share_cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(test), Ok(())) => Err(test),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(test), Err(cleanup)) => Err(failure(format!(
            "Aliyun temporary personal-share test failed ({test}); cleanup also failed ({cleanup})"
        ))),
    }
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and the current user's authorized Aliyun session"]
fn live_aliyun_trash_already_absent_file_is_idempotent() -> LiveResult<()> {
    require_live_authorization()?;
    let state = CloudState::new(session_root()?)?;
    let http = LiveTransport::new()?;
    let result = trash(
        &state,
        &http,
        CloudProvider::Aliyun,
        "0000000000000000000000000000000000000000",
    );
    http.report();
    result.map_err(|error| {
        failure(format!(
            "Aliyun trash should be idempotent for an already absent file: {}",
            error_kind(&error)
        ))
    })
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and the current user's authorized UC session"]
fn live_uc_complete_crud_and_transfer_matrix() -> LiveResult<()> {
    run_provider(CloudProvider::Uc)
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and an existing file in the current user's UC root directory; read-only direct-download verification"]
fn live_uc_existing_file_download() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let credentials_root = session_root()?;
    let credentials = SystemCredentialStore::new(credentials_root.clone()).map_err(|error| {
        failure(format!(
            "UC credential store initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    if credentials
        .load(provider.key())
        .map_err(|error| {
            failure(format!(
                "UC credential decrypt failed: {}",
                error_kind(&error)
            ))
        })?
        .is_none()
    {
        return Err(failure("UC is not currently authorized"));
    }
    let state = CloudState::new(credentials_root).map_err(|error| {
        failure(format!(
            "UC state initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    let http = LiveTransport::new()?;
    let directory = step(provider, "list root for existing-file download", || {
        driver_list(
            &state,
            &http,
            provider,
            ProviderRegistry::global().get(provider).root_directory_id,
        )
    })?;
    let file = directory
        .items
        .into_iter()
        .filter(|item| {
            item.kind == CloudFileKind::File && item.size.is_some_and(|size| size <= 1024 * 1024)
        })
        .min_by_key(|item| item.size)
        .ok_or_else(|| {
            failure("UC root has no small file available for read-only download testing")
        })?;
    let result = step(provider, "resolve existing-file download", || {
        resolve_download(&state, &http, provider, &file.id)
    })
    .and_then(download_bytes);
    http.report();
    result.map(|_| ())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1; read-only check that pending UC downloads are bound to the currently authorized account"]
fn live_uc_pending_download_account_binding() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let credentials_root = session_root()?;
    let state = CloudState::new(credentials_root).map_err(|error| {
        failure(format!(
            "UC state initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    let http = LiveTransport::new()?;
    let account = step(
        provider,
        "read current account for pending-download binding",
        || {
            ProviderRegistry::global()
                .driver(provider, &state.store, &http)
                .account()
        },
    )?;
    let record = state
        .auth_supervisor
        .record(&state.store, provider)
        .map_err(|error| {
            failure(format!(
                "UC authorization record read failed: {}",
                error_kind(&error)
            ))
        })?;
    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| failure("APPDATA is required for the task store"))?
        .join("com.versadown.desktop");
    let connection = Connection::open_with_flags(
        app_data.join("tasks.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (pending, matching, unbound): (i64, i64, i64) = connection.query_row(
        "SELECT
            COUNT(*),
            COALESCE(SUM(CASE WHEN a.account_id = ?1 THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN a.account_id IS NULL THEN 1 ELSE 0 END), 0)
         FROM tasks t
         LEFT JOIN cloud_download_accounts a ON a.id = t.id
         WHERE t.source LIKE 'cloud://uc/%'
           AND t.status = 'paused'
           AND t.failure_code = 'authorization_pending'",
        [&account.id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let record_matches = record.account_id.as_deref() == Some(&account.id);
    eprintln!(
        "LIVE-E2E uc pending-download binding: pending={pending}; matching={matching}; unbound={unbound}; supervisor_matches={record_matches}"
    );
    if pending > 0 && (matching != pending || unbound > 0 || !record_matches) {
        return Err(failure(
            "a pending UC download is not bound to the current authorized account",
        ));
    }
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and a pending UC task; compares only redacted account/session relationships"]
fn live_uc_current_session_and_pending_task_owners_are_consistent() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let credentials_root = session_root()?;
    let credentials = SystemCredentialStore::new(credentials_root.clone()).map_err(|error| {
        failure(format!(
            "UC credential store initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    let bytes = credentials
        .load(provider.key())?
        .ok_or_else(|| failure("UC is not currently authorized"))?;
    let session: UcSession =
        serde_json::from_slice(&bytes).map_err(|_| failure("UC session has an invalid shape"))?;
    let persisted_account_id = session.account_id.clone();
    let current_uid = session
        .cookie
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| (name == "__uid" && !value.is_empty()).then(|| value.to_owned()));
    let mut server_session = session;
    server_session.account_id = None;
    let http = LiveTransport::new()?;
    let server_account_id = UcClient::new(server_session, &http)
        .verified_account_id()?
        .ok_or_else(|| failure("UC did not expose a stable current account identity"))?;
    let state = CloudState::new(credentials_root)?;
    let record = state.auth_supervisor.record(&state.store, provider)?;

    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| failure("APPDATA is required for the task store"))?
        .join("com.versadown.desktop");
    let connection = Connection::open_with_flags(
        app_data.join("tasks.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = connection.prepare(
        "SELECT t.source, a.account_id FROM tasks t
         JOIN cloud_download_accounts a ON a.id = t.id
         WHERE a.provider = 'uc'
           AND t.source LIKE 'cloud://uc/%'
           AND t.status = 'paused'
           AND t.failure_code = 'authorization_pending'",
    )?;
    let pending = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let stable_owner_matches = pending
        .iter()
        .filter(|(_, owner)| owner == &server_account_id)
        .count();
    let current_uid_matches = current_uid.as_ref().map_or(0, |current_uid| {
        pending
            .iter()
            .filter(|(_, owner)| owner == current_uid)
            .count()
    });
    let mut resolvable = 0;
    let mut source_missing = 0;
    for (source, _) in &pending {
        let (_, file_id) =
            parse_stable_cloud_source(source).map_err(|error| failure(error_kind(&error)))?;
        match resolve_download(&state, &http, provider, &file_id) {
            Ok(_) => resolvable += 1,
            Err(CloudError::SourceFileMissing) => source_missing += 1,
            Err(error) => {
                return Err(failure(format!(
                    "UC pending source ownership check failed: {}",
                    error_kind(&error)
                )))
            }
        }
    }
    let persisted_matches_server = persisted_account_id.as_deref() == Some(&server_account_id);
    let supervisor_matches_server = record.account_id.as_deref() == Some(&server_account_id);
    http.report();
    eprintln!(
        "LIVE-E2E uc owner consistency: pending={}; persisted-matches-server={persisted_matches_server}; supervisor-matches-server={supervisor_matches_server}; stable-owner-matches={stable_owner_matches}; current-uid-matches={current_uid_matches}; resolvable={resolvable}; source-missing={source_missing}",
        pending.len(),
    );
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1; reconciles only the saved UC account metadata with the current authenticated browser session"]
fn live_uc_reconciles_saved_account_with_the_current_browser_user() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let credentials_root = session_root()?;
    let state = CloudState::new(credentials_root)?;
    let http = LiveTransport::new()?;

    let account = ProviderRegistry::global()
        .driver(provider, &state.store, &http)
        .verified_account()
        .map_err(|error| {
            failure(format!(
                "UC current account reconciliation failed: {}",
                error_kind(&error)
            ))
        })?;
    state.auth_supervisor.ready(
        &state.store,
        provider,
        Some(account.id.clone()),
        Some(account.auth_method),
        None,
    )?;
    let saved: UcSession = serde_json::from_slice(
        &state
            .store
            .load(provider.key())?
            .ok_or_else(|| failure("UC is not currently authorized"))?,
    )
    .map_err(|_| failure("UC session has an invalid shape"))?;
    let mut live_session = saved.clone();
    live_session.account_id = None;
    let live_account_id = UcClient::new(live_session, &http)
        .verified_account_id()?
        .ok_or_else(|| failure("UC did not expose a stable current account identity"))?;
    let reconciled = account.id == live_account_id
        && saved.account_id.as_deref() == Some(live_account_id.as_str())
        && state
            .auth_supervisor
            .record(&state.store, provider)?
            .account_id
            .as_deref()
            == Some(live_account_id.as_str());

    http.report();
    eprintln!("LIVE-E2E uc saved account reconciliation: reconciled={reconciled}");
    if !reconciled {
        return Err(failure(
            "the saved UC account identity still differs from the current browser user",
        ));
    }
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1; read-only check that UC exposes a stable member identity"]
fn live_uc_member_identity_is_not_a_rotating_browser_marker() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let credentials_root = session_root()?;
    let credentials = SystemCredentialStore::new(credentials_root).map_err(|error| {
        failure(format!(
            "UC credential store initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    let bytes = credentials
        .load(provider.key())
        .map_err(|error| {
            failure(format!(
                "UC credential decrypt failed: {}",
                error_kind(&error)
            ))
        })?
        .ok_or_else(|| failure("UC is not currently authorized"))?;
    let session: UcSession =
        serde_json::from_slice(&bytes).map_err(|_| failure("UC session has an invalid shape"))?;
    let http = LiveTransport::new()?;
    let identity = step(provider, "read verified member identity", || {
        UcClient::new(session, &http).verified_account_id()
    })?
    .ok_or_else(|| failure("UC member response did not contain a stable identity"))?;
    http.report();
    if identity.starts_with("AAR/") {
        return Err(failure(
            "UC returned a rotating browser marker instead of a stable identity",
        ));
    }
    eprintln!("LIVE-E2E uc member identity: verified-stable=true");
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and a legacy pending UC task; read-only classification before recovery"]
fn live_uc_legacy_pending_download_sources_are_classified_by_current_session() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| failure("APPDATA is required for the task store"))?
        .join("com.versadown.desktop");
    let connection = Connection::open_with_flags(
        app_data.join("tasks.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = connection.prepare(
        "SELECT t.source, t.name FROM tasks t
         JOIN cloud_download_accounts a ON a.id = t.id
         WHERE a.provider = 'uc'
           AND a.account_id LIKE 'AAR/%'
           AND t.source LIKE 'cloud://uc/%'
           AND t.status = 'paused'
           AND t.failure_code = 'authorization_pending'",
    )?;
    let sources = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if sources.is_empty() {
        return Err(failure(
            "no legacy pending UC download was available for verification",
        ));
    }
    let state = CloudState::new(session_root()?).map_err(|error| {
        failure(format!(
            "UC state initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    let http = LiveTransport::new()?;
    let mut resolvable = 0;
    let mut missing = 0;
    let root = driver_list(
        &state,
        &http,
        provider,
        ProviderRegistry::global().get(provider).root_directory_id,
    )?;
    let mut exact_id_in_root = 0;
    let mut same_name_in_root = 0;
    for (source, name) in &sources {
        let (source_provider, file_id) =
            parse_stable_cloud_source(source).map_err(|error| failure(error_kind(&error)))?;
        if source_provider != provider {
            return Err(failure("legacy UC task source selected the wrong provider"));
        }
        exact_id_in_root += usize::from(root.items.iter().any(|item| item.id == file_id));
        same_name_in_root += usize::from(root.items.iter().any(|item| item.name == *name));
        match resolve_download(&state, &http, provider, &file_id) {
            Ok(_) => resolvable += 1,
            Err(CloudError::SourceFileMissing) => missing += 1,
            Err(error) => {
                return Err(failure(format!(
                    "UC pending-download source classification failed: {}",
                    error_kind(&error)
                )))
            }
        }
    }
    http.report();
    eprintln!(
        "LIVE-E2E uc legacy pending sources: candidates={}; resolvable={resolvable}; source-missing={missing}; exact-id-in-root={exact_id_in_root}; same-name-in-root={same_name_in_root}",
        sources.len(),
    );
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and VERSADOWN_LIVE_UC_FILE_NAME; read-only comparison of the current root object and a paused task source"]
fn live_uc_named_root_file_and_pending_task_identity() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let file_name = std::env::var("VERSADOWN_LIVE_UC_FILE_NAME")
        .map_err(|_| failure("VERSADOWN_LIVE_UC_FILE_NAME is required"))?;
    let state = CloudState::new(session_root()?).map_err(|error| {
        failure(format!(
            "UC state initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    let http = LiveTransport::new()?;
    let root = driver_list(
        &state,
        &http,
        provider,
        ProviderRegistry::global().get(provider).root_directory_id,
    )?;
    let root_ids = root
        .items
        .iter()
        .filter(|item| item.kind == "file" && item.name == file_name)
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();
    let root_file_count = root.items.iter().filter(|item| item.kind == "file").count();
    let root_folder_count = root
        .items
        .iter()
        .filter(|item| item.kind == "folder")
        .count();

    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| failure("APPDATA is required for the task store"))?
        .join("com.versadown.desktop");
    let connection = Connection::open_with_flags(
        app_data.join("tasks.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = connection.prepare(
        "SELECT t.source FROM tasks t
         JOIN cloud_download_accounts a ON a.id = t.id
         WHERE a.provider = 'uc'
           AND t.name = ?1
           AND t.source LIKE 'cloud://uc/%'
           AND t.status = 'paused'
           AND t.failure_code = 'authorization_pending'",
    )?;
    let task_ids = statement
        .query_map([file_name.as_str()], |row| row.get::<_, String>(0))?
        .map(|source| {
            let source = source?;
            parse_stable_cloud_source(&source)
                .map(|(_, file_id)| file_id)
                .map_err(|_| rusqlite::Error::InvalidQuery)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let root_resolvable = root_ids
        .iter()
        .filter(|file_id| resolve_download(&state, &http, provider, file_id).is_ok())
        .count();
    let task_resolvable = task_ids
        .iter()
        .filter(|file_id| resolve_download(&state, &http, provider, file_id).is_ok())
        .count();
    let same_object = task_ids
        .iter()
        .any(|task_id| root_ids.iter().any(|root_id| root_id == task_id));
    http.report();
    eprintln!(
        "LIVE-E2E uc named file identity: root-files={root_file_count}; root-folders={root_folder_count}; root-matches={}; pending-tasks={}; same-object={same_object}; root-resolvable={root_resolvable}; task-resolvable={task_resolvable}",
        root_ids.len(),
        task_ids.len(),
    );
    if root_ids.is_empty() {
        return Err(failure(
            "the named UC file was not returned by a fresh root listing",
        ));
    }
    if root_resolvable != root_ids.len() {
        return Err(failure(
            "a named UC root file could not resolve a current download URL",
        ));
    }
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and an existing small UC file; verifies the production aria2 handoff without retaining a signed URL or file"]
fn live_uc_existing_file_aria2_download() -> LiveResult<()> {
    require_live_authorization()?;
    let provider = CloudProvider::Uc;
    let credentials_root = session_root()?;
    let credentials = SystemCredentialStore::new(credentials_root.clone()).map_err(|error| {
        failure(format!(
            "UC credential store initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    if credentials
        .load(provider.key())
        .map_err(|error| {
            failure(format!(
                "UC credential decrypt failed: {}",
                error_kind(&error)
            ))
        })?
        .is_none()
    {
        return Err(failure("UC is not currently authorized"));
    }
    let state = CloudState::new(credentials_root).map_err(|error| {
        failure(format!(
            "UC state initialization failed: {}",
            error_kind(&error)
        ))
    })?;
    let http = LiveTransport::new()?;
    let directory = step(
        provider,
        "list root for aria2 existing-file download",
        || {
            driver_list(
                &state,
                &http,
                provider,
                ProviderRegistry::global().get(provider).root_directory_id,
            )
        },
    )?;
    let file = directory
        .items
        .into_iter()
        .filter(|item| {
            item.kind == CloudFileKind::File && item.size.is_some_and(|size| size <= 1024 * 1024)
        })
        .min_by_key(|item| item.size)
        .ok_or_else(|| failure("UC root has no small file available for aria2 download testing"))?;
    let resolved = step(provider, "resolve existing-file aria2 download", || {
        resolve_download(&state, &http, provider, &file.id)
    })?;
    http.report();

    // The signed URL and downloaded bytes live only in this temporary test
    // directory. The durable task intentionally uses a non-sensitive marker.
    let root = tempfile::tempdir()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let mut aria2 = Aria2Settings::default();
    aria2.rpc_port = listener.local_addr()?.port();
    aria2.rpc_token = format!("uc-live-download-{}", aria2.rpc_port);
    aria2.max_connections_per_server = 1;
    drop(listener);
    let config = Aria2LaunchConfig {
        binary: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
        data_dir: root.path().join("data"),
        download_dir: root.path().join("downloads"),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let (mut process, client) = Aria2Process::start(config.clone())
            .await
            .map_err(|_| failure("UC aria2 test engine failed to start"))?;
        let store = Arc::new(TaskStore::open_in_memory()?);
        let service = DownloadService::new(Arc::new(client), Arc::clone(&store));
        let task = store.create(NewTask {
            source: format!(
                "cloud://uc/{}",
                url::form_urlencoded::byte_serialize(file.id.as_bytes()).collect::<String>()
            ),
            name: resolved.name.clone(),
            kind: "cloud".into(),
            is_private: true,
        })?;
        let output = config.download_dir.join(&resolved.name);
        let url = resolved.url.clone();
        let mut options = resolved.into_uri_submission_options();
        options.directory = Some(config.download_dir.clone());
        options.max_connections_per_server = Some(1);
        options.max_tries = Some(1);
        options.retry_wait_seconds = Some(0);

        let result = async {
            service
                .submit_uri_with_options(task.id, vec![url], options)
                .await
                .map_err(|_| failure("aria2 did not accept the UC download task"))?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                let current = service
                    .reconcile_task(task.id)
                    .await
                    .map_err(|_| failure("aria2 could not reconcile the UC download task"))?;
                match current.status {
                    TaskStatus::Completed => {
                        let length = fs::metadata(&output)?.len();
                        if length == 0 {
                            return Err(failure("aria2 reported a zero-byte UC download"));
                        }
                        return Ok(());
                    }
                    TaskStatus::Failed => {
                        return Err(failure("aria2 reported a failed UC download"));
                    }
                    _ if tokio::time::Instant::now() >= deadline => {
                        return Err(failure("aria2 UC download exceeded the 60-second deadline"));
                    }
                    _ => tokio::time::sleep(Duration::from_millis(250)).await,
                }
            }
        }
        .await;
        let _ = process.terminate_fallback();
        result
    });
    drop(runtime);
    result
}

#[test]
#[ignore = "requires the current user's authorized UC session; covers folder operations when account policy blocks uploads"]
fn live_uc_folder_operations_matrix() -> LiveResult<()> {
    run_uc_folder_matrix()
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1, VERSADOWN_LIVE_UC_UPLOAD_PATH and an authorized UC session"]
fn live_uc_requested_file_upload() -> LiveResult<()> {
    run_uc_requested_upload()
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and the current user's authorized UC session; creates, cancels, and cleans up one isolated personal share"]
fn live_uc_personal_share_lifecycle() -> LiveResult<()> {
    run_uc_personal_share_lifecycle()
}

#[test]
#[ignore = "read-only cleanup audit for VersaDown-E2E Aliyun and UC test roots"]
fn live_no_aliyun_or_uc_e2e_artifacts_remain() -> LiveResult<()> {
    require_live_authorization()?;
    let state = CloudState::new(session_root()?)?;
    let http = LiveTransport::new()?;
    let mut leftovers = Vec::new();
    for provider in [CloudProvider::Aliyun, CloudProvider::Uc] {
        let descriptor = ProviderRegistry::global().get(provider);
        for directory_id in [
            descriptor.root_directory_id,
            descriptor.recycle_bin_directory_id,
        ] {
            let directory = driver_list(&state, &http, provider, directory_id)?;
            leftovers.extend(
                directory
                    .items
                    .into_iter()
                    .filter(|item| {
                        item.name
                            .starts_with(&format!("VersaDown-E2E-{}-", provider.key()))
                    })
                    .map(|item| format!("{}:{}", provider.key(), item.name)),
            );
        }
    }
    if leftovers.is_empty() {
        Ok(())
    } else {
        Err(failure(format!(
            "isolated E2E artifacts remain: {}",
            leftovers.join(", ")
        )))
    }
}
