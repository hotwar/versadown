//! Password key derivation for MEGA accounts.
//!
//! * Version 1 accounts use MEGA's legacy "prepare key" loop plus a keyed
//!   string hash of the e-mail address as the login proof.
//! * Version 2 accounts use PBKDF2-HMAC-SHA512 over a server supplied salt;
//!   the first half of the output is the password key, the second half the
//!   login proof.
//!
//! Algorithms follow the public megajs reference (`lib/crypto/aes.mjs`); the
//! code is independent and checked against Python-generated vectors.

use crate::cloud::mega::crypto::encode_base64url;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Block};
use hmac::{Hmac, Mac};
use sha2::Sha512;
use zeroize::Zeroizing;

const V1_ROUNDS: usize = 65_536;
const V1_START: [u8; 16] = [
    0x93, 0xC4, 0x67, 0xE3, 0x7D, 0xB0, 0xC7, 0xA4, 0xD1, 0xBE, 0x3F, 0x81, 0x01, 0x52, 0xCB, 0x56,
];
const STRING_HASH_ROUNDS: usize = 16_384;
const V2_ITERATIONS: u32 = 100_000;

/// What the password yields: the AES key that protects the master key, and
/// the proof (`uh`) sent to the server instead of the password.
pub struct LoginSecrets {
    pub password_key: Zeroizing<[u8; 16]>,
    pub user_hash: String,
}

fn cipher(key: &[u8; 16]) -> Aes128 {
    Aes128::new(&(*key).into())
}

fn encrypt_block(cipher: &Aes128, block: &mut [u8; 16]) {
    let mut value = Block::from(*block);
    cipher.encrypt_block(&mut value);
    block.copy_from_slice(&value);
}

/// Legacy password key: 65536 rounds in which a fixed block is encrypted once
/// per 16-byte (zero padded) chunk of the password, keyed by that chunk.
fn v1_password_key(password: &[u8]) -> Zeroizing<[u8; 16]> {
    let ciphers: Vec<Aes128> = password
        .chunks(16)
        .map(|chunk| {
            let mut key = Zeroizing::new([0u8; 16]);
            key[..chunk.len()].copy_from_slice(chunk);
            cipher(&key)
        })
        .collect();
    let mut block = V1_START;
    for _ in 0..V1_ROUNDS {
        for cipher in &ciphers {
            encrypt_block(cipher, &mut block);
        }
    }
    Zeroizing::new(block)
}

/// Login proof of a v1 account: the lower-cased e-mail folded into one block
/// (big-endian words XORed round-robin), encrypted 16384 times with the
/// password key; bytes 0..4 and 8..12 of the result.
fn v1_user_hash(email: &str, password_key: &[u8; 16]) -> String {
    let mut accumulator = [0u8; 16];
    for (index, word) in email.as_bytes().chunks(4).enumerate() {
        let slot = (index & 3) * 4;
        for (offset, byte) in word.iter().enumerate() {
            accumulator[slot + offset] ^= byte;
        }
    }
    let cipher = cipher(password_key);
    for _ in 0..STRING_HASH_ROUNDS {
        encrypt_block(&cipher, &mut accumulator);
    }
    let mut proof = [0u8; 8];
    proof[..4].copy_from_slice(&accumulator[..4]);
    proof[4..].copy_from_slice(&accumulator[8..12]);
    encode_base64url(&proof)
}

pub fn derive_v1(email: &str, password: &str) -> LoginSecrets {
    let password_key = v1_password_key(password.as_bytes());
    let user_hash = v1_user_hash(email, &password_key);
    LoginSecrets {
        password_key,
        user_hash,
    }
}

/// First output block of PBKDF2-HMAC-SHA512 (64 bytes; MEGA only needs 32).
fn pbkdf2_sha512_first_block(password: &[u8], salt: &[u8], iterations: u32) -> Zeroizing<[u8; 64]> {
    let keyed = Hmac::<Sha512>::new_from_slice(password).expect("HMAC accepts any key length");
    let mut first = keyed.clone();
    first.update(salt);
    first.update(&1u32.to_be_bytes());
    let mut round: [u8; 64] = first.finalize().into_bytes().into();
    let mut accumulated = round;
    for _ in 1..iterations {
        let mut mac = keyed.clone();
        mac.update(&round);
        round = mac.finalize().into_bytes().into();
        for (total, byte) in accumulated.iter_mut().zip(round) {
            *total ^= byte;
        }
    }
    Zeroizing::new(accumulated)
}

pub fn derive_v2(password: &str, salt: &[u8]) -> LoginSecrets {
    let derived = pbkdf2_sha512_first_block(password.as_bytes(), salt, V2_ITERATIONS);
    let mut password_key = Zeroizing::new([0u8; 16]);
    password_key.copy_from_slice(&derived[..16]);
    LoginSecrets {
        password_key,
        user_hash: encode_base64url(&derived[16..32]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::mega::account::test_vectors::*;
    use crate::cloud::mega::crypto::decode_base64url;

    #[test]
    fn v1_derivation_matches_independent_vectors() {
        for (email, password, key, user_hash, _) in V1_CASES {
            let secrets = derive_v1(&email.to_lowercase(), password);
            assert_eq!(*secrets.password_key, *key, "{email}");
            assert_eq!(secrets.user_hash, *user_hash, "{email}");
        }
    }

    #[test]
    fn pbkdf2_matches_the_standard_library_vector() {
        let derived = pbkdf2_sha512_first_block(b"abc", b"salty", 1000);
        assert_eq!(derived[..32], PBKDF2_PASSWORD_ABC_FIRST_32);
    }

    #[test]
    fn v2_derivation_splits_key_and_proof_like_the_vector() {
        let salt = decode_base64url(V2_SALT_B64).unwrap();
        let secrets = derive_v2(V2_PASSWORD, &salt);
        assert_eq!(*secrets.password_key, V2_PASSWORD_KEY);
        assert_eq!(secrets.user_hash, V2_USER_HASH);
    }
}
