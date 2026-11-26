//! Opt-in, strictly READ-ONLY live checks of the MEGA account integration,
//! driven through the product code (registry driver, range gateway, aria2).
//!
//! Run (single thread, the listing is one heavy call):
//! `VERSADOWN_LIVE_CLOUD=1 VERSADOWN_LIVE_SESSION_ROOT=~/.local/share/com.versadown.desktop \
//!  cargo test --test live_mega_account -- --ignored --nocapture --test-threads=1`
//!
//! Safety: every API request goes through [`GuardedHttp`], which refuses any
//! command other than `ug` / `uq` / `f` / `g` (user info, quota, node listing,
//! download URL). The credential store is only ever read. Names, handles,
//! session ids and keys are never printed; only counts are.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::{BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use uuid::Uuid;
use versadown_lib::aria2::{Aria2Api, Aria2LaunchConfig, Aria2Process};
use versadown_lib::cloud::{
    commands::plan_cloud_directory_download,
    driver::ProviderDriver,
    mega::account::MegaSession,
    mega::crypto::{decode_base64url, decrypt_ecb, verify_file_mac, FileKey},
    range_gateway::RangeGateway,
    CloudError, CloudFile, CloudFileKind, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport,
    CloudProvider, CloudShareOptions, CloudStorageCapacity, ProviderRegistry,
    ReqwestTransport, SystemCredentialStore,
};
use versadown_lib::{DownloadService, NewTask, TaskStore, UriSubmissionOptions};

type LiveResult<T> = Result<T, String>;

const API_GATEWAY: &str = "https://g.api.mega.co.nz/cs";
/// Commands the guard lets through: user info, quota, node listing, download URL.
const READ_ONLY_COMMANDS: [&str; 4] = ["ug", "uq", "f", "g"];
const MAX_TRANSFER_BYTES: u64 = 20_000_000;
const SCRATCH: &str = "/tmp/cloud-squad/mega-live";

// --------------------------------------------------------------------------
// Read-only transport guard
// --------------------------------------------------------------------------

struct GuardedHttp {
    inner: ReqwestTransport,
    sent: Mutex<BTreeMap<String, usize>>,
    refused: Mutex<Vec<String>>,
}

impl GuardedHttp {
    fn sent_count(&self) -> usize {
        self.sent.lock().unwrap().values().sum()
    }
    fn refused_count(&self) -> usize {
        self.refused.lock().unwrap().len()
    }
}

impl CloudHttpTransport for GuardedHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
        let command = request
            .body
            .as_deref()
            .and_then(|body| serde_json::from_str::<Value>(body).ok())
            .and_then(|value| value.get(0)?.get("a")?.as_str().map(str::to_owned));
        match command {
            Some(command)
                if READ_ONLY_COMMANDS.contains(&command.as_str())
                    && request.url.starts_with(API_GATEWAY) =>
            {
                *self.sent.lock().unwrap().entry(command).or_default() += 1;
                self.inner.send(request)
            }
            other => {
                self.refused
                    .lock()
                    .unwrap()
                    .push(other.unwrap_or_else(|| "<none>".into()));
                Err(CloudError::Provider("read-only guard refused a request".into()))
            }
        }
    }
    fn manages_cookie_session(&self) -> bool {
        self.inner.manages_cookie_session()
    }
    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, CloudError> {
        self.inner.cookies_for_url(url)
    }
    fn isolated_session(&self) -> Result<Option<Box<dyn CloudHttpTransport>>, CloudError> {
        self.inner.isolated_session()
    }
}

// --------------------------------------------------------------------------
// Context and snapshot
// --------------------------------------------------------------------------

struct Ctx {
    store: SystemCredentialStore,
    http: GuardedHttp,
}

fn ctx() -> LiveResult<&'static Ctx> {
    static CTX: OnceLock<Ctx> = OnceLock::new();
    if std::env::var("VERSADOWN_LIVE_CLOUD").as_deref() != Ok("1") {
        return Err("live cloud regression requires VERSADOWN_LIVE_CLOUD=1".into());
    }
    if let Some(ctx) = CTX.get() {
        return Ok(ctx);
    }
    let root = std::env::var_os("VERSADOWN_LIVE_SESSION_ROOT")
        .ok_or("VERSADOWN_LIVE_SESSION_ROOT is required")?;
    let store = SystemCredentialStore::new(PathBuf::from(root).join("cloud-sessions"))
        .map_err(|error| error.to_string())?;
    let http = GuardedHttp {
        inner: ReqwestTransport::new().map_err(|error| error.to_string())?,
        sent: Mutex::default(),
        refused: Mutex::default(),
    };
    Ok(CTX.get_or_init(|| Ctx { store, http }))
}

fn driver(ctx: &'static Ctx) -> ProviderDriver<'static> {
    ProviderRegistry::global().driver(CloudProvider::Mega, &ctx.store, &ctx.http)
}

fn root_id() -> &'static str {
    ProviderRegistry::global()
        .get(CloudProvider::Mega)
        .root_directory_id
}

struct Entry {
    file: CloudFile,
    depth: usize,
}

struct Snapshot {
    /// Everything the product driver can list below the drive root.
    entries: Vec<Entry>,
    /// Children count the driver returned per directory id (root = `root_id()`).
    listed_children: HashMap<String, usize>,
    /// The raw `{"a":"f","c":1}` reply, fetched separately for cross-checking.
    raw: Value,
    user_handle: String,
}

fn raw_listing(ctx: &'static Ctx, session: &MegaSession) -> LiveResult<Value> {
    let request = CloudHttpRequest {
        method: "POST",
        url: format!("{API_GATEWAY}?id={}&sid={}", 7_000_000_000u64 + 1, session.sid()),
        headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
        body: Some(serde_json::json!([{"a": "f", "c": 1}]).to_string()),
    };
    let response = ctx.http.send(request).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
    match value {
        Value::Array(mut items) if items.len() == 1 => Ok(items.remove(0)),
        _ => Err("unexpected raw listing shape".into()),
    }
}

fn snapshot() -> LiveResult<&'static Snapshot> {
    static SNAPSHOT: OnceLock<Result<Snapshot, String>> = OnceLock::new();
    SNAPSHOT
        .get_or_init(|| {
            let ctx = ctx()?;
            let session = MegaSession::load(&ctx.store).map_err(|e| e.to_string())?;
            let mut driver = driver(ctx);
            let mut entries = Vec::new();
            let mut listed_children = HashMap::new();
            let mut queue = vec![(root_id().to_owned(), 0usize)];
            while let Some((id, depth)) = queue.pop() {
                let directory = driver.list_directory(&id).map_err(|e| e.to_string())?;
                listed_children.insert(id.clone(), directory.items.len());
                for file in directory.items {
                    if file.kind == CloudFileKind::Folder {
                        queue.push((file.id.clone(), depth + 1));
                    }
                    entries.push(Entry { file, depth });
                }
            }
            let raw = raw_listing(ctx, &session)?;
            Ok(Snapshot {
                entries,
                listed_children,
                raw,
                user_handle: session.user_handle().to_owned(),
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn raw_nodes(snapshot: &Snapshot) -> &[Value] {
    snapshot.raw["f"].as_array().map_or(&[], Vec::as_slice)
}

fn is_file_or_folder(node: &Value) -> bool {
    matches!(node["t"].as_u64(), Some(0 | 1))
}

fn raw_children<'a>(snapshot: &'a Snapshot) -> HashMap<&'a str, Vec<&'a Value>> {
    let mut map: HashMap<&str, Vec<&Value>> = HashMap::new();
    for node in raw_nodes(snapshot) {
        if let Some(parent) = node["p"].as_str() {
            map.entry(parent).or_default().push(node);
        }
    }
    map
}

fn raw_drive_root(snapshot: &Snapshot) -> LiveResult<&str> {
    raw_nodes(snapshot)
        .iter()
        .find(|node| node["t"].as_u64() == Some(2))
        .and_then(|node| node["h"].as_str())
        .ok_or_else(|| "no cloud-drive root in the raw listing".into())
}

/// Raw handle of each driver id (the registry root id maps to the drive root).
fn raw_handle<'a>(snapshot: &'a Snapshot, id: &'a str) -> LiveResult<&'a str> {
    if id == root_id() {
        raw_drive_root(snapshot)
    } else {
        Ok(id)
    }
}

/// A data-dependent check with nothing to check. Loudly NOT a pass: set
/// `VERSADOWN_LIVE_REQUIRE_DATA=1` to turn this into a failure.
fn skip_without_data(item: &str, reason: &str) {
    eprintln!("SKIPPED {item}: {reason}; nothing was verified");
    assert!(
        std::env::var("VERSADOWN_LIVE_REQUIRE_DATA").as_deref() != Ok("1"),
        "item {item} needs data: {reason}"
    );
}

fn name_problem(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        Some("empty")
    } else if name.contains('\u{FFFD}') {
        Some("replacement-char")
    } else if name.chars().any(char::is_control) {
        Some("control-char")
    } else {
        None
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

// --------------------------------------------------------------------------
// 1. Session restore
// --------------------------------------------------------------------------

#[test]
#[ignore = "live MEGA account (read-only)"]
fn live_mega_01_session_restore_and_health() {
    let ctx = ctx().unwrap();
    let session = MegaSession::load(&ctx.store).expect("stored MEGA session");
    assert!(!session.sid().is_empty());
    assert!(session.master_key().is_ok(), "master key must decode");
    let mut driver = driver(ctx);
    let account = driver.account().expect("account() with the stored session");
    assert_eq!(account.provider, CloudProvider::Mega);
    assert!(!account.id.is_empty());
    assert!(account.display_name.contains('@'), "display name is the e-mail");
    assert_eq!(account.id, session.user_handle(), "live id equals the stored handle");
    match driver.storage_capacity().expect("storage_capacity()") {
        CloudStorageCapacity::Limited { total_bytes, used_bytes } => {
            assert!(total_bytes > 0 && used_bytes <= total_bytes);
            eprintln!(
                "OK 1: session healthy; quota used {:.1}% of total",
                used_bytes as f64 * 100.0 / total_bytes as f64
            );
        }
        other => panic!("unexpected capacity {other:?}"),
    }
}

// --------------------------------------------------------------------------
// 2. Tree browsing
// --------------------------------------------------------------------------

#[test]
#[ignore = "live MEGA account (read-only)"]
fn live_mega_02_browse_tree_names_sizes_times() {
    let snapshot = snapshot().unwrap();
    let files = snapshot.entries.iter().filter(|e| e.file.kind == CloudFileKind::File).count();
    let folders = snapshot.entries.len() - files;
    let depth = snapshot.entries.iter().map(|e| e.depth + 1).max().unwrap_or(0);
    let root_children = snapshot.listed_children[root_id()];

    let mut name_problems = 0;
    let mut bad_time = 0;
    let mut missing_time = 0;
    let mut file_without_size = 0;
    let mut folder_with_size = 0;
    let mut zero_size_files = 0;
    let mut total_bytes = 0u64;
    let mut ids = BTreeSet::new();
    let mut duplicate_ids = 0;
    let now = now_seconds();
    for entry in &snapshot.entries {
        let file = &entry.file;
        if name_problem(&file.name).is_some() {
            name_problems += 1;
        }
        if !ids.insert(file.id.as_str()) {
            duplicate_ids += 1;
        }
        match file.modified_at.as_deref().map(str::parse::<u64>) {
            None => missing_time += 1,
            Some(Ok(seconds)) if (1_356_998_400..=now + 86_400).contains(&seconds) => {}
            _ => bad_time += 1,
        }
        match (file.kind, file.size) {
            (CloudFileKind::File, None) => file_without_size += 1,
            (CloudFileKind::File, Some(0)) => zero_size_files += 1,
            (CloudFileKind::File, Some(size)) => total_bytes += size,
            (CloudFileKind::Folder, Some(_)) => folder_with_size += 1,
            (CloudFileKind::Folder, None) => {}
        }
    }

    // Cross-check against the raw listing: own-tree nodes the driver dropped
    // silently (an undecryptable node is skipped, never shown as a placeholder).
    let children = raw_children(snapshot);
    let mut stack = vec![raw_drive_root(snapshot).unwrap()];
    let mut raw_reachable = 0usize;
    while let Some(handle) = stack.pop() {
        for node in children.get(handle).into_iter().flatten().filter(|n| is_file_or_folder(n)) {
            raw_reachable += 1;
            if node["t"].as_u64() == Some(1) {
                stack.push(node["h"].as_str().unwrap());
            }
        }
    }
    let silently_skipped = raw_reachable.saturating_sub(snapshot.entries.len());
    eprintln!(
        "OK 2: root children {root_children}; files {files}; folders {folders}; max depth {depth}; \
         zero-size files {zero_size_files}; total {} MiB; raw reachable nodes {raw_reachable}; \
         silently skipped {silently_skipped}; name problems {name_problems}; bad time {bad_time}; \
         missing time {missing_time}",
        total_bytes / (1024 * 1024)
    );
    assert_eq!(name_problems, 0, "names must all decrypt to readable text");
    assert_eq!(duplicate_ids, 0);
    assert_eq!(file_without_size, 0);
    assert_eq!(folder_with_size, 0);
    assert_eq!(bad_time, 0, "timestamps must be within 2013..now");
    assert_eq!(silently_skipped, 0, "own-tree nodes were dropped by the driver");
}

// --------------------------------------------------------------------------
// 3. Inbound shares ("Shared with me")
// --------------------------------------------------------------------------

#[test]
#[ignore = "live MEGA account (read-only)"]
fn live_mega_03_inbound_shares_report() {
    let ctx = ctx().unwrap();
    let snapshot = snapshot().unwrap();
    let nodes = raw_nodes(snapshot);
    let mut by_type: BTreeMap<u64, usize> = BTreeMap::new();
    for node in nodes {
        *by_type.entry(node["t"].as_u64().unwrap_or(99)).or_default() += 1;
    }
    let handles: BTreeSet<&str> = nodes.iter().filter_map(|n| n["h"].as_str()).collect();
    // Inbound share roots: file/folder nodes owned by another user (`su`) whose
    // parent is not part of this account's tree.
    let roots: Vec<&Value> = nodes
        .iter()
        .filter(|n| is_file_or_folder(n))
        .filter(|n| {
            n["su"].as_str().is_some_and(|owner| !owner.is_empty())
                || n["sk"].as_str().is_some_and(|key| !key.is_empty())
        })
        .filter(|n| n["p"].as_str().is_none_or(|parent| !handles.contains(parent)))
        .collect();
    let children = raw_children(snapshot);
    let mut descendants = 0usize;
    let mut listed_children = 0usize;
    let rsa_wrapped = roots.iter().filter(|n| n["sk"].as_str().is_some()).count();
    let mut list_errors = 0usize;
    let mut ok_lists = 0usize;
    let mut driver = driver(ctx);
    for root in &roots {
        let handle = root["h"].as_str().unwrap();
        let mut stack = vec![handle];
        while let Some(current) = stack.pop() {
            for child in children.get(current).into_iter().flatten().filter(|n| is_file_or_folder(n)) {
                descendants += 1;
                if child["t"].as_u64() == Some(1) {
                    stack.push(child["h"].as_str().unwrap());
                }
            }
        }
        match driver.list_directory(handle) {
            Ok(directory) => {
                ok_lists += 1;
                listed_children += directory.items.len();
            }
            Err(_) => list_errors += 1,
        }
    }
    let reachable_from_root_listing = snapshot
        .entries
        .iter()
        .any(|entry| roots.iter().any(|root| root["h"].as_str() == Some(entry.file.id.as_str())));
    eprintln!(
        "OK 3: raw node types {by_type:?}; inbound share roots {} (RSA-wrapped share key `sk`: {rsa_wrapped}); raw descendants {descendants}; \
         list_directory(root handle) ok {ok_lists} / err {list_errors}; direct children the driver \
         could decrypt {listed_children}; share roots visible in the normal browse tree: {reachable_from_root_listing}",
        roots.len()
    );
    assert!(!reachable_from_root_listing);
}

// --------------------------------------------------------------------------
// 4. Large directory completeness
// --------------------------------------------------------------------------

#[test]
#[ignore = "live MEGA account (read-only)"]
fn live_mega_04_large_directory_is_complete() {
    let snapshot = snapshot().unwrap();
    let children = raw_children(snapshot);
    let (largest_id, listed) = snapshot
        .listed_children
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(id, count)| (id.as_str(), *count))
        .expect("at least the root is listed");
    let handle = raw_handle(snapshot, largest_id).unwrap();
    let raw_count = children.get(handle).map_or(0, |nodes| nodes.iter().filter(|n| is_file_or_folder(n)).count());
    let mut mismatched_directories = 0;
    for (id, listed_count) in &snapshot.listed_children {
        let handle = raw_handle(snapshot, id).unwrap();
        let raw = children.get(handle).map_or(0, |nodes| nodes.iter().filter(|n| is_file_or_folder(n)).count());
        if raw != *listed_count {
            mismatched_directories += 1;
        }
    }
    eprintln!(
        "OK 4: raw node count (single `f` reply, no paging) {}; directories {}; largest directory: \
         driver {listed} vs raw {raw_count}; directories with listed != raw: {mismatched_directories}",
        raw_nodes(snapshot).len(),
        snapshot.listed_children.len()
    );
    assert_eq!(listed, raw_count);
    assert_eq!(mismatched_directories, 0);
}

// --------------------------------------------------------------------------
// 5. Download one file through gateway + aria2, then MAC verification
// --------------------------------------------------------------------------

fn account_file_key(ctx: &Ctx, snapshot: &Snapshot, handle: &str) -> LiveResult<FileKey> {
    let session = MegaSession::load(&ctx.store).map_err(|e| e.to_string())?;
    let master = session.master_key().map_err(|e| e.to_string())?;
    let node = raw_nodes(snapshot)
        .iter()
        .find(|node| node["h"].as_str() == Some(handle))
        .ok_or("node missing from the raw listing")?;
    for alternative in node["k"].as_str().ok_or("node without key")?.split('/') {
        let Some((owner, encrypted)) = alternative.split_once(':') else { continue };
        if owner != snapshot.user_handle {
            continue;
        }
        let raw = decode_base64url(encrypted)
            .and_then(|bytes| decrypt_ecb(&master, &bytes))
            .ok_or("node key does not decrypt")?;
        return FileKey::from_bytes(&raw).ok_or_else(|| "node key has the wrong length".into());
    }
    Err("no key alternative owned by this account".into())
}

fn smallest_nonzero_file_at_most(snapshot: &Snapshot, limit: u64) -> Option<&CloudFile> {
    snapshot
        .entries
        .iter()
        .map(|entry| &entry.file)
        .filter(|file| file.kind == CloudFileKind::File)
        .filter(|file| file.size.is_some_and(|size| size > 0 && size <= limit))
        .max_by_key(|file| file.size)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live MEGA account (read-only, downloads one file <= 20 MB)"]
async fn live_mega_05_download_one_file_and_verify_mac() {
    let ctx = ctx().unwrap();
    let snapshot = tokio::task::spawn_blocking(snapshot).await.unwrap().unwrap();
    let Some(file) = smallest_nonzero_file_at_most(snapshot, MAX_TRANSFER_BYTES).cloned() else {
        skip_without_data("5", "the account holds no file of 1..=20 MB");
        return;
    };
    let expected_size = file.size.unwrap();

    let resolved = {
        let id = file.id.clone();
        tokio::task::spawn_blocking(move || driver(ctx).resolve_download(&id))
            .await
            .unwrap()
            .expect("resolve_download through the registry driver")
    };
    assert!(resolved.url.starts_with("https://"));
    assert!(resolved.headers.is_empty());
    assert_eq!(resolved.name, file.name);
    let transform = resolved.transform.clone().expect("decrypting transform");
    assert_eq!(transform.total_size(), expected_size);

    std::fs::create_dir_all(SCRATCH).unwrap();
    let scratch = tempfile::tempdir_in(SCRATCH).unwrap();
    let scratch_path = scratch.path().to_path_buf();

    let gateway = RangeGateway::start().await.unwrap();
    let registration = gateway
        .register_transformed(Uuid::new_v4(), resolved.url.clone(), vec![], None, transform, false)
        .unwrap();
    let local_url = registration.url().to_owned();

    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store.create(NewTask::direct(&local_url)).unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("mega-account-live-{}", aria2.rpc_port);
    drop(reserved);
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = versadown_lib::engine_binary::EngineBinary::for_current_target()
        .unwrap()
        .resolve(&manifest, &manifest)
        .unwrap();
    let output = scratch_path.join("downloads");
    let (mut process, client) = Aria2Process::start(Aria2LaunchConfig {
        binary,
        data_dir: scratch_path.join("engine"),
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
                max_connections_per_server: Some(
                    ProviderRegistry::global()
                        .get(CloudProvider::Mega)
                        .download_concurrency
                        .max_connections_per_file,
                ),
                max_tries: Some(1),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let (state, message) = loop {
        let status = client.tell_status(submitted.aria_gid.as_deref().unwrap()).await.unwrap();
        if matches!(status.status.as_str(), "error" | "complete") {
            break (status.status, status.error_message);
        }
        assert!(started.elapsed() < Duration::from_secs(600), "transfer timed out");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    process.terminate_fallback().unwrap();
    assert_eq!(state, "complete", "aria2: {message:?}");

    let path = output.join("download.bin");
    let length = std::fs::metadata(&path).unwrap().len();
    assert_eq!(length, expected_size, "downloaded length equals the listed size");

    let key = account_file_key(ctx, snapshot, &file.id).unwrap();
    let verified = verify_file_mac(&key, BufReader::new(File::open(&path).unwrap()), length).unwrap();
    assert!(verified, "whole-file MAC of the decrypted download must match");

    // Negative control: one flipped byte must fail the MAC.
    let tampered = scratch_path.join("tampered.bin");
    std::fs::copy(&path, &tampered).unwrap();
    let mut bytes = std::fs::read(&tampered).unwrap();
    let middle = (length / 2) as usize;
    bytes[middle] = !bytes[middle];
    let mut handle = std::fs::OpenOptions::new().write(true).open(&tampered).unwrap();
    handle.seek(SeekFrom::Start(0)).unwrap();
    handle.write_all(&bytes).unwrap();
    drop(handle);
    assert!(!verify_file_mac(&key, BufReader::new(File::open(&tampered).unwrap()), length).unwrap());

    drop(registration);
    drop(scratch);
    let removed = !Path::new(&scratch_path).exists();
    eprintln!(
        "OK 5: downloaded {length} bytes via gateway+aria2 ({} connections), MAC verified, \
         tampered copy rejected; temp dir removed: {removed}",
        ProviderRegistry::global().get(CloudProvider::Mega).download_concurrency.max_connections_per_file
    );
    assert!(removed);
}

// --------------------------------------------------------------------------
// 6. Directory download plan
// --------------------------------------------------------------------------

fn subtree<'a>(
    snapshot: &'a Snapshot,
    by_parent: &HashMap<&'a str, Vec<&'a Entry>>,
    folder: &str,
) -> (Vec<&'a Entry>, u64) {
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![folder];
    let _ = snapshot;
    while let Some(current) = stack.pop() {
        for entry in by_parent.get(current).into_iter().flatten() {
            match entry.file.kind {
                CloudFileKind::Folder => stack.push(entry.file.id.as_str()),
                CloudFileKind::File => {
                    total += entry.file.size.unwrap_or(0);
                    files.push(*entry);
                }
            }
        }
    }
    (files, total)
}

#[test]
#[ignore = "live MEGA account (read-only)"]
fn live_mega_06_directory_download_plan_matches_listing() {
    let ctx = ctx().unwrap();
    let snapshot = snapshot().unwrap();
    let mut by_parent: HashMap<&str, Vec<&Entry>> = HashMap::new();
    for entry in &snapshot.entries {
        by_parent.entry(entry.file.parent_id.as_str()).or_default().push(entry);
    }
    // Prefer a folder that itself has a sub-folder and the most files, within 20 MB.
    let candidate = snapshot
        .entries
        .iter()
        .filter(|e| e.file.kind == CloudFileKind::Folder)
        .filter_map(|folder| {
            let (files, total) = subtree(snapshot, &by_parent, &folder.file.id);
            let nested = by_parent
                .get(folder.file.id.as_str())
                .is_some_and(|c| c.iter().any(|e| e.file.kind == CloudFileKind::Folder));
            (!files.is_empty() && total <= MAX_TRANSFER_BYTES && files.len() <= 300)
                .then_some((folder, files, total, nested))
        })
        .max_by_key(|(_, files, _, nested)| (*nested, files.len()));
    let Some((folder, files, total, nested)) = candidate else {
        skip_without_data("6", "the account holds no folder of <= 20 MB with at least one file");
        return;
    };

    let mut driver = driver(ctx);
    let plan = plan_cloud_directory_download(CloudProvider::Mega, &folder.file.id, &folder.file.name, |id| {
        driver.list_directory(id)
    })
    .expect("directory plan");

    let planned: BTreeMap<&str, (Option<u64>, usize)> = plan
        .files
        .iter()
        .map(|f| (f.file_id.as_str(), (f.size, f.relative_directory.len())))
        .collect();
    assert_eq!(planned.len(), plan.files.len(), "no duplicate planned files");
    let listed: BTreeMap<&str, (Option<u64>, usize)> = files
        .iter()
        .map(|e| {
            // Relative directory = root folder + one component per level below it.
            let mut depth = 1usize;
            let mut parent = e.file.parent_id.as_str();
            while parent != folder.file.id {
                parent = snapshot
                    .entries
                    .iter()
                    .find(|x| x.file.id == parent)
                    .map(|x| x.file.parent_id.as_str())
                    .expect("parent chain");
                depth += 1;
            }
            (e.file.id.as_str(), (e.file.size, depth))
        })
        .collect();
    assert_eq!(planned, listed, "plan (ids, sizes, directory depth) equals the listing");
    // Cross-check the file count against the raw node tree.
    let children = raw_children(snapshot);
    let mut stack = vec![folder.file.id.as_str()];
    let mut raw_files = 0;
    while let Some(handle) = stack.pop() {
        for node in children.get(handle).into_iter().flatten() {
            match node["t"].as_u64() {
                Some(1) => stack.push(node["h"].as_str().unwrap()),
                Some(0) => raw_files += 1,
                _ => {}
            }
        }
    }
    assert_eq!(raw_files, plan.files.len(), "plan equals the raw node tree");

    // Informational: whole-drive plan and empty-folder behaviour.
    let whole = plan_cloud_directory_download(CloudProvider::Mega, root_id(), "MEGA", |id| driver.list_directory(id))
        .map(|p| format!("ok, {} files", p.files.len()))
        .unwrap_or_else(|e| format!("error: {e}"));
    let empty = snapshot.entries.iter().find(|e| {
        e.file.kind == CloudFileKind::Folder
            && snapshot.listed_children.get(&e.file.id).copied() == Some(0)
    });
    let empty_result = empty.map(|e| {
        plan_cloud_directory_download(CloudProvider::Mega, &e.file.id, &e.file.name, |id| driver.list_directory(id))
            .map(|_| "unexpected ok".to_owned())
            .unwrap_or_else(|e| format!("rejected: {e}"))
    });
    eprintln!(
        "OK 6: folder plan {} files / {} KiB / nested={nested}, matches listing and raw tree; \
         whole-drive plan: {whole}; empty folder plan: {empty_result:?}",
        plan.files.len(),
        total / 1024
    );
}

// --------------------------------------------------------------------------
// 7. Capability declaration and write rejection
// --------------------------------------------------------------------------

#[test]
#[ignore = "live MEGA account (read-only)"]
fn live_mega_07_capabilities_are_read_only() {
    let ctx = ctx().unwrap();
    let capabilities = ProviderRegistry::global().get(CloudProvider::Mega).capabilities;
    assert!(capabilities.list && capabilities.download);
    for (name, enabled) in [
        ("create_directory", capabilities.create_directory),
        ("rename", capabilities.rename),
        ("move", capabilities.r#move),
        ("copy", capabilities.copy),
        ("trash", capabilities.trash),
        ("delete", capabilities.delete),
        ("restore", capabilities.restore),
        ("upload", capabilities.upload),
        ("share_create", capabilities.share_create),
        ("share_list", capabilities.share_list),
        ("share_cancel", capabilities.share_cancel),
        ("share_update_pass_code", capabilities.share_update_pass_code),
        ("share_transfer", capabilities.share_transfer),
    ] {
        assert!(!enabled, "capability {name} must be false for MEGA accounts");
    }

    // Calling the write operations must return ProviderUnsupported without any
    // request (the guard would refuse one and count it).
    let before = (ctx.http.sent_count(), ctx.http.refused_count());
    let mut driver = driver(ctx);
    let ids = vec!["AAAAAAAA".to_owned()];
    let unsupported = |name: &str, result: Result<(), CloudError>| {
        assert!(matches!(result, Err(CloudError::ProviderUnsupported)), "{name}: {result:?}");
    };
    unsupported("create_directory", driver.create_directory("AAAAAAAA", "x").map(|_| ()));
    unsupported("rename", driver.rename("AAAAAAAA", "x"));
    unsupported("move", driver.move_files(&ids, "BBBBBBBB"));
    unsupported("copy", driver.copy_files(&ids, "BBBBBBBB"));
    unsupported("trash", driver.trash(&ids));
    unsupported("delete", driver.delete(&ids));
    unsupported("restore", driver.restore(&ids));
    unsupported("create_share", driver.create_share(&ids, &CloudShareOptions { name: "x".into(), pass_code: None, generate_pass_code: false, expires_in_days: None }).map(|_| ()));
    unsupported("list_shares", driver.list_shares().map(|_| ()));
    unsupported("cancel_shares", driver.cancel_shares(&ids));
    unsupported("update_share_pass_code", driver.update_share_pass_code("a", "b"));
    let after = (ctx.http.sent_count(), ctx.http.refused_count());
    assert_eq!(before, after, "write calls must not reach the network");
    let recycle = ProviderRegistry::global().get(CloudProvider::Mega).recycle_bin_directory_id;
    let recycle_result = driver.list_directory(recycle).map(|d| d.items.len());
    eprintln!(
        "OK 7: all write capabilities false; 11 write calls returned ProviderUnsupported with 0 \
         requests (sent before/after {}->{}, refused {}); list(recycle-bin id) -> {:?}",
        before.0,
        after.0,
        ctx.http.refused_count(),
        recycle_result.map_err(|e| e.to_string())
    );
}

/// Final tally of what the guard let through during this process.
#[test]
#[ignore = "live MEGA account (read-only)"]
fn live_mega_99_request_tally() {
    let ctx = ctx().unwrap();
    eprintln!(
        "request tally (this test process only): {:?}; refused {}",
        ctx.http.sent.lock().unwrap(),
        ctx.http.refused_count()
    );
    assert_eq!(ctx.http.refused_count(), 0);
}
