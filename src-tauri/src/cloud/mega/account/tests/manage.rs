//! Changing the account through an in-process fake of MEGA's command
//! endpoint: every command's request body, and how error numbers surface.
//! Keys and listings come from the independent vectors in `test_vectors.rs`
//! and `manage_vectors.rs`; the fake only decides which command gets which
//! reply and records what was sent.

use super::*;
use crate::cloud::mega::account::keys::Keyring;
use crate::cloud::mega::account::manage_vectors as vectors;
use crate::cloud::mega::account::nodes::find_node;
use crate::cloud::mega::account::seal::{
    attribute_key, name_attributes, pack_file_key, seal_attributes, wrap_key,
};
use crate::cloud::mega::crypto::{
    condensed_mac, decode_base64url, decrypt_ecb, decrypt_node_attributes, encode_base64url,
    verify_file_mac, FileKey,
};
use crate::cloud::mega::link::parse_mega_link;
use crate::cloud::{CloudProvider, CloudShareOptions, ProviderRegistry};
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

const RECYCLE: &str = "__versa_down_mega_recycle_bin__";
const INCOMING: &str = "__versa_down_mega_incoming_shares__";

// ----------------------------------------------------------------- helpers ---

fn listing_value() -> Value {
    serde_json::from_str(LISTING).unwrap()
}

fn node_mut<'a>(listing: &'a mut Value, handle: &str) -> &'a mut Value {
    listing["f"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["h"] == handle)
        .unwrap()
}

fn keyring_of(listing: &Value) -> Keyring {
    Keyring::new(USER_HANDLE, &MASTER_KEY).with_listing_shares(listing)
}

fn node_key(listing: &Value, handle: &str) -> Zeroizing<Vec<u8>> {
    find_node(listing, &keyring_of(listing), handle)
        .unwrap()
        .key
}

/// Replaces a node's sealed attributes (keeps its key).
fn set_attributes(listing: &mut Value, handle: &str, attributes: Value) {
    let key = node_key(listing, handle);
    let map: Map<String, Value> = attributes.as_object().unwrap().clone();
    let sealed = seal_attributes(&attribute_key(&key).unwrap(), &map).unwrap();
    node_mut(listing, handle)["a"] = json!(sealed);
}

fn add_node(listing: &mut Value, node: Value) {
    listing["f"].as_array_mut().unwrap().push(node);
}

/// A file node owned by the account with the given packed key.
fn own_file(handle: &str, parent: &str, name: &str, key: &[u8; 32], size: u64) -> Value {
    let sealed = seal_attributes(&attribute_key(key).unwrap(), &name_attributes(name)).unwrap();
    json!({
        "h": handle, "p": parent, "u": USER_HANDLE, "t": 0, "s": size, "ts": 1700000300,
        "a": sealed,
        "k": format!("{USER_HANDLE}:{}", wrap_key(&MASTER_KEY, key).unwrap()),
    })
}

fn manage_server(
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

fn unwrapped(wrapped: &Value) -> Zeroizing<Vec<u8>> {
    let bytes = decode_base64url(wrapped.as_str().unwrap()).unwrap();
    Zeroizing::new(decrypt_ecb(&MASTER_KEY, &bytes).unwrap())
}

fn attributes_of(key: &[u8], sealed: &Value) -> Map<String, Value> {
    decrypt_node_attributes(&attribute_key(key).unwrap(), sealed.as_str().unwrap()).unwrap()
}

macro_rules! fixture {
    ($http:ident, $client:ident, $listing:expr, $handler:expr) => {
        let $http = manage_server($listing, $handler);
        let store = signed_in_store();
        let cache = ListingCache::default();
        let $client = MegaAccountClient::new(&$http, &store).with_cache(&cache);
    };
}

// ------------------------------------------------------------------ folders ---

#[test]
fn a_new_folder_is_sealed_under_its_own_key_and_wrapped_with_the_master_key() {
    fixture!(
        http,
        client,
        listing_value(),
        |command| match command["a"].as_str().unwrap() {
            "p" => ok(json!({"f": [{"h": "NEWFOLDR", "p": "ROOTNODE", "t": 1, "ts": 1700000999}]})),
            _ => code(-2),
        }
    );
    let created = client.create_directory("0", "  Reports é  ").unwrap();
    assert_eq!(created.id, "NEWFOLDR");
    assert_eq!(created.parent_id, "0");
    assert_eq!(created.name, "Reports é");
    assert_eq!(created.kind, CloudFileKind::Folder);
    assert_eq!(created.modified_at.as_deref(), Some("1700000999"));

    let sent = http.commands("p");
    assert_eq!(sent.len(), 1);
    assert!(sent[0].url.contains(&format!("sid={EXPECTED_SID}")));
    assert_eq!(sent[0].command["t"], "ROOTNODE");
    let node = &sent[0].command["n"][0];
    assert_eq!(node["h"], "xxxxxxxx");
    assert_eq!(node["t"], 1);
    let key = unwrapped(&node["k"]);
    assert_eq!(key.len(), 16);
    assert_eq!(
        attributes_of(&key, &node["a"]),
        name_attributes("Reports é")
    );
    // The folder key never goes over the wire in the clear.
    assert!(!http.all_traffic().contains(&encode_base64url(&key)));
}

#[test]
fn folders_are_created_below_folders_and_refuse_bad_targets_and_names() {
    fixture!(
        http,
        client,
        listing_value(),
        |command| match command["a"].as_str().unwrap() {
            "p" => ok(json!({"f": [{"h": "NEWFOLDR", "p": "DIRAAAAA", "t": 1}]})),
            _ => code(-2),
        }
    );
    client.create_directory("DIRAAAAA", "Child").unwrap();
    assert_eq!(http.commands("p")[0].command["t"], "DIRAAAAA");

    for (parent, name) in [
        ("0", ""),
        ("0", "   "),
        ("0", "a/b"),
        ("0", "a\\b"),
        ("0", ".."),
        ("0", "tab\there"),
    ] {
        assert!(
            matches!(
                client.create_directory(parent, name),
                Err(QuarkError::Mega(MegaFailure::InvalidName))
            ),
            "{name:?}"
        );
    }
    // The recycle bin, a file, an unknown node, the inbox and the trash tree.
    assert!(matches!(
        client.create_directory(RECYCLE, "x"),
        Err(QuarkError::Mega(MegaFailure::InvalidDestination))
    ));
    assert!(matches!(
        client.create_directory("FILEBBBB", "x"),
        Err(QuarkError::SourceFileMissing)
    ));
    assert!(matches!(
        client.create_directory("NOSUCHND", "x"),
        Err(QuarkError::SourceFileMissing)
    ));
    assert!(matches!(
        client.create_directory("INBOXNOD", "x"),
        Err(QuarkError::SourceFileMissing)
    ));
    assert_eq!(http.commands("p").len(), 1);
}

#[test]
fn a_node_created_inside_an_exported_folder_gets_its_key_for_the_share() {
    fixture!(
        http,
        client,
        listing_value(),
        |command| match command["a"].as_str().unwrap() {
            "p" => ok(json!({"f": [{"h": "NEWFOLDR", "p": "SHAREHDL", "t": 1}]})),
            "k" => ok(json!(0)),
            _ => code(-2),
        }
    );
    client.create_directory("SHAREHDL", "Inside").unwrap();
    let created_key = unwrapped(&http.commands("p")[0].command["n"][0]["k"]);
    let share_key = keyring_of(&listing_value()).share_key("SHAREHDL").unwrap();

    let keys = http.commands("k");
    assert_eq!(keys.len(), 1);
    let cr = &keys[0].command["cr"];
    assert_eq!(cr[0], json!(["SHAREHDL"]));
    assert_eq!(cr[1], json!(["NEWFOLDR"]));
    assert_eq!(cr[2][0], 0);
    assert_eq!(cr[2][1], 0);
    let wrapped = decode_base64url(cr[2][2].as_str().unwrap()).unwrap();
    assert_eq!(
        decrypt_ecb(&share_key, &wrapped).unwrap(),
        created_key.to_vec()
    );

    // Outside an exported folder no key request is sent.
    client.create_directory("DIRAAAAA", "Plain").unwrap();
    assert_eq!(http.commands("k").len(), 1);
}

// ------------------------------------------------------------------- rename ---

#[test]
fn rename_reseals_the_attributes_and_keeps_the_ones_it_does_not_know() {
    let mut listing = listing_value();
    set_attributes(
        &mut listing,
        "FILEBBBB",
        json!({"c": "FINGERPRINT", "n": "movie.bin", "lbl": 3}),
    );
    let key = node_key(&listing, "FILEBBBB");
    fixture!(http, client, listing, |_| ok(json!(0)));

    client.rename("FILEBBBB", " renamed é.bin ").unwrap();
    let sent = http.commands("a");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].command["n"], "FILEBBBB");
    let attributes = attributes_of(&key, &sent[0].command["at"]);
    assert_eq!(attributes["n"], "renamed é.bin");
    assert_eq!(attributes["c"], "FINGERPRINT");
    assert_eq!(attributes["lbl"], 3);
}

#[test]
fn rename_works_on_folders_and_in_the_rubbish_but_never_on_special_or_foreign_nodes() {
    fixture!(http, client, listing_value(), |_| ok(json!(0)));
    client.rename("DIRAAAAA", "Docs").unwrap();
    client.rename("FILEINTR", "binned.txt").unwrap();
    assert_eq!(http.commands("a").len(), 2);
    let folder_key = node_key(&listing_value(), "DIRAAAAA");
    assert_eq!(
        attributes_of(&folder_key, &http.commands("a")[0].command["at"])["n"],
        "Docs"
    );

    for special in ["ROOTNODE", "INBOXNOD", "TRASHNOD"] {
        assert!(matches!(
            client.rename(special, "x"),
            Err(QuarkError::Mega(MegaFailure::AccessDenied))
        ));
    }
    // Owned by someone else: its key is not ours, so it cannot be read.
    assert!(matches!(
        client.rename("FILEZZZZ", "x"),
        Err(QuarkError::SourceFileMissing)
    ));
    assert!(matches!(
        client.rename("NOSUCHND", "x"),
        Err(QuarkError::SourceFileMissing)
    ));
    assert!(matches!(
        client.rename("FILEBBBB", "bad/name"),
        Err(QuarkError::Mega(MegaFailure::InvalidName))
    ));
    assert_eq!(http.commands("a").len(), 2);
}

// --------------------------------------------------------------------- move ---

#[test]
fn move_sends_one_command_per_node_and_skips_nodes_already_there() {
    fixture!(http, client, listing_value(), |_| ok(json!(0)));
    client
        .move_files(&["FILEBBBB".into(), "SHAREHDL".into()], "DIRAAAAA")
        .unwrap();
    let moves = http.commands("m");
    assert_eq!(moves.len(), 2);
    assert!(moves.iter().all(|sent| sent.command.get("cr").is_none()));
    assert_eq!(moves[0].command["n"], "FILEBBBB");
    assert_eq!(moves[0].command["t"], "DIRAAAAA");
    assert_eq!(moves[1].command["n"], "SHAREHDL");
    // Already in the destination: nothing to do. The drive root id works too.
    client.move_files(&["FILEAAAA".into()], "DIRAAAAA").unwrap();
    client.move_files(&["FILEAAAA".into()], "0").unwrap();
    let moves = http.commands("m");
    assert_eq!(moves.len(), 3);
    assert_eq!(moves[2].command["t"], "ROOTNODE");
}

#[test]
fn move_refuses_cycles_the_rubbish_and_other_trees_before_sending_anything() {
    fixture!(http, client, listing_value(), |_| ok(json!(0)));
    // Into itself and into its own subfolder.
    for target in ["DIRAAAAA", "DIRBBBBB"] {
        assert!(matches!(
            client.move_files(&["DIRAAAAA".into()], target),
            Err(QuarkError::Mega(MegaFailure::InvalidMove))
        ));
    }
    assert!(matches!(
        client.move_files(&["FILEBBBB".into()], RECYCLE),
        Err(QuarkError::Mega(MegaFailure::InvalidDestination))
    ));
    // Trashed nodes leave the bin through restore, not move.
    assert!(matches!(
        client.move_files(&["FILEINTR".into()], "DIRAAAAA"),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert!(matches!(
        client.move_files(&["ROOTNODE".into()], "DIRAAAAA"),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert!(matches!(
        client.move_files(&["NOSUCHND".into()], "DIRAAAAA"),
        Err(QuarkError::SourceFileMissing)
    ));
    assert!(http.commands("m").is_empty());
}


/// Inspect the actual wire keys independently of the request builder.
fn assert_move_share_keys(command: &Value, listing: &Value, shares: &[&str], handles: &[&str]) {
    let cr = &command["cr"];
    assert_eq!(cr[0], json!(shares));
    assert_eq!(cr[1], json!(handles));
    let requests = cr[2].as_array().unwrap();
    assert_eq!(requests.len(), shares.len() * handles.len() * 3);
    for request in requests.chunks_exact(3) {
        let share = shares[request[0].as_u64().unwrap() as usize];
        let handle = handles[request[1].as_u64().unwrap() as usize];
        let key = keyring_of(listing).share_key(share).unwrap();
        let wrapped = decode_base64url(request[2].as_str().unwrap()).unwrap();
        assert_eq!(decrypt_ecb(&key, &wrapped).unwrap(), node_key(listing, handle).to_vec());
    }
}

fn add_legacy_share(listing: &mut Value, handle: &str, key: &[u8; 16]) {
    listing["ok"].as_array_mut().unwrap().push(json!({
        "h": handle,
        "k": wrap_key(&MASTER_KEY, key).unwrap(),
        "ha": wrap_key(&MASTER_KEY, format!("{handle}{handle}").as_bytes()).unwrap(),
    }));
}

#[test]
fn move_into_a_share_carries_every_subtree_key_on_the_move_request() {
    for (source, expected) in [
        ("FILEBBBB", vec!["FILEBBBB"]),
        ("DIRAAAAA", vec!["DIRAAAAA", "DIRBBBBB", "FILEAAAA", "DEEPFILE"]),
    ] {
        let mut listing = listing_value();
        add_node(&mut listing, own_file("DEEPFILE", "DIRBBBBB", "deep.bin", &DOWNLOAD_NODE_KEY, 3000));
        fixture!(http, client, listing.clone(), |_| ok(json!(0)));
        client.move_files(&[source.into()], "SHAREHDL").unwrap();
        let moves = http.commands("m");
        assert_eq!(moves.len(), 1);
        assert_move_share_keys(&moves[0].command, &listing, &["SHAREHDL"], &expected);
        assert!(http.commands("k").is_empty(), "publishing keys is part of the move, not a later request");
        for handle in expected {
            assert!(!http.all_traffic().contains(&encode_base64url(&node_key(&listing, handle))));
        }
    }
}

#[test]
fn move_within_between_and_below_nested_shares_wraps_for_all_destination_shares() {
    for (nested, own_share, expected) in [
        (true, false, vec!["SHAREHDL"]),
        (false, true, vec!["DIRAAAAA"]),
        (true, true, vec!["DIRAAAAA", "SHAREHDL"]),
    ] {
        let mut listing = listing_value();
        if nested {
            node_mut(&mut listing, "DIRAAAAA")["p"] = json!("SHAREHDL");
        }
        if own_share {
            add_legacy_share(&mut listing, "DIRAAAAA", &[0x63; 16]);
        }
        fixture!(http, client, listing.clone(), |_| ok(json!(0)));
        // This file's only key is encrypted by its original SHAREHDL share.
        client.move_files(&["FILECCCC".into()], "DIRAAAAA").unwrap();
        assert_move_share_keys(&http.commands("m")[0].command, &listing, &expected, &["FILECCCC"]);
        assert!(http.commands("k").is_empty());

        // A true no-op emits neither a move nor any extra key publication.
        client.move_files(&["FILECCCC".into()], "SHAREHDL").unwrap();
        assert_eq!(http.commands("m").len(), 1);
    }
}

#[test]
fn restore_into_an_exported_folder_also_carries_the_node_key() {
    let mut listing = listing_value();
    set_attributes(&mut listing, "FILEINTR", json!({"n": "trashed.txt", "rr": "SHAREHDL"}));
    fixture!(http, client, listing.clone(), |_| ok(json!(0)));
    client.restore(&["FILEINTR".into()]).unwrap();
    assert_move_share_keys(&http.commands("m")[0].command, &listing, &["SHAREHDL"], &["FILEINTR"]);
    assert!(http.commands("k").is_empty());
}

#[test]
fn move_to_a_share_stops_before_writing_when_a_subtree_key_is_unreadable() {
    let mut listing = listing_value();
    node_mut(&mut listing, "DIRBBBBB")["k"] = json!("UNKNOWN1:invalid");
    fixture!(http, client, listing, |_| ok(json!(0)));
    assert!(matches!(
        client.move_files(&["DIRAAAAA".into()], "SHAREHDL"),
        Err(QuarkError::InvalidResponse)
    ));
    assert!(http.commands("m").is_empty() && http.commands("k").is_empty());
}

#[test]
fn move_to_an_exported_folder_without_its_key_never_moves_the_node() {
    let mut listing = listing_value();
    listing["ph"] = json!([{"h": "DIRAAAAA", "ph": "PUBLICAA"}]);
    fixture!(http, client, listing, |command| match command["a"].as_str().unwrap() {
        "uga" => code(-9),
        _ => ok(json!(0)),
    });
    assert!(matches!(
        client.move_files(&["FILEBBBB".into()], "DIRAAAAA"),
        Err(QuarkError::Mega(MegaFailure::KeysUnsupported))
    ));
    assert!(http.commands("m").is_empty() && http.commands("k").is_empty());
}

#[test]
fn a_refused_shared_move_is_reported_without_a_second_key_request() {
    fixture!(http, client, listing_value(), |_| code(-11));
    assert!(matches!(
        client.move_files(&["FILEBBBB".into()], "SHAREHDL"),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert_eq!(http.commands("m").len(), 1);
    assert!(http.commands("k").is_empty());
}

// ------------------------------------------------------------- rubbish bin ---

#[test]
fn trash_remembers_the_origin_then_moves_to_the_rubbish() {
    let mut listing = listing_value();
    set_attributes(
        &mut listing,
        "FILEAAAA",
        json!({"n": "notes 笔记.txt", "c": "KEEPME"}),
    );
    let key = node_key(&listing, "FILEAAAA");
    fixture!(http, client, listing, |_| ok(json!(0)));

    client
        .trash(&["FILEAAAA".into(), "FILEBBBB".into()])
        .unwrap();
    let all: Vec<(String, Value)> = {
        // Commands in the order they were sent.
        let mut recorded: Vec<(String, Value)> = Vec::new();
        for name in ["a", "m"] {
            for sent in http.commands(name) {
                recorded.push((name.into(), sent.command));
            }
        }
        recorded
    };
    assert_eq!(all.iter().filter(|(name, _)| name == "a").count(), 2);
    let moves: Vec<&Value> = all
        .iter()
        .filter(|(name, _)| name == "m")
        .map(|(_, c)| c)
        .collect();
    assert_eq!(moves.len(), 2);
    assert!(moves.iter().all(|command| command["t"] == "TRASHNOD"));

    let first = http.commands("a")[0].command.clone();
    assert_eq!(first["n"], "FILEAAAA");
    let attributes = attributes_of(&key, &first["at"]);
    assert_eq!(attributes["rr"], "DIRAAAAA");
    assert_eq!(attributes["c"], "KEEPME");
    assert_eq!(attributes["n"], "notes 笔记.txt");
    let second = http.commands("a")[1].command.clone();
    assert_eq!(
        attributes_of(&node_key(&listing_value(), "FILEBBBB"), &second["at"])["rr"],
        "ROOTNODE"
    );
}

#[test]
fn trash_skips_what_is_already_binned_and_refuses_roots_and_foreign_nodes() {
    fixture!(http, client, listing_value(), |_| ok(json!(0)));
    client.trash(&["FILEINTR".into()]).unwrap();
    assert!(http.commands("m").is_empty() && http.commands("a").is_empty());
    for special in ["ROOTNODE", "TRASHNOD", "INBOXNOD"] {
        assert!(matches!(
            client.trash(&[special.into()]),
            Err(QuarkError::Mega(MegaFailure::AccessDenied))
        ));
    }
    assert!(matches!(
        client.trash(&["NOSUCHND".into()]),
        Err(QuarkError::SourceFileMissing)
    ));
    assert!(http.commands("m").is_empty());
}

#[test]
fn the_recycle_bin_lists_its_direct_entries_under_the_registry_id() {
    fixture!(http, client, listing_value(), |_| code(-2));
    let bin = client.list(RECYCLE).unwrap();
    assert_eq!(bin.id, RECYCLE);
    assert_eq!(names(&bin), vec![("trashed.bin", CloudFileKind::File)]);
    assert_eq!(bin.items[0].id, "FILEINTR");
    assert_eq!(bin.items[0].parent_id, RECYCLE);
    // A trashed folder's content is browsable by its handle.
    assert!(client.list("TRASHNOD").is_ok());
    assert!(http.commands("m").is_empty());
}

#[test]
fn delete_only_removes_direct_entries_of_the_rubbish_for_good() {
    fixture!(http, client, listing_value(), |_| ok(json!(0)));
    client.delete(&["FILEINTR".into()]).unwrap();
    let deletes = http.commands("d");
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].command["n"], "FILEINTR");

    for bad in [
        vec![],
        vec!["FILEBBBB".into()],
        vec!["FILEINTR".into(), "DIRAAAAA".into()],
        vec!["".into()],
    ] {
        assert!(
            matches!(client.delete(&bad), Err(QuarkError::InvalidInput(_))),
            "{bad:?}"
        );
    }
    assert_eq!(http.commands("d").len(), 1);
}

#[test]
fn restore_goes_back_to_the_recorded_folder_or_falls_back_to_the_root() {
    // No origin recorded: the drive root.
    fixture!(http, client, listing_value(), |_| ok(json!(0)));
    client.restore(&["FILEINTR".into()]).unwrap();
    assert_eq!(http.commands("m")[0].command["t"], "ROOTNODE");
    assert_eq!(http.commands("m")[0].command["n"], "FILEINTR");
    assert!(matches!(
        client.restore(&["FILEBBBB".into()]),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.restore(&[]),
        Err(QuarkError::InvalidInput(_))
    ));

    // Origin recorded and still a folder of the drive.
    for (origin, expected) in [
        ("DIRAAAAA", "DIRAAAAA"),
        // A file, a missing node, a node in the rubbish and garbage all fall back.
        ("FILEBBBB", "ROOTNODE"),
        ("GONEGONE", "ROOTNODE"),
        ("TRASHNOD", "ROOTNODE"),
        ("bad handle!", "ROOTNODE"),
    ] {
        let mut listing = listing_value();
        set_attributes(
            &mut listing,
            "FILEINTR",
            json!({"n": "trashed.txt", "rr": origin}),
        );
        fixture!(http, client, listing, |_| ok(json!(0)));
        client.restore(&["FILEINTR".into()]).unwrap();
        assert_eq!(
            http.commands("m")[0].command["t"],
            expected,
            "origin {origin}"
        );
    }
}

// ------------------------------------------------------------ error mapping ---

#[test]
fn change_errors_map_to_the_projects_failures() {
    let keyed: [(i64, &str); 4] = [
        (-11, "cloud.error.mega.accessDenied"),
        (-17, "cloud.error.mega.storageFull"),
        (-10, "cloud.error.mega.invalidMove"),
        (-16, "cloud.error.mega.accountBlocked"),
    ];
    for (number, key) in keyed {
        fixture!(http, client, listing_value(), move |_| code(number));
        let error = client
            .move_files(&["FILEBBBB".into()], "DIRAAAAA")
            .unwrap_err();
        assert_eq!(error_text(error), key, "{number}");
        assert_eq!(http.commands("m").len(), 1, "{number} is not retried");
    }
    fixture!(http, client, listing_value(), |_| code(-9));
    assert!(matches!(
        client.move_files(&["FILEBBBB".into()], "DIRAAAAA"),
        Err(QuarkError::SourceFileMissing)
    ));
    fixture!(http, client, listing_value(), |_| code(-15));
    assert!(matches!(
        client.move_files(&["FILEBBBB".into()], "DIRAAAAA"),
        Err(QuarkError::AuthorizationExpired)
    ));
    // A number nobody mapped never echoes anything but the number.
    fixture!(http, client, listing_value(), |_| code(-77));
    assert_eq!(
        client.rename("FILEBBBB", "x").unwrap_err().to_string(),
        "cloud.error.mega.apiError:-77"
    );
}

#[test]
fn try_again_and_rate_limit_answers_are_retried_then_reported_as_busy() {
    for number in [-3, -4] {
        fixture!(http, client, listing_value(), move |_| code(number));
        let error = client
            .move_files(&["FILEBBBB".into()], "DIRAAAAA")
            .unwrap_err();
        assert!(
            matches!(error, QuarkError::Mega(MegaFailure::ServiceBusy)),
            "{number}"
        );
        // One attempt plus four retries.
        assert_eq!(http.commands("m").len(), 5, "{number}");
    }
    // A gateway hiccup (HTTP 502) is retried the same way and may recover.
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    fixture!(http, client, listing_value(), move |_| {
        if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
            (502, String::new())
        } else {
            ok(json!(0))
        }
    });
    client.move_files(&["FILEBBBB".into()], "DIRAAAAA").unwrap();
    assert_eq!(http.commands("m").len(), 3);
}

#[test]
fn a_change_always_starts_from_a_fresh_listing_and_leaves_none_behind() {
    fixture!(http, client, listing_value(), |_| ok(json!(0)));
    client.list("0").unwrap();
    client.list("0").unwrap();
    assert_eq!(http.commands("f").len(), 1, "reads share one listing");
    client.rename("FILEBBBB", "a.bin").unwrap();
    assert_eq!(http.commands("f").len(), 2, "a change re-fetches");
    client.list("0").unwrap();
    assert_eq!(
        http.commands("f").len(),
        3,
        "and the cache is dropped afterwards"
    );
}

// ------------------------------------------------------------------ sharing ---

#[test]
fn a_file_link_carries_the_file_key_in_the_fragment() {
    fixture!(
        http,
        client,
        listing_value(),
        |command| match command["a"].as_str().unwrap() {
            "l" => (200, json!(["PUBHDL12"]).to_string()),
            _ => code(-2),
        }
    );
    let options = CloudShareOptions {
        name: "x".into(),
        pass_code: None,
        generate_pass_code: false,
        expires_in_days: None,
    };
    let share = client.create_share(&["FILEBBBB".into()], &options).unwrap();
    assert_eq!(share.id, "FILEBBBB");
    assert_eq!(share.name, "movie.bin");
    assert_eq!(share.kind, CloudFileKind::File);
    assert_eq!(share.source_file_ids, vec!["FILEBBBB".to_string()]);
    assert_eq!(share.pass_code, None);
    assert_eq!(
        share.url,
        format!(
            "https://mega.nz/file/PUBHDL12#{}",
            encode_base64url(&DOWNLOAD_NODE_KEY)
        )
    );
    let link = parse_mega_link(&share.url).unwrap();
    assert_eq!(link.id, "PUBHDL12");
    assert_eq!(link.key.as_slice(), DOWNLOAD_NODE_KEY.as_slice());
    assert_eq!(http.commands("l")[0].command["n"], "FILEBBBB");
    assert!(http.commands("l")[0].command.get("d").is_none());
    assert!(http.commands("s2").is_empty());
}

#[test]
fn links_refuse_pass_codes_expiry_several_items_and_foreign_nodes() {
    fixture!(http, client, listing_value(), |_| (
        200,
        json!(["PUBHDL12"]).to_string()
    ));
    let plain = CloudShareOptions {
        name: "x".into(),
        pass_code: None,
        generate_pass_code: false,
        expires_in_days: None,
    };
    for options in [
        CloudShareOptions {
            pass_code: Some("abcd".into()),
            ..plain.clone()
        },
        CloudShareOptions {
            generate_pass_code: true,
            ..plain.clone()
        },
        CloudShareOptions {
            expires_in_days: Some(7),
            ..plain.clone()
        },
    ] {
        assert!(matches!(
            client.create_share(&["FILEBBBB".into()], &options),
            Err(QuarkError::Mega(MegaFailure::ShareOptionsUnsupported))
        ));
    }
    assert!(matches!(
        client.create_share(&["FILEBBBB".into(), "DIRAAAAA".into()], &plain),
        Err(QuarkError::Mega(MegaFailure::ShareSingleOnly))
    ));
    assert!(matches!(
        client.create_share(&[], &plain),
        Err(QuarkError::Mega(MegaFailure::ShareSingleOnly))
    ));
    assert!(matches!(
        client.create_share(&["FILEINTR".into()], &plain),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert!(http.commands("l").is_empty());
}

#[test]
fn a_folder_without_a_share_key_is_not_linked_and_nothing_is_sent() {
    // An account without MEGA's key manager (no `^!keys`, `-9`) cannot record a
    // new share key, so a new folder link is reported instead of being half
    // created.
    fixture!(
        http,
        client,
        listing_value(),
        |command| match command["a"].as_str().unwrap() {
            "uga" => code(-9),
            _ => code(-2),
        }
    );
    let options = CloudShareOptions {
        name: "x".into(),
        pass_code: None,
        generate_pass_code: false,
        expires_in_days: None,
    };
    let error = client
        .create_share(&["DIRAAAAA".into()], &options)
        .unwrap_err();
    assert_eq!(error_text(error), "cloud.error.mega.folderShareUnsupported");
    assert!(http.commands("s2").is_empty());
    assert!(http.commands("l").is_empty());
    assert!(http.commands("upv").is_empty());
}

#[test]
fn an_already_exported_folder_reuses_its_share_key() {
    fixture!(
        http,
        client,
        listing_value(),
        |command| match command["a"].as_str().unwrap() {
            "l" => (200, json!(["FOLDPUB2"]).to_string()),
            _ => code(-2),
        }
    );
    let options = CloudShareOptions {
        name: "x".into(),
        pass_code: None,
        generate_pass_code: false,
        expires_in_days: None,
    };
    let share = client.create_share(&["SHAREHDL".into()], &options).unwrap();
    assert!(http.commands("s2").is_empty());
    let key = keyring_of(&listing_value()).share_key("SHAREHDL").unwrap();
    assert_eq!(
        parse_mega_link(&share.url).unwrap().key.as_slice(),
        key.as_slice()
    );
}

#[test]
fn my_shares_lists_exported_nodes_with_their_links_and_cancel_withdraws_them() {
    let mut listing = listing_value();
    // The listing's `ph` array: node handle, public handle, creation time.
    // A link of a trashed node and one of an unknown node are not shown.
    listing["ph"] = json!([
        {"h": "FILEBBBB", "ph": "FILEPUB1", "ts": 1700001000},
        {"h": "SHAREHDL", "ph": "FOLDPUB2", "ts": 1700002000},
        {"h": "FILEINTR", "ph": "TRASHPUB", "ts": 1700003000},
        {"h": "GONEGONE", "ph": "STALEPUB", "ts": 1700004000},
    ]);
    fixture!(
        http,
        client,
        listing,
        |command| match command["a"].as_str().unwrap() {
            // No key manager: only the link is withdrawn.
            "uga" => code(-9),
            _ => ok(json!(0)),
        }
    );

    let shares = client.list_shares().unwrap();
    assert_eq!(shares.len(), 2);
    let folder = shares.iter().find(|share| share.id == "SHAREHDL").unwrap();
    assert_eq!(folder.kind, CloudFileKind::Folder);
    assert_eq!(folder.name, "Shared folder");
    let key = keyring_of(&listing_value()).share_key("SHAREHDL").unwrap();
    assert_eq!(
        folder.url,
        format!(
            "https://mega.nz/folder/FOLDPUB2#{}",
            encode_base64url(&*key)
        )
    );
    assert_eq!(folder.created_at.as_deref(), Some("1700002000"));
    let file = shares.iter().find(|share| share.id == "FILEBBBB").unwrap();
    assert_eq!(
        file.url,
        format!(
            "https://mega.nz/file/FILEPUB1#{}",
            encode_base64url(&DOWNLOAD_NODE_KEY)
        )
    );

    client
        .cancel_shares(&["FILEBBBB".into(), "SHAREHDL".into()])
        .unwrap();
    let removals = http.commands("l");
    assert_eq!(removals.len(), 2);
    assert!(removals.iter().all(|sent| sent.command["d"] == 1));
    assert_eq!(removals[0].command["n"], "FILEBBBB");
    assert!(matches!(
        client.cancel_shares(&["FILEINTR".into()]),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert_eq!(http.commands("l").len(), 2);
    assert!(http.commands("s2").is_empty() && http.commands("upv").is_empty());
}

#[test]
fn cancelling_a_link_that_is_already_gone_succeeds() {
    // MEGA answers `-9` when the node has no link to withdraw.
    fixture!(http, client, listing_value(), |_| code(-9));
    client.cancel_shares(&["FILEBBBB".into()]).unwrap();
    assert_eq!(http.commands("l").len(), 1);
}

// ------------------------------------------------------------------- upload ---

#[test]
fn the_registry_declares_what_the_client_implements() {
    let capabilities = ProviderRegistry::global()
        .get(CloudProvider::Mega)
        .capabilities;
    assert!(capabilities.create_directory && capabilities.rename && capabilities.r#move);
    assert!(capabilities.trash && capabilities.delete && capabilities.restore);
    assert!(capabilities.upload && capabilities.download);
    assert!(capabilities.share_create && capabilities.share_list && capabilities.share_cancel);
    // Copy re-encrypts every key; links have no pass code or expiry. Saving a
    // public link into the drive goes through the share transfer protocol.
    assert!(capabilities.copy);
    assert!(capabilities.share_transfer && capabilities.recorded_transfer_resolve);
    assert!(!capabilities.share_configurable);
    assert!(!capabilities.share_update_pass_code);
    let descriptor = ProviderRegistry::global().get(CloudProvider::Mega);
    assert_eq!(descriptor.incoming_shares_directory_id, Some(INCOMING));
}

// ----------------------------------------------------------- incoming shares ---

fn incoming_session_store() -> MemoryCredentialStore {
    let store = MemoryCredentialStore::default();
    MegaSession::new(
        "SID".into(),
        &vectors::MASTER_KEY,
        vectors::USER_HANDLE.into(),
        "m@x.io".into(),
    )
    .save(&store)
    .unwrap();
    store
}

fn incoming_server(privk: &'static str) -> FakeMega {
    FakeMega::new(move |_, command| match command["a"].as_str().unwrap() {
        "f" => ok(serde_json::from_str(vectors::INCOMING_LISTING).unwrap()),
        "ug" => ok(json!({"u": vectors::USER_HANDLE, "email": "m@x.io", "privk": privk})),
        "g" => ok(json!({"g": "https://gfs.example/dl/INC", "s": 1234})),
        _ => ok(json!(0)),
    })
}

#[test]
fn shares_from_other_users_open_with_the_rsa_decrypted_share_key() {
    let http = incoming_server(vectors::PRIVK_B64);
    let store = incoming_session_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);

    let shared = client.list(INCOMING).unwrap();
    assert_eq!(shared.id, INCOMING);
    assert_eq!(
        names(&shared),
        vec![(vectors::INCOMING_ROOT_NAME, CloudFileKind::Folder)]
    );
    assert_eq!(shared.items[0].id, vectors::INCOMING_ROOT_HANDLE);
    assert_eq!(shared.items[0].parent_id, INCOMING);

    let inside = client.list(vectors::INCOMING_ROOT_HANDLE).unwrap();
    assert_eq!(
        names(&inside),
        vec![(vectors::INCOMING_FILE_NAME, CloudFileKind::File)]
    );
    assert_eq!(inside.items[0].size, Some(1234));
    // The drive root shows only the account's own folder.
    let root = client.list("0").unwrap();
    assert_eq!(
        names(&root),
        vec![(vectors::OWN_FOLDER_NAME, CloudFileKind::Folder)]
    );
    // The RSA key was needed once for this listing.
    assert_eq!(http.commands("ug").len(), 1);

    // Downloading from a share works with the same key.
    let resolved = client
        .resolve_download(vectors::INCOMING_FILE_HANDLE)
        .unwrap();
    assert_eq!(resolved.name, vectors::INCOMING_FILE_NAME);
    assert_eq!(resolved.url, "https://gfs.example/dl/INC");
}

#[test]
fn nodes_of_other_users_are_read_only_here() {
    let http = incoming_server(vectors::PRIVK_B64);
    let store = incoming_session_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    let file = vectors::INCOMING_FILE_HANDLE.to_owned();
    let folder = vectors::INCOMING_ROOT_HANDLE.to_owned();
    assert!(matches!(
        client.rename(&file, "x"),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert!(matches!(
        client.trash(std::slice::from_ref(&file)),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert!(matches!(
        client.move_files(std::slice::from_ref(&file), "0"),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert!(matches!(
        client.create_directory(&folder, "x"),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    assert!(matches!(
        client.create_share(
            &[file],
            &CloudShareOptions {
                name: "x".into(),
                pass_code: None,
                generate_pass_code: false,
                expires_in_days: None
            }
        ),
        Err(QuarkError::Mega(MegaFailure::AccessDenied))
    ));
    for name in ["a", "m", "p", "d", "l", "s2"] {
        assert!(http.commands(name).is_empty(), "{name}");
    }
}

#[test]
fn a_key_that_does_not_open_the_rsa_blob_hides_the_shares_without_failing_the_drive() {
    // The listing belongs to the vector account, but the private key blob is
    // for another master key: shares stay invisible, the drive still lists.
    let http = incoming_server("AAAAAAAAAAAAAAAAAAAAAA");
    let store = incoming_session_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    assert!(client.list(INCOMING).unwrap().items.is_empty());
    assert_eq!(client.list("0").unwrap().items.len(), 1);
}

#[test]
fn an_account_without_incoming_shares_never_asks_for_its_rsa_key() {
    fixture!(http, client, listing_value(), |_| code(-2));
    assert!(client.list(INCOMING).unwrap().items.is_empty());
    assert!(http.commands("ug").is_empty());
}

// ------------------------------------------------------------ verification ---

#[test]
fn an_account_download_is_checked_against_the_mac_in_its_node_key() {
    let plain: Vec<u8> = (0..300_000usize)
        .map(|i| ((i * 31 + 7) & 0xFF) as u8)
        .collect();
    let aes_key = [9u8; 16];
    let nonce = [4u8; 8];
    let mac = condensed_mac(
        &FileKey {
            aes_key: Zeroizing::new(aes_key),
            nonce,
            meta_mac: [0; 8],
        },
        plain.as_slice(),
        plain.len() as u64,
    )
    .unwrap();
    let packed = pack_file_key(&aes_key, &nonce, &mac);
    assert!(verify_file_mac(
        &FileKey::from_bytes(&*packed).unwrap(),
        plain.as_slice(),
        plain.len() as u64
    )
    .unwrap());

    let mut listing = listing_value();
    add_node(
        &mut listing,
        own_file("VERIFYME", "ROOTNODE", "v.bin", &packed, plain.len() as u64),
    );
    fixture!(http, client, listing, |_| code(-2));
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.bin");
    std::fs::write(&good, &plain).unwrap();
    client.verify_download("VERIFYME", &good).unwrap();

    // One flipped byte, a truncated file and an extended one are rejected.
    let mut tampered = plain.clone();
    tampered[150_000] ^= 1;
    let flipped = dir.path().join("flipped.bin");
    std::fs::write(&flipped, &tampered).unwrap();
    let short = dir.path().join("short.bin");
    std::fs::write(&short, &plain[..299_999]).unwrap();
    let long = dir.path().join("long.bin");
    std::fs::write(&long, [plain.as_slice(), &[0]].concat()).unwrap();
    for bad in [flipped, short, long] {
        assert!(matches!(
            client.verify_download("VERIFYME", &bad),
            Err(QuarkError::Mega(MegaFailure::IntegrityFailed))
        ));
    }
    // A check that cannot run is not a verdict on the file.
    assert!(matches!(
        client.verify_download("VERIFYME", &dir.path().join("missing.bin")),
        Err(QuarkError::SourceFileMissing)
    ));
    assert!(matches!(
        client.verify_download("NOSUCHND", &good),
        Err(QuarkError::SourceFileMissing)
    ));
    // The key never goes over the wire.
    assert!(!http.all_traffic().contains(&encode_base64url(&*packed)));
}
