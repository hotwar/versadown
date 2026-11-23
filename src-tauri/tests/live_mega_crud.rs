//! Opt-in, destructive-within-fixture live CRUD coverage for the MEGA account.
//!
//! Run (single thread):
//! `VERSADOWN_LIVE_CLOUD=1 VERSADOWN_LIVE_SESSION_ROOT=~/.local/share/com.versadown.desktop \
//!  cargo test --test live_mega_crud -- --ignored --nocapture --test-threads=1`
//!
//! Safety, in the style of `live_quark_pikpak_crud.rs`:
//! * Every remote object is created below one unique `VersaDown-E2E-mega-*`
//!   directory. Only ids this run created are ever changed or deleted: each
//!   mutation is checked against the set of ids the run owns first.
//! * The stored session is opened read-only (`ReadOnlyStore` refuses every
//!   write), the session is never refreshed, signed out or revoked, and the
//!   app's data directory is not modified.
//! * [`GuardedHttp`] lets only account commands for this scenario through
//!   (never login, sign-out or account settings).
//! * Cleanup runs whatever the outcome and the result reports any residue.
//!   Session ids, keys, names, handles and links are never printed.
//!
//! Folder links are deliberately not created here: they record a share key in
//! the account's `^!keys` container, and a damaged container would break every
//! share and contact key of the account. The guard refuses `upv` and `s2`.
//!
//! Optional public links (`MEGA_TEST_FILE_LINK`, `MEGA_TEST_FOLDER_LINK`) are
//! saved into the test directory when set. Their keys are never printed.
//!
//! The test walks the product code: the registry driver for every operation
//! (so the capability flags are exercised), `MegaAccountClient::upload_file`
//! for the upload (the registry driver's upload needs the app's `CloudState`,
//! which would write supervisor records into the app data), the range gateway
//! with its decrypting transform for downloads, and the public link adapter
//! for anonymous resolution of the created links.

use std::{
    collections::BTreeSet,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use versadown_lib::cloud::{
    download_transform::DownloadTransform,
    driver::ProviderDriver,
    mega::{
        account::{MegaAccountClient, MegaSession},
        parse_mega_link, verify_downloaded_file, MegaFailure,
    },
    range_gateway::RangeGateway,
    share::{
        provider_share_adapter, CloudShareResolvedEntry, ShareInspect, ShareInspection,
        ShareResolution, ShareResolve,
    },
    CloudError, CloudFile, CloudFileKind, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport,
    CloudProvider, CloudShareOptions, CloudStorageCapacity, CloudUploadTransport, CredentialStore,
    MemoryCredentialStore, ProviderRegistry, ReqwestTransport, SystemCredentialStore,
    UploadHttpRequest, UploadHttpResponse,
};
use versadown_lib::CloudShareCandidate;

type LiveResult<T> = Result<T, Box<dyn Error>>;

const API_GATEWAY: &str = "https://g.api.mega.co.nz/cs";
/// Account commands this scenario needs: user info, quota, node listing,
/// download URL, put nodes (create, copy, save a link), set attributes, move,
/// delete, upload URL, links, share keys for nodes below a share, and reading
/// one user attribute (`uga`, to look for the key manager's `^!keys`).
/// Everything else is refused: in particular `s2` and `upv`, which a folder
/// link needs and which write the account's key container. Those are not
/// exercised live without the user's confirmation.
const ALLOWED_COMMANDS: [&str; 12] = [
    "ug", "uq", "f", "g", "p", "a", "m", "d", "u", "l", "k", "uga",
];
const SMALL_BYTES: usize = 100_000;
const BIG_BYTES: usize = 3_000_000;

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

// --------------------------------------------------------------------------
// Transport guard and read-only credential store
// --------------------------------------------------------------------------

struct GuardedHttp {
    inner: ReqwestTransport,
    refused: Mutex<Vec<String>>,
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
                if ALLOWED_COMMANDS.contains(&command.as_str())
                    && request.url.starts_with(API_GATEWAY) =>
            {
                let response = self.inner.send(request)?;
                // Only the status and a negative error number are ever shown.
                let number = serde_json::from_str::<Value>(&response.body)
                    .ok()
                    .and_then(|value| {
                        let reply = value.get(0).unwrap_or(&value).clone();
                        reply.as_i64().or_else(|| reply.get("e")?.as_i64())
                    })
                    .filter(|number| *number < 0);
                if response.status != 200 || number.is_some() {
                    eprintln!(
                        "API {command}: HTTP {} error number {number:?}",
                        response.status
                    );
                }
                Ok(response)
            }
            other => {
                self.refused
                    .lock()
                    .unwrap()
                    .push(other.unwrap_or_else(|| "<none>".into()));
                Err(CloudError::Provider("live guard refused a request".into()))
            }
        }
    }
}

impl CloudUploadTransport for GuardedHttp {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, CloudError> {
        let host_ok = url::Url::parse(&request.url).ok().is_some_and(|url| {
            url.scheme() == "https"
                && url
                    .host_str()
                    .is_some_and(|host| host.ends_with(".mega.co.nz") || host.ends_with(".mega.nz"))
        });
        if !host_ok || request.method != "POST" {
            self.refused.lock().unwrap().push("<upload>".into());
            return Err(CloudError::Provider("live guard refused an upload".into()));
        }
        self.inner.send_upload(request)
    }
}

/// Reads the app's real credentials; refuses to change them.
struct ReadOnlyStore(SystemCredentialStore);

impl CredentialStore for ReadOnlyStore {
    fn save(&self, _: &str, _: &[u8]) -> Result<(), CloudError> {
        Err(CloudError::Storage)
    }
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, CloudError> {
        self.0.load(key)
    }
    fn remove(&self, _: &str) -> Result<(), CloudError> {
        Err(CloudError::Storage)
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

fn session_store() -> LiveResult<ReadOnlyStore> {
    let root = std::env::var_os("VERSADOWN_LIVE_SESSION_ROOT")
        .ok_or_else(|| failure("VERSADOWN_LIVE_SESSION_ROOT is required"))?;
    Ok(ReadOnlyStore(SystemCredentialStore::new(
        PathBuf::from(root).join("cloud-sessions"),
    )?))
}

// --------------------------------------------------------------------------
// The run: what it owns, and the helpers every step uses
// --------------------------------------------------------------------------

struct Run<'a> {
    store: &'a ReadOnlyStore,
    http: &'a GuardedHttp,
    /// Ids this run created. Nothing else is ever mutated.
    owned: Mutex<BTreeSet<String>>,
    root_id: &'static str,
    recycle_id: &'static str,
}

impl<'a> Run<'a> {
    fn driver(&self) -> ProviderDriver<'a> {
        ProviderRegistry::global().driver(CloudProvider::Mega, self.store, self.http)
    }

    fn client(&self) -> MegaAccountClient<'a> {
        MegaAccountClient::new(self.http, self.store)
    }

    fn own(&self, id: &str) {
        self.owned.lock().unwrap().insert(id.to_owned());
    }

    fn assert_owned(&self, ids: &[String]) -> LiveResult<()> {
        let owned = self.owned.lock().unwrap();
        match ids.iter().find(|id| !owned.contains(id.as_str())) {
            Some(_) => Err(failure("refusing to touch an id this run did not create")),
            None => Ok(()),
        }
    }

    fn list(&self, parent: &str) -> LiveResult<Vec<CloudFile>> {
        self.driver()
            .list_directory(parent)
            .map(|directory| directory.items)
            .map_err(|error| failure(format!("list: {error}")))
    }

    /// Listings are eventually consistent: poll briefly for a condition.
    fn wait_for(
        &self,
        parent: &str,
        what: &str,
        mut condition: impl FnMut(&[CloudFile]) -> bool,
    ) -> LiveResult<Vec<CloudFile>> {
        for attempt in 0..16 {
            let items = self.list(parent)?;
            if condition(&items) {
                return Ok(items);
            }
            if attempt < 15 {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        Err(failure(format!("{what} did not become visible in time")))
    }

    fn create_directory(&self, parent: &str, name: &str) -> LiveResult<CloudFile> {
        let created = self
            .driver()
            .create_directory(parent, name)
            .map_err(|error| failure(format!("create_directory: {error}")))?;
        self.own(&created.id);
        Ok(created)
    }

    fn upload(&self, parent: &str, path: &Path) -> LiveResult<CloudFile> {
        let created = self
            .client()
            .upload_file(self.http, parent, path)
            .map_err(|error| failure(format!("upload: {error}")))?;
        self.own(&created.id);
        Ok(created)
    }

    fn rename(&self, id: &str, name: &str) -> LiveResult<()> {
        self.assert_owned(&[id.to_owned()])?;
        self.driver()
            .rename(id, name)
            .map_err(|error| failure(format!("rename: {error}")))
    }

    fn move_to(&self, ids: &[String], parent: &str) -> LiveResult<()> {
        self.assert_owned(ids)?;
        if parent != self.root_id {
            self.assert_owned(&[parent.to_owned()])?;
        }
        self.driver()
            .move_files(ids, parent)
            .map_err(|error| failure(format!("move: {error}")))
    }

    /// Copies owned nodes below an owned folder and returns the new entries
    /// (found by comparing the folder's listing before and after).
    fn copy_to(&self, ids: &[String], parent: &str) -> LiveResult<Vec<CloudFile>> {
        self.assert_owned(ids)?;
        self.assert_owned(&[parent.to_owned()])?;
        let before: BTreeSet<String> = self.list(parent)?.into_iter().map(|item| item.id).collect();
        self.driver()
            .copy_files(ids, parent)
            .map_err(|error| failure(format!("copy: {error}")))?;
        let after = self.wait_for(parent, "copied entries", |items| {
            items
                .iter()
                .filter(|item| !before.contains(&item.id))
                .count()
                >= ids.len()
        })?;
        let created: Vec<CloudFile> = after
            .into_iter()
            .filter(|item| !before.contains(&item.id))
            .collect();
        for item in &created {
            self.own(&item.id);
        }
        Ok(created)
    }

    /// Saves a public link below an owned folder.
    fn save_link(&self, url: &str, parent: &str) -> LiveResult<CloudFile> {
        self.assert_owned(&[parent.to_owned()])?;
        let link = parse_mega_link(url).map_err(|error| failure(format!("link: {error}")))?;
        let saved = self
            .client()
            .save_public_link(&link, parent)
            .map_err(|error| failure(format!("save link: {error}")))?;
        self.own(&saved.id);
        Ok(saved)
    }

    fn trash(&self, ids: &[String]) -> LiveResult<()> {
        self.assert_owned(ids)?;
        self.driver()
            .trash(ids)
            .map_err(|error| failure(format!("trash: {error}")))
    }

    fn restore(&self, ids: &[String]) -> LiveResult<()> {
        self.assert_owned(ids)?;
        self.driver()
            .restore(ids)
            .map_err(|error| failure(format!("restore: {error}")))
    }

    fn delete(&self, ids: &[String]) -> LiveResult<()> {
        self.assert_owned(ids)?;
        self.driver()
            .delete(ids)
            .map_err(|error| failure(format!("delete: {error}")))
    }
}

// --------------------------------------------------------------------------
// Downloads through the product's decrypting gateway
// --------------------------------------------------------------------------

/// Serves `url` through the range gateway with `transform` (what aria2 would
/// be pointed at) and reads the plaintext back.
fn gateway_download(
    url: String,
    headers: Vec<(String, String)>,
    transform: DownloadTransform,
) -> LiveResult<Vec<u8>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let gateway = RangeGateway::start()
            .await
            .map_err(|e| failure(format!("{e:?}")))?;
        let registration = gateway
            .register_transformed(Uuid::new_v4(), url, headers, None, transform, false)
            .map_err(|e| failure(format!("{e:?}")))?;
        let local = registration.url().to_owned();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?;
        let response = client.get(&local).send().await?;
        if !response.status().is_success() {
            return Err(failure(format!(
                "gateway answered HTTP {}",
                response.status()
            )));
        }
        let bytes = response.bytes().await?.to_vec();
        drop(registration);
        Ok(bytes)
    })
}

fn download_node(run: &Run<'_>, id: &str) -> LiveResult<Vec<u8>> {
    let resolved = run
        .driver()
        .resolve_download(id)
        .map_err(|error| failure(format!("resolve_download: {error}")))?;
    let transform = resolved
        .transform
        .ok_or_else(|| failure("MEGA downloads must carry a decrypting transform"))?;
    gateway_download(resolved.url, resolved.headers, transform)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Anonymous download of a created link through the public-link adapter and
/// the same gateway. Returns the bytes of the entry called `name`.
fn download_public_link(url: &str, name: &str, verify: &Path) -> LiveResult<Vec<u8>> {
    let http = ReqwestTransport::new()?;
    let credentials = MemoryCredentialStore::default();
    let adapter = provider_share_adapter(CloudProvider::Mega, &credentials, &http)
        .map_err(|error| failure(format!("adapter: {error}")))?;
    let candidate = CloudShareCandidate {
        provider: CloudProvider::Mega,
        url: url.to_owned(),
        access_code: None,
    };
    let plan = match adapter
        .inspect(&candidate)
        .map_err(|error| failure(format!("anonymous inspect: {error}")))?
    {
        ShareInspection::Plan(plan) => plan,
        ShareInspection::AccessCodeRequired => return Err(failure("MEGA never asks for a code")),
    };
    let entry = plan
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| failure("the link does not list the expected file"))?
        .clone();
    let resolved = CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        plan: plan.clone(),
        entry,
    };
    let ShareResolution::Direct(download) = adapter
        .resolve(&resolved)
        .map_err(|error| failure(format!("anonymous resolve: {error}")))?
    else {
        return Err(failure("MEGA links resolve directly"));
    };
    let bytes = gateway_download(
        download.url,
        download.headers,
        download
            .transform
            .ok_or_else(|| failure("link downloads must carry a transform"))?,
    )?;
    fs::write(verify, &bytes)?;
    verify_downloaded_file(&resolved, verify)
        .map_err(|error| failure(format!("link MAC verification: {error}")))?;
    Ok(bytes)
}

// --------------------------------------------------------------------------
// The scenario
// --------------------------------------------------------------------------

fn unique_name() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("VersaDown-E2E-mega-{millis}")
}

fn random_bytes(length: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; length];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

fn step(label: &str) {
    eprintln!("mega E2E step: {label}");
}

/// Name and size of every file a public link lists.
fn link_entries(url: &str) -> LiveResult<Vec<(String, u64)>> {
    let http = ReqwestTransport::new()?;
    let credentials = MemoryCredentialStore::default();
    let adapter = provider_share_adapter(CloudProvider::Mega, &credentials, &http)
        .map_err(|error| failure(format!("adapter: {error}")))?;
    let candidate = CloudShareCandidate {
        provider: CloudProvider::Mega,
        url: url.to_owned(),
        access_code: None,
    };
    match adapter
        .inspect(&candidate)
        .map_err(|error| failure(format!("anonymous inspect: {error}")))?
    {
        ShareInspection::Plan(plan) => Ok(plan
            .entries
            .iter()
            .map(|entry| (entry.name.clone(), entry.size.unwrap_or(0)))
            .collect()),
        ShareInspection::AccessCodeRequired => Err(failure("MEGA never asks for a code")),
    }
}

/// Saves the optional public test links below the test directory and checks
/// what arrived: names, sizes and, for downloaded files, the MAC.
fn save_public_links(run: &Run<'_>, base_id: &str, local: &Path) -> LiveResult<()> {
    let (Ok(file_url), Ok(folder_url)) = (
        std::env::var("MEGA_TEST_FILE_LINK"),
        std::env::var("MEGA_TEST_FOLDER_LINK"),
    ) else {
        step("save public links: skipped (MEGA_TEST_FILE_LINK / MEGA_TEST_FOLDER_LINK not set)");
        return Ok(());
    };
    step("save a public file link and a public folder link into the drive");
    let saved_dir = run.create_directory(base_id, "saved")?;
    let file = run.save_link(&file_url, &saved_dir.id)?;
    let items = run.wait_for(&saved_dir.id, "the saved file", |items| {
        items.iter().any(|item| item.id == file.id)
    })?;
    let listed = items.iter().find(|item| item.id == file.id).unwrap();
    if listed.kind != CloudFileKind::File || listed.size.unwrap_or(0) == 0 || listed.name.is_empty()
    {
        return Err(failure("the saved file looks wrong in the listing"));
    }
    eprintln!(
        "mega E2E: saved file link, {} bytes",
        listed.size.unwrap_or(0)
    );
    verify_saved_file(run, &file.id, &local.join("saved-file.bin"))?;

    let folder = run.save_link(&folder_url, &saved_dir.id)?;
    if folder.kind != CloudFileKind::Folder {
        return Err(failure("the saved folder link is not a folder"));
    }
    let inside = run.wait_for(&folder.id, "the saved folder contents", |items| {
        !items.is_empty()
    })?;
    eprintln!("mega E2E: saved folder link, {} top entries", inside.len());
    // Every entry the link lists has arrived: count them through the tree.
    let mut files = Vec::new();
    let mut pending = inside;
    while let Some(item) = pending.pop() {
        match item.kind {
            CloudFileKind::File => files.push(item),
            CloudFileKind::Folder => pending.extend(run.list(&item.id)?),
        }
    }
    // What the link lists (names and sizes, read anonymously) is what arrived.
    let mut linked = link_entries(&folder_url)?;
    let mut saved: Vec<(String, u64)> = files
        .iter()
        .map(|item| (item.name.clone(), item.size.unwrap_or(0)))
        .collect();
    linked.sort();
    saved.sort();
    if linked != saved {
        return Err(failure("the saved folder differs from what the link lists"));
    }
    eprintln!(
        "mega E2E: the saved folder matches the link ({} files)",
        saved.len()
    );
    // The smallest file is downloaded and MAC verified.
    let smallest = files
        .iter()
        .min_by_key(|item| item.size.unwrap_or(u64::MAX))
        .ok_or_else(|| failure("the saved folder holds no file"))?;
    verify_saved_file(run, &smallest.id, &local.join("saved-folder-file.bin"))?;
    Ok(())
}

/// Downloads a node of the account through the gateway and checks the MAC of
/// its own key against the bytes.
fn verify_saved_file(run: &Run<'_>, id: &str, path: &Path) -> LiveResult<()> {
    let bytes = download_node(run, id)?;
    fs::write(path, &bytes)?;
    run.client()
        .verify_download(id, path)
        .map_err(|error| failure(format!("saved file MAC verification: {error}")))?;
    eprintln!(
        "mega E2E: saved file downloads and passes the MAC ({} bytes)",
        bytes.len()
    );
    Ok(())
}

fn used_bytes(run: &Run<'_>) -> LiveResult<u64> {
    match run
        .driver()
        .storage_capacity()
        .map_err(|error| failure(format!("quota: {error}")))?
    {
        CloudStorageCapacity::Limited {
            used_bytes,
            total_bytes,
        } => {
            assert!(used_bytes <= total_bytes);
            Ok(used_bytes)
        }
        CloudStorageCapacity::Unlimited { used_bytes } => Ok(used_bytes),
    }
}

struct Fixture {
    base: CloudFile,
    base_name: String,
}

#[allow(clippy::too_many_lines)]
fn scenario(run: &Run<'_>, fixture: &Fixture, local: &Path) -> LiveResult<()> {
    let base_id = fixture.base.id.clone();
    let capabilities = ProviderRegistry::global()
        .get(CloudProvider::Mega)
        .capabilities;
    for (name, enabled) in [
        ("create_directory", capabilities.create_directory),
        ("rename", capabilities.rename),
        ("move", capabilities.r#move),
        ("copy", capabilities.copy),
        ("share_transfer", capabilities.share_transfer),
        (
            "recorded_transfer_resolve",
            capabilities.recorded_transfer_resolve,
        ),
        ("trash", capabilities.trash),
        ("delete", capabilities.delete),
        ("restore", capabilities.restore),
        ("upload", capabilities.upload),
        ("share_create", capabilities.share_create),
        ("share_list", capabilities.share_list),
        ("share_cancel", capabilities.share_cancel),
    ] {
        if !enabled {
            return Err(failure(format!("registry does not enable {name}")));
        }
    }
    let used_before = used_bytes(run)?;

    // The "shared with me" entry exists and an account without incoming
    // shares lists it as empty rather than failing.
    step("incoming shares entry lists without error");
    let incoming = ProviderRegistry::global()
        .get(CloudProvider::Mega)
        .incoming_shares_directory_id
        .ok_or_else(|| failure("the registry has no incoming shares directory"))?;
    let incoming_items = run.list(incoming)?;
    eprintln!("mega E2E: {} incoming share root(s)", incoming_items.len());

    step("upload two random files");
    let small_plain = random_bytes(SMALL_BYTES);
    let big_plain = random_bytes(BIG_BYTES);
    let small_path = local.join("small.bin");
    let big_path = local.join("big.bin");
    fs::write(&small_path, &small_plain)?;
    fs::write(&big_path, &big_plain)?;
    let small = run.upload(&base_id, &small_path)?;
    let big = run.upload(&base_id, &big_path)?;
    if small.size != Some(SMALL_BYTES as u64) || big.size != Some(BIG_BYTES as u64) {
        return Err(failure("upload returned the wrong sizes"));
    }

    step("list confirms both files with their sizes");
    let listed = run.wait_for(&base_id, "uploaded files", |items| {
        items.iter().any(|item| item.id == small.id) && items.iter().any(|item| item.id == big.id)
    })?;
    for (file, expected) in [(&small, SMALL_BYTES as u64), (&big, BIG_BYTES as u64)] {
        let item = listed.iter().find(|item| item.id == file.id).unwrap();
        if item.kind != CloudFileKind::File || item.size != Some(expected) || item.name != file.name
        {
            return Err(failure("listed file differs from what was uploaded"));
        }
    }

    step("download back through the gateway, compare sha256, verify the MAC");
    for (file, plain) in [(&small, &small_plain), (&big, &big_plain)] {
        let downloaded = download_node(run, &file.id)?;
        if sha256(&downloaded) != sha256(plain) {
            return Err(failure("downloaded bytes differ from the uploaded bytes"));
        }
        let path = local.join(format!("dl-{}.bin", file.size.unwrap_or(0)));
        fs::write(&path, &downloaded)?;
        run.client()
            .verify_download(&file.id, &path)
            .map_err(|error| failure(format!("account MAC verification: {error}")))?;
        // A copy with one flipped byte must be rejected.
        let tampered = local.join("tampered.bin");
        let mut flipped = downloaded.clone();
        flipped[downloaded.len() / 2] ^= 1;
        fs::write(&tampered, &flipped)?;
        match run.client().verify_download(&file.id, &tampered) {
            Err(CloudError::Mega(MegaFailure::IntegrityFailed)) => {}
            other => {
                return Err(failure(format!(
                    "tampered copy was not rejected: {other:?}"
                )))
            }
        }
    }

    step("rename");
    let renamed = format!("{}-renamed.bin", fixture.base_name);
    run.rename(&small.id, &renamed)?;
    run.wait_for(&base_id, "renamed file", |items| {
        items
            .iter()
            .any(|item| item.id == small.id && item.name == renamed)
    })?;

    step("create a sub directory and move the renamed file into it");
    let sub = run.create_directory(&base_id, "sub")?;
    run.move_to(std::slice::from_ref(&small.id), &sub.id)?;
    run.wait_for(&sub.id, "moved file", |items| {
        items
            .iter()
            .any(|item| item.id == small.id && item.name == renamed)
    })?;
    run.wait_for(&base_id, "move source", |items| {
        !items.iter().any(|item| item.id == small.id)
    })?;
    // A folder cannot be moved into itself (refused before anything is sent).
    match run
        .driver()
        .move_files(std::slice::from_ref(&base_id), &sub.id)
    {
        Err(CloudError::Mega(MegaFailure::InvalidMove)) => {}
        other => return Err(failure(format!("cycle was not refused: {other:?}"))),
    }

    step("copy a file inside a folder and a whole folder tree");
    let copied_file = run.copy_to(std::slice::from_ref(&big.id), &sub.id)?;
    if copied_file.len() != 1
        || copied_file[0].name != big.name
        || copied_file[0].size != Some(BIG_BYTES as u64)
        || copied_file[0].id == big.id
        || copied_file[0].parent_id != sub.id
    {
        return Err(failure("the copied file differs from its source"));
    }
    let copy_bytes = download_node(run, &copied_file[0].id)?;
    if sha256(&copy_bytes) != sha256(&big_plain) {
        return Err(failure("the copied file downloads different bytes"));
    }
    let copy_path = local.join("copy.bin");
    fs::write(&copy_path, &copy_bytes)?;
    run.client()
        .verify_download(&copied_file[0].id, &copy_path)
        .map_err(|error| failure(format!("copy MAC verification: {error}")))?;
    // The source is untouched and still downloads.
    if sha256(&download_node(run, &big.id)?) != sha256(&big_plain) {
        return Err(failure("copying changed the source file"));
    }
    // The folder `sub` now holds the renamed file and the copy of `big`.
    let folder_copy = run.copy_to(std::slice::from_ref(&sub.id), &base_id)?;
    if folder_copy.len() != 1
        || folder_copy[0].kind != CloudFileKind::Folder
        || folder_copy[0].name != "sub"
        || folder_copy[0].id == sub.id
    {
        return Err(failure("the copied folder differs from its source"));
    }
    let copied_items = run.wait_for(&folder_copy[0].id, "copied folder contents", |items| {
        items.len() == 2
    })?;
    let mut copied_names: Vec<(&str, Option<u64>)> = copied_items
        .iter()
        .map(|item| (item.name.as_str(), item.size))
        .collect();
    copied_names.sort();
    let mut expected_names = vec![
        (renamed.as_str(), Some(SMALL_BYTES as u64)),
        (big.name.as_str(), Some(BIG_BYTES as u64)),
    ];
    expected_names.sort();
    if copied_names != expected_names {
        return Err(failure("the copied folder holds different entries"));
    }
    let copied_small = copied_items
        .iter()
        .find(|item| item.name == renamed)
        .ok_or_else(|| failure("renamed file missing in the copied folder"))?;
    if sha256(&download_node(run, &copied_small.id)?) != sha256(&small_plain) {
        return Err(failure("a file of the copied folder downloads other bytes"));
    }
    // Sources of the folder copy are unchanged.
    if run.list(&sub.id)?.len() != 2 {
        return Err(failure("copying a folder changed its source"));
    }
    // Nothing may be copied into the rubbish bin.
    if !matches!(
        run.driver()
            .copy_files(std::slice::from_ref(&big.id), run.recycle_id),
        Err(CloudError::InvalidInput(_))
    ) {
        return Err(failure("a copy into the recycle bin was not refused"));
    }

    save_public_links(run, &base_id, local)?;

    step("file share: anonymous download through the public-link adapter");
    let plain_options = CloudShareOptions {
        name: "VersaDown acceptance".into(),
        pass_code: None,
        generate_pass_code: false,
        expires_in_days: None,
    };
    run.assert_owned(std::slice::from_ref(&big.id))?;
    let file_share = run
        .driver()
        .create_share(std::slice::from_ref(&big.id), &plain_options)
        .map_err(|error| failure(format!("create_share: {error}")))?;
    if !file_share.url.starts_with("https://mega.nz/file/") || file_share.id != big.id {
        return Err(failure("unexpected file share shape"));
    }
    let linked = download_public_link(&file_share.url, &big.name, &local.join("link-file.bin"))?;
    if sha256(&linked) != sha256(&big_plain) {
        return Err(failure("anonymous file download differs"));
    }
    step("my shares lists the file link, then cancel withdraws it");
    let listed_shares = run
        .driver()
        .list_shares()
        .map_err(|error| failure(format!("list_shares: {error}")))?;
    if !listed_shares
        .iter()
        .any(|share| share.id == big.id && share.url == file_share.url)
    {
        return Err(failure("created file share is not listed"));
    }
    run.assert_owned(std::slice::from_ref(&big.id))?;
    run.driver()
        .cancel_shares(std::slice::from_ref(&big.id))
        .map_err(|error| failure(format!("cancel_shares: {error}")))?;
    let after = run
        .driver()
        .list_shares()
        .map_err(|error| failure(format!("list_shares: {error}")))?;
    if after.iter().any(|share| share.id == big.id) {
        return Err(failure("cancelled file share is still listed"));
    }
    std::thread::sleep(Duration::from_secs(2));
    let gone = download_public_link(&file_share.url, &big.name, &local.join("link-gone.bin"));
    if gone.is_ok() {
        return Err(failure("a withdrawn link still resolves"));
    }

    // Folder links record a share key in `^!keys`, which this account (it has
    // the key manager) keeps for every share and contact: that write is not
    // exercised live. The guard refuses `upv` and `s2`, so a stray attempt
    // would show up as a refusal below.
    step("folder links are not created live (they write the key container)");
    run.assert_owned(std::slice::from_ref(&sub.id))?;
    // Options MEGA links cannot carry are refused before any request.
    let with_code = CloudShareOptions {
        pass_code: Some("abcd".into()),
        ..plain_options.clone()
    };
    if !matches!(
        run.driver()
            .create_share(std::slice::from_ref(&big.id), &with_code),
        Err(CloudError::Mega(MegaFailure::ShareOptionsUnsupported))
    ) {
        return Err(failure("a pass code was not refused"));
    }
    if !matches!(
        run.driver()
            .create_share(&[big.id.clone(), small.id.clone()], &plain_options),
        Err(CloudError::Mega(MegaFailure::ShareSingleOnly))
    ) {
        return Err(failure("several items were not refused"));
    }

    step("trash the sub directory, see it in the recycle bin, restore it");
    run.trash(std::slice::from_ref(&sub.id))?;
    run.wait_for(&base_id, "trashed folder leaving its parent", |items| {
        !items.iter().any(|item| item.id == sub.id)
    })?;
    let bin = run.wait_for(
        run.recycle_id,
        "trashed folder in the recycle bin",
        |items| items.iter().any(|item| item.id == sub.id),
    )?;
    let entry = bin.iter().find(|item| item.id == sub.id).unwrap();
    if entry.name != "sub"
        || entry.kind != CloudFileKind::Folder
        || entry.parent_id != run.recycle_id
    {
        return Err(failure("recycle bin entry looks wrong"));
    }
    // The folder's contents are browsable inside the bin.
    let inside_bin = run.list(&sub.id)?;
    if inside_bin.len() != 2 {
        return Err(failure("a trashed folder lost its contents"));
    }
    run.restore(std::slice::from_ref(&sub.id))?;
    run.wait_for(&base_id, "restored folder back in its parent", |items| {
        items.iter().any(|item| item.id == sub.id)
    })?;
    run.wait_for(run.recycle_id, "restored folder leaving the bin", |items| {
        !items.iter().any(|item| item.id == sub.id)
    })?;
    let restored = run.list(&sub.id)?;
    if restored.len() != 2 {
        return Err(failure("a restored folder lost its contents"));
    }

    step("quota moved by roughly what was stored");
    let used_after = used_bytes(run)?;
    eprintln!(
        "mega E2E: used bytes before {used_before}, after {used_after} (delta {})",
        used_after as i128 - used_before as i128
    );
    Ok(())
}

/// Removes everything this run created and reports what is left.
fn cleanup(run: &Run<'_>, fixture: &Fixture) -> LiveResult<()> {
    let base_id = fixture.base.id.clone();
    let owned: Vec<String> = run.owned.lock().unwrap().iter().cloned().collect();

    // Withdraw links this run may still hold (permanent deletion removes them
    // too, but be explicit). Only nodes that have a link are asked.
    if let Ok(shares) = run.driver().list_shares() {
        for share in shares.iter().filter(|share| owned.contains(&share.id)) {
            let _ = run.driver().cancel_shares(std::slice::from_ref(&share.id));
        }
    }
    // Move the whole fixture to the recycle bin, then delete it for good.
    let root = run.list(run.root_id)?;
    if root
        .iter()
        .any(|item| item.id == base_id && item.name == fixture.base_name)
    {
        run.trash(std::slice::from_ref(&base_id))?;
    }
    for _ in 0..16 {
        let bin = run.list(run.recycle_id)?;
        let mine: Vec<String> = bin
            .iter()
            .filter(|item| owned.contains(&item.id))
            .map(|item| item.id.clone())
            .collect();
        if mine.is_empty() {
            break;
        }
        run.delete(&mine)?;
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

fn residue(run: &Run<'_>, prefix: &str) -> LiveResult<(usize, usize, usize)> {
    let owned: Vec<String> = run.owned.lock().unwrap().iter().cloned().collect();
    let root = run.list(run.root_id)?;
    let bin = run.list(run.recycle_id)?;
    let by_name = root
        .iter()
        .chain(&bin)
        .filter(|item| item.name.starts_with(prefix))
        .count();
    let by_id = root
        .iter()
        .chain(&bin)
        .filter(|item| owned.contains(&item.id))
        .count();
    let links = run
        .driver()
        .list_shares()
        .map_err(|error| failure(format!("list_shares: {error}")))?
        .iter()
        .filter(|share| owned.contains(&share.id))
        .count();
    Ok((by_name, by_id, links))
}

fn run_crud() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let session = MegaSession::load(&store).map_err(|_| failure("no stored MEGA session"))?;
    drop(session);
    let http = GuardedHttp {
        inner: ReqwestTransport::new()?,
        refused: Mutex::new(Vec::new()),
    };
    let descriptor = ProviderRegistry::global().get(CloudProvider::Mega);
    let run = Run {
        store: &store,
        http: &http,
        owned: Mutex::new(BTreeSet::new()),
        root_id: descriptor.root_directory_id,
        recycle_id: descriptor.recycle_bin_directory_id,
    };

    step("validate the stored session");
    run.driver()
        .account()
        .map_err(|error| failure(format!("account: {error}")))?;
    let root_before = run.list(run.root_id)?;
    let base_name = unique_name();
    if root_before.iter().any(|item| item.name == base_name) {
        return Err(failure("unexpected unique test root collision"));
    }
    let top_level_before = root_before.len();

    step("create the unique test directory");
    let base = run.create_directory(run.root_id, &base_name)?;
    let fixture = Fixture {
        base,
        base_name: base_name.clone(),
    };
    let local = tempfile::tempdir()?;

    let result = scenario(&run, &fixture, local.path());

    step("cleanup");
    let cleaned = cleanup(&run, &fixture);
    let (by_name, by_id, links) = residue(&run, "VersaDown-E2E-mega-")?;
    let root_after = run.list(run.root_id)?.len();
    eprintln!(
        "mega E2E residue: by name {by_name}, owned ids {by_id}, links {links}; \
         root entries before {top_level_before}, after {root_after}; guard refused {:?}",
        http.refused.lock().unwrap()
    );
    result?;
    cleaned?;
    if by_name != 0 || by_id != 0 || links != 0 || root_after != top_level_before {
        return Err(failure("the account still holds E2E residue"));
    }
    if !http.refused.lock().unwrap().is_empty() {
        return Err(failure("the live guard refused requests"));
    }
    eprintln!("mega E2E: PASS, no residue");
    Ok(())
}

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1, VERSADOWN_LIVE_SESSION_ROOT and the user's MEGA session"]
fn live_mega_complete_crud() -> LiveResult<()> {
    run_crud()
}

/// Removes any `VersaDown-E2E-mega-*` entry left by an aborted run: top-level
/// entries by their exact prefix only.
#[test]
#[ignore = "permanently removes only VersaDown-E2E-mega-* entries at the drive root and in the recycle bin"]
fn live_cleanup_mega_e2e_residuals() -> LiveResult<()> {
    require_live_authorization()?;
    let store = session_store()?;
    let http = GuardedHttp {
        inner: ReqwestTransport::new()?,
        refused: Mutex::new(Vec::new()),
    };
    let descriptor = ProviderRegistry::global().get(CloudProvider::Mega);
    let run = Run {
        store: &store,
        http: &http,
        owned: Mutex::new(BTreeSet::new()),
        root_id: descriptor.root_directory_id,
        recycle_id: descriptor.recycle_bin_directory_id,
    };
    let prefix = "VersaDown-E2E-mega-";
    for item in run
        .list(run.root_id)?
        .into_iter()
        .filter(|item| item.name.starts_with(prefix))
    {
        run.own(&item.id);
        run.trash(&[item.id])?;
    }
    for item in run
        .list(run.recycle_id)?
        .into_iter()
        .filter(|item| item.name.starts_with(prefix))
    {
        run.own(&item.id);
        run.delete(&[item.id])?;
    }
    let (by_name, _, _) = residue(&run, prefix)?;
    if by_name != 0 {
        return Err(failure("residue remains"));
    }
    Ok(())
}
