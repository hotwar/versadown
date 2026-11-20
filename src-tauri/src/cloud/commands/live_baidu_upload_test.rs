//! Explicit live acceptance of the same batch, progress and session path used by the native picker.
use super::*;
use std::io::Read;

#[test]
#[ignore = "writes one unique synthetic file using an explicitly selected, logged-in profile"]
fn desktop_baidu_upload() -> Result<(), String> {
    if std::env::var("VERSADOWN_LIVE_BAIDU_FIXTURE").as_deref() != Ok("1") {
        return Err("explicit live fixture opt-in required".into());
    }
    let profile = PathBuf::from(std::env::var_os("VERSADOWN_LIVE_BAIDU_UPLOAD_PROBE_DIR")
        .ok_or("select an existing cloud-sessions directory")?);
    if !profile.is_absolute() || !profile.ends_with("cloud-sessions") || !profile.is_dir() {
        return Err("invalid session directory".into());
    }
    let provider = CloudProvider::Baidu;
    let state = Arc::new(CloudState::new(profile).map_err(|_| "open desktop profile")?);
    let session = load_provider_session(provider, &state.store).map_err(|_| "load selected Baidu account")?;
    let client = baidu::BaiduClient::new(session, &state.http);
    let tmp = tempfile::tempdir().map_err(|_| "create fixture directory")?;
    let id = uuid::Uuid::new_v4();
    let name = format!("VD-desktop-upload-{id}.bin");
    let source = tmp.path().join(&name);
    let bytes = id.as_bytes().repeat(65_537);
    std::fs::write(&source, &bytes).map_err(|_| "write local fixture")?;
    // Opening Baidu's drive in the desktop UI displays the account and lists the parent.
    client.account().map_err(|_| "open account")?;
    let list = || list_cloud_directory(provider, &state.store, &state.http, "/").map_err(|_| "list root");
    if list()?.items.iter().any(|entry| entry.name == name) {
        return Err("fixture name already exists".into());
    }
    let result = tauri::async_runtime::block_on(run_upload_session(
        provider, id.to_string(), Channel::new(|_| Ok(())), |tracker, control| {
            upload_selected_paths(
                state.clone(), ProviderRegistry::global().get(provider), provider, "/".into(),
                vec![source], None, tracker, control, Arc::new(crate::bandwidth::BandwidthLimiter::new(0)),
            )
        },
    )).map_err(|error| format!("desktop batch: {}", upload_error_code(&error)))?;
    if let Some(failure) = result.failures.first() {
        return Err(format!("desktop file upload: {}", failure.code));
    }
    let [uploaded] = result.uploaded.as_slice() else { return Err("expected one uploaded file".into()); };
    if result.cancelled || uploaded.name != name || uploaded.size != Some(bytes.len() as u64) {
        return Err("unexpected uploaded fixture metadata".into());
    }
    // Bind cleanup to the returned ID and this run's name before touching the remote file.
    if !list()?.items.iter().any(|entry| entry.id == uploaded.id && entry.name == name) {
        return Err("fixture identity not confirmed by fresh listing".into());
    }
    let check_bytes = (|| {
        let download = client.download(&uploaded.id).map_err(|_| "resolve fixture download")?;
        let http = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(30))
            .build().map_err(|_| "initialize download")?;
        let mut request = http.get(&download.url);
        for (key, value) in download.headers {
            if key.eq_ignore_ascii_case("User-Agent") { request = request.header(key, value); }
        }
        let mut received = Vec::new();
        request.send().and_then(|r| r.error_for_status()).map_err(|_| "download fixture")?
            .take(bytes.len() as u64 + 1).read_to_end(&mut received).map_err(|_| "read fixture")?;
        if received != bytes { return Err("fixture byte comparison failed"); }
        Ok(())
    })();
    match check_bytes {
        Ok(()) => eprintln!("[baidu-live] downloaded bytes match the local fixture"),
        Err(stage) => eprintln!("[baidu-live] downloaded byte check failed: {stage}"),
    }
    client.trash(std::slice::from_ref(&uploaded.id)).map_err(|_| "recycle fixture")?;
    let mut removed = false;
    for delay in [0, 1, 2, 4] {
        std::thread::sleep(std::time::Duration::from_secs(delay));
        if !list()?.items.iter().any(|entry| entry.id == uploaded.id) {
            removed = true;
            break;
        }
    }
    if !removed {
        return Err("fixture still present after recycle".into());
    }
    check_bytes?;
    eprintln!("[baidu-live] desktop upload, byte comparison and recycle passed ({} bytes)", bytes.len());
    Ok(())
}
