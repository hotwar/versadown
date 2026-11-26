use super::*;
use crate::cloud::mega::crypto::encode_base64url;
use crate::cloud::mega::test_vectors::*;
use crate::cloud::share::{CloudShareEntry, CloudSharePlan};
use crate::cloud::CloudProvider;
use serde_json::json;

/// Length for which `FILE_KEY` carries the matching MAC (see `MAC_VECTORS`).
const MAC_LEN: usize = 131_073;

fn pattern(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| ((index * 31 + 7) & 0xFF) as u8)
        .collect()
}

fn file_entry(size: Option<u64>) -> CloudShareResolvedEntry {
    let share_id = "file/AbC12345".to_owned();
    let entry = CloudShareEntry::new(&share_id, "AbC12345".into(), FILE_NAME.into(), vec![], size);
    let plan = CloudSharePlan::new(
        CloudProvider::Mega,
        share_id,
        None,
        vec![entry.clone()],
        json!({"kind": "file", "id": "AbC12345", "key": encode_base64url(&FILE_KEY)}),
    );
    CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        plan,
        entry,
    }
}

fn write(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("download.bin");
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
}

fn is_integrity_failure(result: Result<(), QuarkError>) -> bool {
    matches!(result, Err(QuarkError::Mega(MegaFailure::IntegrityFailed)))
}

#[test]
fn an_intact_download_passes() {
    let (_dir, path) = write(&pattern(MAC_LEN));
    verify_downloaded_file(&file_entry(Some(MAC_LEN as u64)), &path).unwrap();
}

#[test]
fn a_flipped_byte_is_an_integrity_failure() {
    let mut data = pattern(MAC_LEN);
    data[100_000] ^= 0x80;
    let (_dir, path) = write(&data);
    assert!(is_integrity_failure(verify_downloaded_file(
        &file_entry(Some(MAC_LEN as u64)),
        &path
    )));
}

#[test]
fn a_file_of_the_wrong_length_is_an_integrity_failure() {
    let (_dir, path) = write(&pattern(MAC_LEN - 1));
    assert!(is_integrity_failure(verify_downloaded_file(
        &file_entry(Some(MAC_LEN as u64)),
        &path
    )));
}

#[test]
fn a_missing_file_cannot_be_verified_and_is_not_blamed_on_the_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let result = verify_downloaded_file(&file_entry(None), &dir.path().join("gone.bin"));
    assert!(matches!(result, Err(QuarkError::SourceFileMissing)));
}

#[test]
fn a_plan_without_key_material_cannot_be_verified() {
    let mut entry = file_entry(Some(1));
    entry.plan.provider_context = json!({"kind": "file", "id": "AbC12345"});
    let (_dir, path) = write(&[0]);
    assert!(matches!(
        verify_downloaded_file(&entry, &path),
        Err(QuarkError::InvalidResponse)
    ));
}
