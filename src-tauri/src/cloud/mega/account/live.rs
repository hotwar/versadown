//! Opt-in live check of the account's `^!keys` container, strictly read-only.
//!
//! Run (needs the app's saved MEGA session; nothing is ever written):
//! `VERSADOWN_LIVE_CLOUD=1 VERSADOWN_LIVE_SESSION_ROOT=~/.local/share/com.versadown.desktop \
//!  cargo test --lib mega::account::live -- --ignored --nocapture`
//!
//! The guard lets only the `uga` command through (a read of one user
//! attribute). The container is decrypted, parsed and re-serialized, and the
//! containers a share-key edit would write are built locally and compared with
//! the original. Only counts are printed: never keys, handles, values or the
//! session.

use super::client::MegaAccountClient;
use super::gcm::{derive_container_key, NONCE_LEN};
use super::key_container::{Change, FLAG_IN_USE, FLAG_TRUSTED};
use super::session::MegaSession;
use crate::cloud::mega::crypto::decode_base64url;
use crate::cloud::{
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CredentialStore, QuarkError,
    ReqwestTransport, SystemCredentialStore,
};
use rand::RngCore;
use serde_json::Value;

struct ReadOnlyHttp(ReqwestTransport);

impl CloudHttpTransport for ReadOnlyHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let command = request
            .body
            .as_deref()
            .and_then(|body| serde_json::from_str::<Value>(body).ok())
            .and_then(|value| value.get(0)?.get("a")?.as_str().map(str::to_owned));
        if command.as_deref() != Some("uga") {
            return Err(QuarkError::Provider(
                "read-only guard refused a request".into(),
            ));
        }
        self.0.send(request)
    }
}

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

#[test]
#[ignore = "requires VERSADOWN_LIVE_CLOUD=1, VERSADOWN_LIVE_SESSION_ROOT and the user's MEGA session"]
fn live_keys_container_read_only_dry_run() {
    assert_eq!(std::env::var("VERSADOWN_LIVE_CLOUD").as_deref(), Ok("1"));
    let root = std::env::var_os("VERSADOWN_LIVE_SESSION_ROOT").unwrap();
    let store = ReadOnlyStore(
        SystemCredentialStore::new(std::path::PathBuf::from(root).join("cloud-sessions")).unwrap(),
    );
    let http = ReadOnlyHttp(ReqwestTransport::new().unwrap());
    let client = MegaAccountClient::new(&http, &store);
    let session = MegaSession::load(&store).unwrap();

    // The raw value, to prove the re-sealed container is byte-identical.
    let (stored, version) = client
        .api(&session)
        .user_attribute(session.user_handle(), "^!keys")
        .unwrap()
        .expect("this account has no ^!keys container");
    let state = client
        .read_keys(&session)
        .unwrap()
        .expect("container disappeared");
    let container = state.container;
    let derived = derive_container_key(&session.master_key().unwrap()).unwrap();
    let stored_nonce: [u8; NONCE_LEN] = stored[2..2 + NONCE_LEN].try_into().unwrap();
    let identical = container.seal(&derived, &stored_nonce).unwrap() == stored;

    let entries = container.share_entries().unwrap();
    let trusted = entries
        .iter()
        .filter(|entry| entry.flags & FLAG_TRUSTED != 0)
        .count();
    let in_use = entries
        .iter()
        .filter(|entry| entry.flags & FLAG_IN_USE != 0)
        .count();
    let plain = container.serialize().unwrap();
    eprintln!(
        "keys container: stored {} bytes, plaintext {} bytes, version string {} chars, \
         identity matches this account: {}",
        stored.len(),
        plain.len(),
        version.len(),
        decode_base64url(session.user_handle()).as_deref() == container.identity(),
    );
    eprintln!(
        "keys container: generation {}, {} share keys ({} trusted, {} in use)",
        container.generation(),
        entries.len(),
        trusted,
        in_use,
    );
    eprintln!("keys container: parse -> serialize -> seal is byte-identical to the stored value: {identical}");
    assert!(identical, "round trip differs");

    // The edits this client would make, built locally and compared.
    let mut handle = [0u8; 6];
    let mut key = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut handle);
    rand::thread_rng().fill_bytes(&mut key);
    let mut changes = vec![Change::AddShareKey {
        handle,
        key,
        flags: FLAG_TRUSTED,
    }];
    if let Some(first) = entries.first() {
        changes.push(Change::SetFlags {
            handle: first.handle,
            flags: first.flags ^ FLAG_IN_USE,
        });
    }
    for change in changes {
        let updated = container.with_change(change).unwrap();
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let sealed = updated.seal(&derived, &nonce).unwrap();
        let reopened = super::key_container::KeyContainer::open(&derived, &sealed).unwrap();
        let comparison = container.compare_with_updated(&reopened, change).unwrap();
        let kind = match change {
            Change::AddShareKey { .. } => "add a share key",
            Change::SetFlags { .. } => "flip an in-use flag",
        };
        eprintln!(
            "keys container, dry run ({kind}): {} records, {} unchanged byte for byte, \
             share keys {} -> {}, generation {} -> {}, sealed {} bytes",
            comparison.records,
            comparison.unchanged_records,
            comparison.share_keys_before,
            comparison.share_keys_after,
            comparison.generation_before,
            comparison.generation_after,
            sealed.len(),
        );
    }
    eprintln!("keys container: nothing was written (the guard only passes `uga`)");
}

// ---------------------------------------------------------------------------
// Folder share with the key manager, written for real (opt-in, dedicated
// test account). Everything lives below one `VersaDown-E2E-mega-keys-*`
// folder; the link is withdrawn and the folder deleted afterwards. The share
// key entry stays in `^!keys` (the product has no operation that removes one),
// with its "in use" flag cleared again.
// ---------------------------------------------------------------------------

use crate::cloud::mega::adapter::MegaShareAdapter;
use crate::cloud::share::{
    CloudShareResolvedEntry, ShareInspect, ShareInspection, ShareResolution, ShareResolve,
};
use crate::cloud::{
    CloudProvider, CloudShareOptions, CloudUploadTransport, MemoryCredentialStore,
    UploadHttpRequest, UploadHttpResponse,
};
use crate::parser::CloudShareCandidate;

const WRITE_COMMANDS: [&str; 14] = [
    "ug", "uq", "f", "g", "p", "a", "m", "d", "u", "l", "k", "uga", "upv", "s2",
];

struct GuardedHttp {
    inner: ReqwestTransport,
    refused: std::sync::Mutex<Vec<String>>,
}

impl CloudHttpTransport for GuardedHttp {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let command = request
            .body
            .as_deref()
            .and_then(|body| serde_json::from_str::<Value>(body).ok())
            .and_then(|value| value.get(0)?.get("a")?.as_str().map(str::to_owned));
        match command {
            Some(name) if WRITE_COMMANDS.contains(&name.as_str()) => self.inner.send(request),
            other => {
                self.refused.lock().unwrap().push(other.unwrap_or_default());
                Err(QuarkError::Provider("live guard refused a request".into()))
            }
        }
    }
}

impl CloudUploadTransport for GuardedHttp {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
        self.inner.send_upload(request)
    }
}

fn entries_of(plan_entries: &[(String, Option<u64>)]) -> Vec<(String, u64)> {
    let mut entries: Vec<(String, u64)> = plan_entries
        .iter()
        .map(|(name, size)| (name.clone(), size.unwrap_or(0)))
        .collect();
    entries.sort();
    entries
}

/// What the link shows to an anonymous visitor: (name, size) of every file,
/// and the plaintext of the file called `fetch` downloaded through the range
/// gateway (decrypting with the key from the link).
type AnonymousView = (Vec<(String, u64)>, Vec<u8>);

fn anonymous_view(url: &str, fetch: &str) -> Result<AnonymousView, QuarkError> {
    let http = ReqwestTransport::new()?;
    let credentials = MemoryCredentialStore::default();
    let adapter = MegaShareAdapter {
        credentials: &credentials,
        http: &http,
        account_quota: false,
    };
    let candidate = CloudShareCandidate {
        provider: CloudProvider::Mega,
        url: url.to_owned(),
        access_code: None,
    };
    let ShareInspection::Plan(plan) = adapter.inspect(&candidate)? else {
        return Err(QuarkError::InvalidResponse);
    };
    let listed: Vec<(String, Option<u64>)> = plan
        .entries
        .iter()
        .map(|entry| (entry.name.clone(), entry.size))
        .collect();
    let entry = plan
        .entries
        .iter()
        .find(|entry| entry.name == fetch)
        .ok_or(QuarkError::SourceFileMissing)?
        .clone();
    let ShareResolution::Direct(download) = adapter.resolve(&CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        plan,
        entry,
    })?
    else {
        return Err(QuarkError::InvalidResponse);
    };
    let transform = download.transform.ok_or(QuarkError::InvalidResponse)?;
    let bytes = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| QuarkError::Storage)?
        .block_on(async {
            let gateway = crate::cloud::range_gateway::RangeGateway::start()
                .await
                .map_err(|_| QuarkError::Storage)?;
            let registration = gateway
                .register_transformed(
                    uuid::Uuid::new_v4(),
                    download.url,
                    download.headers,
                    None,
                    transform,
                    false,
                )
                .map_err(|_| QuarkError::Storage)?;
            let response = reqwest::Client::new()
                .get(registration.url())
                .send()
                .await
                .map_err(|_| QuarkError::InvalidResponse)?;
            if !response.status().is_success() {
                return Err(QuarkError::InvalidResponse);
            }
            Ok(response
                .bytes()
                .await
                .map_err(|_| QuarkError::InvalidResponse)?
                .to_vec())
        })?;
    Ok((entries_of(&listed), bytes))
}

fn counts(state: &super::key_container::KeyContainer) -> (u32, usize, usize, usize) {
    let entries = state.share_entries().unwrap();
    (
        state.generation(),
        entries.len(),
        entries
            .iter()
            .filter(|e| e.flags & FLAG_TRUSTED != 0)
            .count(),
        entries
            .iter()
            .filter(|e| e.flags & FLAG_IN_USE != 0)
            .count(),
    )
}

#[test]
#[ignore = "WRITES the account's ^!keys: requires VERSADOWN_LIVE_CLOUD=1, VERSADOWN_LIVE_SESSION_ROOT, VERSADOWN_LIVE_WRITE_KEYS=1 and a dedicated test account"]
fn live_folder_share_with_key_manager_writes_keys() {
    assert_eq!(std::env::var("VERSADOWN_LIVE_CLOUD").as_deref(), Ok("1"));
    assert_eq!(
        std::env::var("VERSADOWN_LIVE_WRITE_KEYS").as_deref(),
        Ok("1")
    );
    let root = std::env::var_os("VERSADOWN_LIVE_SESSION_ROOT").unwrap();
    let store = ReadOnlyStore(
        SystemCredentialStore::new(std::path::PathBuf::from(root).join("cloud-sessions")).unwrap(),
    );
    let http = GuardedHttp {
        inner: ReqwestTransport::new().unwrap(),
        refused: std::sync::Mutex::new(Vec::new()),
    };
    let client = MegaAccountClient::new(&http, &store);
    let session = MegaSession::load(&store).unwrap();
    let step = |label: &str| eprintln!("keys E2E step: {label}");

    step("read ^!keys before");
    let before = client
        .read_keys(&session)
        .unwrap()
        .expect("no key manager")
        .container;
    let (generation_before, shares_before, trusted_before, in_use_before) = counts(&before);
    eprintln!(
        "keys E2E before: generation {generation_before}, {shares_before} share keys \
         ({trusted_before} trusted, {in_use_before} in use)"
    );

    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let name = format!("VersaDown-E2E-mega-keys-{millis}");
    let dir = tempfile::tempdir().unwrap();
    let mut created: Vec<String> = Vec::new();
    let outcome = (|| -> Result<(), String> {
        let fail = |what: &str, error: QuarkError| format!("{what}: {error}");
        step("create the test folder tree and two files");
        let base = client
            .create_directory("0", &name)
            .map_err(|e| fail("create", e))?;
        created.push(base.id.clone());
        let inner = client
            .create_directory(&base.id, "inner")
            .map_err(|e| fail("create inner", e))?;
        for (parent, file, size) in [
            (&base.id, "top.bin", 40_000usize),
            (&inner.id, "deep.bin", 30_000),
        ] {
            let path = dir.path().join(file);
            std::fs::write(&path, vec![7u8; size]).unwrap();
            client
                .upload_file(&http, parent, &path)
                .map_err(|e| fail("upload", e))?;
        }

        step("create the folder link (writes ^!keys twice)");
        let options = CloudShareOptions {
            name: "keys e2e".into(),
            pass_code: None,
            generate_pass_code: false,
            expires_in_days: None,
        };
        let share = client
            .create_share(std::slice::from_ref(&base.id), &options)
            .map_err(|e| fail("create_share", e))?;
        if !share.url.starts_with("https://mega.nz/folder/") {
            return Err("unexpected link shape".into());
        }
        // The container afterwards: exactly the planned two edits, nothing else.
        let after = client
            .read_keys(&session)
            .map_err(|e| fail("read after", e))?
            .ok_or("container vanished")?
            .container;
        let (generation_after, shares_after, trusted_after, in_use_after) = counts(&after);
        eprintln!(
            "keys E2E after create: generation {generation_after}, {shares_after} share keys \
             ({trusted_after} trusted, {in_use_after} in use); re-read and parsed OK"
        );
        let binary = super::key_manager::binary_handle(&base.id).unwrap();
        let entry = after
            .share_entry(&binary)
            .map_err(|e| fail("entry", e))?
            .ok_or("key missing")?;
        let expected = before
            .with_change(Change::AddShareKey {
                handle: binary,
                key: *entry.key,
                flags: FLAG_TRUSTED,
            })
            .and_then(|c| {
                c.with_change(Change::SetFlags {
                    handle: binary,
                    flags: FLAG_TRUSTED | FLAG_IN_USE,
                })
            })
            .map_err(|e| fail("expected", e))?;
        if expected != after {
            return Err("the container differs from before + exactly the new share key".into());
        }
        eprintln!("keys E2E: container == before + one share key (trusted, in use), generation +2, all other records unchanged");

        step("anonymous view of the link");
        let (listed, bytes) =
            anonymous_view(&share.url, "top.bin").map_err(|e| fail("anonymous", e))?;
        let intact = bytes == vec![7u8; 40_000];
        eprintln!(
            "keys E2E: link lists {} files; anonymous download of one file: {} bytes, plaintext identical: {intact}",
            listed.len(),
            bytes.len()
        );
        if listed
            != vec![
                ("deep.bin".to_owned(), 30_000),
                ("top.bin".to_owned(), 40_000),
            ]
            || !intact
        {
            return Err("the link does not show the uploaded files".into());
        }

        step("a file uploaded after the link is visible through it (share key for new nodes)");
        let path = dir.path().join("late.bin");
        std::fs::write(&path, vec![9u8; 20_000]).unwrap();
        client
            .upload_file(&http, &inner.id, &path)
            .map_err(|e| fail("late upload", e))?;
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (listed, late) =
            anonymous_view(&share.url, "late.bin").map_err(|e| fail("anonymous 2", e))?;
        eprintln!(
            "keys E2E: late file downloads anonymously: {} bytes, identical: {}",
            late.len(),
            late == vec![9u8; 20_000]
        );
        if listed.len() != 3 || late != vec![9u8; 20_000] {
            return Err(format!(
                "the link lists {} files after the late upload",
                listed.len()
            ));
        }

        step("my shares lists it; creating it again is idempotent and writes nothing");
        let listed_shares = client.list_shares().map_err(|e| fail("list_shares", e))?;
        if !listed_shares
            .iter()
            .any(|s| s.id == base.id && s.url == share.url)
        {
            return Err("created link is not listed".into());
        }
        let again = client
            .create_share(std::slice::from_ref(&base.id), &options)
            .map_err(|e| fail("create again", e))?;
        let same = client
            .read_keys(&session)
            .map_err(|e| fail("reread", e))?
            .unwrap()
            .container;
        if again.url != share.url || same != after {
            return Err("a second create changed the link or the container".into());
        }

        step("withdraw the link");
        client
            .cancel_shares(std::slice::from_ref(&base.id))
            .map_err(|e| fail("cancel", e))?;
        let cancelled = client
            .read_keys(&session)
            .map_err(|e| fail("reread 2", e))?
            .unwrap()
            .container;
        let (generation_c, shares_c, trusted_c, in_use_c) = counts(&cancelled);
        eprintln!(
            "keys E2E after cancel: generation {generation_c}, {shares_c} share keys \
             ({trusted_c} trusted, {in_use_c} in use); re-read and parsed OK"
        );
        let expected = after
            .with_change(Change::SetFlags {
                handle: binary,
                flags: FLAG_TRUSTED,
            })
            .map_err(|e| fail("expected 2", e))?;
        if expected != cancelled {
            return Err("cancelling changed more than the in-use flag".into());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        if anonymous_view(&share.url, "top.bin").is_ok() {
            return Err("a withdrawn link still opens".into());
        }
        if client
            .list_shares()
            .map_err(|e| fail("list after", e))?
            .iter()
            .any(|s| s.id == base.id)
        {
            return Err("withdrawn link still listed".into());
        }
        Ok(())
    })();

    step("cleanup");
    for id in &created {
        let _ = client.trash(std::slice::from_ref(id));
        let _ = client.delete(std::slice::from_ref(id));
    }
    let leftover = client
        .list("0")
        .unwrap()
        .items
        .iter()
        .filter(|item| item.name.starts_with("VersaDown-E2E-mega-keys-"))
        .count();
    let end = client.read_keys(&session).unwrap().unwrap().container;
    let (generation_end, shares_end, trusted_end, in_use_end) = counts(&end);
    eprintln!(
        "keys E2E final: generation {generation_end}, {shares_end} share keys ({trusted_end} trusted, {in_use_end} in use); \
         E2E folders left at the root: {leftover}; guard refused {:?}",
        http.refused.lock().unwrap()
    );
    outcome.expect("keys E2E failed");
    assert_eq!(leftover, 0);
    assert!(http.refused.lock().unwrap().is_empty());
}
