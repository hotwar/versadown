//! Copying: the request a copy sends (keys packed for the account, attributes
//! re-sealed), `cr` for copies below shared folders, and error numbers. File
//! values come from the independent Python vectors in `keys_vectors.rs`.

use super::*;
use crate::cloud::mega::account::keys_vectors::*;
use crate::cloud::mega::account::manage_vectors as incoming;
use crate::cloud::mega::account::seal::{
    attribute_key, name_attributes, seal_attributes, wrap_key,
};
use crate::cloud::mega::crypto::{decode_base64url, decrypt_ecb, decrypt_node_attributes};
use serde_json::Map;

const USER: &str = "COPYUSER";
const ROOT: &str = "COPYROOT";
const SOURCE_FOLDER: &str = "SRCFOLDR";
const SUB_FOLDER: &str = "SUBFOLDR";
const FILE_A: &str = "FILEAAAA";
const FILE_B: &str = "FILEBBBB";
const TARGET: &str = "TARGETDR";
const RECYCLE: &str = "__versa_down_mega_recycle_bin__";

fn wrapped(master: &[u8; 16], key: &[u8]) -> String {
    format!("{USER}:{}", wrap_key(master, key).unwrap())
}

fn sealed(key: &[u8], attributes: &Map<String, Value>) -> String {
    seal_attributes(&attribute_key(key).unwrap(), attributes).unwrap()
}

fn named(name: &str) -> Map<String, Value> {
    name_attributes(name)
}

fn file_a_key() -> [u8; 32] {
    COPY_FILE_KEY
}

fn file_b_key() -> [u8; 32] {
    let mut key = COPY_FILE_KEY;
    key.reverse();
    key
}

fn listing() -> Value {
    let master = &COPY_MASTER;
    let with_rr: Map<String, Value> = serde_json::from_str(COPY_FILE_ATTRS_JSON).unwrap();
    json!({"f": [
        {"h": ROOT, "p": "", "u": USER, "t": 2, "ts": 1600000000},
        {"h": "INBOXHDL", "p": "", "u": USER, "t": 3, "ts": 1600000000},
        {"h": "RUBBISH1", "p": "", "u": USER, "t": 4, "ts": 1600000000},
        {"h": SOURCE_FOLDER, "p": ROOT, "u": USER, "t": 1, "ts": 1700000000,
         "k": wrapped(master, &COPY_FOLDER_KEY), "a": sealed(&COPY_FOLDER_KEY, &named("Source"))},
        {"h": SUB_FOLDER, "p": SOURCE_FOLDER, "u": USER, "t": 1, "ts": 1700000000,
         "k": wrapped(master, &[0x33; 16]), "a": sealed(&[0x33; 16], &named("inner"))},
        {"h": FILE_A, "p": SOURCE_FOLDER, "u": USER, "t": 0, "s": 77, "ts": 1700000000,
         "k": wrapped(master, &file_a_key()), "a": sealed(&file_a_key(), &with_rr)},
        {"h": FILE_B, "p": SUB_FOLDER, "u": USER, "t": 0, "s": 5, "ts": 1700000000,
         "k": wrapped(master, &file_b_key()), "a": sealed(&file_b_key(), &named("b.bin"))},
        {"h": TARGET, "p": ROOT, "u": USER, "t": 1, "ts": 1700000000,
         "k": wrapped(master, &[0x44; 16]), "a": sealed(&[0x44; 16], &named("Target"))},
        {"h": "TRASHED1", "p": "RUBBISH1", "u": USER, "t": 0, "s": 1, "ts": 1700000000,
         "k": wrapped(master, &file_a_key()), "a": sealed(&file_a_key(), &named("gone"))},
    ]})
}

fn copy_store() -> MemoryCredentialStore {
    let store = MemoryCredentialStore::default();
    MegaSession::new(EXPECTED_SID.into(), &COPY_MASTER, USER.into(), EMAIL.into())
        .save(&store)
        .unwrap();
    store
}

/// A fake whose `p` answers with the nodes it was given, as MEGA does:
/// new handles `NEW00001`... in request order, attributes as sent.
fn echo_put(command: &Value) -> (u16, String) {
    let created: Vec<Value> = command["n"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, node)| {
            json!({"h": format!("NEW0000{index}"), "p": command["t"], "t": node["t"],
                   "a": node["a"], "ts": 1700009999})
        })
        .collect();
    ok(json!({"f": created}))
}

fn copy_server(
    listing: Value,
    handler: impl Fn(&Value) -> (u16, String) + Send + Sync + 'static,
) -> FakeMega {
    server(v2_account(), move |command| {
        match command["a"].as_str().unwrap() {
            "f" => ok(listing.clone()),
            _ => handler(command),
        }
    })
}

fn unwrap_key(wrapped: &Value) -> Vec<u8> {
    let bytes = decode_base64url(wrapped.as_str().unwrap()).unwrap();
    decrypt_ecb(&COPY_MASTER, &bytes).unwrap()
}

fn attributes_under(key: &[u8], sealed: &Value) -> Map<String, Value> {
    decrypt_node_attributes(&attribute_key(key).unwrap(), sealed.as_str().unwrap()).unwrap()
}

macro_rules! copy_fixture {
    ($http:ident, $client:ident, $listing:expr, $handler:expr) => {
        let $http = copy_server($listing, $handler);
        let store = copy_store();
        let cache = ListingCache::default();
        let $client = MegaAccountClient::new(&$http, &store).with_cache(&cache);
    };
}

#[test]
fn a_copied_file_keeps_its_key_wrapped_for_the_account_and_drops_the_restore_marker() {
    copy_fixture!(http, client, listing(), echo_put);
    client.copy_files(&[FILE_A.into()], TARGET).unwrap();
    let sent = http.commands("p");
    assert_eq!(sent.len(), 1);
    assert!(sent[0].url.contains(&format!("sid={EXPECTED_SID}")));
    assert_eq!(sent[0].command["t"], TARGET);
    let nodes = sent[0].command["n"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    // Equal to the independently computed values.
    assert_eq!(nodes[0]["h"], FILE_A);
    assert_eq!(nodes[0]["t"], 0);
    assert_eq!(nodes[0]["k"], COPY_FILE_K);
    assert_eq!(nodes[0]["a"], COPY_FILE_A);
    assert!(nodes[0].get("p").is_none());
    // Neither key nor marker go out in the clear.
    let traffic = http.all_traffic();
    assert!(!traffic.contains("RESTORE1"));
    assert!(
        !traffic.contains(&crate::cloud::mega::crypto::encode_base64url(
            &COPY_FILE_KEY
        ))
    );
}

#[test]
fn a_copied_folder_carries_every_node_with_fresh_folder_keys() {
    copy_fixture!(http, client, listing(), echo_put);
    client.copy_files(&[SOURCE_FOLDER.into()], TARGET).unwrap();
    let sent = &http.commands("p")[0].command;
    assert_eq!(sent["t"], TARGET);
    let nodes = sent["n"].as_array().unwrap();
    // Parents come before children; the top has no parent, the rest point at
    // their parent's source handle.
    let order: Vec<&str> = nodes.iter().map(|n| n["h"].as_str().unwrap()).collect();
    assert_eq!(order, [SOURCE_FOLDER, SUB_FOLDER, FILE_A, FILE_B]);
    let parents: Vec<Option<&str>> = nodes.iter().map(|n| n["p"].as_str()).collect();
    assert_eq!(
        parents,
        [
            None,
            Some(SOURCE_FOLDER),
            Some(SOURCE_FOLDER),
            Some(SUB_FOLDER)
        ]
    );
    // Files keep their key (re-wrapped for the account); folders get new ones.
    assert_eq!(unwrap_key(&nodes[2]["k"]), file_a_key());
    assert_eq!(unwrap_key(&nodes[3]["k"]), file_b_key());
    let top_key = unwrap_key(&nodes[0]["k"]);
    let sub_key = unwrap_key(&nodes[1]["k"]);
    assert_eq!((top_key.len(), sub_key.len()), (16, 16));
    assert_ne!(top_key, COPY_FOLDER_KEY);
    assert_ne!(sub_key, [0x33; 16]);
    assert_ne!(top_key, sub_key);
    // Names survive, sealed under the node's (new) key.
    assert_eq!(attributes_under(&top_key, &nodes[0]["a"]), named("Source"));
    assert_eq!(attributes_under(&sub_key, &nodes[1]["a"]), named("inner"));
    assert_eq!(
        attributes_under(&file_b_key(), &nodes[3]["a"]),
        named("b.bin")
    );
    assert_eq!(nodes[2]["a"], COPY_FILE_A);
    assert!(!http
        .all_traffic()
        .contains(&crate::cloud::mega::crypto::encode_base64url(&top_key)));
}

#[test]
fn every_selected_node_is_its_own_request_and_all_are_checked_before_any_is_sent() {
    copy_fixture!(http, client, listing(), echo_put);
    // A folder selected together with a file inside it.
    client
        .copy_files(&[SOURCE_FOLDER.into(), FILE_B.into()], TARGET)
        .unwrap();
    assert_eq!(http.commands("p").len(), 2);
    assert_eq!(http.commands("p")[1].command["n"][0]["h"], FILE_B);
    // One bad id (here a node in the rubbish bin) stops everything first.
    copy_fixture!(http, client, listing(), echo_put);
    assert!(matches!(
        client.copy_files(&[FILE_A.into(), "TRASHED1".into()], TARGET),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert!(http.commands("p").is_empty());
    // Nothing selected is nothing to do, not even a listing.
    client.copy_files(&[], TARGET).unwrap();
    assert!(http.commands("p").is_empty());
}

#[test]
fn copies_go_only_to_folders_of_the_drive_and_only_from_real_nodes() {
    copy_fixture!(http, client, listing(), echo_put);
    for source in ["NOSUCHND", "bad handle!", ROOT, "RUBBISH1", "INBOXHDL"] {
        assert!(
            client.copy_files(&[source.into()], TARGET).is_err(),
            "{source}"
        );
    }
    // Targets: the rubbish bin, a file, an unknown id.
    for target in [RECYCLE, FILE_B, "NOSUCHND"] {
        assert!(
            client.copy_files(&[FILE_A.into()], target).is_err(),
            "{target}"
        );
    }
    // The drive root is fine.
    client.copy_files(&[FILE_A.into()], "0").unwrap();
    assert_eq!(http.commands("p")[0].command["t"], ROOT);
    assert_eq!(http.commands("p").len(), 1);
}

#[test]
fn a_copy_below_a_shared_folder_also_sends_the_share_keys_of_the_new_nodes() {
    let master = &COPY_MASTER;
    let share_key = [0x66u8; 16];
    let mut listing = listing();
    // TARGET is a folder the account exports: its share key is in `ok`.
    let doubled = format!("{TARGET}{TARGET}");
    listing["ok"] = json!([{
        "h": TARGET,
        "k": wrap_key(master, &share_key).unwrap(),
        "ha": wrap_key(master, doubled.as_bytes()).unwrap(),
    }]);
    copy_fixture!(
        http,
        client,
        listing,
        |command| match command["a"].as_str().unwrap() {
            "p" => echo_put(command),
            _ => ok(json!(0)),
        }
    );
    client.copy_files(&[SOURCE_FOLDER.into()], TARGET).unwrap();
    let cr = &http.commands("k")[0].command["cr"];
    assert_eq!(cr[0], json!([TARGET]));
    let handles: Vec<&str> = cr[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap())
        .collect();
    assert_eq!(handles, ["NEW00000", "NEW00001", "NEW00002", "NEW00003"]);
    let sent_nodes = http.commands("p")[0].command["n"]
        .as_array()
        .unwrap()
        .clone();
    let items = cr[2].as_array().unwrap();
    for (index, node) in sent_nodes.iter().enumerate() {
        assert_eq!(items[3 * index], 0);
        assert_eq!(items[3 * index + 1], index);
        let wrapped_for_share = decode_base64url(items[3 * index + 2].as_str().unwrap()).unwrap();
        assert_eq!(
            decrypt_ecb(&share_key, &wrapped_for_share).unwrap(),
            unwrap_key(&node["k"]),
            "node {index}"
        );
    }
}

#[test]
fn share_keys_are_withheld_when_the_reply_cannot_be_matched_to_what_was_sent() {
    let master = &COPY_MASTER;
    let mut listing = listing();
    let doubled = format!("{TARGET}{TARGET}");
    listing["ok"] = json!([{
        "h": TARGET,
        "k": wrap_key(master, &[0x66u8; 16]).unwrap(),
        "ha": wrap_key(master, doubled.as_bytes()).unwrap(),
    }]);
    copy_fixture!(
        http,
        client,
        listing,
        |command| match command["a"].as_str().unwrap() {
            // Attributes that are not the ones sent: the mapping is not trusted.
            "p" => ok(json!({"f": [{"h": "NEW00000", "p": TARGET, "t": 0, "a": "AAAA"}]})),
            _ => ok(json!(0)),
        }
    );
    client.copy_files(&[FILE_A.into()], TARGET).unwrap();
    assert!(http.commands("k").is_empty());
}

#[test]
fn files_shared_by_other_accounts_are_copied_with_keys_re_encrypted_for_this_one() {
    let store = MemoryCredentialStore::default();
    MegaSession::new(
        "SID".into(),
        &incoming::MASTER_KEY,
        incoming::USER_HANDLE.into(),
        "m@x.io".into(),
    )
    .save(&store)
    .unwrap();
    let http = FakeMega::new(|_, command| match command["a"].as_str().unwrap() {
        "f" => ok(serde_json::from_str(incoming::INCOMING_LISTING).unwrap()),
        "ug" => {
            ok(json!({"u": incoming::USER_HANDLE, "email": "m@x.io", "privk": incoming::PRIVK_B64}))
        }
        "p" => echo_put(command),
        _ => ok(json!(0)),
    });
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    client
        .copy_files(&[incoming::INCOMING_FILE_HANDLE.into()], "0")
        .unwrap();
    let node = &http.commands("p")[0].command["n"][0];
    // The key arrives as the file's own key, wrapped with the master key.
    let bytes = decode_base64url(node["k"].as_str().unwrap()).unwrap();
    assert_eq!(
        decrypt_ecb(&incoming::MASTER_KEY, &bytes).unwrap(),
        incoming::INCOMING_FILE_NODE_KEY
    );
    assert_eq!(
        decrypt_node_attributes(
            &attribute_key(&incoming::INCOMING_FILE_NODE_KEY).unwrap(),
            node["a"].as_str().unwrap()
        )
        .unwrap(),
        named(incoming::INCOMING_FILE_NAME)
    );
}

#[test]
fn a_node_below_an_unreadable_one_is_not_silently_left_out() {
    let mut listing = listing();
    // The sub folder's key does not open it (wrong key material).
    for node in listing["f"].as_array_mut().unwrap() {
        if node["h"] == SUB_FOLDER {
            node["a"] = json!("AAAAAAAAAAAAAAAAAAAAAA");
        }
    }
    copy_fixture!(http, client, listing, echo_put);
    assert!(matches!(
        client.copy_files(&[SOURCE_FOLDER.into()], TARGET),
        Err(QuarkError::InvalidResponse)
    ));
    assert!(http.commands("p").is_empty());
}

#[test]
fn an_oversized_copy_is_refused_before_any_request() {
    let mut listing = listing();
    for index in 0..5_000 {
        let handle = format!("BULK{index:04}");
        listing["f"].as_array_mut().unwrap().push(json!({
            "h": handle, "p": SOURCE_FOLDER, "u": USER, "t": 0, "s": 1, "ts": 1,
            "k": wrapped(&COPY_MASTER, &file_b_key()),
            "a": sealed(&file_b_key(), &named("x")),
        }));
    }
    copy_fixture!(http, client, listing, echo_put);
    let error = client
        .copy_files(&[SOURCE_FOLDER.into()], TARGET)
        .unwrap_err();
    assert_eq!(error_text(error), "cloud.error.mega.copyTooLarge");
    assert!(http.commands("p").is_empty());
}

#[test]
fn copy_error_numbers_surface_as_their_failures() {
    for (number, expected) in [
        (-17, "cloud.error.mega.storageFull"),
        (-11, "cloud.error.mega.accessDenied"),
    ] {
        copy_fixture!(http, client, listing(), move |_| code(number));
        let error = client.copy_files(&[FILE_A.into()], TARGET).unwrap_err();
        assert_eq!(error_text(error), expected, "{number}");
    }
    copy_fixture!(http, client, listing(), |_| code(-9));
    assert!(matches!(
        client.copy_files(&[FILE_A.into()], TARGET),
        Err(QuarkError::SourceFileMissing)
    ));
    copy_fixture!(http, client, listing(), |_| code(-15));
    assert!(matches!(
        client.copy_files(&[FILE_A.into()], TARGET),
        Err(QuarkError::AuthorizationExpired)
    ));
}
