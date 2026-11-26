//! Reading and editing the account's `^!keys` attribute.
//!
//! Accounts created since 2023 keep their share keys in this versioned
//! attribute instead of letting the node listing carry them. An edit is a
//! compare-and-set: the value is read with its attribute version, one change
//! is made (a share key added, or a flag of one flipped), and the new value is
//! stored with the version it was based on (`upv`). When another client wrote
//! in between, MEGA answers `-23`; the value is then read again and the
//! change re-planned. Before anything is sent the new container is decrypted
//! and parsed once more and compared with the old one, so a bug here cannot
//! turn into a damaged key container.
//!
//! Flow and checks follow MEGA's open source SDK (`KeyManager::commit`,
//! `tryCommit`, `updateAttribute`, `isValidKeysContainer` in `megaclient.cpp`);
//! no code is copied.

use super::client::MegaAccountClient;
use super::gcm::{derive_container_key, NONCE_LEN};
use super::key_container::{Change, KeyContainer, NodeHandle};
use super::manage::Snapshot;
use super::session::MegaSession;
use crate::cloud::mega::crypto::decode_base64url;
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::QuarkError;
use rand::{rngs::OsRng, RngCore};
use zeroize::Zeroizing;

const ATTRIBUTE: &str = "^!keys";
const MAX_ATTEMPTS: usize = 5;

/// A container as stored, with the attribute version it was read at.
pub(super) struct KeyState {
    pub container: KeyContainer,
    pub version: String,
}

/// Which change to make to the container that was just read; `None` when
/// the container already is as wanted.
pub(super) type Plan<'a> = &'a dyn Fn(&KeyContainer) -> Result<Option<Change>, QuarkError>;

fn derived_key(session: &MegaSession) -> Result<Zeroizing<[u8; 16]>, QuarkError> {
    let master = session.master_key()?;
    derive_container_key(&master).ok_or(QuarkError::Storage)
}

/// The container must be this account's: its identity record is the user
/// handle (8 bytes).
fn check_identity(session: &MegaSession, container: &KeyContainer) -> Result<(), QuarkError> {
    let own = decode_base64url(session.user_handle()).filter(|handle| handle.len() == 8);
    match (own, container.identity()) {
        (Some(own), Some(identity)) if own == identity => Ok(()),
        (None, _) => Ok(()),
        _ => Err(QuarkError::Mega(MegaFailure::KeysUnsupported)),
    }
}

/// A node handle as `^!keys` stores it: the 6 bytes behind the 8 characters.
pub(super) fn binary_handle(handle: &str) -> Option<NodeHandle> {
    decode_base64url(handle)?.try_into().ok()
}

impl MegaAccountClient<'_> {
    /// The container, or `None` for an account without a key manager.
    pub(super) fn read_keys(&self, session: &MegaSession) -> Result<Option<KeyState>, QuarkError> {
        let Some((stored, version)) = self
            .api(session)
            .user_attribute(session.user_handle(), ATTRIBUTE)?
        else {
            return Ok(None);
        };
        let derived = derived_key(session)?;
        let container = KeyContainer::open(&derived, &stored)?;
        check_identity(session, &container)?;
        Ok(Some(KeyState { container, version }))
    }

    /// Makes the change `plan` asks for, retrying from a fresh read when
    /// another client got there first. Returns once the stored container is
    /// confirmed (read back) to be as planned.
    pub(super) fn update_keys(
        &self,
        session: &MegaSession,
        plan: Plan<'_>,
    ) -> Result<(), QuarkError> {
        let derived = derived_key(session)?;
        let mut newest_generation = 0;
        for _ in 0..MAX_ATTEMPTS {
            let state = self
                .read_keys(session)?
                .ok_or(QuarkError::Mega(MegaFailure::FolderShareUnsupported))?;
            // A container older than one already seen means the server (or
            // someone in between) is serving a stale copy: never build on it.
            if state.container.generation() < newest_generation {
                return Err(QuarkError::Mega(MegaFailure::KeysUnsupported));
            }
            newest_generation = state.container.generation();
            let Some(change) = plan(&state.container)? else {
                return Ok(());
            };
            let updated = state.container.with_change(change)?;
            let mut nonce = [0u8; NONCE_LEN];
            OsRng.fill_bytes(&mut nonce);
            let sealed = updated.seal(&derived, &nonce)?;
            // Parse what is about to be sent, from the bytes, and require that
            // it differs from what was read only by the planned change.
            let reopened = KeyContainer::open(&derived, &sealed)?;
            state.container.compare_with_updated(&reopened, change)?;
            match self
                .api(session)
                .put_user_attribute(ATTRIBUTE, &sealed, &state.version)
            {
                Ok(()) => return self.confirm_keys(session, plan),
                Err(QuarkError::Mega(MegaFailure::KeysConflict)) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(QuarkError::Mega(MegaFailure::KeysConflict))
    }

    /// After a write, the stored container must satisfy the plan (nothing
    /// left to change). Another client may have raised the generation again.
    fn confirm_keys(&self, session: &MegaSession, plan: Plan<'_>) -> Result<(), QuarkError> {
        let state = self
            .read_keys(session)?
            .ok_or(QuarkError::Mega(MegaFailure::KeysUnsupported))?;
        match plan(&state.container)? {
            None => Ok(()),
            Some(_) => Err(QuarkError::Mega(MegaFailure::KeysUnsupported)),
        }
    }

    /// The share key of one of the account's own folders: the node listing's
    /// (older shares), else the container's. Reads `^!keys` only when the
    /// folder is in fact shared or linked.
    pub(super) fn share_key_above(
        &self,
        snapshot: &Snapshot,
        parent: &str,
    ) -> Result<Option<super::keys::ShareKey>, QuarkError> {
        if let Some(found) = snapshot
            .keyring
            .exported_share_above(&snapshot.listing, parent)?
        {
            return Ok(Some(found));
        }
        let chain = super::nodes::ancestry(&snapshot.listing, parent)?;
        let exported: Vec<&String> = chain
            .iter()
            .filter(|handle| super::nodes::is_exported(&snapshot.listing, handle))
            .collect();
        if exported.is_empty() {
            return Ok(None);
        }
        let Some(state) = self.read_keys(&snapshot.session)? else {
            return Ok(None);
        };
        for handle in exported {
            let Some(binary) = binary_handle(handle) else {
                continue;
            };
            if let Some(entry) = state.container.share_entry(&binary)? {
                return Ok(Some((handle.clone(), entry.key)));
            }
        }
        Ok(None)
    }
}
