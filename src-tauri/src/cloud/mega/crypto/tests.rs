use super::*;
use crate::cloud::mega::test_vectors::*;

/// The deterministic plaintext the vectors were generated from.
fn pattern(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| ((index * 31 + 7) & 0xFF) as u8)
        .collect()
}

fn vector_key() -> FileKey {
    FileKey::from_bytes(&FILE_KEY).unwrap()
}

#[test]
fn key_fields_match_the_independent_vector() {
    let key = vector_key();
    assert_eq!(*key.aes_key, FILE_AES_KEY);
    assert_eq!(key.nonce, FILE_NONCE);
    assert_eq!(decode_base64url(FILE_KEY_B64).unwrap(), FILE_KEY);
    assert_eq!(encode_base64url(&FILE_KEY), FILE_KEY_B64);
}

#[test]
fn key_of_the_wrong_length_is_rejected() {
    assert!(FileKey::from_bytes(&FILE_KEY[..24]).is_none());
    assert!(FileKey::from_bytes(&[0u8; 33]).is_none());
}

#[test]
fn attributes_decrypt_to_the_name_only_with_the_right_key() {
    assert_eq!(
        decrypt_node_name(&FILE_AES_KEY, FILE_ATTR_B64).as_deref(),
        Some(FILE_NAME)
    );
    let mut wrong = FILE_AES_KEY;
    wrong[0] ^= 1;
    assert_eq!(decrypt_node_name(&wrong, FILE_ATTR_B64), None);
    assert_eq!(decrypt_node_name(&FILE_AES_KEY, "not base64 !"), None);
    // A truncated blob is not a whole number of AES blocks.
    assert_eq!(decrypt_node_name(&FILE_AES_KEY, &FILE_ATTR_B64[..20]), None);
}

#[test]
fn condensed_mac_matches_every_independent_vector() {
    let key = vector_key();
    for (size, expected) in MAC_VECTORS {
        let data = pattern(*size as usize);
        let mac = condensed_mac(&key, data.as_slice(), *size).unwrap();
        assert_eq!(&mac, expected, "size {size}");
    }
}

#[test]
fn verify_file_mac_accepts_the_matching_file_and_rejects_tampering() {
    let key = vector_key();
    let mut data = pattern(131_073);
    assert!(verify_file_mac(&key, data.as_slice(), 131_073).unwrap());
    data[70_000] ^= 0x01;
    assert!(!verify_file_mac(&key, data.as_slice(), 131_073).unwrap());
    // A different size produces a different MAC.
    let data = pattern(131_072);
    assert!(!verify_file_mac(&key, data.as_slice(), 131_072).unwrap());
}

#[test]
fn verify_file_mac_reports_a_truncated_stream_as_an_error() {
    let key = vector_key();
    let data = pattern(100);
    assert!(verify_file_mac(&key, data.as_slice(), 101).is_err());
}

#[test]
fn empty_files_have_no_mac_to_verify() {
    assert!(verify_file_mac(&vector_key(), std::io::empty(), 0).unwrap());
}
