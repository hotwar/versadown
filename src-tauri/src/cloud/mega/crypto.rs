//! MEGA cryptographic primitives (all AES-128): node key folding, ECB node-key
//! decryption, CBC attribute decryption and the condensed file MAC.
//!
//! Layout of a 32-byte file key `k` (as found in a link or, after ECB
//! decryption, in a folder listing):
//! - AES key  = `k[0..16] XOR k[16..32]`
//! - nonce    = `k[16..24]` (CTR counter occupies the other 8 bytes)
//! - meta-MAC = `k[24..32]`

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Block};
use base64::{
    alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
    Engine,
};
use std::io::{self, Read};
use zeroize::Zeroizing;

const BLOCK: usize = 16;
/// MAC chunks start at 128 KiB and grow by 128 KiB up to 1 MiB.
const MAC_CHUNK_START: usize = 128 * 1024;
const MAC_CHUNK_MAX: usize = 1024 * 1024;

const BASE64URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// MEGA's "base64" is URL-safe without padding; tolerate stray padding.
pub fn decode_base64url(text: &str) -> Option<Vec<u8>> {
    BASE64URL.decode(text.trim()).ok()
}

pub fn encode_base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Clone)]
pub struct FileKey {
    pub aes_key: Zeroizing<[u8; 16]>,
    pub nonce: [u8; 8],
    pub meta_mac: [u8; 8],
}

impl FileKey {
    pub fn from_bytes(raw: &[u8]) -> Option<Self> {
        let raw: &[u8; 32] = raw.try_into().ok()?;
        let mut aes_key = [0u8; 16];
        for (index, byte) in aes_key.iter_mut().enumerate() {
            *byte = raw[index] ^ raw[index + 16];
        }
        Some(Self {
            aes_key: Zeroizing::new(aes_key),
            nonce: raw[16..24].try_into().ok()?,
            meta_mac: raw[24..32].try_into().ok()?,
        })
    }
}

fn cipher(key: &[u8; 16]) -> Aes128 {
    Aes128::new(&(*key).into())
}

/// AES-ECB without padding over whole blocks (node keys inside a folder).
pub fn decrypt_ecb(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(BLOCK) {
        return None;
    }
    let cipher = cipher(key);
    let mut out = data.to_vec();
    for chunk in out.chunks_exact_mut(BLOCK) {
        let mut block = Block::try_from(&*chunk).ok()?;
        cipher.decrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    Some(out)
}

/// AES-CBC with an all-zero IV and no padding.
fn decrypt_cbc_zero_iv(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(BLOCK) {
        return None;
    }
    let cipher = cipher(key);
    let mut previous = [0u8; BLOCK];
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(BLOCK) {
        let mut block = Block::try_from(chunk).ok()?;
        cipher.decrypt_block(&mut block);
        out.extend(block.iter().zip(previous).map(|(plain, mask)| plain ^ mask));
        previous.copy_from_slice(chunk);
    }
    Some(out)
}

/// Decrypt a node's `a` attribute blob into its JSON object. `None` means the
/// key does not decrypt it (the plaintext must start with `MEGA{"`).
pub fn decrypt_node_attributes(
    attribute_key: &[u8; 16],
    encoded_attributes: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let encrypted = decode_base64url(encoded_attributes)?;
    let plain = decrypt_cbc_zero_iv(attribute_key, &encrypted)?;
    let end = plain
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(plain.len());
    let text = std::str::from_utf8(&plain[..end]).ok()?;
    let json = text.strip_prefix("MEGA")?;
    if !json.starts_with("{\"") {
        return None;
    }
    match serde_json::from_str(json).ok()? {
        serde_json::Value::Object(attributes) => Some(attributes),
        _ => None,
    }
}

/// Decrypt a node's `a` attribute blob and return its name.
pub fn decrypt_node_name(attribute_key: &[u8; 16], encoded_attributes: &str) -> Option<String> {
    decrypt_node_attributes(attribute_key, encoded_attributes)?
        .get("n")?
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// The `(offset, length)` pieces MEGA cuts a file of `size` bytes into for its
/// MAC: 128 KiB, then 256 KiB and so on up to 1 MiB, which then repeats. An
/// empty file yields one empty piece. Uploads send exactly these pieces, so
/// each piece's MAC can be computed independently of the others.
pub fn chunk_ranges(size: u64) -> impl Iterator<Item = (u64, u64)> {
    let mut offset = 0u64;
    let mut next = MAC_CHUNK_START as u64;
    let mut emitted_empty = false;
    std::iter::from_fn(move || {
        if size == 0 {
            return (!std::mem::replace(&mut emitted_empty, true)).then_some((0, 0));
        }
        if offset >= size {
            return None;
        }
        let length = next.min(size - offset);
        let piece = (offset, length);
        offset += length;
        next = (next + MAC_CHUNK_START as u64).min(MAC_CHUNK_MAX as u64);
        Some(piece)
    })
}

/// Condensed MAC of a whole plaintext stream (`size` bytes), as stored in the
/// last 8 bytes of a file key.
pub fn condensed_mac(key: &FileKey, mut plaintext: impl Read, size: u64) -> io::Result<[u8; 8]> {
    let cipher = cipher(&key.aes_key);
    let mut iv = [0u8; BLOCK];
    iv[..8].copy_from_slice(&key.nonce);
    iv[8..].copy_from_slice(&key.nonce);
    let mut combined = [0u8; BLOCK];
    let mut remaining = size;
    let mut chunk_size = MAC_CHUNK_START;
    let mut buffer = Vec::with_capacity(chunk_size);
    while remaining > 0 {
        let take = remaining.min(chunk_size as u64) as usize;
        buffer.resize(take, 0);
        plaintext.read_exact(&mut buffer)?;
        remaining -= take as u64;
        let chunk_mac = chunk_mac(&cipher, &iv, &buffer);
        for (byte, mac) in combined.iter_mut().zip(chunk_mac) {
            *byte ^= mac;
        }
        let mut block = Block::from(combined);
        cipher.encrypt_block(&mut block);
        combined.copy_from_slice(&block);
        chunk_size = (chunk_size + MAC_CHUNK_START).min(MAC_CHUNK_MAX);
    }
    let mut folded = [0u8; 8];
    for index in 0..4 {
        folded[index] = combined[index] ^ combined[index + 4];
        folded[index + 4] = combined[index + 8] ^ combined[index + 12];
    }
    Ok(folded)
}

/// CBC-MAC of one chunk: the last ciphertext block of a CBC encryption (IV =
/// nonce twice) with a zero-padded final block.
fn chunk_mac(cipher: &Aes128, iv: &[u8; BLOCK], chunk: &[u8]) -> [u8; BLOCK] {
    let mut state = *iv;
    for part in chunk.chunks(BLOCK) {
        for (byte, input) in state.iter_mut().zip(part) {
            *byte ^= input;
        }
        let mut block = Block::from(state);
        cipher.encrypt_block(&mut block);
        state.copy_from_slice(&block);
    }
    state
}

/// True when `plaintext` (the whole file) matches the MAC embedded in `key`.
/// An empty file carries no MAC to check.
pub fn verify_file_mac(key: &FileKey, plaintext: impl Read, size: u64) -> io::Result<bool> {
    if size == 0 {
        return Ok(true);
    }
    Ok(condensed_mac(key, plaintext, size)? == key.meta_mac)
}

#[cfg(test)]
mod tests;
