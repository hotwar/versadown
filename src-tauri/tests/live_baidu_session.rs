//! Opt-in acceptance against one explicitly selected VD profile. Every write targets
//! synthetic data under a new UUID. No credentials or share links are reported.
use std::{io::Read, path::PathBuf, thread, time::Duration};
use versadown_lib::cloud::{
    baidu::{BaiduClient, BaiduSession, BAIDU_RECYCLE_BIN_ID},
    CloudDirectory, CloudFile, CloudShareOptions, CredentialStore, QuarkError, ReqwestTransport,
    SystemCredentialStore,
};

fn step<T>(label: &str, result: Result<T, QuarkError>) -> Result<T, String> {
    match result {
        Ok(value) => { eprintln!("[baidu-live] {label} ok"); Ok(value) }
        Err(error) => {
            let code = serde_json::to_value(&error).unwrap_or_default();
            Err(format!("{label}: {}", code["code"].as_str().unwrap_or("unknown")))
        }
    }
}

fn find_new(
    mut list: impl FnMut() -> Result<CloudDirectory, QuarkError>,
    name: &str,
    kind: &str,
) -> Result<CloudFile, String> {
    for wait in [0, 1, 2, 4] {
        thread::sleep(Duration::from_secs(wait));
        let matches: Vec<_> = step("list", list())?.items.into_iter()
            .filter(|item| item.name == name && item.kind == kind).collect();
        match matches.len() {
            0 => continue,
            1 => return Ok(matches.into_iter().next().unwrap()),
            _ => return Err("ambiguous fixture identity".into()),
        }
    }
    Err("fixture absent from listing".into())
}

#[test]
#[ignore = "uses a selected authorized account; creates, shares, restores and removes only this run's fixture"]
fn authorized_baidu_drive_lifecycle() -> Result<(), String> {
    if std::env::var("VERSADOWN_LIVE_BAIDU_FIXTURE").as_deref() != Ok("1") {
        return Err("explicit live fixture opt-in required".into());
    }
    let root = PathBuf::from(std::env::var_os("VERSADOWN_LIVE_BAIDU_UPLOAD_PROBE_DIR")
        .ok_or("an existing VD session directory is required")?);
    if !root.is_absolute() || !root.ends_with("cloud-sessions") || !root.is_dir() {
        return Err("invalid session directory".into());
    }
    let store = step("open profile", SystemCredentialStore::new(root))?;
    let bytes = zeroize::Zeroizing::new(step("read saved session", store.load("baidu"))?.ok_or("no saved session")?);
    let session: BaiduSession = serde_json::from_slice(&bytes).map_err(|_| "invalid saved session")?;
    let http = step("start transport", ReqwestTransport::new())?;
    let client = BaiduClient::new(session.clone(), &http);
    // Read-only follow-up for a fixture for which Baidu requested human verification.
    if let Some(fixture) = std::env::var_os("VERSADOWN_LIVE_BAIDU_CHECK_RECYCLED_FIXTURE") {
        let fixture = fixture.to_str().ok_or("invalid fixture name")?;
        let id = fixture.strip_prefix("VD-session-test-").ok_or("invalid fixture name")?;
        if uuid::Uuid::parse_str(id).ok().map(|id| id.simple().to_string()).as_deref() != Some(id) {
            return Err("invalid fixture name".into());
        }
        step("account after verification challenge", client.account())?;
        if step("root after verification challenge", client.directory("/"))?.items.iter().any(|item| item.name == fixture) {
            return Err("fixture unexpectedly present in root".into());
        }
        let recycled = find_new(|| client.list_recycle_bin(), fixture, "folder")?;
        if recycled.parent_id != BAIDU_RECYCLE_BIN_ID { return Err("invalid recycle entry".into()); }
        eprintln!("[baidu-live] exact fixture confirmed in recycle bin and absent from root");
        return Ok(());
    }
    let name = format!("VD-session-test-{}", uuid::Uuid::new_v4().simple());
    let root_dir = step("cold root directory", client.directory("/"))?;
    if root_dir.items.iter().any(|item| item.name == name) { return Err("fixture name already exists".into()); }
    let owner = step("account", client.account())?;
    let mut errors = Vec::new();
    errors.extend(step("capacity", client.storage_capacity()).err());
    errors.extend(step("share listing", client.list_shares()).err());
    errors.extend(step("recycle listing", client.list_recycle_bin()).err());
    eprintln!("[baidu-live] fixture={name}");
    step("create fixture", client.mkdir("/", &name))?;
    let base = find_new(|| client.directory("/"), &name, "folder")?;
    let io_result = (|| {
        step("create child directory", client.mkdir(&base.id, "child"))?;
        let child = find_new(|| client.directory(&base.id), "child", "folder")?;
        let temp = tempfile::tempdir().map_err(|_| "create local fixture")?;
        let payload = format!("VD synthetic test {name}\n");
        let local = temp.path().join("payload.txt");
        std::fs::write(&local, &payload).map_err(|_| "write local fixture")?;
        // A fresh transport using saved credentials exercises first-action upload.
        let cold_http = step("restart transport", ReqwestTransport::new())?;
        let cold = BaiduClient::new(session.clone(), &cold_http);
        let uploaded = step("cold upload", cold.upload_file(&child.id, &local, &cold_http))?;
        if step("same account after restart", cold.account())?.id != owner.id { return Err("account changed".into()); }
        if find_new(|| client.directory(&child.id), "payload.txt", "file")?.id != uploaded.id {
            return Err("uploaded identity does not match nested listing".into());
        }
        let download = step("resolve download", client.download(&uploaded.id))?;
        let url = url::Url::parse(&download.url).map_err(|_| "invalid download address")?;
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30))
            .build().map_err(|_| "download client")?;
        let mut request = http.get(url);
        for (key, value) in download.headers {
            // A signed location can be served by a CDN. Never attach account cookies to it.
            if key.eq_ignore_ascii_case("User-Agent") { request = request.header(key, value); }
        }
        let mut body = Vec::new();
        request.send().and_then(|r| r.error_for_status()).map_err(|_| "download rejected")?
            .take(payload.len() as u64 + 1).read_to_end(&mut body).map_err(|_| "read download")?;
        if body != payload.as_bytes() { return Err("downloaded fixture mismatch".into()); }
        eprintln!("[baidu-live] downloaded bytes verified");
        step("rename", client.rename(&uploaded.id, "renamed.txt"))?;
        let renamed = find_new(|| client.directory(&child.id), "renamed.txt", "file")?;
        step("copy", client.copy_files(std::slice::from_ref(&renamed.id), &base.id))?;
        find_new(|| client.directory(&base.id), "renamed.txt", "file")?;
        step("rename before move", client.rename(&renamed.id, "moved.txt"))?;
        let moving = find_new(|| client.directory(&child.id), "moved.txt", "file")?;
        step("move", client.move_files(std::slice::from_ref(&moving.id), &base.id))?;
        find_new(|| client.directory(&base.id), "moved.txt", "file")?;
        Ok::<(), String>(())
    })();
    let share_result = (|| {
        let share = step("create protected one-day fixture share", client.create_share(
            std::slice::from_ref(&base.id),
            &CloudShareOptions { name: name.clone(), pass_code: None, generate_pass_code: true, expires_in_days: Some(1) },
        ))?;
        let listed = step("list new share", client.list_shares())
            .map(|rows| rows.iter().any(|row| row.id == share.id));
        let cancelled = step("cancel fixture share", client.cancel_shares(std::slice::from_ref(&share.id)));
        // A lost response can follow a successful cancellation. Only read back the exact share.
        let still_listed = step("check cancelled share", client.list_shares())?
            .iter().any(|row| row.id == share.id);
        if still_listed {
            return Err(cancelled.err().unwrap_or_else(|| "cancelled share still listed".into()));
        }
        if !listed? { return Err("new share absent from listing".to_owned()); }
        Ok(())
    })();
    let recycle_result = (|| {
        step("trash fixture", client.trash(std::slice::from_ref(&base.id)))?;
        let trashed = find_new(|| client.list_recycle_bin(), &name, "folder")?;
        if trashed.parent_id != BAIDU_RECYCLE_BIN_ID { return Err("unexpected recycle entry".to_owned()); }
        step("restore fixture", client.restore_recycle_entries(std::slice::from_ref(&trashed.id)))?;
        let restored = find_new(|| client.directory("/"), &name, "folder")?;
        step("trash restored fixture", client.trash(std::slice::from_ref(&restored.id)))?;
        let trash = find_new(|| client.list_recycle_bin(), &name, "folder")?;
        step("permanently delete fixture", client.delete_recycle_entries(&[trash.id]))?;
        Ok(())
    })();
    errors.extend([io_result, share_result, recycle_result].into_iter().filter_map(Result::err));
    if errors.is_empty() { Ok(()) } else { Err(errors.join(" | ")) }
}
