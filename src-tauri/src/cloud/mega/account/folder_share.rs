//! Public links of folders for accounts with MEGA's key manager.
//!
//! The order follows MEGA's open source SDK (`MegaClient::exportnode`,
//! `setshare`, `setShareCompletion`, `CommandSetShare`, `CommandSetPH` in
//! `megaclient.cpp` and `megaclient_commands.cpp`); no code is copied:
//!
//! 1. A new 16-byte share key is made and recorded in `^!keys` (flag
//!    "trusted") first, so the key is never lost once the share exists.
//! 2. `s2` creates the folder's link share. It carries all-zero placeholders
//!    for `ok`/`ha` (the key is not sent to MEGA) and, for a new share, `cr`:
//!    every node key below the folder encrypted with the share key.
//! 3. The key's "in use" flag is set in `^!keys`.
//! 4. `l` exports the folder; the link is `.../folder/<public handle>#<key>`.
//!
//! Withdrawing a link removes the folder's link share (`s2`) and, when no
//! other share of the folder is left, clears the "in use" flag.

use super::client::MegaAccountClient;
use super::key_container::{Change, FLAG_IN_USE, FLAG_TRUSTED};
use super::key_manager::{binary_handle, KeyState};
use super::manage::{random_key, Snapshot};
use super::nodes::{link_handle, share_recipients, subtree, ListedNode};
use super::seal::wrap_key;
use super::share::share_of;
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::{CloudShare, QuarkError};
use serde_json::{json, Value};
use zeroize::Zeroizing;

/// The recipient the API reports for a folder's public link share.
const LINK_RECIPIENT: &str = "EXP";

fn unsupported() -> QuarkError {
    QuarkError::Mega(MegaFailure::KeysUnsupported)
}

/// `cr`: `[[share handle], [node handles], [0, index, wrapped key, ...]]`,
/// every key encrypted with the share key. `None` if a key cannot be wrapped.
pub(super) fn share_keys_cr(
    share_handle: &str,
    share_key: &[u8; 16],
    nodes: &[(&str, &[u8])],
) -> Option<Value> {
    let mut requests = Vec::with_capacity(nodes.len() * 3);
    for (index, (_, key)) in nodes.iter().enumerate() {
        requests.extend([json!(0), json!(index), json!(wrap_key(share_key, key)?)]);
    }
    let handles: Vec<&str> = nodes.iter().map(|(handle, _)| *handle).collect();
    Some(json!([[share_handle], handles, requests]))
}

impl MegaAccountClient<'_> {
    /// The share key of a folder of the account: the listing's, else
    /// `^!keys`'. The container is read at most once per `cache`.
    pub(super) fn folder_share_key(
        &self,
        snapshot: &Snapshot,
        handle: &str,
        cache: &mut Option<Option<KeyState>>,
    ) -> Result<Option<Zeroizing<[u8; 16]>>, QuarkError> {
        if let Some(key) = snapshot.keyring.share_key(handle) {
            return Ok(Some(key));
        }
        if cache.is_none() {
            *cache = Some(self.read_keys(&snapshot.session)?);
        }
        let (Some(Some(state)), Some(binary)) = (cache.as_ref(), binary_handle(handle)) else {
            return Ok(None);
        };
        Ok(state.container.share_entry(&binary)?.map(|entry| entry.key))
    }

    /// Creates (or finds) the link of one of the account's folders.
    pub(super) fn export_folder(
        &self,
        snapshot: &Snapshot,
        node: &ListedNode,
    ) -> Result<CloudShare, QuarkError> {
        let handle = node.handle.as_str();
        let existing_link = link_handle(&snapshot.listing, handle);
        // A key in the node listing belongs to a share that already exists
        // (older accounts, and shares made before the key manager): only the
        // link is missing.
        if let Some(key) = snapshot.keyring.share_key(handle) {
            let public_handle = match existing_link {
                Some(public_handle) => public_handle,
                None => self.api(&snapshot.session).export_link(handle)?,
            };
            return Ok(share_of(node, &public_handle, None, &*key));
        }
        let Some(state) = self.read_keys(&snapshot.session)? else {
            // An older account cannot record a new share key: no half-way.
            return Err(QuarkError::Mega(MegaFailure::FolderShareUnsupported));
        };
        let binary = binary_handle(handle).ok_or(QuarkError::InvalidResponse)?;
        let key = match (
            state.container.share_entry(&binary)?,
            existing_link.is_some(),
        ) {
            (Some(entry), _) => entry.key,
            // A link whose key is nowhere would be re-keyed: never.
            (None, true) => return Err(unsupported()),
            (None, false) => self.record_new_share_key(snapshot, handle)?,
        };
        let public_handle = match existing_link {
            Some(public_handle) => public_handle,
            None => self.start_link_share(snapshot, node, &key)?,
        };
        Ok(share_of(node, &public_handle, None, &*key))
    }

    /// Step 1: a fresh share key, stored in `^!keys` as trusted.
    fn record_new_share_key(
        &self,
        snapshot: &Snapshot,
        handle: &str,
    ) -> Result<Zeroizing<[u8; 16]>, QuarkError> {
        let binary = binary_handle(handle).ok_or(QuarkError::InvalidResponse)?;
        let key = random_key::<16>();
        let wanted = *key;
        self.update_keys(&snapshot.session, &|container| {
            match container.share_entry(&binary)? {
                None => Ok(Some(Change::AddShareKey {
                    handle: binary,
                    key: wanted,
                    flags: FLAG_TRUSTED,
                })),
                // The same key again (an earlier attempt got this far).
                Some(entry) if *entry.key == wanted => Ok(None),
                // Another client recorded a different key meanwhile.
                Some(_) => Err(unsupported()),
            }
        })?;
        Ok(key)
    }

    /// Steps 2 to 4 for a folder that has a recorded key but no link.
    fn start_link_share(
        &self,
        snapshot: &Snapshot,
        node: &ListedNode,
        key: &[u8; 16],
    ) -> Result<String, QuarkError> {
        let handle = node.handle.as_str();
        let new_share = share_recipients(&snapshot.listing, handle).is_empty();
        let cr = if new_share {
            let tree = subtree(&snapshot.listing, &snapshot.keyring, handle)?;
            let pairs: Vec<(&str, &[u8])> = tree
                .nodes
                .iter()
                .map(|member| (member.node.handle.as_str(), &member.node.key[..]))
                .collect();
            Some(share_keys_cr(handle, key, &pairs).ok_or(QuarkError::Storage)?)
        } else {
            None
        };
        let api = self.api(&snapshot.session);
        api.create_link_share(handle, cr)?;
        self.set_in_use(snapshot, handle, true)?;
        api.export_link(handle)
    }

    /// Sets or clears the "in use" flag of a recorded share key.
    fn set_in_use(
        &self,
        snapshot: &Snapshot,
        handle: &str,
        in_use: bool,
    ) -> Result<(), QuarkError> {
        let binary = binary_handle(handle).ok_or(QuarkError::InvalidResponse)?;
        self.update_keys(&snapshot.session, &|container| {
            let Some(entry) = container.share_entry(&binary)? else {
                // Clearing the flag of a key that was never recorded is moot;
                // setting it on a missing key is a bug.
                return if in_use { Err(unsupported()) } else { Ok(None) };
            };
            let flags = if in_use {
                entry.flags | FLAG_IN_USE
            } else {
                entry.flags & !FLAG_IN_USE
            };
            Ok((flags != entry.flags).then_some(Change::SetFlags {
                handle: binary,
                flags,
            }))
        })
    }

    /// After `l` with `d` for a folder: removes the link share too, and frees
    /// the share key's "in use" flag unless the folder is shared with people.
    /// Accounts without a key manager keep the legacy behaviour (link only).
    pub(super) fn withdraw_folder_share(
        &self,
        snapshot: &Snapshot,
        handle: &str,
    ) -> Result<(), QuarkError> {
        if self.read_keys(&snapshot.session)?.is_none() {
            return Ok(());
        }
        match self.api(&snapshot.session).remove_link_share(handle) {
            // No link share (any more): the goal is reached.
            Ok(()) | Err(QuarkError::SourceFileMissing) => {}
            Err(error) => return Err(error),
        }
        let others = share_recipients(&snapshot.listing, handle)
            .iter()
            .any(|recipient| recipient != LINK_RECIPIENT);
        if others {
            return Ok(());
        }
        self.set_in_use(snapshot, handle, false)
    }
}
