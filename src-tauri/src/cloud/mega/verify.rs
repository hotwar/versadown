//! Whole-file integrity check of a finished download against the MAC that the
//! link key carries. aria2 never sees the key: it only receives plaintext from
//! the gateway, so this is the only point that proves the decrypted file is the
//! one the sharer uploaded.

use super::adapter::entry_target;
use super::crypto::{verify_file_mac, FileKey};
use super::failure::MegaFailure;
use crate::cloud::share::CloudShareResolvedEntry;
use crate::cloud::QuarkError;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Read buffer for the streaming MAC; the MAC itself works on at most 1 MiB.
const READ_BUFFER: usize = 1024 * 1024;

/// Checks `path` against the MAC stored in the entry's key. A mismatching
/// length or MAC is reported as [`MegaFailure::IntegrityFailed`]; any other
/// error means the check could not be performed (missing file, unreadable
/// plan) and says nothing about the file itself. The file is streamed, never
/// loaded into memory.
pub fn verify_downloaded_file(
    entry: &CloudShareResolvedEntry,
    path: &Path,
) -> Result<(), QuarkError> {
    let target = entry_target(entry)?;
    let key = FileKey::from_bytes(&target.key).ok_or(QuarkError::InvalidResponse)?;
    verify_file_against_key(&key, entry.entry.size, path)
}

/// The shared check behind both link-based and account-based downloads: the
/// file at `path` must have the expected length (when known) and match the
/// MAC inside `key`.
pub(crate) fn verify_file_against_key(
    key: &FileKey,
    expected_size: Option<u64>,
    path: &Path,
) -> Result<(), QuarkError> {
    let file = File::open(path).map_err(|_| QuarkError::SourceFileMissing)?;
    let length = file
        .metadata()
        .map_err(|_| QuarkError::SourceFileMissing)?
        .len();
    if expected_size.is_some_and(|size| size != length) {
        return Err(QuarkError::Mega(MegaFailure::IntegrityFailed));
    }
    let matches = verify_file_mac(key, BufReader::with_capacity(READ_BUFFER, file), length)
        // A read error mid-file (or a file that shrank) is not proof of tampering.
        .map_err(|_| QuarkError::SourceFileMissing)?;
    if matches {
        Ok(())
    } else {
        Err(QuarkError::Mega(MegaFailure::IntegrityFailed))
    }
}

#[cfg(test)]
mod tests;
