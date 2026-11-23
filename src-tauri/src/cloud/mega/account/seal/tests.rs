//! Sealing against the independent Python vectors (`manage_vectors.rs`).

use super::*;
use crate::cloud::mega::account::manage_vectors::*;
use crate::cloud::mega::crypto::{decode_base64url, decrypt_node_attributes, encode_base64url};

fn attributes(json: &str) -> Map<String, Value> {
    serde_json::from_str::<Value>(json)
        .unwrap()
        .as_object()
        .unwrap()
        .clone()
}

fn case_key(kind: &str) -> Zeroizing<[u8; 16]> {
    match kind {
        "folder" => Zeroizing::new(ATTR_FOLDER_KEY),
        "file" => attribute_key(&ATTR_FILE_KEY).unwrap(),
        other => panic!("unknown key kind {other}"),
    }
}

#[test]
fn sealed_attributes_equal_the_independent_vectors_and_open_again() {
    for (json, kind, expected) in ATTR_CASES {
        let key = case_key(kind);
        let map = attributes(json);
        assert_eq!(
            seal_attributes(&key, &map).as_deref(),
            Some(*expected),
            "{json}"
        );
        assert_eq!(decrypt_node_attributes(&key, expected), Some(map));
    }
}

#[test]
fn a_file_seals_under_the_folded_key_and_a_folder_under_its_own() {
    let folded = attribute_key(&ATTR_FILE_KEY).unwrap();
    for (index, byte) in folded.iter().enumerate() {
        assert_eq!(*byte, ATTR_FILE_KEY[index] ^ ATTR_FILE_KEY[index + 16]);
    }
    assert_eq!(*attribute_key(&ATTR_FOLDER_KEY).unwrap(), ATTR_FOLDER_KEY);
    assert!(attribute_key(&[0; 24]).is_none());
    assert!(attribute_key(&[]).is_none());
}

#[test]
fn packed_upload_keys_equal_the_vectors() {
    for (length, _, meta_mac, packed, wrapped) in UPLOAD_CASES {
        let key = pack_file_key(&UPLOAD_AES_KEY, &UPLOAD_NONCE, meta_mac);
        assert_eq!(*key, *packed, "length {length}");
        assert_eq!(wrap_key(&MASTER_KEY, &*key).as_deref(), Some(*wrapped));
        // Unpacking recovers the AES key, nonce and MAC.
        let file_key = FileKey::from_bytes(&*key).unwrap();
        assert_eq!(*file_key.aes_key, UPLOAD_AES_KEY);
        assert_eq!(file_key.nonce, UPLOAD_NONCE);
        assert_eq!(file_key.meta_mac, *meta_mac);
    }
}

#[test]
fn folder_keys_and_share_keys_wrap_like_the_vectors() {
    assert_eq!(
        wrap_key(&MASTER_KEY, &FOLDER_NODE_KEY).as_deref(),
        Some(FOLDER_NODE_KEY_WRAPPED_B64)
    );
    assert_eq!(
        wrap_key(&MASTER_KEY, &SHARE_KEY).as_deref(),
        Some(SHARE_OK_B64)
    );
    let doubled = format!("{SHARE_HANDLE}{SHARE_HANDLE}");
    assert_eq!(
        wrap_key(&MASTER_KEY, doubled.as_bytes()).as_deref(),
        Some(SHARE_HA_B64)
    );
}

#[test]
fn the_cr_request_equals_the_vector() {
    let mut handles = Vec::new();
    let mut triples = Vec::new();
    for (index, (handle, key)) in SHARE_CR_NODES.iter().enumerate() {
        handles.push(*handle);
        triples.extend([
            serde_json::json!(0),
            serde_json::json!(index),
            serde_json::json!(wrap_key(&SHARE_KEY, key).unwrap()),
        ]);
    }
    let request = serde_json::json!([[SHARE_HANDLE], handles, triples]);
    assert_eq!(request.to_string(), SHARE_CR_JSON);
}

#[test]
fn only_whole_blocks_are_encrypted_and_wrapping_needs_whole_blocks() {
    assert!(encrypt_cbc_zero_iv(&[0; 16], &[]).is_none());
    assert!(encrypt_cbc_zero_iv(&[0; 16], &[0; 17]).is_none());
    assert!(wrap_key(&[0; 16], &[0; 15]).is_none());
    assert_eq!(
        encode_base64url(&decode_base64url(SHARE_OK_B64).unwrap()),
        SHARE_OK_B64
    );
}

#[test]
fn names_follow_the_rules_mega_clients_apply() {
    for good in [
        "a.txt",
        "文档 ✓.txt",
        "x",
        "with space",
        "...",
        ".hidden",
        &"a".repeat(255),
    ] {
        assert!(validate_name(good).is_ok(), "{good:?}");
    }
    for bad in [
        "",
        " ",
        "\t",
        ".",
        "..",
        " .. ",
        "a/b",
        "a\\b",
        "line\nbreak",
        "nul\0",
        &"a".repeat(256),
    ] {
        assert!(
            matches!(
                validate_name(bad),
                Err(QuarkError::Mega(MegaFailure::InvalidName))
            ),
            "{bad:?}"
        );
    }
}
