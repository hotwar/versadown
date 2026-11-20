//! Live acceptance of the native desktop upload backend after file selection.
//! Set VERSADOWN_LIVE_BAIDU_FIXTURE=1 and select the account profile explicitly.
//! Portable mode takes VERSADOWN_RUN_ROOT and optionally cross-checks VERSADOWN_LIVE_BAIDU_UPLOAD_PROBE_DIR.
//! With VERSADOWN_RUN_ROOT unset, VERSADOWN_LIVE_BAIDU_UPLOAD_PROBE_DIR selects the profile directly.
use super::*;
use std::io::Read;

fn live_upload_profile(selected: Option<&Path>, run_root: Option<&Path>) -> Result<(PathBuf, &'static str), &'static str> {
    if let Some(profile) = selected {
        if !profile.is_absolute() || !profile.ends_with("cloud-sessions") || !profile.is_dir() {
            return Err("select an existing absolute cloud-sessions directory");
        }
    }
    let Some(run_root) = run_root else {
        // Windows credential namespaces use the stored path spelling.
        return selected.map(|profile| (profile.to_owned(), "selected-profile"))
            .ok_or("select a session directory or a portable run root");
    };
    if !run_root.is_absolute() || !run_root.is_dir() {
        return Err("VERSADOWN_RUN_ROOT must be an existing absolute directory");
    }
    // Match the portable host: canonicalize the root before appending the data subdirectories.
    let expected = run_root.canonicalize().map_err(|_| "resolve portable run root")?
        .join("data").join("cloud-sessions");
    if !expected.is_dir() {
        return Err("the portable run root has no existing cloud-sessions directory");
    }
    if let Some(profile) = selected {
        if profile.canonicalize().map_err(|_| "resolve selected profile")?
            != expected.canonicalize().map_err(|_| "resolve portable profile")?
        {
            return Err("selected profile differs from VERSADOWN_RUN_ROOT/data/cloud-sessions");
        }
    }
    Ok((expected, "portable"))
}

#[test]
fn desktop_baidu_upload_profile_selection_matches_the_portable_profile() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("quick");
    let profile = root.join("data").join("cloud-sessions");
    std::fs::create_dir_all(&profile).unwrap();
    let expected = (root.canonicalize().unwrap().join("data").join("cloud-sessions"), "portable");
    assert_eq!(live_upload_profile(None, Some(&root)).unwrap(), expected);
    assert_eq!(live_upload_profile(Some(&root.join("data/./cloud-sessions")), Some(&root)).unwrap(), expected);
    assert_eq!(live_upload_profile(Some(&profile), None).unwrap(), (profile, "selected-profile"));
}

#[test]
fn desktop_baidu_upload_profile_selection_rejects_other_or_missing_roots_before_opening_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("quick");
    let regular = temp.path().join("regular").join("cloud-sessions");
    std::fs::create_dir_all(root.join("data/cloud-sessions")).unwrap();
    std::fs::create_dir_all(&regular).unwrap();
    assert!(live_upload_profile(Some(&regular), Some(&root)).is_err());
    for invalid in [temp.path(), Path::new(""), Path::new("relative")] {
        assert!(live_upload_profile(Some(&regular), Some(invalid)).is_err());
    }
    assert!(live_upload_profile(Some(Path::new("cloud-sessions")), None).is_err());
    assert!(live_upload_profile(None, None).is_err());
}

#[test]
#[ignore = "writes one unique synthetic file using an explicitly selected, logged-in profile"]
fn desktop_baidu_upload() -> Result<(), String> {
    if std::env::var("VERSADOWN_LIVE_BAIDU_FIXTURE").as_deref() != Ok("1") {
        return Err("explicit live fixture opt-in required".into());
    }
    let selected = std::env::var_os("VERSADOWN_LIVE_BAIDU_UPLOAD_PROBE_DIR").map(PathBuf::from);
    let run_root = std::env::var_os("VERSADOWN_RUN_ROOT").map(PathBuf::from);
    let (profile, profile_mode) = live_upload_profile(selected.as_deref(), run_root.as_deref())?;
    eprintln!("[baidu-live] native desktop upload backend after file selection, profile={profile_mode}");
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
    eprintln!("[baidu-live] native desktop upload backend after file selection, profile={profile_mode}: upload, byte comparison and recycle passed ({} bytes)", bytes.len());
    Ok(())
}
