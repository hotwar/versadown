//! Write-side key and attribute handling: the inverse of what `nodes.rs` and
//! `keys.rs` read.
//!
//! * A node's attributes (`a`) are `MEGA` + compact JSON, zero padded to whole
//!   blocks, AES-128-CBC encrypted with a zero IV under the node's attribute
//!   key (a file's key halves XORed, a folder's own 16-byte key).
//! * A node's key (`k`) is AES-128-ECB encrypted with the master key (own
//!   nodes) or with a share key.
//! * A file key packs the AES key, the CTR nonce and the 8-byte meta-MAC:
//!   `k[16..24]` nonce, `k[24..32]` meta-MAC, `k[0..16]` AES key XOR `k[16..32]`.
//!
//! Written from the public megajs 1.3.x reference (`lib/crypto/aes.mjs`,
//! `lib/mutable-file.mjs`) and MEGA's SDK behaviour; no code is copied.

use super::keys::encrypt_ecb;
use crate::cloud::mega::crypto::{encode_base64url, FileKey};
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::QuarkError;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Block};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

const BLOCK: usize = 16;
const MAX_NAME_BYTES: usize = 255;

/// AES-CBC with an all-zero IV; `data` must be whole blocks.
pub fn encrypt_cbc_zero_iv(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(BLOCK) {
        return None;
    }
    let cipher = Aes128::new(&(*key).into());
    let mut previous = [0u8; BLOCK];
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(BLOCK) {
        let mut mixed = [0u8; BLOCK];
        for (slot, (plain, mask)) in mixed.iter_mut().zip(chunk.iter().zip(previous)) {
            *slot = plain ^ mask;
        }
        let mut block = Block::from(mixed);
        cipher.encrypt_block(&mut block);
        previous.copy_from_slice(&block);
        out.extend_from_slice(&block);
    }
    Some(out)
}

/// The `a` value for a node: attributes sealed under `key`.
pub fn seal_attributes(key: &[u8; 16], attributes: &Map<String, Value>) -> Option<String> {
    let json = serde_json::to_vec(attributes).ok()?;
    let mut plain = Vec::with_capacity(4 + json.len() + BLOCK);
    plain.extend_from_slice(b"MEGA");
    plain.extend_from_slice(&json);
    plain.resize(plain.len().next_multiple_of(BLOCK), 0);
    encrypt_cbc_zero_iv(key, &plain).map(|sealed| encode_base64url(&sealed))
}

/// Attributes for a node that has only a name.
pub fn name_attributes(name: &str) -> Map<String, Value> {
    let mut attributes = Map::new();
    attributes.insert("n".into(), Value::String(name.to_owned()));
    attributes
}

/// The key that seals a node's attributes: a file key folds to 16 bytes, a
/// folder key is used as is.
pub fn attribute_key(raw_node_key: &[u8]) -> Option<Zeroizing<[u8; 16]>> {
    match raw_node_key.len() {
        32 => FileKey::from_bytes(raw_node_key).map(|key| key.aes_key),
        16 => {
            let key: [u8; 16] = raw_node_key.try_into().ok()?;
            Some(Zeroizing::new(key))
        }
        _ => None,
    }
}

/// The 32-byte key of a finished upload.
pub fn pack_file_key(
    aes_key: &[u8; 16],
    nonce: &[u8; 8],
    meta_mac: &[u8; 8],
) -> Zeroizing<[u8; 32]> {
    let mut packed = Zeroizing::new([0u8; 32]);
    packed[16..24].copy_from_slice(nonce);
    packed[24..32].copy_from_slice(meta_mac);
    for index in 0..16 {
        packed[index] = aes_key[index] ^ packed[index + 16];
    }
    packed
}

/// A node key (or share key) wrapped for transport: base64url of the AES-ECB
/// encryption under `wrapping`.
pub fn wrap_key(wrapping: &[u8; 16], raw: &[u8]) -> Option<String> {
    encrypt_ecb(wrapping, raw).map(|wrapped| encode_base64url(&wrapped))
}

/// Names MEGA clients accept: non-empty, no path separators or control
/// characters, and not `.` or `..`.
pub fn validate_name(name: &str) -> Result<(), QuarkError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || name.len() > MAX_NAME_BYTES
        || trimmed == "."
        || trimmed == ".."
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(QuarkError::Mega(MegaFailure::InvalidName));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
