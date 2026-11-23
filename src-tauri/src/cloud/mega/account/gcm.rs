//! The cipher of the `^!keys` attribute: AES-128-GCM with a 96-bit nonce, a
//! 16-byte tag and no associated data, keyed by an HKDF-SHA256 derivation of
//! the account's master key. Both primitives come from RustCrypto (`aes-gcm`,
//! `hkdf`); nothing is implemented here.
//!
//! Layout and derivation follow MEGA's open source SDK (`KeyManager::setKey`,
//! `fromKeysContainer`, `toKeysContainer` in `megaclient.cpp`): the key is
//! `HKDF-SHA256(ikm = master key, salt = none, info = 0x01)` cut to 16 bytes,
//! and the stored value is `0x14 0x00 || nonce(12) || ciphertext || tag(16)`.
//! The independent Python vectors in `tests/keys.rs` pin both.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use sha2_digest11::Sha256;
use zeroize::Zeroizing;

pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// Leading bytes of every stored `^!keys` value: format 20, reserved 0.
pub const CONTAINER_HEADER: [u8; 2] = [20, 0];

/// `HKDF-SHA256(master, salt = none, info = [1])`, 16 bytes.
pub fn derive_container_key(master_key: &[u8; 16]) -> Option<Zeroizing<[u8; 16]>> {
    let mut key = Zeroizing::new([0u8; 16]);
    Hkdf::<Sha256>::new(None, master_key)
        .expand(&[1u8], &mut *key)
        .ok()?;
    Some(key)
}

/// `ciphertext || tag`.
pub fn seal(key: &[u8; 16], nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Option<Vec<u8>> {
    Aes128Gcm::new(&(*key).into())
        .encrypt(&Nonce::from(*nonce), plaintext)
        .ok()
}

/// The plaintext of `ciphertext || tag`, or `None` when the tag does not
/// verify (wrong key, or the value was altered).
pub fn open(key: &[u8; 16], nonce: &[u8; NONCE_LEN], sealed: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    Aes128Gcm::new(&(*key).into())
        .decrypt(&Nonce::from(*nonce), sealed)
        .ok()
        .map(Zeroizing::new)
}
