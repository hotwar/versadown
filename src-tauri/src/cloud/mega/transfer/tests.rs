//! The share transfer protocol for MEGA: link -> plan -> transfer -> recorded
//! refresh, against a fake that serves the public API and the account API.
//! Key values come from the independent vectors in `account/keys_vectors.rs`.

use super::*;
use crate::cloud::mega::account::keys_vectors::*;
use crate::cloud::mega::crypto::encode_base64url;
use crate::cloud::share::{
    provider_share_adapter, CloudSharePlan, CloudSharePlanStore, ShareInspect, ShareInspection,
};
use crate::cloud::{
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudStorageCapacity,
    MemoryCredentialStore,
};
use crate::parser::CloudShareCandidate;
use serde_json::{json, Value};
use std::sync::Mutex;

const USER: &str = "XFERUSER";
const FILE_LINK: &str = "FILELINK";
const FOLDER_LINK: &str = "FOLDLINK";
const SID: &str = "XFERSESSIONID";

struct World {
    requests: Mutex<Vec<(String, Value)>>,
}

impl World {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self, name: &str) -> Vec<(String, Value)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, command)| command["a"] == name)
            .cloned()
            .collect()
    }
}

fn saved_node(handle: &str, wrapped_key: &str, sealed: &str, size: u64) -> Value {
    json!({
        "h": handle, "p": "XFERROOT", "u": USER, "t": 0, "s": size, "ts": 1700000000,
        "k": format!("{USER}:{wrapped_key}"),
        "a": sealed,
    })
}

fn account_listing() -> Value {
    let folder_listing: Value = serde_json::from_str(IMPORT_LISTING).unwrap();
    json!({"f": [
        {"h": "XFERROOT", "p": "", "u": USER, "t": 2, "ts": 1600000000},
        saved_node("SAVEDFIL", LINK_FILE_K, LINK_FILE_AT, 4096),
        saved_node("SAVEDIN1", IMPORT_FILE1_K, folder_listing["f"][2]["a"].as_str().unwrap(), 1234),
    ]})
}

impl CloudHttpTransport for World {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let command: Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        let command = command[0].clone();
        self.requests
            .lock()
            .unwrap()
            .push((request.url.clone(), command.clone()));
        let reply = if request.url.contains("sid=") {
            assert!(request.url.contains(SID));
            match command["a"].as_str().unwrap() {
                "f" => account_listing(),
                "p" => {
                    let node = &command["n"][0];
                    let handle = if node.get("ph").is_some() {
                        "SAVEDFIL"
                    } else {
                        "SAVEDIN1"
                    };
                    json!({"f": [{"h": handle, "p": "XFERROOT", "t": 0, "a": node["a"], "ts": 1}]})
                }
                "g" => {
                    json!({"g": "https://gfs.example/dl/saved", "s": if command["n"] == "SAVEDFIL" { 4096 } else { 1234 }})
                }
                "uq" => json!({"mstrg": 20_000_000_000u64, "cstrg": 1_000_000u64}),
                _ => json!(0),
            }
        } else {
            match command["a"].as_str().unwrap() {
                "g" if command["p"] == FILE_LINK => json!({"s": 4096, "at": LINK_FILE_AT}),
                "f" if request.url.contains(&format!("&n={FOLDER_LINK}")) => {
                    serde_json::from_str(IMPORT_LISTING).unwrap()
                }
                _ => json!(-9),
            }
        };
        Ok(CloudHttpResponse {
            status: 200,
            body: json!([reply]).to_string(),
            set_cookies: Vec::new(),
        })
    }
}

fn credentials() -> MemoryCredentialStore {
    let store = MemoryCredentialStore::default();
    crate::cloud::mega::account::MegaSession::new(
        SID.into(),
        &IMPORT_MASTER,
        USER.into(),
        "x@example.com".into(),
    )
    .save(&store)
    .unwrap();
    store
}

fn plan_store() -> (CloudSharePlanStore, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = CloudSharePlanStore::new_for_test(
        crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
    );
    (store, directory)
}

fn inspect(world: &World, url: String) -> CloudSharePlan {
    let credentials = MemoryCredentialStore::default();
    let adapter = provider_share_adapter(CloudProvider::Mega, &credentials, world).unwrap();
    let candidate = CloudShareCandidate {
        provider: CloudProvider::Mega,
        url,
        access_code: None,
    };
    match adapter.inspect(&candidate).unwrap() {
        ShareInspection::Plan(plan) => plan,
        ShareInspection::AccessCodeRequired => unreachable!(),
    }
}

fn entry_named(plan: &CloudSharePlan, name: &str) -> CloudShareResolvedEntry {
    CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        entry: plan
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
            .clone(),
        plan: plan.clone(),
    }
}

#[test]
fn a_file_link_is_saved_to_the_drive_root_journalled_and_served_from_the_account() {
    let world = World::new();
    let plan = inspect(
        &world,
        format!(
            "https://mega.nz/file/{FILE_LINK}#{}",
            encode_base64url(&LINK_FILE_KEY)
        ),
    );
    let entry = entry_named(&plan, "linked.iso");
    let (store, _guard) = plan_store();
    store.upsert(&plan).unwrap();
    let credentials = credentials();
    let adapter = provider_share_adapter(CloudProvider::Mega, &credentials, &world).unwrap();

    let download = adapter.transfer(&entry, &store).unwrap();
    assert_eq!(download.url, "https://gfs.example/dl/saved");
    assert_eq!(download.name, "linked.iso");
    assert!(
        download.transform.is_some(),
        "the account copy is decrypted too"
    );

    // One import, below the drive root, naming the link's public handle.
    let imports = world.commands("p");
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].1["t"], "XFERROOT");
    assert_eq!(imports[0].1["n"][0]["ph"], FILE_LINK);
    assert_eq!(imports[0].1["n"][0]["k"], LINK_FILE_K);
    // The saved node is journalled for exactly this source and account.
    let owner = adapter.transfer_owner_fingerprint(&entry).unwrap().unwrap();
    let recorded = store
        .reusable_transfer(&plan.source_for(&entry.entry.entry_id), &owner)
        .unwrap()
        .unwrap();
    assert_eq!(recorded.created_file_id, "SAVEDFIL");

    // Refreshing the recorded copy only asks for its download URL.
    let before = world.commands("p").len();
    let refreshed = adapter
        .refresh_recorded_transfer(&entry, "SAVEDFIL")
        .unwrap();
    assert!(matches!(
        refreshed.result.unwrap(),
        RecordedTransferDownload::Available(download) if download.url == "https://gfs.example/dl/saved"
    ));
    assert_eq!(world.commands("p").len(), before);
}

#[test]
fn a_file_of_a_folder_link_is_saved_alone_with_its_own_key() {
    let world = World::new();
    let plan = inspect(
        &world,
        format!(
            "https://mega.nz/folder/{FOLDER_LINK}#{}",
            encode_base64url(&IMPORT_FOLDER_KEY)
        ),
    );
    let entry = entry_named(&plan, "paper.txt");
    let (store, _guard) = plan_store();
    store.upsert(&plan).unwrap();
    let credentials = credentials();
    let adapter = provider_share_adapter(CloudProvider::Mega, &credentials, &world).unwrap();
    let download = adapter.transfer(&entry, &store).unwrap();
    assert_eq!(download.name, "paper.txt");
    let imports = world.commands("p");
    assert_eq!(imports.len(), 1);
    let node = &imports[0].1["n"][0];
    assert_eq!(node["h"], "IMPFILE1");
    assert!(node.get("p").is_none());
    assert_eq!(node["k"], IMPORT_FILE1_K);
    let owner = adapter.transfer_owner_fingerprint(&entry).unwrap().unwrap();
    let recorded = store
        .reusable_transfer(&plan.source_for(&entry.entry.entry_id), &owner)
        .unwrap()
        .unwrap();
    assert_eq!(recorded.created_file_id, "SAVEDIN1");
}

#[test]
fn the_owner_fingerprint_follows_the_account_and_a_signed_out_user_cannot_transfer() {
    let world = World::new();
    let plan = inspect(
        &world,
        format!(
            "https://mega.nz/file/{FILE_LINK}#{}",
            encode_base64url(&LINK_FILE_KEY)
        ),
    );
    let entry = entry_named(&plan, "linked.iso");
    let signed_in = credentials();
    let adapter = provider_share_adapter(CloudProvider::Mega, &signed_in, &world).unwrap();
    let owner = adapter.transfer_owner_fingerprint(&entry).unwrap().unwrap();
    assert_eq!(owner.len(), 64);
    assert_eq!(
        owner,
        crate::cloud::share::stable_owner_fingerprint(CloudProvider::Mega, USER).unwrap()
    );
    // A new session of the same account keeps it; another account differs.
    let relogin = MemoryCredentialStore::default();
    crate::cloud::mega::account::MegaSession::new(
        "OTHERSID".into(),
        &IMPORT_MASTER,
        USER.into(),
        "x@example.com".into(),
    )
    .save(&relogin)
    .unwrap();
    let again = provider_share_adapter(CloudProvider::Mega, &relogin, &world).unwrap();
    assert_eq!(
        again.transfer_owner_fingerprint(&entry).unwrap().unwrap(),
        owner
    );
    let other = MemoryCredentialStore::default();
    crate::cloud::mega::account::MegaSession::new(
        SID.into(),
        &IMPORT_MASTER,
        "OTHERUSR".into(),
        "y@example.com".into(),
    )
    .save(&other)
    .unwrap();
    let different = provider_share_adapter(CloudProvider::Mega, &other, &world).unwrap();
    assert_ne!(
        different
            .transfer_owner_fingerprint(&entry)
            .unwrap()
            .unwrap(),
        owner
    );

    // Nobody signed in: nothing is sent and nothing is journalled.
    let nobody = MemoryCredentialStore::default();
    let adapter = provider_share_adapter(CloudProvider::Mega, &nobody, &world).unwrap();
    let (store, _guard) = plan_store();
    store.upsert(&plan).unwrap();
    let before = world.requests.lock().unwrap().len();
    assert!(matches!(
        adapter.transfer(&entry, &store),
        Err(QuarkError::NotAuthorized)
    ));
    assert_eq!(world.requests.lock().unwrap().len(), before);
}

#[test]
fn transfer_capacity_is_the_accounts_free_space_and_foreign_entries_are_refused() {
    let world = World::new();
    let plan = inspect(
        &world,
        format!(
            "https://mega.nz/file/{FILE_LINK}#{}",
            encode_base64url(&LINK_FILE_KEY)
        ),
    );
    let entry = entry_named(&plan, "linked.iso");
    let credentials = credentials();
    let adapter = ShareCapabilitiesProbe::new(&credentials, &world);
    assert!(matches!(
        adapter.capacity(&entry),
        CloudStorageCapacity::Limited {
            total_bytes: 20_000_000_000,
            used_bytes: 1_000_000
        }
    ));
    let mut foreign = entry.clone();
    foreign.provider = CloudProvider::Quark;
    assert!(matches!(
        MegaShareAdapter {
            credentials: &credentials,
            http: &world,
            account_quota: false
        }
        .transfer_capacity(&foreign),
        Err(QuarkError::InvalidResponse)
    ));
    assert!(matches!(
        MegaShareAdapter {
            credentials: &credentials,
            http: &world,
            account_quota: false
        }
        .refresh_recorded_transfer(&foreign, "SAVEDFIL"),
        Err(QuarkError::InvalidResponse)
    ));
}

/// The trait method is reached through the adapter itself (the capability
/// set has no accessor for it).
struct ShareCapabilitiesProbe<'a>(MegaShareAdapter<'a>);

impl<'a> ShareCapabilitiesProbe<'a> {
    fn new(credentials: &'a MemoryCredentialStore, http: &'a World) -> Self {
        Self(MegaShareAdapter {
            credentials,
            http,
            account_quota: false,
        })
    }

    fn capacity(&self, entry: &CloudShareResolvedEntry) -> CloudStorageCapacity {
        self.0.transfer_capacity(entry).unwrap()
    }
}
