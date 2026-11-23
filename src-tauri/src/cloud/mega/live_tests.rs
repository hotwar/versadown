//! Opt-in end-to-end checks against the real MEGA service, driving the real
//! pipeline: adapter -> temporary URL -> range gateway (decrypting) -> bundled
//! aria2 -> MAC verification of the finished file.
//!
//! Run with public, permissively licensed links (never committed):
//! `MEGA_TEST_FILE_LINK=... MEGA_TEST_FOLDER_LINK=... cargo test --lib
//! mega::live_tests -- --ignored --nocapture --test-threads=1`.
//! `MEGA_TEST_FOLDER_FILE` optionally names the folder file to download
//! (default: the smallest). Links and keys are never printed.
//!
//! `live_public_links_account_quota_vs_anonymous` also needs the app's saved
//! (free test) MEGA session, which is only read:
//! `VERSADOWN_LIVE_CLOUD=1 VERSADOWN_LIVE_SESSION_ROOT=~/.local/share/com.versadown.desktop`.
//! It downloads the same links anonymously and with the session, verifies the
//! MAC of every file, and compares the account's transfer counters (`uq`)
//! around each phase. The session id is never printed.

use super::adapter::MegaShareAdapter;
use super::account::MegaSession;
use super::failure::MegaFailure;
use super::verify_downloaded_file;
use crate::cloud::range_gateway::RangeGateway;
use crate::cloud::share::{
    CloudShareEntry, CloudSharePlan, CloudShareResolvedEntry, ShareInspect, ShareInspection,
    ShareResolution, ShareResolve,
};
use crate::cloud::{
    CloudHttpRequest, CloudHttpTransport, CloudProvider, CredentialStore, MemoryCredentialStore,
    QuarkError, ReqwestTransport, SystemCredentialStore,
};
use crate::parser::CloudShareCandidate;
use crate::{DownloadService, NewTask, TaskStore, UriSubmissionOptions};
use crate::aria2::{Aria2Api, Aria2LaunchConfig, Aria2Process};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

fn link(variable: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| panic!("{variable} is not set"))
}

fn inspect(http: &dyn CloudHttpTransport, url: String) -> CloudSharePlan {
    let credentials = MemoryCredentialStore::default();
    let adapter = MegaShareAdapter {
        credentials: &credentials,
        http,
        account_quota: false,
    };
    let candidate = CloudShareCandidate {
        provider: CloudProvider::Mega,
        url,
        access_code: None,
    };
    match adapter.inspect(&candidate).expect("inspect") {
        ShareInspection::Plan(plan) => plan,
        ShareInspection::AccessCodeRequired => panic!("MEGA never asks for an access code"),
    }
}

/// The app's credential store opened for reading only.
struct ReadOnlyStore(SystemCredentialStore);

impl CredentialStore for ReadOnlyStore {
    fn save(&self, _: &str, _: &[u8]) -> Result<(), QuarkError> {
        Err(QuarkError::Storage)
    }
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, QuarkError> {
        self.0.load(key)
    }
    fn remove(&self, _: &str) -> Result<(), QuarkError> {
        Err(QuarkError::Storage)
    }
}

/// Resolve, serve through the gateway, download with aria2, return the file.
async fn download(
    gateway: &RangeGateway,
    plan: &CloudSharePlan,
    entry: &CloudShareEntry,
    connections: u8,
) -> (PathBuf, tempfile::TempDir) {
    download_with(gateway, plan, entry, connections, None, false).await
}

/// Like [`download`]; with a store the URL is requested with its session.
async fn download_with(
    gateway: &RangeGateway,
    plan: &CloudSharePlan,
    entry: &CloudShareEntry,
    connections: u8,
    session_store: Option<Arc<ReadOnlyStore>>,
    expect_account_quota: bool,
) -> (PathBuf, tempfile::TempDir) {
    let resolved = CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        plan: plan.clone(),
        entry: entry.clone(),
    };
    let wants_account = expect_account_quota;
    let resolution = {
        let resolved = resolved.clone();
        tokio::task::spawn_blocking(move || {
            let http = ReqwestTransport::new().unwrap();
            let anonymous = MemoryCredentialStore::default();
            let credentials: &dyn CredentialStore = match &session_store {
                Some(store) => store.as_ref(),
                None => &anonymous,
            };
            MegaShareAdapter {
                credentials,
                http: &http,
                account_quota: session_store.is_some(),
            }
            .resolve(&resolved)
        })
        .await
        .unwrap()
        .expect("resolve")
    };
    let ShareResolution::Direct(download) = resolution else {
        panic!("MEGA resolves directly")
    };
    assert!(download.url.starts_with("https://"), "ssl:2 must yield https");
    // A requested session must have been accepted, not silently dropped.
    assert_eq!(
        download.transform.as_ref().unwrap().uses_account_quota(),
        wants_account,
        "the URL was issued against the wrong quota"
    );
    let registration = gateway
        .register_transformed(
            Uuid::new_v4(),
            download.url,
            vec![],
            None,
            download.transform.expect("transform"),
            false,
        )
        .unwrap();
    let local_url = registration.url().to_owned();

    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store.create(NewTask::direct(&local_url)).unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = crate::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("mega-live-{}", aria2.rpc_port);
    drop(reserved);
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = crate::engine_binary::EngineBinary::for_current_target()
        .unwrap()
        .resolve(&manifest, &manifest)
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("downloads");
    let (mut process, client) = Aria2Process::start(Aria2LaunchConfig {
        binary,
        data_dir: root.path().join("engine"),
        download_dir: output.clone(),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    })
    .await
    .unwrap();
    let service = DownloadService::new(Arc::new(client.clone()), store);
    let submitted = service
        .submit_uri_with_options(
            task.id,
            vec![local_url],
            UriSubmissionOptions {
                directory: Some(output.clone()),
                output_name: Some("download.bin".into()),
                max_connections_per_server: Some(connections),
                max_tries: Some(1),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let (state, message) = loop {
        let status = client
            .tell_status(submitted.aria_gid.as_deref().unwrap())
            .await
            .unwrap();
        if matches!(status.status.as_str(), "error" | "complete") {
            break (status.status, status.error_message);
        }
        assert!(
            started.elapsed() < Duration::from_secs(900),
            "transfer timed out"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    process.terminate_fallback().unwrap();
    assert_eq!(state, "complete", "aria2: {message:?}");
    (output.join("download.bin"), root)
}

fn verified(resolved: &CloudShareResolvedEntry, path: &std::path::Path) {
    let length = std::fs::metadata(path).unwrap().len();
    eprintln!("downloaded {length} bytes");
    verify_downloaded_file(resolved, path).expect("MAC verification of the real download");
    // The same file with one flipped byte must be rejected.
    let tampered = path.with_extension("tampered");
    std::fs::copy(path, &tampered).unwrap();
    let mut file = std::fs::OpenOptions::new().write(true).open(&tampered).unwrap();
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(length / 2)).unwrap();
    file.write_all(&[!std::fs::read(path).unwrap()[(length / 2) as usize]])
        .unwrap();
    drop(file);
    assert!(matches!(
        verify_downloaded_file(resolved, &tampered),
        Err(QuarkError::Mega(MegaFailure::IntegrityFailed))
    ));
    eprintln!("MAC verification passed; tampered copy rejected");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs network access and MEGA_TEST_FILE_LINK"]
async fn live_single_file_downloads_and_passes_the_mac() {
    let url = link("MEGA_TEST_FILE_LINK");
    let plan = {
        let url = url.clone();
        tokio::task::spawn_blocking(move || inspect(&ReqwestTransport::new().unwrap(), url))
            .await
            .unwrap()
    };
    let entry = plan.entries[0].clone();
    eprintln!("file: {} ({:?} bytes)", entry.name, entry.size);
    let gateway = RangeGateway::start().await.unwrap();
    let (path, _root) = download(&gateway, &plan, &entry, 4).await;
    let resolved = CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        plan,
        entry,
    };
    verified(&resolved, &path);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs network access and MEGA_TEST_FOLDER_LINK"]
async fn live_folder_lists_entries_and_downloads_one_file() {
    let url = link("MEGA_TEST_FOLDER_LINK");
    let plan = tokio::task::spawn_blocking(move || inspect(&ReqwestTransport::new().unwrap(), url))
        .await
        .unwrap();
    eprintln!("folder entries: {}", plan.entries.len());
    for entry in plan.entries.iter().take(30) {
        eprintln!(
            "  {}/{} ({:?} bytes)",
            entry.relative_directory.join("/"),
            entry.name,
            entry.size
        );
    }
    let wanted = std::env::var("MEGA_TEST_FOLDER_FILE").ok();
    let entry = match wanted {
        Some(name) => plan.entries.iter().find(|entry| entry.name == name),
        None => plan
            .entries
            .iter()
            .filter(|entry| entry.size.is_some_and(|size| size > 0))
            .min_by_key(|entry| entry.size),
    }
    .expect("a file to download")
    .clone();
    eprintln!("downloading: {} ({:?} bytes)", entry.name, entry.size);
    let gateway = RangeGateway::start().await.unwrap();
    let (path, _root) = download(&gateway, &plan, &entry, 4).await;
    let resolved = CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        plan,
        entry,
    };
    verified(&resolved, &path);
}

/// The numeric transfer counters of the account (`uq` with `xfer`). Only
/// numbers are kept: nothing else of the reply is printed. The quota bucket
/// array `tah` (bytes per hour, free IP-based quota) is summed into `tah_sum`.
async fn transfer_counters(session: &MegaSession) -> std::collections::BTreeMap<String, i64> {
    let session = session.clone();
    tokio::task::spawn_blocking(move || transfer_counters_blocking(&session))
        .await
        .unwrap()
}

fn transfer_counters_blocking(session: &MegaSession) -> std::collections::BTreeMap<String, i64> {
    let http = ReqwestTransport::new().unwrap();
    let response = http
        .send(CloudHttpRequest {
            method: "POST",
            url: format!(
                "https://g.api.mega.co.nz/cs?id={}&sid={}",
                std::process::id(),
                crate::cloud::types::encoded(session.sid())
            ),
            headers: std::collections::BTreeMap::from([(
                "Content-Type".into(),
                "application/json".into(),
            )]),
            body: Some(r#"[{"a":"uq","xfer":1,"pro":1}]"#.into()),
        })
        .expect("uq request");
    let reply: serde_json::Value = serde_json::from_str(&response.body).expect("uq reply");
    let object = reply[0].as_object().unwrap_or_else(|| panic!("uq answered {}", reply[0]));
    let mut counters: std::collections::BTreeMap<String, i64> = object
        .iter()
        .filter_map(|(key, value)| Some((key.clone(), value.as_i64()?)))
        .collect();
    if let Some(buckets) = object.get("tah").and_then(|value| value.as_array()) {
        counters.insert("tah_len".into(), buckets.len() as i64);
        counters.insert(
            "tah_sum".into(),
            buckets.iter().filter_map(|bucket| bucket.as_i64()).sum(),
        );
    }
    counters
}

fn print_counters(label: &str, counters: &std::collections::BTreeMap<String, i64>) {
    let line = counters
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("uq {label}: {line}");
}

async fn download_everything(
    label: &str,
    file_plan: &CloudSharePlan,
    folder_plan: &CloudSharePlan,
    session_store: Option<Arc<ReadOnlyStore>>,
    expect_account_quota: bool,
) {
    let gateway = RangeGateway::start().await.unwrap();
    let targets = file_plan
        .entries
        .iter()
        .map(|entry| (file_plan, entry))
        .chain(folder_plan.entries.iter().map(|entry| (folder_plan, entry)));
    for (plan, entry) in targets {
        let (path, _root) =
            download_with(
            &gateway,
            plan,
            entry,
            4,
            session_store.clone(),
            expect_account_quota,
        )
        .await;
        let resolved = CloudShareResolvedEntry {
            provider: CloudProvider::Mega,
            plan: plan.clone(),
            entry: entry.clone(),
        };
        eprintln!("[{label}] {} ({:?} bytes)", entry.name, entry.size);
        verified(&resolved, &path);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs network access, MEGA_TEST_FILE_LINK, MEGA_TEST_FOLDER_LINK, VERSADOWN_LIVE_CLOUD=1 and VERSADOWN_LIVE_SESSION_ROOT"]
async fn live_public_links_account_quota_vs_anonymous() {
    assert_eq!(std::env::var("VERSADOWN_LIVE_CLOUD").as_deref(), Ok("1"));
    let root = PathBuf::from(std::env::var_os("VERSADOWN_LIVE_SESSION_ROOT").unwrap());
    let store = Arc::new(ReadOnlyStore(
        SystemCredentialStore::new(root.join("cloud-sessions")).unwrap(),
    ));
    let session = MegaSession::load(store.as_ref()).expect("the app's MEGA session");
    let (file_url, folder_url) = (link("MEGA_TEST_FILE_LINK"), link("MEGA_TEST_FOLDER_LINK"));
    let (file_plan, folder_plan) = tokio::task::spawn_blocking(move || {
        let http = ReqwestTransport::new().unwrap();
        (inspect(&http, file_url), inspect(&http, folder_url))
    })
    .await
    .unwrap();

    let before = transfer_counters(&session).await;
    print_counters("before anonymous downloads", &before);
    download_everything("anonymous", &file_plan, &folder_plan, None, false).await;
    let after_anonymous = transfer_counters(&session).await;
    print_counters("after anonymous downloads", &after_anonymous);

    // The session is carried only when the account is a paid one: a free test
    // account must be judged free and take the anonymous path.
    let paid = {
        let session = session.clone();
        tokio::task::spawn_blocking(move || {
            super::account::session_is_paid(&ReqwestTransport::new().unwrap(), &session).unwrap()
        })
        .await
        .unwrap()
    };
    eprintln!("account judged paid: {paid}");
    download_everything(
        "with account session enabled",
        &file_plan,
        &folder_plan,
        Some(Arc::clone(&store)),
        paid,
    )
    .await;
    let after_account = transfer_counters(&session).await;
    print_counters("after account downloads", &after_account);

    for (key, value) in &after_account {
        let anonymous = after_anonymous.get(key).copied().unwrap_or(0);
        let initial = before.get(key).copied().unwrap_or(0);
        if *value != anonymous || anonymous != initial {
            eprintln!(
                "counter {key}: start {initial} -> after anonymous {anonymous} -> after account {value}"
            );
        }
    }
}
