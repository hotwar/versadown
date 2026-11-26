//! Whole-file integrity check of a finished download from the account.
//!
//! aria2 only ever sees plaintext, so the MAC inside the node key is checked
//! against the finished file afterwards. The key comes from the account's own
//! (encrypted) session and node listing; the task record never carries it.

use super::client::MegaAccountClient;
use super::nodes::find_file;
use super::session::MegaSession;
use crate::cloud::mega::crypto::FileKey;
use crate::cloud::mega::verify::verify_file_against_key;
use crate::cloud::QuarkError;
use std::path::Path;

impl MegaAccountClient<'_> {
    /// Checks the downloaded `path` against the MAC of the account file
    /// `file_id`. A mismatch is `MegaFailure::IntegrityFailed`; any other error
    /// means the check could not run (signed out, file gone from the account,
    /// file moved) and says nothing about the downloaded file.
    pub fn verify_download(&self, file_id: &str, path: &Path) -> Result<(), QuarkError> {
        let session = MegaSession::load(self.store)?;
        let listing = self.listing(&session)?;
        let keyring = Self::keyring(&session, &listing)?;
        let node = find_file(&listing, &keyring, file_id)?;
        let key = FileKey::from_bytes(&node.key).ok_or(QuarkError::InvalidResponse)?;
        verify_file_against_key(&key, Some(node.size), path)
    }
}
