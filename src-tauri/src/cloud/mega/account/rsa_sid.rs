//! The account's RSA key: recovering the session id from the login response's
//! `csid`, and share keys of folders other users shared with this account.
//!
//! The account's RSA private key (`privk`) is stored on the server encrypted
//! with the master key as four MPIs (`p`, `q`, `d`, `u`). An MPI is a 2-byte
//! big-endian bit length followed by the big-endian magnitude. Both `csid` and
//! the `sk` of an incoming share are one MPI holding `m^e mod n`; the session
//! id is the first 43 bytes of `m`, a share key the first 16.
//! The key stays in memory only for the length of the call that needs it.

use crate::cloud::mega::crypto::{decode_base64url, decrypt_ecb, encode_base64url};
use rsa::BigUint;
use zeroize::Zeroizing;

const SESSION_ID_BYTES: usize = 43;
const SHARE_KEY_BYTES: usize = 16;

/// Splits one MPI off the front of `data`: its value and the remaining bytes.
fn read_mpi(data: &[u8]) -> Option<(BigUint, &[u8])> {
    let bits = usize::from(u16::from_be_bytes([*data.first()?, *data.get(1)?]));
    let length = bits.div_ceil(8);
    let magnitude = data.get(2..2 + length)?;
    Some((BigUint::from_bytes_be(magnitude), &data[2 + length..]))
}

/// The modulus and private exponent of the key blob `privk`, already
/// decrypted with the master key. Trailing padding is ignored.
fn decode_private_key(blob: &[u8]) -> Option<(BigUint, BigUint)> {
    let (p, rest) = read_mpi(blob)?;
    let (q, rest) = read_mpi(rest)?;
    let (d, _) = read_mpi(rest)?;
    let zero = BigUint::from(0u8);
    if p == zero || q == zero || d == zero {
        return None;
    }
    Some((p * q, d))
}

/// What is needed to RSA-decrypt: modulus and private exponent.
pub struct RsaKey {
    modulus: BigUint,
    exponent: BigUint,
}

impl RsaKey {
    /// Opens `privk`, or `None` when the master key does not decrypt it.
    pub fn open(master_key: &[u8; 16], encrypted_private_key: &str) -> Option<Self> {
        let encrypted = decode_base64url(encrypted_private_key)?;
        let blob = Zeroizing::new(decrypt_ecb(master_key, &encrypted)?);
        let (modulus, exponent) = decode_private_key(&blob)?;
        Some(Self { modulus, exponent })
    }

    /// The plaintext (leading zeros dropped) of one RSA-encrypted MPI.
    fn decrypt(&self, encrypted_mpi: &str) -> Option<Zeroizing<Vec<u8>>> {
        let encoded = decode_base64url(encrypted_mpi)?;
        let (ciphertext, rest) = read_mpi(&encoded)?;
        if !rest.is_empty() || ciphertext >= self.modulus {
            return None;
        }
        Some(Zeroizing::new(
            ciphertext
                .modpow(&self.exponent, &self.modulus)
                .to_bytes_be(),
        ))
    }

    /// The 16-byte share key inside an incoming share's `sk`.
    pub fn share_key(&self, sk: &str) -> Option<Zeroizing<[u8; 16]>> {
        let plain = self.decrypt(sk)?;
        let key: [u8; SHARE_KEY_BYTES] = plain.get(..SHARE_KEY_BYTES)?.try_into().ok()?;
        Some(Zeroizing::new(key))
    }
}

/// Session id for a `csid` response, or `None` when the blob does not parse
/// or the plaintext is too short (wrong key).
pub fn decrypt_session_id(
    master_key: &[u8; 16],
    encrypted_private_key: &str,
    csid: &str,
) -> Option<String> {
    let plain = RsaKey::open(master_key, encrypted_private_key)?.decrypt(csid)?;
    (plain.len() >= SESSION_ID_BYTES).then(|| encode_base64url(&plain[..SESSION_ID_BYTES]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::mega::account::test_vectors::*;

    #[test]
    fn csid_decrypts_to_the_vector_session_id() {
        assert_eq!(
            decrypt_session_id(&MASTER_KEY, PRIVK_B64, CSID_B64).as_deref(),
            Some(EXPECTED_SID)
        );
    }

    #[test]
    fn a_wrong_master_key_yields_no_session() {
        let mut wrong = MASTER_KEY;
        wrong[0] ^= 1;
        assert_eq!(decrypt_session_id(&wrong, PRIVK_B64, CSID_B64), None);
    }

    #[test]
    fn malformed_blobs_are_rejected() {
        assert_eq!(decrypt_session_id(&MASTER_KEY, PRIVK_B64, "AAAA"), None);
        assert_eq!(
            decrypt_session_id(&MASTER_KEY, "not base64 !", CSID_B64),
            None
        );
        // Trailing bytes after the ciphertext MPI.
        let mut csid = decode_base64url(CSID_B64).unwrap();
        csid.push(0);
        assert_eq!(
            decrypt_session_id(&MASTER_KEY, PRIVK_B64, &encode_base64url(&csid)),
            None
        );
    }
}
