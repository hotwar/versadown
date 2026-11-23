//! Key hierarchy of a signed-in account.
//!
//! Every node key in the node listing is AES-128-ECB encrypted, either with
//! the account's master key (own nodes, `k = "<user handle>:<key>"`) or with
//! the key of a share (`k = "<share handle>:<key>"`). A node can carry several
//! `/`-separated alternatives. Share keys come from the listing's `ok`
//! entries: the key is encrypted with the master key and authenticated by
//! `ha = AES-ECB(master key, handle || handle)`.

use super::nodes::ancestry;
use crate::cloud::mega::crypto::{decode_base64url, decrypt_ecb};
use crate::cloud::QuarkError;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Block};
use serde_json::Value;
use std::collections::HashMap;
use zeroize::Zeroizing;

/// AES-128-ECB over whole blocks, no padding.
pub fn encrypt_ecb(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return None;
    }
    let cipher = Aes128::new(&(*key).into());
    let mut out = data.to_vec();
    for chunk in out.chunks_exact_mut(16) {
        let mut block = Block::try_from(&*chunk).ok()?;
        cipher.encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    Some(out)
}

/// A share's handle and its 16-byte key.
pub type ShareKey = (String, Zeroizing<[u8; 16]>);

pub struct Keyring {
    user_handle: String,
    master_key: Zeroizing<[u8; 16]>,
    shares: HashMap<String, Zeroizing<[u8; 16]>>,
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

impl Keyring {
    pub fn new(user_handle: &str, master_key: &[u8; 16]) -> Self {
        Self {
            user_handle: user_handle.to_owned(),
            master_key: Zeroizing::new(*master_key),
            shares: HashMap::new(),
        }
    }

    /// Learns the share keys of a node listing. Entries whose authenticator
    /// does not verify are ignored.
    pub fn with_listing_shares(mut self, listing: &Value) -> Self {
        for entry in listing["ok"].as_array().into_iter().flatten() {
            if let Some((handle, key)) = self.verified_share_key(entry) {
                self.shares.insert(handle, key);
            }
        }
        self
    }

    /// Adds share keys learned elsewhere (the RSA-decrypted keys of folders
    /// other users shared with this account).
    pub fn with_incoming_shares(mut self, keys: &[ShareKey]) -> Self {
        for (handle, key) in keys {
            self.shares.insert(handle.clone(), key.clone());
        }
        self
    }

    fn verified_share_key(&self, entry: &Value) -> Option<ShareKey> {
        let handle = entry["h"].as_str()?;
        let authenticator = decode_base64url(entry["ha"].as_str()?)?;
        let doubled = format!("{handle}{handle}");
        let expected = encrypt_ecb(&self.master_key, doubled.as_bytes())?;
        if !constant_time_eq(&expected, &authenticator) {
            return None;
        }
        let encrypted = decode_base64url(entry["k"].as_str()?)?;
        let key = Zeroizing::new(decrypt_ecb(&self.master_key, &encrypted)?);
        let key: [u8; 16] = key.as_slice().try_into().ok()?;
        Some((handle.to_owned(), Zeroizing::new(key)))
    }

    /// The key of the share rooted at `handle`, when this account exports it.
    pub fn share_key(&self, handle: &str) -> Option<Zeroizing<[u8; 16]>> {
        self.shares.get(handle).map(|key| Zeroizing::new(**key))
    }

    /// The nearest exported folder at or above `handle` and its share key.
    pub fn exported_share_above(
        &self,
        listing: &Value,
        handle: &str,
    ) -> Result<Option<ShareKey>, QuarkError> {
        for candidate in ancestry(listing, handle)? {
            if let Some(key) = self.share_key(&candidate) {
                return Ok(Some((candidate, key)));
            }
        }
        Ok(None)
    }

    /// The decrypted key of a node (`expected_len` is 32 for files, 16 for
    /// folders), or `None` when no known key opens any alternative.
    pub fn node_key(&self, encoded: &str, expected_len: usize) -> Option<Zeroizing<Vec<u8>>> {
        for alternative in encoded.split('/') {
            let Some((owner, encrypted)) = alternative.split_once(':') else {
                continue;
            };
            let wrapping = if owner == self.user_handle {
                &self.master_key
            } else if let Some(share) = self.shares.get(owner) {
                share
            } else {
                continue;
            };
            let Some(encrypted) = decode_base64url(encrypted) else {
                continue;
            };
            let Some(key) = decrypt_ecb(wrapping, &encrypted).map(Zeroizing::new) else {
                continue;
            };
            if key.len() == expected_len {
                return Some(key);
            }
        }
        None
    }
}
