//! The decrypted `^!keys` container of an account that uses MEGA's key
//! manager.
//!
//! A container is a sequence of records `tag(1) || length(3, big endian) ||
//! body`. It holds the account's private keys, its share keys and more. This
//! client only ever adds a share key or flips a flag of one, so the container
//! is kept as the ordered list of its raw records: every record it does not
//! change (including tags it has never heard of) is written back byte for
//! byte and in the same place. Only the generation counter and the share key
//! record are rebuilt.
//!
//! Layout from MEGA's open source SDK (`KeyManager::unserialize`,
//! `serialize`, `deserializeShareKeys` in `megaclient.cpp`): tag 1 version,
//! 2 creation time, 3 identity (user handle), 4 generation (big endian), 48
//! share keys (`node handle(6) || key(16) || flags(1)` each; flag bit 0 =
//! trusted, bit 1 = in use). No code is copied.

use super::gcm::{self, CONTAINER_HEADER, NONCE_LEN};
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::QuarkError;
use zeroize::Zeroizing;

pub const TAG_VERSION: u8 = 1;
pub const TAG_IDENTITY: u8 = 3;
pub const TAG_GENERATION: u8 = 4;
pub const TAG_SHARE_KEYS: u8 = 48;

/// The only container format this client understands and may write.
const SUPPORTED_VERSION: u8 = 1;
const HEADER_LEN: usize = 4;
const MAX_RECORD_LEN: usize = 0xFF_FFFF;
const SHARE_ENTRY_LEN: usize = 6 + 16 + 1;
const IDENTITY_LEN: usize = 8;

/// Share key flag bits.
pub const FLAG_TRUSTED: u8 = 0b01;
pub const FLAG_IN_USE: u8 = 0b10;

fn unsupported() -> QuarkError {
    QuarkError::Mega(MegaFailure::KeysUnsupported)
}

/// A node handle in its 6-byte binary form.
pub type NodeHandle = [u8; 6];

#[derive(Clone, PartialEq, Eq)]
pub struct Record {
    pub tag: u8,
    pub body: Zeroizing<Vec<u8>>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ShareEntry {
    pub handle: NodeHandle,
    pub key: Zeroizing<[u8; 16]>,
    pub flags: u8,
}

/// The one change a new container may carry compared with the one it was
/// built from (besides the generation counter).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Change {
    AddShareKey {
        handle: NodeHandle,
        key: [u8; 16],
        flags: u8,
    },
    SetFlags {
        handle: NodeHandle,
        flags: u8,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub struct KeyContainer {
    records: Vec<Record>,
}

/// What a comparison of two containers found (counts only, never contents).
#[derive(Debug, PartialEq, Eq)]
pub struct Comparison {
    pub records: usize,
    pub unchanged_records: usize,
    pub share_keys_before: usize,
    pub share_keys_after: usize,
    pub generation_before: u32,
    pub generation_after: u32,
}

impl KeyContainer {
    /// Parses the decrypted plaintext. Any malformed or trailing byte is an
    /// error: a container that cannot be understood completely is not edited.
    pub fn parse(plain: &[u8]) -> Result<Self, QuarkError> {
        let mut records = Vec::new();
        let mut rest = plain;
        while !rest.is_empty() {
            if rest.len() < HEADER_LEN {
                return Err(unsupported());
            }
            let length =
                usize::from(rest[1]) << 16 | usize::from(rest[2]) << 8 | usize::from(rest[3]);
            let end = HEADER_LEN + length;
            if rest.len() < end {
                return Err(unsupported());
            }
            records.push(Record {
                tag: rest[0],
                body: Zeroizing::new(rest[HEADER_LEN..end].to_vec()),
            });
            rest = &rest[end..];
        }
        let container = Self { records };
        container.validate()?;
        Ok(container)
    }

    /// Decrypts a stored `^!keys` value.
    pub fn open(derived_key: &[u8; 16], stored: &[u8]) -> Result<Self, QuarkError> {
        let body = stored
            .strip_prefix(&CONTAINER_HEADER)
            .filter(|body| body.len() > NONCE_LEN + gcm::TAG_LEN)
            .ok_or_else(unsupported)?;
        let (nonce, sealed) = body.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| unsupported())?;
        let plain = gcm::open(derived_key, &nonce, sealed).ok_or_else(unsupported)?;
        Self::parse(&plain)
    }

    /// Structure every container must have: one each of the records this
    /// client reads, of the right size, in a format version it understands.
    fn validate(&self) -> Result<(), QuarkError> {
        for tag in [TAG_VERSION, TAG_IDENTITY, TAG_GENERATION, TAG_SHARE_KEYS] {
            if self
                .records
                .iter()
                .filter(|record| record.tag == tag)
                .count()
                != 1
            {
                return Err(unsupported());
            }
        }
        if self.single(TAG_VERSION).map(|body| &body[..]) != Some(&[SUPPORTED_VERSION][..]) {
            return Err(unsupported());
        }
        if self.single(TAG_IDENTITY).map(|body| body.len()) != Some(IDENTITY_LEN)
            || self.single(TAG_GENERATION).map(|body| body.len()) != Some(4)
        {
            return Err(unsupported());
        }
        if self.generation() == 0 {
            return Err(unsupported());
        }
        self.share_entries().map(|_| ())
    }

    fn single(&self, tag: u8) -> Option<&Zeroizing<Vec<u8>>> {
        self.records
            .iter()
            .find(|record| record.tag == tag)
            .map(|record| &record.body)
    }

    /// The generation counter; it grows by one with every write.
    pub fn generation(&self) -> u32 {
        self.single(TAG_GENERATION)
            .and_then(|body| <[u8; 4]>::try_from(&body[..]).ok())
            .map_or(0, u32::from_be_bytes)
    }

    /// The user handle the container was made for.
    pub fn identity(&self) -> Option<&[u8]> {
        self.single(TAG_IDENTITY).map(|body| &body[..])
    }

    pub fn share_entries(&self) -> Result<Vec<ShareEntry>, QuarkError> {
        let body = self.single(TAG_SHARE_KEYS).ok_or_else(unsupported)?;
        if body.len() % SHARE_ENTRY_LEN != 0 {
            return Err(unsupported());
        }
        body.chunks_exact(SHARE_ENTRY_LEN)
            .map(|entry| {
                Ok(ShareEntry {
                    handle: entry[..6].try_into().map_err(|_| unsupported())?,
                    key: Zeroizing::new(entry[6..22].try_into().map_err(|_| unsupported())?),
                    flags: entry[22],
                })
            })
            .collect()
    }

    pub fn share_entry(&self, handle: &NodeHandle) -> Result<Option<ShareEntry>, QuarkError> {
        Ok(self
            .share_entries()?
            .into_iter()
            .find(|entry| &entry.handle == handle))
    }

    /// A copy with the change applied and the generation raised by one.
    /// Adding a handle that is already present is refused.
    pub fn with_change(&self, change: Change) -> Result<Self, QuarkError> {
        let mut entries = self.share_entries()?;
        match change {
            Change::AddShareKey { handle, key, flags } => {
                if entries.iter().any(|entry| entry.handle == handle) {
                    return Err(unsupported());
                }
                entries.push(ShareEntry {
                    handle,
                    key: Zeroizing::new(key),
                    flags,
                });
            }
            Change::SetFlags { handle, flags } => {
                let entry = entries
                    .iter_mut()
                    .find(|entry| entry.handle == handle)
                    .ok_or_else(unsupported)?;
                entry.flags = flags;
            }
        }
        let generation = self
            .generation()
            .checked_add(1)
            .ok_or_else(unsupported)?
            .to_be_bytes();
        let share_body = Zeroizing::new(
            entries
                .iter()
                .flat_map(|entry| {
                    entry
                        .handle
                        .iter()
                        .chain(entry.key.iter())
                        .chain(std::iter::once(&entry.flags))
                        .copied()
                })
                .collect::<Vec<u8>>(),
        );
        let records = self
            .records
            .iter()
            .map(|record| match record.tag {
                TAG_GENERATION => Record {
                    tag: record.tag,
                    body: Zeroizing::new(generation.to_vec()),
                },
                TAG_SHARE_KEYS => Record {
                    tag: record.tag,
                    body: share_body.clone(),
                },
                _ => record.clone(),
            })
            .collect();
        let updated = Self { records };
        updated.validate()?;
        Ok(updated)
    }

    pub fn serialize(&self) -> Result<Zeroizing<Vec<u8>>, QuarkError> {
        let mut out = Zeroizing::new(Vec::new());
        for record in &self.records {
            let length = record.body.len();
            if length > MAX_RECORD_LEN {
                return Err(unsupported());
            }
            out.extend_from_slice(&[
                record.tag,
                (length >> 16) as u8,
                (length >> 8) as u8,
                length as u8,
            ]);
            out.extend_from_slice(&record.body);
        }
        Ok(out)
    }

    /// The value to store: header, nonce, ciphertext and tag.
    pub fn seal(
        &self,
        derived_key: &[u8; 16],
        nonce: &[u8; NONCE_LEN],
    ) -> Result<Vec<u8>, QuarkError> {
        let plain = self.serialize()?;
        let mut stored = CONTAINER_HEADER.to_vec();
        stored.extend_from_slice(nonce);
        stored.extend_from_slice(&gcm::seal(derived_key, nonce, &plain).ok_or(QuarkError::Storage)?);
        Ok(stored)
    }

    /// Checks that `updated` differs from `self` only by `change` and the
    /// generation counter raised by exactly one: the same records in the same
    /// order, every other record byte for byte equal, and the share keys equal
    /// to the old ones plus (or with) exactly the announced change.
    pub fn compare_with_updated(
        &self,
        updated: &Self,
        change: Change,
    ) -> Result<Comparison, QuarkError> {
        if self.records.len() != updated.records.len() {
            return Err(unsupported());
        }
        let mut unchanged = 0;
        for (before, after) in self.records.iter().zip(&updated.records) {
            if before.tag != after.tag {
                return Err(unsupported());
            }
            match before.tag {
                TAG_GENERATION | TAG_SHARE_KEYS => {}
                _ if before.body == after.body => unchanged += 1,
                _ => return Err(unsupported()),
            }
        }
        if updated.generation() != self.generation().checked_add(1).ok_or_else(unsupported)? {
            return Err(unsupported());
        }
        let before = self.share_entries()?;
        let after = updated.share_entries()?;
        let expected_len = before.len() + usize::from(matches!(change, Change::AddShareKey { .. }));
        if after.len() != expected_len {
            return Err(unsupported());
        }
        let mut flagged = 0;
        for (index, entry) in after.iter().enumerate() {
            let expected = match change {
                Change::AddShareKey { handle, key, flags } if index == before.len() => ShareEntry {
                    handle,
                    key: Zeroizing::new(key),
                    flags,
                },
                Change::SetFlags { handle, flags } => {
                    let mut original = before.get(index).cloned().ok_or_else(unsupported)?;
                    if original.handle == handle {
                        original.flags = flags;
                        flagged += 1;
                    }
                    original
                }
                _ => before.get(index).cloned().ok_or_else(unsupported)?,
            };
            if *entry != expected {
                return Err(unsupported());
            }
        }
        // A flag change must have found exactly one existing entry.
        if matches!(change, Change::SetFlags { .. }) && flagged != 1 {
            return Err(unsupported());
        }
        Ok(Comparison {
            records: self.records.len(),
            unchanged_records: unchanged,
            share_keys_before: before.len(),
            share_keys_after: after.len(),
            generation_before: self.generation(),
            generation_after: updated.generation(),
        })
    }
}
