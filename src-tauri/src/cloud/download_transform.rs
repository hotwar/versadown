//! Byte transformations that the local range gateway applies between a
//! provider's storage servers and aria2.
//!
//! Some providers store end-to-end encrypted objects. The ciphertext is
//! useless to aria2, so the gateway fetches it, decrypts it on the fly and
//! serves the plaintext with ordinary HTTP Range semantics.

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use ctr::Ctr128BE;
use std::fmt;
use zeroize::Zeroizing;

type AesCtr = Ctr128BE<Aes128>;

/// Describes how an upstream object maps to the plain file.
#[derive(Clone)]
pub enum DownloadTransform {
    /// The upstream serves AES-128-CTR ciphertext of exactly `total_size`
    /// bytes. Byte ranges are addressed by a `/<start>-<end>` URL path suffix
    /// (`end` inclusive) instead of the `Range` header, and responses carry no
    /// `Content-Range`, so the total size is known from the provider's
    /// metadata. The keystream block counter is a big-endian counter that
    /// starts at zero after the 8-byte `nonce`.
    AesCtrPathRange {
        key: Zeroizing<[u8; 16]>,
        nonce: [u8; 8],
        total_size: u64,
        /// The upstream URL was issued against a signed-in account's
        /// transfer quota rather than the caller's IP quota. Only the wording
        /// of a quota failure depends on it.
        account_quota: bool,
    },
}

// The key is as sensitive as the file it protects.
impl fmt::Debug for DownloadTransform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AesCtrPathRange { total_size, .. } => formatter
                .debug_struct("AesCtrPathRange")
                .field("total_size", total_size)
                .finish_non_exhaustive(),
        }
    }
}

impl DownloadTransform {
    pub fn aes_ctr_path_range(key: [u8; 16], nonce: [u8; 8], total_size: u64) -> Self {
        Self::AesCtrPathRange {
            key: Zeroizing::new(key),
            nonce,
            total_size,
            account_quota: false,
        }
    }

    /// Marks the upstream URL as issued against an account's transfer quota.
    pub fn with_account_quota(mut self) -> Self {
        let Self::AesCtrPathRange { account_quota, .. } = &mut self;
        *account_quota = true;
        self
    }

    pub fn uses_account_quota(&self) -> bool {
        match self {
            Self::AesCtrPathRange { account_quota, .. } => *account_quota,
        }
    }

    pub fn total_size(&self) -> u64 {
        match self {
            Self::AesCtrPathRange { total_size, .. } => *total_size,
        }
    }

    /// Upstream URL for the inclusive byte range `start..=end`.
    pub fn range_url(&self, upstream: &str, start: u64, end: u64) -> Option<String> {
        let mut url = url::Url::parse(upstream).ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push(&format!("{start}-{end}"));
        Some(url.into())
    }

    /// A decryptor positioned at the absolute plaintext offset `offset`.
    /// Any offset works, including ones that are not block aligned.
    pub fn decryptor(&self, offset: u64) -> Decryptor {
        match self {
            Self::AesCtrPathRange { key, nonce, .. } => {
                let mut iv = [0u8; 16];
                iv[..8].copy_from_slice(nonce);
                let mut cipher = AesCtr::new(&(**key).into(), &iv.into());
                cipher.seek(offset);
                Decryptor { cipher }
            }
        }
    }
}

/// Stateful stream decryptor; chunk boundaries do not need to be aligned.
pub struct Decryptor {
    cipher: AesCtr,
}

impl Decryptor {
    pub fn apply(&mut self, buffer: &mut [u8]) {
        self.cipher.apply_keystream(buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_url_appends_inclusive_range_and_keeps_query() {
        let transform = DownloadTransform::aes_ctr_path_range([0; 16], [0; 8], 100);
        assert_eq!(
            transform
                .range_url("https://gfs.example/dl/TOKEN", 5, 9)
                .as_deref(),
            Some("https://gfs.example/dl/TOKEN/5-9")
        );
        assert_eq!(
            transform
                .range_url("https://gfs.example/dl/TOKEN/?x=1", 0, 0)
                .as_deref(),
            Some("https://gfs.example/dl/TOKEN/0-0?x=1")
        );
    }

    #[test]
    fn debug_never_prints_the_key() {
        let transform = DownloadTransform::aes_ctr_path_range([7; 16], [1; 8], 9);
        assert_eq!(
            format!("{transform:?}"),
            "AesCtrPathRange { total_size: 9, .. }"
        );
    }

    fn hex_decode(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    /// Ciphertext produced by an independent implementation (Python
    /// `cryptography`); the plaintext is a known byte pattern.
    #[test]
    fn decrypts_an_independent_ciphertext_from_any_offset() {
        use crate::cloud::mega::test_vectors::*;
        let ciphertext = hex_decode(CIPHERTEXT_HEX);
        let transform = DownloadTransform::aes_ctr_path_range(
            FILE_AES_KEY,
            FILE_NONCE,
            ciphertext.len() as u64,
        );
        for offset in [0usize, 1, 15, 16, 17, 31, 4096, 4999] {
            let mut data = ciphertext[offset..].to_vec();
            let mut decryptor = transform.decryptor(offset as u64);
            // Feed it in odd-sized pieces so block boundaries never line up.
            for piece in data.chunks_mut(37) {
                decryptor.apply(piece);
            }
            let expected = (offset..ciphertext.len())
                .map(|index| ((index * 31 + 7) & 0xFF) as u8)
                .collect::<Vec<_>>();
            assert_eq!(data, expected, "offset {offset}");
        }
    }

    #[test]
    fn seeking_matches_a_sequential_stream_at_unaligned_offsets() {
        let transform = DownloadTransform::aes_ctr_path_range([3; 16], [9; 8], 200);
        let mut whole = vec![0u8; 200];
        transform.decryptor(0).apply(&mut whole);
        for offset in [0usize, 1, 15, 16, 17, 33, 199] {
            let mut tail = vec![0u8; 200 - offset];
            let mut decryptor = transform.decryptor(offset as u64);
            let middle = tail.len() / 2;
            let (head, rest) = tail.split_at_mut(middle);
            decryptor.apply(head);
            decryptor.apply(rest);
            assert_eq!(tail, whole[offset..], "offset {offset}");
        }
    }
}
