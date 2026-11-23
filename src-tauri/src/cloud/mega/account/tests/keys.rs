//! AES-GCM, the `^!keys` key derivation and the container, against the
//! independent Python vectors in `keys_vectors.rs`.

use crate::cloud::mega::account::gcm::{self, derive_container_key};
use crate::cloud::mega::account::key_container::{
    Change, KeyContainer, FLAG_IN_USE, FLAG_TRUSTED, TAG_GENERATION, TAG_IDENTITY, TAG_SHARE_KEYS,
    TAG_VERSION,
};
use crate::cloud::mega::account::keys_vectors::*;
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::QuarkError;

fn refused(result: Result<impl Sized, QuarkError>) -> bool {
    matches!(result, Err(QuarkError::Mega(MegaFailure::KeysUnsupported)))
}

fn container() -> KeyContainer {
    KeyContainer::parse(CONTAINER_PLAIN).unwrap()
}

type Edit<'a> = (u8, &'a dyn Fn(&[u8]) -> Vec<u8>);

/// The plaintext with records rewritten: for each (tag, edit) the record's new
/// body is `edit(old body)`.
fn edited_many(edits: &[Edit<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = CONTAINER_PLAIN;
    while !rest.is_empty() {
        let length = usize::from(rest[1]) << 16 | usize::from(rest[2]) << 8 | usize::from(rest[3]);
        let mut body = rest[4..4 + length].to_vec();
        for (tag, edit) in edits {
            if *tag == rest[0] {
                body = edit(&body);
            }
        }
        out.push(rest[0]);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(&body);
        rest = &rest[4 + length..];
    }
    out
}

fn edited(tag: u8, edit: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    edited_many(&[(tag, &edit)])
}

#[test]
fn gcm_matches_every_independent_vector_and_rejects_tampering() {
    for (key, nonce, plain, sealed) in GCM_CASES {
        assert_eq!(&gcm::seal(key, nonce, plain).unwrap(), sealed);
        assert_eq!(&**gcm::open(key, nonce, sealed).unwrap(), *plain);
        let mut flipped = sealed.to_vec();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(gcm::open(key, nonce, &flipped).is_none(), "tag");
        if !plain.is_empty() {
            let mut flipped = sealed.to_vec();
            flipped[0] ^= 1;
            assert!(gcm::open(key, nonce, &flipped).is_none(), "ciphertext");
        }
        let mut other_nonce = *nonce;
        other_nonce[0] ^= 1;
        assert!(gcm::open(key, &other_nonce, sealed).is_none());
        let mut other_key = *key;
        other_key[0] ^= 1;
        assert!(gcm::open(&other_key, nonce, sealed).is_none());
    }
    // Shorter than a tag is never valid.
    assert!(gcm::open(&[0; 16], &[0; 12], &[0; 15]).is_none());
}

#[test]
fn the_container_key_is_the_hkdf_of_the_master_key() {
    assert_eq!(*derive_container_key(&KEYS_MASTER).unwrap(), KEYS_DERIVED);
}

#[test]
fn a_stored_container_opens_and_exposes_its_fields() {
    let opened = KeyContainer::open(&KEYS_DERIVED, CONTAINER_STORED).unwrap();
    assert_eq!(opened.generation(), CONTAINER_GENERATION);
    assert_eq!(opened.identity().unwrap(), KEYS_IDENTITY);
    let entries = opened.share_entries().unwrap();
    assert_eq!(entries.len(), CONTAINER_SHARE_COUNT);
    assert_eq!(entries[0].handle, SHARE_A_HANDLE);
    assert_eq!(*entries[0].key, SHARE_A_KEY);
    assert_eq!(entries[0].flags, FLAG_TRUSTED | FLAG_IN_USE);
    assert_eq!(&**opened.serialize().unwrap(), CONTAINER_PLAIN);
    // A different key does not open it.
    let mut wrong = KEYS_DERIVED;
    wrong[3] ^= 1;
    assert!(refused(KeyContainer::open(&wrong, CONTAINER_STORED)));
    // Nor does a value with the wrong header, or one cut short.
    let mut header = CONTAINER_STORED.to_vec();
    header[0] = 21;
    assert!(refused(KeyContainer::open(&KEYS_DERIVED, &header)));
    assert!(refused(KeyContainer::open(
        &KEYS_DERIVED,
        &CONTAINER_STORED[..20]
    )));
}

#[test]
fn adding_a_share_key_changes_only_that_record_and_the_generation() {
    let before = container();
    let change = Change::AddShareKey {
        handle: ADDED_HANDLE,
        key: ADDED_KEY,
        flags: ADDED_FLAGS,
    };
    let after = before.with_change(change).unwrap();
    assert_eq!(&**after.serialize().unwrap(), CONTAINER_ADDED_PLAIN);
    assert_eq!(
        after.seal(&KEYS_DERIVED, &ADDED_NONCE).unwrap(),
        CONTAINER_ADDED_STORED
    );
    // The two records of tags nobody here knows are still the last bytes.
    assert!(CONTAINER_ADDED_PLAIN.ends_with(UNKNOWN_RECORDS));
    assert!(after.serialize().unwrap().ends_with(UNKNOWN_RECORDS));
    // The original is untouched (the change builds a new container).
    assert_eq!(&**before.serialize().unwrap(), CONTAINER_PLAIN);
    let comparison = before.compare_with_updated(&after, change).unwrap();
    assert_eq!(comparison.records, CONTAINER_RECORD_COUNT);
    // Everything but the generation and the share key record is unchanged.
    assert_eq!(comparison.unchanged_records, CONTAINER_RECORD_COUNT - 2);
    assert_eq!(comparison.share_keys_before, CONTAINER_SHARE_COUNT);
    assert_eq!(comparison.share_keys_after, CONTAINER_SHARE_COUNT + 1);
    assert_eq!(comparison.generation_before, CONTAINER_GENERATION);
    assert_eq!(comparison.generation_after, CONTAINER_GENERATION + 1);
    // The sealed value parses back into the same container.
    let reopened = KeyContainer::open(&KEYS_DERIVED, CONTAINER_ADDED_STORED).unwrap();
    assert!(reopened == after);
}

#[test]
fn setting_a_flag_changes_only_that_entry_and_the_generation() {
    let before = container();
    let change = Change::SetFlags {
        handle: FLAG_HANDLE,
        flags: FLAG_NEW,
    };
    let after = before.with_change(change).unwrap();
    assert_eq!(&**after.serialize().unwrap(), CONTAINER_FLAGGED_PLAIN);
    assert_eq!(
        after.seal(&KEYS_DERIVED, &FLAG_NONCE).unwrap(),
        CONTAINER_FLAGGED_STORED
    );
    let comparison = before.compare_with_updated(&after, change).unwrap();
    assert_eq!(comparison.share_keys_before, comparison.share_keys_after);
    assert_eq!(comparison.generation_after, CONTAINER_GENERATION + 1);
    // Flags of a key that is not there, and a key added twice, are refused.
    assert!(refused(before.with_change(Change::SetFlags {
        handle: ADDED_HANDLE,
        flags: 1
    })));
    assert!(refused(before.with_change(Change::AddShareKey {
        handle: SHARE_A_HANDLE,
        key: ADDED_KEY,
        flags: 1
    })));
}

#[test]
fn the_comparison_rejects_anything_beyond_the_announced_change() {
    let before = container();
    let change = Change::AddShareKey {
        handle: ADDED_HANDLE,
        key: ADDED_KEY,
        flags: ADDED_FLAGS,
    };
    let good = CONTAINER_ADDED_PLAIN.to_vec();
    assert!(before
        .compare_with_updated(&KeyContainer::parse(&good).unwrap(), change)
        .is_ok());

    // A byte changed in a record this client does not own (here the private
    // keys, an unknown record, the backups).
    for position in [good.len() - 3, good.len() - 60, 200, 300] {
        let mut altered = good.clone();
        altered[position] ^= 1;
        let Ok(parsed) = KeyContainer::parse(&altered) else {
            continue;
        };
        // Either the announced change no longer holds or another record moved.
        let verdict = before.compare_with_updated(&parsed, change);
        assert!(refused(verdict), "byte {position}");
    }
    // Wrong key, wrong flags, wrong handle for the announced entry.
    for other in [
        Change::AddShareKey {
            handle: ADDED_HANDLE,
            key: [1; 16],
            flags: ADDED_FLAGS,
        },
        Change::AddShareKey {
            handle: ADDED_HANDLE,
            key: ADDED_KEY,
            flags: 0b11,
        },
        Change::AddShareKey {
            handle: [9; 6],
            key: ADDED_KEY,
            flags: ADDED_FLAGS,
        },
    ] {
        assert!(refused(before.compare_with_updated(
            &KeyContainer::parse(&good).unwrap(),
            other
        )));
    }
    // Generation raised by two, a share key dropped, an entry reordered.
    let generation_two = edited(TAG_GENERATION, |_| {
        (CONTAINER_GENERATION + 2).to_be_bytes().to_vec()
    });
    let dropped = edited(TAG_SHARE_KEYS, |body| body[23..].to_vec());
    let reordered = edited(TAG_SHARE_KEYS, |body| [&body[23..46], &body[..23]].concat());
    for altered in [generation_two, dropped, reordered] {
        let parsed = KeyContainer::parse(&altered).unwrap();
        assert!(refused(before.compare_with_updated(&parsed, change)));
    }
    // A flag change that also changed a key (generation raised correctly).
    let flag = Change::SetFlags {
        handle: FLAG_HANDLE,
        flags: FLAG_NEW,
    };
    let next = |_: &[u8]| (CONTAINER_GENERATION + 1).to_be_bytes().to_vec();
    let rekey = |body: &[u8]| {
        let mut body = body.to_vec();
        body[23 + 6] ^= 1;
        body[23 + 22] = FLAG_NEW;
        body
    };
    let rekeyed = edited_many(&[(TAG_GENERATION, &next), (TAG_SHARE_KEYS, &rekey)]);
    assert!(refused(before.compare_with_updated(
        &KeyContainer::parse(&rekeyed).unwrap(),
        flag
    )));
}

#[test]
fn containers_that_cannot_be_understood_completely_are_refused() {
    // Truncated in the middle of a record, a dangling header, trailing bytes.
    for cut in [10, CONTAINER_PLAIN.len() - 1, CONTAINER_PLAIN.len() - 2] {
        assert!(
            refused(KeyContainer::parse(&CONTAINER_PLAIN[..cut])),
            "cut {cut}"
        );
    }
    let mut trailing = CONTAINER_PLAIN.to_vec();
    trailing.extend_from_slice(&[1, 2, 3]);
    assert!(refused(KeyContainer::parse(&trailing)));
    // A newer format version, a zero generation, a foreign-sized identity, a
    // share record that is not whole entries, a missing record, a duplicate.
    assert!(refused(KeyContainer::parse(&edited(
        TAG_VERSION,
        |_| vec![2]
    ))));
    assert!(refused(KeyContainer::parse(&edited(
        TAG_GENERATION,
        |_| vec![0; 4]
    ))));
    assert!(refused(KeyContainer::parse(&edited(
        TAG_IDENTITY,
        |body| body[..7].to_vec()
    ))));
    assert!(refused(KeyContainer::parse(&edited(
        TAG_SHARE_KEYS,
        |body| body[..40].to_vec()
    ))));
    assert!(refused(KeyContainer::parse(&CONTAINER_PLAIN[(4 + 1)..])));
    let doubled = [CONTAINER_PLAIN, &CONTAINER_PLAIN[..5]].concat();
    assert!(refused(KeyContainer::parse(&doubled)));
    assert!(refused(KeyContainer::parse(&[])));
}
