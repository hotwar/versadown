//! Folder links for an account with MEGA's key manager: every command's
//! request body, the order of the steps, and what happens on version
//! conflicts, stale or foreign containers and error numbers. The fake keeps a
//! real `^!keys` value (versioned, sealed) built from the Python vectors, and
//! decrypts what the client stores with the same independent key.

use super::*;
use crate::cloud::mega::account::gcm::derive_container_key;
use crate::cloud::mega::account::key_container::{Change, KeyContainer, FLAG_IN_USE, FLAG_TRUSTED};
use crate::cloud::mega::account::keys_vectors::*;
use crate::cloud::mega::account::seal::{
    attribute_key, name_attributes, seal_attributes, wrap_key,
};
use crate::cloud::mega::crypto::{decode_base64url, decrypt_ecb, encode_base64url};
use crate::cloud::mega::link::parse_mega_link;
use crate::cloud::{CloudShareOptions, QuarkError};
use std::sync::Arc;

const FOLDER: &str = "FOLDKEYS";
const CHILD_FOLDER: &str = "DEEPFOLD";
const FILE_ONE: &str = "CHILDFIL";
const FILE_TWO: &str = "DEEPFILE";
const PLACEHOLDER: &str = "AAAAAAAAAAAAAAAAAAAAAA";

fn folder_binary() -> [u8; 6] {
    decode_base64url(FOLDER).unwrap().try_into().unwrap()
}

fn folder_key() -> [u8; 16] {
    [0x41; 16]
}

fn child_folder_key() -> [u8; 16] {
    [0x42; 16]
}

fn file_key(seed: u8) -> [u8; 32] {
    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = seed.wrapping_mul(index as u8 + 3).wrapping_add(seed);
    }
    key
}

fn node(handle: &str, parent: &str, kind: u8, key: &[u8], name: &str) -> Value {
    let sealed = seal_attributes(&attribute_key(key).unwrap(), &name_attributes(name)).unwrap();
    json!({
        "h": handle, "p": parent, "u": KEYS_USER_HANDLE, "t": kind, "ts": 1700000300,
        "s": if kind == 0 { json!(10) } else { Value::Null },
        "a": sealed,
        "k": format!("{KEYS_USER_HANDLE}:{}", wrap_key(&KEYS_MASTER, key).unwrap()),
    })
}

fn listing() -> Value {
    json!({"f": [
        {"h": "ROOTNODE", "p": "", "u": KEYS_USER_HANDLE, "t": 2, "ts": 1600000000},
        node(FOLDER, "ROOTNODE", 1, &folder_key(), "Shared out"),
        node(CHILD_FOLDER, FOLDER, 1, &child_folder_key(), "inner"),
        node(FILE_ONE, FOLDER, 0, &file_key(5), "one.txt"),
        node(FILE_TWO, CHILD_FOLDER, 0, &file_key(9), "two.txt"),
    ]})
}

fn exported_listing(recipients: &[&str]) -> Value {
    let mut listing = listing();
    listing["ph"] = json!([{"h": FOLDER, "ph": "OLDLINK1", "ts": 1700001000}]);
    listing["s"] = Value::Array(
        recipients
            .iter()
            .map(|user| json!({"h": FOLDER, "u": user, "r": 0, "ts": 1700001000}))
            .collect(),
    );
    listing
}

type Meddle = Box<dyn Fn(&mut Attribute) + Send>;

/// The server's copy of `^!keys`.
struct Attribute {
    stored: Option<Vec<u8>>,
    version: u32,
    /// Runs before each `upv` is examined (another client writing meanwhile).
    meddle: Option<Meddle>,
    writes: u32,
}

impl Attribute {
    fn current(&self) -> KeyContainer {
        let derived = derive_container_key(&KEYS_MASTER).unwrap();
        KeyContainer::open(&derived, self.stored.as_ref().unwrap()).unwrap()
    }

    fn replace(&mut self, container: &KeyContainer) {
        let derived = derive_container_key(&KEYS_MASTER).unwrap();
        self.stored = Some(container.seal(&derived, &[0x77; 12]).unwrap());
        self.version += 1;
    }

    fn version_text(&self) -> String {
        format!("ver{}", self.version)
    }
}

type Shared = Arc<Mutex<Attribute>>;

fn attribute_with(stored: Option<&[u8]>) -> Shared {
    Arc::new(Mutex::new(Attribute {
        stored: stored.map(<[u8]>::to_vec),
        version: 1,
        meddle: None,
        writes: 0,
    }))
}

fn key_server(
    listing: Value,
    attribute: &Shared,
    handler: fn(&Value) -> (u16, String),
) -> FakeMega {
    let attribute = Arc::clone(attribute);
    server(v2_account(), move |command| {
        match command["a"].as_str().unwrap() {
            "f" => ok(listing.clone()),
            "uga" => {
                let state = attribute.lock().unwrap();
                match &state.stored {
                    None => code(-9),
                    Some(stored) => {
                        ok(json!({"av": encode_base64url(stored), "v": state.version_text()}))
                    }
                }
            }
            "upv" => {
                let mut state = attribute.lock().unwrap();
                if let Some(meddle) = state.meddle.take() {
                    meddle(&mut state);
                    // Keep meddling on the next write too if it asked to.
                }
                let pair = &command["^!keys"];
                if pair[1] != state.version_text().as_str() {
                    return code(-23);
                }
                state.stored = Some(decode_base64url(pair[0].as_str().unwrap()).unwrap());
                state.version += 1;
                state.writes += 1;
                ok(json!({"^!keys": state.version_text()}))
            }
            _ => handler(command),
        }
    })
}

fn key_client<'a>(
    http: &'a FakeMega,
    store: &'a MemoryCredentialStore,
    cache: &'a ListingCache,
) -> MegaAccountClient<'a> {
    MegaAccountClient::new(http, store).with_cache(cache)
}

fn key_store() -> MemoryCredentialStore {
    let store = MemoryCredentialStore::default();
    MegaSession::new(
        EXPECTED_SID.into(),
        &KEYS_MASTER,
        KEYS_USER_HANDLE.into(),
        EMAIL.into(),
    )
    .save(&store)
    .unwrap();
    store
}

fn link_options() -> CloudShareOptions {
    CloudShareOptions {
        name: "x".into(),
        pass_code: None,
        generate_pass_code: false,
        expires_in_days: None,
    }
}

fn sequence(http: &FakeMega) -> Vec<String> {
    http.requests
        .lock()
        .unwrap()
        .iter()
        .map(|recorded| recorded.command["a"].as_str().unwrap().to_owned())
        .filter(|name| name != "f" && name != "ug")
        .collect()
}

fn base_handler(command: &Value) -> (u16, String) {
    match command["a"].as_str().unwrap() {
        "s2" => ok(json!({"r": [0]})),
        "l" if command.get("d").is_some() => ok(json!(0)),
        "l" => (200, json!(["NEWLINK1"]).to_string()),
        _ => code(-2),
    }
}

fn unwrap_with(key: &[u8; 16], wrapped: &Value) -> Vec<u8> {
    decrypt_ecb(key, &decode_base64url(wrapped.as_str().unwrap()).unwrap()).unwrap()
}

fn is_error(result: Result<impl Sized, QuarkError>, text: &str) -> bool {
    match result {
        Err(error) => error_text(error) == text,
        Ok(_) => false,
    }
}

// ---------------------------------------------------------------- creating ---

#[test]
fn a_new_folder_link_records_the_key_first_then_shares_flags_and_exports() {
    let attribute = attribute_with(Some(CONTAINER_STORED));
    let http = key_server(listing(), &attribute, base_handler);
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);

    let share = client
        .create_share(&[FOLDER.into()], &link_options())
        .unwrap();
    assert_eq!(
        sequence(&http),
        ["uga", "uga", "upv", "uga", "s2", "uga", "upv", "uga", "l"],
        "read keys, record the key, share, set in-use, link"
    );

    let state = attribute.lock().unwrap();
    let container = state.current();
    let entry = container.share_entry(&folder_binary()).unwrap().unwrap();
    // Trusted and in use; every other entry and record is as it was.
    assert_eq!(entry.flags, FLAG_TRUSTED | FLAG_IN_USE);
    assert_eq!(
        container.share_entries().unwrap().len(),
        CONTAINER_SHARE_COUNT + 1
    );
    assert_eq!(container.generation(), CONTAINER_GENERATION + 2);
    let original = KeyContainer::open(
        &derive_container_key(&KEYS_MASTER).unwrap(),
        CONTAINER_STORED,
    )
    .unwrap();
    for (before, after) in original
        .share_entries()
        .unwrap()
        .iter()
        .zip(container.share_entries().unwrap())
    {
        assert!(*before == after);
    }
    assert!(container.serialize().unwrap().ends_with(UNKNOWN_RECORDS));
    assert_eq!(state.writes, 2);

    // The attribute is read for this user by its name.
    let read = &http.commands("uga")[0].command;
    assert_eq!(read["u"], KEYS_USER_HANDLE);
    assert_eq!(read["ua"], "^!keys");
    assert_eq!(read["v"], 1);
    assert!(http.commands("upv")[0]
        .url
        .contains(&format!("sid={EXPECTED_SID}")));

    // The two writes carry the version they were based on.
    let writes = http.commands("upv");
    assert_eq!(writes[0].command["^!keys"][1], "ver1");
    assert_eq!(writes[1].command["^!keys"][1], "ver2");

    // `s2`: the all-zero placeholders, one link recipient, and the keys of
    // the whole subtree encrypted with the (never transmitted) share key.
    let s2 = &http.commands("s2")[0].command;
    assert_eq!(s2["n"], FOLDER);
    assert_eq!(s2["ok"], PLACEHOLDER);
    assert_eq!(s2["ha"], PLACEHOLDER);
    assert_eq!(s2["s"], json!([{"u": "EXP", "r": 0}]));
    let cr = &s2["cr"];
    assert_eq!(cr[0], json!([FOLDER]));
    let nodes: Vec<&str> = cr[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap())
        .collect();
    assert_eq!(nodes.len(), 4);
    let expected: [(&str, Vec<u8>); 4] = [
        (FOLDER, folder_key().to_vec()),
        (CHILD_FOLDER, child_folder_key().to_vec()),
        (FILE_ONE, file_key(5).to_vec()),
        (FILE_TWO, file_key(9).to_vec()),
    ];
    let items = cr[2].as_array().unwrap();
    assert_eq!(items.len(), 12);
    for (index, handle) in nodes.iter().enumerate() {
        assert_eq!(items[3 * index], 0);
        assert_eq!(items[3 * index + 1], index);
        let wanted = expected.iter().find(|(name, _)| name == handle).unwrap();
        assert_eq!(unwrap_with(&entry.key, &items[3 * index + 2]), wanted.1);
    }

    // The link: public handle from `l`, the share key in the fragment.
    assert_eq!(
        share.url,
        format!(
            "https://mega.nz/folder/NEWLINK1#{}",
            encode_base64url(&*entry.key)
        )
    );
    assert_eq!(
        parse_mega_link(&share.url).unwrap().key.as_slice(),
        &*entry.key
    );
    assert_eq!(share.kind, CloudFileKind::Folder);
    assert_eq!(http.commands("l")[0].command["n"], FOLDER);

    // The share key reaches MEGA only inside the sealed attribute: never as
    // plain base64 in any request.
    assert!(!http
        .all_traffic()
        .replace(&share.url, "")
        .contains(&encode_base64url(&*entry.key)));
}

#[test]
fn a_version_conflict_rereads_and_keeps_the_other_clients_change() {
    let attribute = attribute_with(Some(CONTAINER_STORED));
    // The first write finds that another client changed the container: it
    // added a share key (generation + 1, new version).
    attribute.lock().unwrap().meddle = Some(Box::new(|state| {
        let other = state
            .current()
            .with_change(Change::AddShareKey {
                handle: ADDED_HANDLE,
                key: ADDED_KEY,
                flags: ADDED_FLAGS,
            })
            .unwrap();
        state.replace(&other);
    }));
    let http = key_server(listing(), &attribute, base_handler);
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);

    client
        .create_share(&[FOLDER.into()], &link_options())
        .unwrap();
    let state = attribute.lock().unwrap();
    let container = state.current();
    // Original two, the other client's one, ours: all there, generation +1
    // (the other client) +2 (ours: key and flag).
    assert_eq!(
        container.share_entries().unwrap().len(),
        CONTAINER_SHARE_COUNT + 2
    );
    assert!(container.share_entry(&ADDED_HANDLE).unwrap().is_some());
    assert!(container.share_entry(&folder_binary()).unwrap().is_some());
    assert_eq!(container.generation(), CONTAINER_GENERATION + 3);
    // The rejected write was followed by a fresh read before the next try.
    assert_eq!(http.commands("upv").len(), 3);
    assert!(container.serialize().unwrap().ends_with(UNKNOWN_RECORDS));
}

#[test]
fn endless_conflicts_stop_before_anything_is_shared() {
    let attribute = attribute_with(Some(CONTAINER_STORED));
    {
        // Every write finds the container changed again.
        fn busy(state: &mut Attribute) {
            let next = state.current().with_change(Change::AddShareKey {
                handle: [state.version as u8; 6],
                key: [state.version as u8; 16],
                flags: 1,
            });
            state.replace(&next.unwrap());
            state.meddle = Some(Box::new(busy));
        }
        attribute.lock().unwrap().meddle = Some(Box::new(busy));
    }
    let http = key_server(listing(), &attribute, base_handler);
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    let result = client.create_share(&[FOLDER.into()], &link_options());
    assert!(is_error(result, "cloud.error.mega.keysConflict"));
    assert_eq!(http.commands("upv").len(), 5, "bounded retries");
    assert!(http.commands("s2").is_empty() && http.commands("l").is_empty());
}

#[test]
fn a_container_older_than_one_already_seen_is_never_built_on() {
    let attribute = attribute_with(Some(CONTAINER_STORED));
    // After the first conflict the server serves an older generation.
    attribute.lock().unwrap().meddle = Some(Box::new(|state| {
        let plain = KeyContainer::open(
            &derive_container_key(&KEYS_MASTER).unwrap(),
            CONTAINER_STORED,
        )
        .unwrap()
        .serialize()
        .unwrap()
        .to_vec();
        let position = plain
            .windows(8)
            .position(|window| window == [4, 0, 0, 4, 0, 0, 0, CONTAINER_GENERATION as u8])
            .unwrap();
        let mut older = plain;
        older[position + 7] -= 1;
        state.replace(&KeyContainer::parse(&older).unwrap());
    }));
    let http = key_server(listing(), &attribute, base_handler);
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    let result = client.create_share(&[FOLDER.into()], &link_options());
    assert!(is_error(result, "cloud.error.mega.keysUnsupported"));
    assert!(http.commands("s2").is_empty());
    assert_eq!(attribute.lock().unwrap().writes, 0);
}

#[test]
fn a_container_of_another_account_or_unreadable_one_is_refused_untouched() {
    // Opens with this account's key but names another user.
    let derived = derive_container_key(&KEYS_MASTER).unwrap();
    let mut foreign = KeyContainer::open(&derived, CONTAINER_STORED)
        .unwrap()
        .serialize()
        .unwrap()
        .to_vec();
    let position = foreign
        .windows(KEYS_IDENTITY.len())
        .position(|window| window == KEYS_IDENTITY)
        .unwrap();
    foreign[position] ^= 0xFF;
    let foreign = KeyContainer::parse(&foreign)
        .unwrap()
        .seal(&derived, &[3; 12])
        .unwrap();
    let mut garbage = CONTAINER_STORED.to_vec();
    let last = garbage.len() - 1;
    garbage[last] ^= 1;
    for stored in [foreign, garbage] {
        let attribute = attribute_with(Some(&stored));
        let http = key_server(listing(), &attribute, base_handler);
        let store = key_store();
        let cache = ListingCache::default();
        let client = key_client(&http, &store, &cache);
        let result = client.create_share(&[FOLDER.into()], &link_options());
        assert!(is_error(result, "cloud.error.mega.keysUnsupported"));
        assert!(http.commands("upv").is_empty() && http.commands("s2").is_empty());
    }
}

#[test]
fn an_existing_link_is_returned_with_the_recorded_key_and_nothing_is_written() {
    let attribute = attribute_with(Some(CONTAINER_STORED));
    // Give the folder a recorded share key first.
    {
        let mut state = attribute.lock().unwrap();
        let with_key = state
            .current()
            .with_change(Change::AddShareKey {
                handle: folder_binary(),
                key: [0x5A; 16],
                flags: FLAG_TRUSTED | FLAG_IN_USE,
            })
            .unwrap();
        state.replace(&with_key);
    }
    let http = key_server(exported_listing(&["EXP"]), &attribute, base_handler);
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    let share = client
        .create_share(&[FOLDER.into()], &link_options())
        .unwrap();
    assert_eq!(
        share.url,
        format!(
            "https://mega.nz/folder/OLDLINK1#{}",
            encode_base64url(&[0x5A; 16])
        )
    );
    assert!(http.commands("upv").is_empty());
    assert!(http.commands("s2").is_empty() && http.commands("l").is_empty());
    // A link whose key is nowhere is never re-keyed.
    let attribute = attribute_with(Some(CONTAINER_STORED));
    let http = key_server(exported_listing(&["EXP"]), &attribute, base_handler);
    let client = key_client(&http, &store, &cache);
    let result = client.create_share(&[FOLDER.into()], &link_options());
    assert!(is_error(result, "cloud.error.mega.keysUnsupported"));
    assert!(http.commands("upv").is_empty());
}

#[test]
fn a_share_that_answers_with_its_own_key_is_not_linked() {
    let attribute = attribute_with(Some(CONTAINER_STORED));
    let http = key_server(listing(), &attribute, |command| {
        match command["a"].as_str().unwrap() {
            "s2" => ok(json!({"ok": "AAAAAAAAAAAAAAAAAAAAAB", "r": [0]})),
            other => base_handler(&json!({"a": other})),
        }
    });
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    let result = client.create_share(&[FOLDER.into()], &link_options());
    assert!(is_error(result, "cloud.error.mega.keysUnsupported"));
    assert!(http.commands("l").is_empty());
}

#[test]
fn error_numbers_of_the_new_commands_surface_without_half_done_work() {
    // `-9` for the attribute is "no key manager": the legacy outcome.
    let attribute = attribute_with(None);
    let http = key_server(listing(), &attribute, base_handler);
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    assert!(is_error(
        client.create_share(&[FOLDER.into()], &link_options()),
        "cloud.error.mega.folderShareUnsupported"
    ));
    // `s2` refused with access denied: reported, no link requested.
    let attribute = attribute_with(Some(CONTAINER_STORED));
    let http = key_server(listing(), &attribute, |command| {
        match command["a"].as_str().unwrap() {
            "s2" => code(-11),
            _ => code(-2),
        }
    });
    let client = key_client(&http, &store, &cache);
    assert!(is_error(
        client.create_share(&[FOLDER.into()], &link_options()),
        "cloud.error.mega.accessDenied"
    ));
    assert!(http.commands("l").is_empty());
    // The attribute write answering "too large" (-2) stops before `s2`.
    let attribute = attribute_with(Some(CONTAINER_STORED));
    let http = FakeMega::new({
        let attribute = Arc::clone(&attribute);
        move |url, command| match command["a"].as_str().unwrap() {
            "us0" | "us" => unreachable!(),
            _ if !url.contains(EXPECTED_SID) => code(-15),
            "f" => ok(listing()),
            "uga" => {
                let state = attribute.lock().unwrap();
                ok(json!({"av": encode_base64url(state.stored.as_ref().unwrap()), "v": "ver1"}))
            }
            "upv" => code(-2),
            _ => code(-2),
        }
    });
    let client = key_client(&http, &store, &cache);
    assert!(client
        .create_share(&[FOLDER.into()], &link_options())
        .is_err());
    assert!(http.commands("s2").is_empty() && http.commands("l").is_empty());
}

// --------------------------------------------------------------- withdrawing ---

fn container_with_folder_key(flags: u8) -> Shared {
    let attribute = attribute_with(Some(CONTAINER_STORED));
    {
        let mut state = attribute.lock().unwrap();
        let with_key = state
            .current()
            .with_change(Change::AddShareKey {
                handle: folder_binary(),
                key: [0x5A; 16],
                flags,
            })
            .unwrap();
        state.replace(&with_key);
    }
    attribute
}

#[test]
fn withdrawing_a_folder_link_removes_the_link_share_and_frees_the_key() {
    let attribute = container_with_folder_key(FLAG_TRUSTED | FLAG_IN_USE);
    let http = key_server(exported_listing(&["EXP"]), &attribute, base_handler);
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    client.cancel_shares(&[FOLDER.into()]).unwrap();
    assert_eq!(sequence(&http), ["l", "uga", "s2", "uga", "upv", "uga"]);
    assert_eq!(http.commands("l")[0].command["d"], 1);
    let s2 = &http.commands("s2")[0].command;
    assert_eq!(s2["n"], FOLDER);
    assert_eq!(s2["s"], json!([{"u": "EXP"}]));
    assert!(s2.get("ok").is_none() && s2.get("cr").is_none());
    let entry = attribute
        .lock()
        .unwrap()
        .current()
        .share_entry(&folder_binary())
        .unwrap()
        .unwrap();
    // The key stays (it identifies the folder's links), only "in use" drops.
    assert_eq!(entry.flags, FLAG_TRUSTED);
    assert_eq!(*entry.key, [0x5A; 16]);
}

#[test]
fn a_folder_shared_with_people_keeps_its_key_in_use() {
    let attribute = container_with_folder_key(FLAG_TRUSTED | FLAG_IN_USE);
    let http = key_server(
        exported_listing(&["EXP", "OTHERUSER01"]),
        &attribute,
        base_handler,
    );
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    client.cancel_shares(&[FOLDER.into()]).unwrap();
    assert_eq!(http.commands("s2").len(), 1);
    assert!(http.commands("upv").is_empty());
    let entry = attribute
        .lock()
        .unwrap()
        .current()
        .share_entry(&folder_binary())
        .unwrap()
        .unwrap();
    assert_eq!(entry.flags, FLAG_TRUSTED | FLAG_IN_USE);
}

#[test]
fn withdrawing_a_link_without_a_share_or_a_recorded_key_still_succeeds() {
    // The share is already gone (`-9`) and the key was never recorded here.
    let attribute = attribute_with(Some(CONTAINER_STORED));
    let http = key_server(
        exported_listing(&["EXP"]),
        &attribute,
        |command| match command["a"].as_str().unwrap() {
            "s2" | "l" => code(-9),
            _ => code(-2),
        },
    );
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    client.cancel_shares(&[FOLDER.into()]).unwrap();
    assert!(http.commands("upv").is_empty());
}

// ------------------------------------------------------------------ listing ---

#[test]
fn my_shares_takes_a_folder_links_key_from_the_container() {
    let attribute = container_with_folder_key(FLAG_TRUSTED | FLAG_IN_USE);
    let http = key_server(exported_listing(&["EXP"]), &attribute, base_handler);
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    let shares = client.list_shares().unwrap();
    assert_eq!(shares.len(), 1);
    assert_eq!(
        shares[0].url,
        format!(
            "https://mega.nz/folder/OLDLINK1#{}",
            encode_base64url(&[0x5A; 16])
        )
    );
    assert_eq!(http.commands("uga").len(), 1, "read once");
    // A folder link whose key is in neither place is left out, as before.
    let attribute = attribute_with(Some(CONTAINER_STORED));
    let http = key_server(exported_listing(&["EXP"]), &attribute, base_handler);
    let client = key_client(&http, &store, &cache);
    assert!(client.list_shares().unwrap().is_empty());
}

// ---------------------------------------------------------- nodes below a link ---

#[test]
fn a_new_folder_below_a_linked_folder_gets_its_share_key_from_the_container() {
    let attribute = container_with_folder_key(FLAG_TRUSTED | FLAG_IN_USE);
    let http = key_server(
        exported_listing(&["EXP"]),
        &attribute,
        |command| match command["a"].as_str().unwrap() {
            "p" => ok(json!({"f": [{"h": "NEWFOLDR", "p": FOLDER, "t": 1}]})),
            "k" => ok(json!(0)),
            _ => code(-2),
        },
    );
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    client.create_directory(FOLDER, "fresh").unwrap();
    let sent = &http.commands("k")[0].command["cr"];
    assert_eq!(sent[0], json!([FOLDER]));
    assert_eq!(sent[1], json!(["NEWFOLDR"]));
    let created = &http.commands("p")[0].command["n"][0];
    let new_key = {
        let wrapped = decode_base64url(created["k"].as_str().unwrap()).unwrap();
        decrypt_ecb(&KEYS_MASTER, &wrapped).unwrap()
    };
    assert_eq!(unwrap_with(&[0x5A; 16], &sent[2][2]), new_key);
}

#[test]
fn a_new_folder_in_an_unshared_folder_never_reads_the_container() {
    let attribute = attribute_with(Some(CONTAINER_STORED));
    let http = key_server(listing(), &attribute, |command| {
        match command["a"].as_str().unwrap() {
            "p" => ok(json!({"f": [{"h": "NEWFOLDR", "p": FOLDER, "t": 1}]})),
            _ => code(-2),
        }
    });
    let store = key_store();
    let cache = ListingCache::default();
    let client = key_client(&http, &store, &cache);
    client.create_directory(FOLDER, "fresh").unwrap();
    assert!(http.commands("uga").is_empty() && http.commands("k").is_empty());
}
