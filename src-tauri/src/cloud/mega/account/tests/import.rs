//! Saving public links into the account: the `p` request for a file link and
//! for a folder link (keys converted from the link's to the account's), and
//! how bad keys, missing links and error numbers surface. Values come from
//! the independent Python vectors in `keys_vectors.rs`.

use super::*;
use crate::cloud::mega::account::keys_vectors::*;
use crate::cloud::mega::account::seal::attribute_key;
use crate::cloud::mega::crypto::{
    decode_base64url, decrypt_ecb, decrypt_node_attributes, encode_base64url,
};
use crate::cloud::mega::link::{parse_mega_link, MegaLink, MegaLinkKind};
use serde_json::Map;
use zeroize::Zeroizing;

const USER: &str = "IMPUSER1";
const ROOT: &str = "IMPDRIVE";
const TARGET: &str = "IMPTARGT";
const FOLDER_LINK: &str = "FOLDLINK";
const FILE_LINK: &str = "FILELINK";

fn import_store() -> MemoryCredentialStore {
    let store = MemoryCredentialStore::default();
    MegaSession::new(
        EXPECTED_SID.into(),
        &IMPORT_MASTER,
        USER.into(),
        EMAIL.into(),
    )
    .save(&store)
    .unwrap();
    store
}

fn account_listing() -> Value {
    json!({"f": [
        {"h": ROOT, "p": "", "u": USER, "t": 2, "ts": 1600000000},
        {"h": TARGET, "p": ROOT, "u": USER, "t": 1, "ts": 1700000000,
         "k": format!("{USER}:{}", crate::cloud::mega::account::seal::wrap_key(&IMPORT_MASTER, &[0x21; 16]).unwrap()),
         "a": crate::cloud::mega::account::seal::seal_attributes(&[0x21; 16], &crate::cloud::mega::account::seal::name_attributes("Target")).unwrap()},
    ]})
}

fn named(name: &str) -> Map<String, Value> {
    crate::cloud::mega::account::seal::name_attributes(name)
}

/// A fake serving both APIs: the signed-in account's commands (they carry
/// `sid=`) go to `account`, anonymous ones (file info, folder listing, which
/// carries `&n=`) to `public`.
fn public_and_account(
    public: fn(&Value, &str) -> (u16, String),
    account: fn(&Value) -> (u16, String),
) -> FakeMega {
    FakeMega::new(move |url, command| {
        if url.contains("sid=") {
            if command["a"] == "f" {
                return ok(account_listing());
            }
            return account(command);
        }
        public(command, url)
    })
}

fn public_default(command: &Value, url: &str) -> (u16, String) {
    match command["a"].as_str().unwrap() {
        "g" if command["p"] == FILE_LINK => ok(json!({"s": 4096, "at": LINK_FILE_AT})),
        "f" if url.contains(&format!("&n={FOLDER_LINK}")) => {
            ok(serde_json::from_str(IMPORT_LISTING).unwrap())
        }
        _ => code(-9),
    }
}

fn echo_created(command: &Value) -> (u16, String) {
    match command["a"].as_str().unwrap() {
        "p" => {
            let created: Vec<Value> = command["n"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, node)| {
                    json!({"h": format!("SAVED00{index}"), "p": command["t"], "t": node["t"],
                           "a": node["a"], "ts": 1700009999})
                })
                .collect();
            ok(json!({"f": created}))
        }
        _ => ok(json!(0)),
    }
}

fn file_link(key: &[u8]) -> MegaLink {
    MegaLink {
        kind: MegaLinkKind::File,
        id: FILE_LINK.into(),
        key: Zeroizing::new(key.to_vec()),
    }
}

fn folder_link(key: &[u8]) -> MegaLink {
    MegaLink {
        kind: MegaLinkKind::Folder,
        id: FOLDER_LINK.into(),
        key: Zeroizing::new(key.to_vec()),
    }
}

fn unwrap_master(wrapped: &Value) -> Vec<u8> {
    decrypt_ecb(
        &IMPORT_MASTER,
        &decode_base64url(wrapped.as_str().unwrap()).unwrap(),
    )
    .unwrap()
}

fn under(key: &[u8], sealed: &Value) -> Map<String, Value> {
    decrypt_node_attributes(&attribute_key(key).unwrap(), sealed.as_str().unwrap()).unwrap()
}

macro_rules! import_fixture {
    ($http:ident, $client:ident, $public:expr, $account:expr) => {
        let $http = public_and_account($public, $account);
        let store = import_store();
        let cache = ListingCache::default();
        let $client = MegaAccountClient::new(&$http, &store).with_cache(&cache);
    };
}

#[test]
fn a_file_link_is_saved_with_its_key_wrapped_for_the_account() {
    import_fixture!(http, client, public_default, echo_created);
    let saved = client
        .save_public_link(&file_link(&LINK_FILE_KEY), TARGET)
        .unwrap();
    assert_eq!(saved.id, "SAVED000");
    assert_eq!(saved.name, "linked.iso");
    assert_eq!(saved.kind, CloudFileKind::File);
    assert_eq!(saved.size, Some(4096));
    assert_eq!(saved.parent_id, TARGET);

    let sent = &http.commands("p")[0];
    assert!(sent.url.contains(&format!("sid={EXPECTED_SID}")));
    assert_eq!(sent.command["t"], TARGET);
    let node = &sent.command["n"][0];
    // Named by the link's public handle; independent vector for key and attrs.
    assert_eq!(node["ph"], FILE_LINK);
    assert!(node.get("h").is_none() && node.get("p").is_none());
    assert_eq!(node["t"], 0);
    assert_eq!(node["k"], LINK_FILE_K);
    assert_eq!(node["a"], LINK_FILE_AT);
    // The link's key never reaches the server in the clear.
    assert!(!http
        .all_traffic()
        .contains(&encode_base64url(&LINK_FILE_KEY)));
}

#[test]
fn a_file_links_wrong_key_or_dead_link_stops_before_anything_is_created() {
    import_fixture!(http, client, public_default, echo_created);
    let mut wrong = LINK_FILE_KEY;
    wrong[0] ^= 0x55;
    assert!(matches!(
        client.save_public_link(&file_link(&wrong), TARGET),
        Err(QuarkError::Mega(MegaFailure::KeyInvalid))
    ));
    let mut gone = file_link(&LINK_FILE_KEY);
    gone.id = "NOSUCHLN".into();
    assert!(matches!(
        client.save_public_link(&gone, TARGET),
        Err(QuarkError::Mega(MegaFailure::LinkUnavailable))
    ));
    // A key of the wrong size (a folder key on a file link).
    assert!(client
        .save_public_link(&file_link(&IMPORT_FOLDER_KEY), TARGET)
        .is_err());
    assert!(http.commands("p").is_empty());
}

#[test]
fn a_folder_link_is_saved_node_by_node_with_keys_converted_to_the_account() {
    import_fixture!(http, client, public_default, echo_created);
    let saved = client
        .save_public_link(&folder_link(&IMPORT_FOLDER_KEY), TARGET)
        .unwrap();
    assert_eq!(saved.name, "Shared");
    assert_eq!(saved.kind, CloudFileKind::Folder);
    assert_eq!(saved.id, "SAVED000");

    let sent = &http.commands("p");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].command["t"], TARGET);
    let nodes = sent[0].command["n"].as_array().unwrap();
    let order: Vec<&str> = nodes.iter().map(|n| n["h"].as_str().unwrap()).collect();
    // Level by level: parents always before their children.
    assert_eq!(order, ["IMPROOT1", "IMPSUB01", "IMPFILE2", "IMPFILE1"]);
    let parents: Vec<Option<&str>> = nodes.iter().map(|n| n["p"].as_str()).collect();
    assert_eq!(
        parents,
        [None, Some("IMPROOT1"), Some("IMPROOT1"), Some("IMPSUB01")]
    );
    assert!(nodes.iter().all(|node| node.get("ph").is_none()));
    // Files: the key from the link, wrapped for the account (vectors).
    assert_eq!(nodes[3]["k"], IMPORT_FILE1_K);
    assert_eq!(nodes[3]["a"], IMPORT_FILE1_A);
    assert_eq!(nodes[2]["k"], IMPORT_FILE2_K);
    assert_eq!(nodes[2]["a"], IMPORT_FILE2_A);
    // Folders: fresh keys, wrapped for the account, names intact.
    let root_key = unwrap_master(&nodes[0]["k"]);
    let sub_key = unwrap_master(&nodes[1]["k"]);
    assert_eq!((root_key.len(), sub_key.len()), (16, 16));
    assert_ne!(root_key, sub_key);
    assert_eq!(under(&root_key, &nodes[0]["a"]), named("Shared"));
    assert_eq!(under(&sub_key, &nodes[1]["a"]), named("docs"));
    // Neither the link key nor a file key is sent in the clear.
    let traffic = http.all_traffic();
    assert!(!traffic.contains(&encode_base64url(&IMPORT_FOLDER_KEY)));
    assert!(!traffic.contains(&encode_base64url(&IMPORT_FILE1_KEY)));
    assert!(!traffic.contains(&encode_base64url(&root_key)));
}

#[test]
fn a_folder_link_with_the_wrong_key_or_an_unreadable_node_is_refused() {
    import_fixture!(http, client, public_default, echo_created);
    let mut wrong = IMPORT_FOLDER_KEY;
    wrong[5] ^= 1;
    assert!(matches!(
        client.save_public_link(&folder_link(&wrong), TARGET),
        Err(QuarkError::Mega(MegaFailure::KeyInvalid))
    ));
    // A file key of the wrong size for a folder link.
    assert!(client
        .save_public_link(&folder_link(&LINK_FILE_KEY), TARGET)
        .is_err());
    // One node that the key does not open: not saved half-way.
    fn holey(command: &Value, url: &str) -> (u16, String) {
        let mut listing: Value = serde_json::from_str(IMPORT_LISTING).unwrap();
        if command["a"] == "f" && url.contains("&n=") {
            listing["f"][2]["k"] = json!("IMPROOT1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
            return ok(listing);
        }
        public_default(command, url)
    }
    import_fixture!(http2, client2, holey, echo_created);
    assert!(matches!(
        client2.save_public_link(&folder_link(&IMPORT_FOLDER_KEY), TARGET),
        Err(QuarkError::InvalidResponse)
    ));
    assert!(http.commands("p").is_empty() && http2.commands("p").is_empty());
}

#[test]
fn one_file_of_a_folder_link_is_saved_with_its_own_key_and_sealed_attributes() {
    import_fixture!(http, client, public_default, echo_created);
    let saved = client
        .save_folder_file(FOLDER_LINK, "IMPFILE1", &IMPORT_FILE1_KEY, ROOT)
        .unwrap();
    assert_eq!(saved.name, "paper.txt");
    assert_eq!(saved.size, Some(1234));
    assert_eq!(saved.parent_id, ROOT);
    let sent = &http.commands("p")[0].command;
    assert_eq!(sent["t"], ROOT);
    let node = &sent["n"][0];
    assert_eq!(node["h"], "IMPFILE1");
    assert!(node.get("p").is_none() && node.get("ph").is_none());
    assert_eq!(node["k"], IMPORT_FILE1_K);
    // The attributes stay exactly as the link has them (same key).
    let listing: Value = serde_json::from_str(IMPORT_LISTING).unwrap();
    assert_eq!(node["a"], listing["f"][2]["a"]);
    // Wrong key, a folder handle, an unknown handle, a bad handle.
    let mut wrong = IMPORT_FILE1_KEY;
    wrong[1] ^= 1;
    assert!(matches!(
        client.save_folder_file(FOLDER_LINK, "IMPFILE1", &wrong, ROOT),
        Err(QuarkError::Mega(MegaFailure::KeyInvalid))
    ));
    for handle in ["IMPSUB01", "NOSUCHND", "bad handle!"] {
        assert!(client
            .save_folder_file(FOLDER_LINK, handle, &IMPORT_FILE1_KEY, ROOT)
            .is_err());
    }
    assert_eq!(http.commands("p").len(), 1);
}

#[test]
fn saving_goes_only_into_folders_of_the_drive_and_surfaces_error_numbers() {
    import_fixture!(http, client, public_default, echo_created);
    for target in ["__versa_down_mega_recycle_bin__", "NOSUCHND", ""] {
        assert!(client
            .save_public_link(&file_link(&LINK_FILE_KEY), target)
            .is_err());
    }
    assert!(http.commands("p").is_empty());
    for (number, text) in [
        (-17, "cloud.error.mega.storageFull"),
        (-11, "cloud.error.mega.accessDenied"),
    ] {
        fn reject_p(_: &Value) -> (u16, String) {
            code(-17)
        }
        fn deny_p(_: &Value) -> (u16, String) {
            code(-11)
        }
        let account: fn(&Value) -> (u16, String) = if number == -17 { reject_p } else { deny_p };
        import_fixture!(refusing, client, public_default, account);
        let error = client
            .save_public_link(&file_link(&LINK_FILE_KEY), TARGET)
            .unwrap_err();
        assert_eq!(error_text(error), text);
    }
}

#[test]
fn parsed_links_save_like_constructed_ones() {
    let url = format!(
        "https://mega.nz/file/{FILE_LINK}#{}",
        encode_base64url(&LINK_FILE_KEY)
    );
    import_fixture!(http, client, public_default, echo_created);
    let link = parse_mega_link(&url).unwrap();
    assert_eq!(
        client.save_public_link(&link, "0").unwrap().name,
        "linked.iso"
    );
    assert_eq!(http.commands("p")[0].command["t"], ROOT);
}
