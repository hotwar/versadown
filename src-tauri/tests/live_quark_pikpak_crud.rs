//! Opt-in, destructive-within-fixture live CRUD coverage for Quark and PikPak.
//!
//! Every remote object is created below one unique `VersaDown-E2E-*`
//! directory. Cleanup permanently deletes only ids proven to belong to that
//! directory (or its explicitly created descendants).

use versadown_lib::cloud::{
    auth_supervisor::{recover_in_order, RecoveryPhase},
    commands::CloudState,
    driver::ProviderDriver,
    CloudError, CloudFile, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudProvider,
    CloudUploadTransport, CredentialStore, ProviderRegistry, QuarkClient, ReqwestTransport,
    ResolvedCloudDownload, SessionWorkKind, SystemCredentialStore, UploadHttpRequest,
    UploadHttpResponse,
};
use std::{
    collections::BTreeMap,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type LiveResult<T> = Result<T, Box<dyn Error>>;

struct LiveHttp(ReqwestTransport);

impl LiveHttp {
    fn new() -> Result<Self, CloudError> {
        Ok(Self(ReqwestTransport::new()?))
    }
}

impl CloudHttpTransport for LiveHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
        let endpoint = url::Url::parse(&request.url).ok().map(|url| format!("{}{}", url.host_str().unwrap_or("unknown"), url.path())).unwrap_or_default();
        let response = self.0.send(request)?;
        let value = serde_json::from_str::<serde_json::Value>(&response.body).unwrap_or_default();
        eprintln!("API {endpoint}: HTTP {} code {:?} status {:?}", response.status, value.get("code").and_then(serde_json::Value::as_i64), value.get("status").and_then(serde_json::Value::as_i64));
        if !(200..300).contains(&response.status) {
            let message = value.get("message").or_else(|| value.get("error")).and_then(serde_json::Value::as_str).unwrap_or("").to_lowercase();
            let categories: Vec<_> = ["client", "客户端", "storage", "capacity", "容量", "quota", "origin", "跨域", "csrf", "token", "sign", "签名", "login", "登录", "risk", "验证", "permission", "权限"].into_iter().filter(|word| message.contains(word)).collect();
            eprintln!("API rejection categories: {categories:?}");
        }
        Ok(response)
    }
    fn manages_cookie_session(&self) -> bool { self.0.manages_cookie_session() }
    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, CloudError> {
        self.0.cookies_for_url(url)
    }
    fn isolated_session(&self) -> Result<Option<Box<dyn CloudHttpTransport>>, CloudError> {
        self.0.isolated_session()
    }
}

impl CloudUploadTransport for LiveHttp {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, CloudError> {
        self.0.send_upload(request)
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

fn session_root() -> LiveResult<PathBuf> {
    let app_data = std::env::var_os("APPDATA")
        .ok_or_else(|| failure("APPDATA is required for the encrypted session store"))?;
    Ok(PathBuf::from(app_data)
        .join("com.versadown.desktop")
        .join("cloud-sessions"))
}

fn unique_name(provider: CloudProvider, suffix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("VersaDown-E2E-{}-{millis}-{suffix}", provider.key())
}

fn error_kind(error: &CloudError) -> &'static str {
    match error {
        CloudError::NotAuthorized => "not-authorized",
        CloudError::AccountMismatch => "account-mismatch",
        CloudError::AuthorizationExpired => "authorization-expired",
        CloudError::WebAuthorizationRequired => "web-authorization-required",
        CloudError::BaiduOperationVerificationRequired => "baidu-operation-verification-required",
        CloudError::CaptchaRequired => "captcha-required",
        CloudError::PikpakRegionalMembershipRestricted => "regional-membership-restricted",
        CloudError::PikpakOperationVerificationRequired(_) => "operation-verification-required",
        CloudError::AuthorizationPending => "authorization-pending",
        CloudError::SourceFileMissing => "source-file-missing",
        CloudError::DownloadCheckpointInvalid => "download-checkpoint-invalid",
        CloudError::DownloadEngineUnavailable => "download-engine-unavailable",
        CloudError::DownloadSubmissionFailed => "download-submission-failed",
        CloudError::SessionRenewedRetryOperation => "session-renewed-retry-operation",
        CloudError::InvalidResponse => "invalid-response",
        CloudError::Storage => "storage",
        CloudError::Transport(_) => "transport",
        CloudError::Provider(_) => "provider",
        CloudError::InvalidInput(_) => "invalid-input",
        CloudError::UploadUnavailable => "upload-unavailable",
        CloudError::ProviderUnsupported => "provider-unsupported",
    }
}

fn supervised<T>(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    kind: SessionWorkKind,
    mut operation: impl FnMut(&mut ProviderDriver<'_>) -> Result<T, CloudError>,
) -> Result<T, CloudError> {
    state.auth_supervisor.run_checked(
        &state.store,
        provider,
        kind,
        |snapshot| {
            let mut driver = ProviderRegistry::global().driver(provider, snapshot, http);
            operation(&mut driver)
        },
        |phase: RecoveryPhase| {
            let mut driver = ProviderRegistry::global().driver(provider, &state.store, http);
            recover_in_order(provider, &state.store, driver.auth(), phase, || Ok(false))
        },
        || Ok(()),
    )
}

fn read<T>(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    operation: impl FnMut(&mut ProviderDriver<'_>) -> Result<T, CloudError>,
) -> LiveResult<T> {
    supervised(state, http, provider, SessionWorkKind::Read, operation)
        .map_err(|error| failure(format!("{} read: {}", provider.key(), error_kind(&error))))
}

fn mutate<T>(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    operation: impl FnMut(&mut ProviderDriver<'_>) -> Result<T, CloudError>,
) -> LiveResult<T> {
    supervised(state, http, provider, SessionWorkKind::Mutation, operation).map_err(|error| {
        failure(format!(
            "{} mutation: {} ({error})",
            provider.key(),
            error_kind(&error)
        ))
    })
}

fn upload(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    parent: &str,
    path: &Path,
) -> LiveResult<CloudFile> {
    let mut driver = ProviderRegistry::global().driver(provider, &state.store, http);
    driver
        .upload(state, http, parent, path)
        .map_err(|error| failure(format!("{} upload: {}", provider.key(), error_kind(&error))))
}

fn wait_find_name(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    parent: &str,
    name: &str,
    kind: &str,
) -> LiveResult<CloudFile> {
    for attempt in 0..12 {
        let listed = read(state, http, provider, |driver| {
            driver.list_directory(parent)
        })?;
        if let Some(item) = listed
            .items
            .iter()
            .find(|item| item.name == name && item.kind == kind)
        {
            return Ok(item.clone());
        }
        if attempt < 11 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Err(failure(format!(
        "created fixture {kind} {name:?} is not listed after provider consistency wait"
    )))
}

fn wait_find_id(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    parent: &str,
    id: &str,
) -> LiveResult<CloudFile> {
    for attempt in 0..12 {
        let listed = read(state, http, provider, |driver| {
            driver.list_directory(parent)
        })?;
        if let Some(item) = listed.items.iter().find(|item| item.id == id) {
            return Ok(item.clone());
        }
        if attempt < 11 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Err(failure(format!(
        "created fixture id {id:?} is not listed after provider consistency wait"
    )))
}

fn download_bytes(download: &ResolvedCloudDownload) -> LiveResult<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()?;
    let mut request = client.get(&download.url);
    for (key, value) in &download.headers {
        request = request.header(key, value);
    }
    let response = request
        .send()
        .map_err(|_| failure("direct download transport failed"))?;
    if !response.status().is_success() {
        return Err(failure(format!(
            "direct download returned HTTP {}",
            response.status()
        )));
    }
    Ok(response.bytes()?.to_vec())
}

fn download_tree(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    parent: &str,
    prefix: &str,
    out: &mut BTreeMap<String, Vec<u8>>,
) -> LiveResult<()> {
    let directory = read(state, http, provider, |driver| {
        driver.list_directory(parent)
    })?;
    for item in directory.items {
        let relative = if prefix.is_empty() {
            item.name.clone()
        } else {
            format!("{prefix}/{}", item.name)
        };
        if item.kind == "folder" {
            download_tree(state, http, provider, &item.id, &relative, out)?;
        } else {
            let download = read(state, http, provider, |driver| {
                driver.resolve_download(&item.id)
            })?;
            out.insert(relative, download_bytes(&download)?);
        }
    }
    Ok(())
}

fn trash_and_delete(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    id: &str,
    name: &str,
) -> LiveResult<()> {
    mutate(state, http, provider, |driver| {
        driver.trash(&[id.to_owned()])
    })?;
    let recycle = ProviderRegistry::global()
        .get(provider)
        .recycle_bin_directory_id;
    let mut recycle_entry = None;
    for attempt in 0..12 {
        let trashed = read(state, http, provider, |driver| {
            driver.list_directory(recycle)
        })?;
        recycle_entry = trashed
            .items
            .iter()
            .find(|item| item.id == id || item.name == name)
            .cloned();
        if recycle_entry.is_some() {
            break;
        }
        if attempt < 11 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    let recycle_entry = recycle_entry.ok_or_else(|| {
        failure("fixture is not listed in recycle bin after provider consistency wait")
    })?;
    mutate(state, http, provider, |driver| {
        driver.delete(&[recycle_entry.id.clone()])
    })?;
    let after = read(state, http, provider, |driver| {
        driver.list_directory(recycle)
    })?;
    if after
        .items
        .iter()
        .any(|item| item.id == recycle_entry.id || item.name == name)
    {
        return Err(failure(
            "fixture remained in recycle bin after permanent delete",
        ));
    }
    Ok(())
}

fn trash_and_confirm_absent(
    state: &CloudState,
    http: &LiveHttp,
    provider: CloudProvider,
    parent: &str,
    id: &str,
) -> LiveResult<()> {
    mutate(state, http, provider, |driver| {
        driver.trash(&[id.to_owned()])
    })?;
    for attempt in 0..12 {
        let after = read(state, http, provider, |driver| {
            driver.list_directory(parent)
        })?;
        if !after.items.iter().any(|item| item.id == id) {
            return Ok(());
        }
        if attempt < 11 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Err(failure(
        "fixture remained in its active directory after provider consistency wait",
    ))
}

fn run_provider(provider: CloudProvider) -> LiveResult<()> {
    require_live_authorization()?;
    let state = CloudState::new(session_root()?)?;
    let http = LiveHttp::new()?;
    let descriptor = ProviderRegistry::global().get(provider);
    let root_id = descriptor.root_directory_id;

    // Validate and proactively recover the real persisted session before any mutation.
    eprintln!("{} E2E phase: validate-session", provider.key());
    read(&state, &http, provider, |driver| driver.account())?;
    eprintln!("{} E2E phase: list-root", provider.key());
    let root_before = read(&state, &http, provider, |driver| {
        driver.list_directory(root_id)
    })?;

    let base_name = unique_name(provider, "root");
    if root_before.items.iter().any(|item| item.name == base_name) {
        return Err(failure("unexpected unique test root collision"));
    }
    eprintln!("{} E2E phase: create-root", provider.key());
    let base = mutate(&state, &http, provider, |driver| {
        driver.create_directory(root_id, &base_name)
    })?;

    let local_root = std::env::temp_dir().join(unique_name(provider, "local"));
    fs::create_dir_all(local_root.join("directory").join("nested"))?;
    fs::create_dir_all(local_root.join("directory").join("empty"))?;
    let single_payload = format!("single-file-payload:{base_name}").into_bytes();
    let delete_payload = format!("delete-file-payload:{base_name}").into_bytes();
    let top_payload = format!("top-payload:{base_name}").into_bytes();
    let nested_payload = format!("nested-payload:{base_name}").into_bytes();
    fs::write(local_root.join("single.txt"), &single_payload)?;
    fs::write(local_root.join("delete-file.txt"), &delete_payload)?;
    fs::write(local_root.join("directory").join("top.txt"), &top_payload)?;
    fs::write(
        local_root.join("directory").join("nested").join("leaf.txt"),
        &nested_payload,
    )?;
    let mut base_needs_cleanup = true;

    let result = (|| -> LiveResult<()> {
        wait_find_id(&state, &http, provider, root_id, &base.id)?;

        eprintln!("{} E2E phase: single-file", provider.key());
        // Single-file upload, rename, move, and download.
        let single = upload(
            &state,
            &http,
            provider,
            &base.id,
            &local_root.join("single.txt"),
        )?;
        let renamed_file = unique_name(provider, "renamed-file.txt");
        mutate(&state, &http, provider, |driver| {
            driver.rename(&single.id, &renamed_file)
        })?;
        let target = mutate(&state, &http, provider, |driver| {
            driver.create_directory(&base.id, "move-target")
        })?;
        mutate(&state, &http, provider, |driver| {
            driver.move_files(&[single.id.clone()], &target.id)
        })?;
        let moved_single =
            wait_find_name(&state, &http, provider, &target.id, &renamed_file, "file")?;
        let single_download = read(&state, &http, provider, |driver| {
            driver.resolve_download(&moved_single.id)
        })?;
        if download_bytes(&single_download)? != single_payload {
            return Err(failure("single-file download bytes mismatch"));
        }

        eprintln!("{} E2E phase: directory", provider.key());
        // Recursive directory upload, folder rename, folder move, and recursive download.
        let uploaded_dir = mutate(&state, &http, provider, |driver| {
            driver.create_directory(&base.id, "directory")
        })?;
        let nested = mutate(&state, &http, provider, |driver| {
            driver.create_directory(&uploaded_dir.id, "nested")
        })?;
        mutate(&state, &http, provider, |driver| {
            driver.create_directory(&uploaded_dir.id, "empty")
        })?;
        upload(
            &state,
            &http,
            provider,
            &uploaded_dir.id,
            &local_root.join("directory").join("top.txt"),
        )?;
        upload(
            &state,
            &http,
            provider,
            &nested.id,
            &local_root.join("directory").join("nested").join("leaf.txt"),
        )?;
        let renamed_folder = unique_name(provider, "renamed-folder");
        mutate(&state, &http, provider, |driver| {
            driver.rename(&uploaded_dir.id, &renamed_folder)
        })?;
        mutate(&state, &http, provider, |driver| {
            driver.move_files(&[uploaded_dir.id.clone()], &target.id)
        })?;
        let moved_folder = wait_find_name(
            &state,
            &http,
            provider,
            &target.id,
            &renamed_folder,
            "folder",
        )?;
        let mut downloaded = BTreeMap::new();
        download_tree(
            &state,
            &http,
            provider,
            &moved_folder.id,
            "",
            &mut downloaded,
        )?;
        let expected = BTreeMap::from([
            ("nested/leaf.txt".to_owned(), nested_payload.clone()),
            ("top.txt".to_owned(), top_payload.clone()),
        ]);
        if downloaded != expected {
            return Err(failure(
                "recursive directory download tree or bytes mismatch",
            ));
        }

        eprintln!("{} E2E phase: delete-file", provider.key());
        // File deletion and recursive directory deletion are verified in their active
        // parent. The top-level fixture is permanently removed below, recursively
        // cleaning these already-trashed descendants without selecting unrelated trash.
        let delete_file = upload(
            &state,
            &http,
            provider,
            &base.id,
            &local_root.join("delete-file.txt"),
        )?;
        trash_and_confirm_absent(&state, &http, provider, &base.id, &delete_file.id)?;

        eprintln!("{} E2E phase: delete-directory", provider.key());
        let delete_dir = mutate(&state, &http, provider, |driver| {
            driver.create_directory(&base.id, "delete-directory")
        })?;
        let delete_child = mutate(&state, &http, provider, |driver| {
            driver.create_directory(&delete_dir.id, "child")
        })?;
        upload(
            &state,
            &http,
            provider,
            &delete_child.id,
            &local_root.join("delete-file.txt"),
        )?;
        trash_and_confirm_absent(&state, &http, provider, &base.id, &delete_dir.id)?;

        eprintln!("{} E2E phase: cleanup-root", provider.key());
        trash_and_delete(&state, &http, provider, &base.id, &base_name)?;
        base_needs_cleanup = false;
        Ok(())
    })();

    if base_needs_cleanup {
        // Best-effort cleanup is constrained to the unique test root id.
        let _ = trash_and_delete(&state, &http, provider, &base.id, &base_name);
    }
    let _ = fs::remove_dir_all(&local_root);
    result
}

fn cleanup_provider_residuals(provider: CloudProvider) -> LiveResult<()> {
    require_live_authorization()?;
    let state = CloudState::new(session_root()?)?;
    let http = LiveHttp::new()?;
    let descriptor = ProviderRegistry::global().get(provider);
    let prefix = format!("VersaDown-E2E-{}-", provider.key());
    let root = read(&state, &http, provider, |driver| {
        driver.list_directory(descriptor.root_directory_id)
    })?;
    for item in root
        .items
        .iter()
        .filter(|item| item.name.starts_with(&prefix))
    {
        mutate(&state, &http, provider, |driver| {
            driver.trash(&[item.id.clone()])
        })?;
    }
    let recycle_id = descriptor.recycle_bin_directory_id;
    let recycle = read(&state, &http, provider, |driver| {
        driver.list_directory(recycle_id)
    })?;
    for item in recycle
        .items
        .iter()
        .filter(|item| item.name.starts_with(&prefix))
    {
        mutate(&state, &http, provider, |driver| {
            driver.delete(&[item.id.clone()])
        })?;
    }
    let active_after = read(&state, &http, provider, |driver| {
        driver.list_directory(descriptor.root_directory_id)
    })?;
    let recycle_after = read(&state, &http, provider, |driver| {
        driver.list_directory(recycle_id)
    })?;
    if active_after
        .items
        .iter()
        .chain(&recycle_after.items)
        .any(|item| item.name.starts_with(&prefix))
    {
        return Err(failure(
            "provider still contains an VersaDown E2E residual",
        ));
    }
    Ok(())
}

fn audit_provider_residuals(provider: CloudProvider) -> LiveResult<(usize, usize)> {
    require_live_authorization()?;
    let state = CloudState::new(session_root()?)?;
    let http = LiveHttp::new()?;
    let descriptor = ProviderRegistry::global().get(provider);
    let prefix = format!("VersaDown-E2E-{}-", provider.key());
    let active = read(&state, &http, provider, |driver| {
        driver.list_directory(descriptor.root_directory_id)
    })?;
    let recycle = read(&state, &http, provider, |driver| {
        driver.list_directory(descriptor.recycle_bin_directory_id)
    })?;
    Ok((
        active
            .items
            .iter()
            .filter(|item| item.name.starts_with(&prefix))
            .count(),
        recycle
            .items
            .iter()
            .filter(|item| item.name.starts_with(&prefix))
            .count(),
    ))
}

// Shared live acceptance for the reported personal-share failures. Every
// selected remote ID belongs to this invocation's unique fixture directory.
fn run_reported_share_lifecycle(provider: CloudProvider) -> LiveResult<()> {
    use versadown_lib::cloud::CloudShareOptions;
    require_live_authorization()?;
    let state = CloudState::new(session_root()?)?;
    let http = LiveHttp::new()?;
    let root_id = ProviderRegistry::global().get(provider).root_directory_id;
    eprintln!("{} share phase: validate-session", provider.key());
    read(&state, &http, provider, |driver| driver.account())?;
    let name = unique_name(provider, "share-root");
    let base = mutate(&state, &http, provider, |driver| driver.create_directory(root_id, &name))?;
    let local = tempfile::tempdir()?;
    let path = local.path().join("VersaDown-share-test.txt");
    fs::write(&path, b"VersaDown isolated share acceptance fixture\n")?;
    let mut share_id = None;
    let result = (|| -> LiveResult<()> {
        eprintln!("{} share phase: upload", provider.key());
        let file = upload(&state, &http, provider, &base.id, &path)?;
        let options = CloudShareOptions { name: "VersaDown acceptance".into(), pass_code: (provider == CloudProvider::Uc).then(|| "a7B2".into()), generate_pass_code: provider != CloudProvider::Uc, expires_in_days: Some(1) };
        eprintln!("{} share phase: create", provider.key());
        let share = mutate(&state, &http, provider, |driver| driver.create_share(&[file.id.clone()], &options))?;
        share_id = Some(share.id.clone());
        if share.url.is_empty() { return Err(failure("created share URL is missing")); }
        eprintln!("{} share phase: list", provider.key());
        let listed = read(&state, &http, provider, |driver| driver.list_shares())?;
        if !listed.iter().any(|item| item.id == share.id) { return Err(failure("created share is not listed")); }
        eprintln!("{} share phase: cancel", provider.key());
        mutate(&state, &http, provider, |driver| driver.cancel_shares(&[share.id.clone()]))?;
        share_id = None;
        let after = read(&state, &http, provider, |driver| driver.list_shares())?;
        if after.iter().any(|item| item.id == share.id) { return Err(failure("cancelled share remains active")); }
        Ok(())
    })();
    if let Some(id) = share_id {
        let _ = mutate(&state, &http, provider, |driver| driver.cancel_shares(&[id.clone()]));
    }
    let cleanup = trash_and_delete(&state, &http, provider, &base.id, &name);
    if cleanup.is_ok() { eprintln!("{} share fixture cleanup: PASS", provider.key()); }
    result?;
    cleanup
}

fn run_requested_quark_upload() -> LiveResult<()> {
    require_live_authorization()?;
    let path = std::env::var_os("VERSADOWN_LIVE_QUARK_UPLOAD_PATH")
        .map(PathBuf::from)
        .ok_or_else(|| failure("VERSADOWN_LIVE_QUARK_UPLOAD_PATH is required"))?;
    let metadata = fs::metadata(&path)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| failure("requested upload filename is invalid"))?;
    let provider = CloudProvider::Quark;
    let http = LiveHttp::new()?;
    let root_id = ProviderRegistry::global().get(provider).root_directory_id;
    let store = SystemCredentialStore::new(session_root()?)?;
    let cookie = store
        .load(provider.key())?
        .ok_or_else(|| failure("Quark is not currently authorized"))?;
    let cookie = String::from_utf8(cookie)
        .map_err(|_| failure("stored Quark session is not valid UTF-8"))?;
    let client = QuarkClient::new(cookie, &http);

    client.account()?;
    let check_directory_name = unique_name(provider, "upload-check");
    let check_directory_response = client.mkdir(root_id, &check_directory_name)?;
    let check_directory_value: serde_json::Value =
        serde_json::from_str(&check_directory_response.body)?;
    let check_directory_id = check_directory_value
        .pointer("/data/fid")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| failure("Quark did not return the check directory id"))?;
    let root_listing = client.directory(root_id)?;
    if !root_listing
        .items
        .iter()
        .any(|item| item.id == check_directory_id)
    {
        return Err(failure("Quark did not list the created check directory"));
    }

    let uploaded = client.upload_file(check_directory_id, &path, &http)?;
    if uploaded.name != name || uploaded.size != Some(metadata.len()) {
        return Err(failure("requested upload returned mismatched metadata"));
    }
    let listed = client
        .directory(check_directory_id)?
        .items
        .into_iter()
        .find(|item| item.name == name && item.kind == "file")
        .ok_or_else(|| failure("Quark did not list the requested uploaded file"))?;
    if listed.size != Some(metadata.len()) {
        return Err(failure("requested upload listing returned mismatched size"));
    }
    eprintln!(
        "Quark requested upload: PASS (kept in {check_directory_name}/{name}, {} bytes)",
        metadata.len()
    );
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and the current user's Quark session"]
fn live_quark_complete_crud() -> LiveResult<()> {
    run_provider(CloudProvider::Quark)
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1, VERSADOWN_LIVE_QUARK_UPLOAD_PATH and the current user's Quark session"]
fn live_quark_requested_file_upload() -> LiveResult<()> {
    run_requested_quark_upload()
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1 and the current user's PikPak session"]
fn live_pikpak_complete_crud() -> LiveResult<()> {
    run_provider(CloudProvider::Pikpak)
}

#[test]
#[ignore = "permanently removes only VersaDown-E2E-quark-* test roots"]
fn live_cleanup_quark_e2e_residuals() -> LiveResult<()> {
    cleanup_provider_residuals(CloudProvider::Quark)
}

#[test]
#[ignore = "permanently removes only VersaDown-E2E-pikpak-* test roots"]
fn live_cleanup_pikpak_e2e_residuals() -> LiveResult<()> {
    cleanup_provider_residuals(CloudProvider::Pikpak)
}

#[test]
#[ignore = "read-only count of VersaDown-E2E-pikpak-* test roots"]
fn live_audit_pikpak_e2e_residuals() -> LiveResult<()> {
    let (active, recycle) = audit_provider_residuals(CloudProvider::Pikpak)?;
    eprintln!("PikPak E2E residuals: active={active}, recycle={recycle}");
    Ok(())
}

#[test]
#[ignore = "requires saved UC session; creates and removes only its own isolated fixture"]
fn live_reported_uc_share_lifecycle() -> LiveResult<()> {
    run_reported_share_lifecycle(CloudProvider::Uc)
}

#[test]
#[ignore = "requires saved Xunlei session; creates and removes only its own isolated fixture"]
fn live_reported_xunlei_share_lifecycle() -> LiveResult<()> {
    run_reported_share_lifecycle(CloudProvider::Xunlei)
}

#[test]
#[ignore = "requires saved Baidu session; creates and removes only its own isolated fixture"]
fn live_reported_baidu_share_lifecycle() -> LiveResult<()> {
    run_reported_share_lifecycle(CloudProvider::Baidu)
}
