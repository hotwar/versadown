//! Changing the account's own tree: new folders, rename, move, the rubbish
//! bin (trash, restore, delete for good).
//!
//! Every change starts from a fresh node listing (the cached one may be a few
//! seconds old) and invalidates the cache afterwards. Only nodes below the
//! Cloud Drive root (or, for restore and delete, direct entries of the rubbish
//! bin) are touched: the inbox and other people's shares are refused.
//!
//! Commands: `p` (put nodes), `a` (set attributes), `m` (move), `d` (delete).
//! Written from the public megajs 1.3.x reference (`lib/mutable-file.mjs`,
//! `lib/storage.mjs`) and MEGA's SDK behaviour; no code is copied.

use super::api::AccountApi;
use super::client::{special_ids, Listing, MegaAccountClient};
use super::keys::Keyring;
use super::nodes::{
    decrypt, drive_root, find_node, is_within, raw_node, rubbish_root, tree_of, Tree,
};
use super::seal::{attribute_key, name_attributes, seal_attributes, validate_name, wrap_key};
use super::session::MegaSession;
use crate::cloud::common::validate_recycle_entry_ids;
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::mega::folder::valid_handle;
use crate::cloud::{CloudFile, CloudFileKind, QuarkError};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use std::sync::Arc;
use zeroize::Zeroizing;

/// The attribute under which a node remembers where it was before it went to
/// the rubbish bin (the same name MEGA's own clients use).
const RESTORE_ATTRIBUTE: &str = "rr";

const TYPE_FOLDER: u64 = 1;
const TYPE_CLOUD_DRIVE: u64 = 2;

/// One consistent view for a change: session, fresh listing and keys.
pub(super) struct Snapshot {
    pub session: MegaSession,
    pub listing: Arc<Listing>,
    pub keyring: Keyring,
}

pub(super) fn random_key<const N: usize>() -> Zeroizing<[u8; N]> {
    let mut key = Zeroizing::new([0u8; N]);
    OsRng.fill_bytes(&mut *key);
    key
}

impl MegaAccountClient<'_> {
    /// A listing fetched now, never from the cache.
    pub(super) fn snapshot(&self) -> Result<Snapshot, QuarkError> {
        let session = MegaSession::load(self.store)?;
        self.cache.invalidate();
        let listing = self.listing(&session)?;
        let keyring = Self::keyring(&session, &listing)?;
        Ok(Snapshot {
            session,
            listing,
            keyring,
        })
    }

    pub(super) fn api<'s>(&'s self, session: &'s MegaSession) -> AccountApi<'s> {
        AccountApi::with_session(self.http, session.sid())
    }

    /// Runs a change and forgets the cached listing whatever its outcome:
    /// even a partly applied change must not hide behind a stale cache.
    fn changing<T>(
        &self,
        change: impl FnOnce(&Snapshot) -> Result<T, QuarkError>,
    ) -> Result<T, QuarkError> {
        let snapshot = self.snapshot()?;
        let result = change(&snapshot);
        self.cache.invalidate();
        result
    }

    /// The node handle behind a destination id, which must be a folder (or the
    /// drive root) of the Cloud Drive tree.
    pub(super) fn writable_parent(
        &self,
        listing: &Value,
        parent_id: &str,
    ) -> Result<String, QuarkError> {
        let ids = special_ids();
        if parent_id == ids.root {
            return drive_root(listing);
        }
        if parent_id == ids.recycle_bin {
            return Err(QuarkError::InvalidInput("回收站不能作为目标目录".into()));
        }
        let node = valid_handle(parent_id)
            .then(|| raw_node(listing, parent_id))
            .transpose()?
            .flatten()
            .ok_or(QuarkError::SourceFileMissing)?;
        if !matches!(node["t"].as_u64(), Some(TYPE_FOLDER | TYPE_CLOUD_DRIVE)) {
            return Err(QuarkError::SourceFileMissing);
        }
        match tree_of(listing, parent_id)? {
            Some(Tree::Drive) => Ok(parent_id.to_owned()),
            Some(_) => Err(QuarkError::Mega(MegaFailure::AccessDenied)),
            None => Err(QuarkError::SourceFileMissing),
        }
    }

    pub fn create_directory(&self, parent_id: &str, name: &str) -> Result<CloudFile, QuarkError> {
        let name = name.trim();
        validate_name(name)?;
        self.changing(|snapshot| {
            let parent = self.writable_parent(&snapshot.listing, parent_id)?;
            let master = snapshot.session.master_key()?;
            let key = random_key::<16>();
            let sealed =
                seal_attributes(&key, &name_attributes(name)).ok_or(QuarkError::Storage)?;
            let wrapped = wrap_key(&master, &*key).ok_or(QuarkError::Storage)?;
            let reply = self.api(&snapshot.session).put_nodes(
                &parent,
                vec![json!({"h": "xxxxxxxx", "t": 1, "a": sealed, "k": wrapped})],
            )?;
            let created = created_node(&reply)?;
            self.share_new_nodes(snapshot, &parent, &[(created.handle.as_str(), &key[..])]);
            Ok(CloudFile {
                id: created.handle,
                parent_id: parent_id.to_owned(),
                name: name.to_owned(),
                kind: CloudFileKind::Folder,
                size: None,
                modified_at: created.timestamp.map(|seconds| seconds.to_string()),
            })
        })
    }

    pub fn rename(&self, file_id: &str, name: &str) -> Result<(), QuarkError> {
        let name = name.trim();
        validate_name(name)?;
        self.changing(|snapshot| {
            let node = self.own_node(snapshot, file_id, &[Tree::Drive, Tree::Rubbish])?;
            let mut attributes = node.attributes.clone();
            attributes.insert("n".into(), Value::String(name.to_owned()));
            self.store_attributes(snapshot, &node.handle, &node.key, &attributes)
        })
    }

    pub fn move_files(&self, file_ids: &[String], parent_id: &str) -> Result<(), QuarkError> {
        self.changing(|snapshot| {
            let parent = self.writable_parent(&snapshot.listing, parent_id)?;
            for id in file_ids {
                let node = self.own_raw(snapshot, id, &[Tree::Drive])?;
                if node["p"].as_str() == Some(parent.as_str()) {
                    continue;
                }
                // A folder cannot go into itself or below itself.
                if is_within(&snapshot.listing, &parent, id)? {
                    return Err(QuarkError::Mega(MegaFailure::InvalidMove));
                }
                self.api(&snapshot.session).move_node(id, &parent)?;
            }
            Ok(())
        })
    }

    /// Moves the nodes to the rubbish bin, noting where each one came from so
    /// that a restore can put it back.
    pub fn trash(&self, file_ids: &[String]) -> Result<(), QuarkError> {
        self.changing(|snapshot| {
            let rubbish = rubbish_root(&snapshot.listing)?;
            for id in file_ids {
                let raw = self.own_raw(snapshot, id, &[Tree::Drive, Tree::Rubbish])?;
                if tree_of(&snapshot.listing, id)? == Some(Tree::Rubbish) {
                    continue;
                }
                if let Some(origin) = raw["p"].as_str().filter(|handle| valid_handle(handle)) {
                    self.remember_origin(snapshot, id, origin)?;
                }
                self.api(&snapshot.session).move_node(id, &rubbish)?;
            }
            Ok(())
        })
    }

    /// Permanently deletes direct entries of the rubbish bin.
    pub fn delete(&self, file_ids: &[String]) -> Result<(), QuarkError> {
        self.changing(|snapshot| {
            self.check_recycle_entries(snapshot, file_ids)?;
            for id in file_ids {
                self.api(&snapshot.session).delete_node(id)?;
            }
            Ok(())
        })
    }

    /// Moves direct entries of the rubbish bin back to where they came from,
    /// or to the drive root when that folder is gone (or never recorded).
    pub fn restore(&self, file_ids: &[String]) -> Result<(), QuarkError> {
        self.changing(|snapshot| {
            self.check_recycle_entries(snapshot, file_ids)?;
            let root = drive_root(&snapshot.listing)?;
            for id in file_ids {
                let origin = self.recorded_origin(snapshot, id)?;
                let target = match origin {
                    Some(handle) if self.is_drive_folder(snapshot, &handle)? => handle,
                    _ => root.clone(),
                };
                self.api(&snapshot.session).move_node(id, &target)?;
            }
            Ok(())
        })
    }

    fn is_drive_folder(&self, snapshot: &Snapshot, handle: &str) -> Result<bool, QuarkError> {
        let is_folder = raw_node(&snapshot.listing, handle)?
            .is_some_and(|node| matches!(node["t"].as_u64(), Some(TYPE_FOLDER | TYPE_CLOUD_DRIVE)));
        Ok(is_folder && tree_of(&snapshot.listing, handle)? == Some(Tree::Drive))
    }

    /// The entries of the rubbish bin as the UI shows them must contain every id.
    fn check_recycle_entries(
        &self,
        snapshot: &Snapshot,
        file_ids: &[String],
    ) -> Result<(), QuarkError> {
        let ids = special_ids();
        let rubbish = rubbish_root(&snapshot.listing)?;
        let entries = super::client::directory_of(
            &snapshot.listing,
            &snapshot.keyring,
            &rubbish,
            ids.recycle_bin,
        )?;
        validate_recycle_entry_ids(file_ids, &entries)
    }

    fn recorded_origin(&self, snapshot: &Snapshot, id: &str) -> Result<Option<String>, QuarkError> {
        let node = find_node(&snapshot.listing, &snapshot.keyring, id)?;
        Ok(node
            .attributes
            .get(RESTORE_ATTRIBUTE)
            .and_then(Value::as_str)
            .filter(|handle| valid_handle(handle))
            .map(str::to_owned))
    }

    fn remember_origin(
        &self,
        snapshot: &Snapshot,
        id: &str,
        origin: &str,
    ) -> Result<(), QuarkError> {
        let node = find_node(&snapshot.listing, &snapshot.keyring, id)?;
        let mut attributes = node.attributes.clone();
        attributes.insert(RESTORE_ATTRIBUTE.into(), Value::String(origin.to_owned()));
        self.store_attributes(snapshot, &node.handle, &node.key, &attributes)
    }

    fn store_attributes(
        &self,
        snapshot: &Snapshot,
        handle: &str,
        node_key: &[u8],
        attributes: &serde_json::Map<String, Value>,
    ) -> Result<(), QuarkError> {
        let key = attribute_key(node_key).ok_or(QuarkError::InvalidResponse)?;
        let sealed = seal_attributes(&key, attributes).ok_or(QuarkError::Storage)?;
        self.api(&snapshot.session).set_attributes(handle, &sealed)
    }

    /// The raw node of an editable (file or folder) node of the allowed trees.
    pub(super) fn own_raw<'s>(
        &self,
        snapshot: &'s Snapshot,
        handle: &str,
        trees: &[Tree],
    ) -> Result<&'s Value, QuarkError> {
        let raw = valid_handle(handle)
            .then(|| raw_node(&snapshot.listing, handle))
            .transpose()?
            .flatten()
            .ok_or(QuarkError::SourceFileMissing)?;
        // The special roots (drive, inbox, rubbish) are never edited.
        if !matches!(raw["t"].as_u64(), Some(0 | TYPE_FOLDER)) {
            return Err(QuarkError::Mega(MegaFailure::AccessDenied));
        }
        match tree_of(&snapshot.listing, handle)? {
            Some(tree) if trees.contains(&tree) => Ok(raw),
            Some(_) => Err(QuarkError::Mega(MegaFailure::AccessDenied)),
            None => Err(QuarkError::SourceFileMissing),
        }
    }

    pub(super) fn own_node(
        &self,
        snapshot: &Snapshot,
        handle: &str,
        trees: &[Tree],
    ) -> Result<super::nodes::ListedNode, QuarkError> {
        let raw = self.own_raw(snapshot, handle, trees)?;
        decrypt(&snapshot.keyring, raw).ok_or(QuarkError::SourceFileMissing)
    }

    /// A node created below an exported folder is invisible to the link's
    /// holders until its key is also encrypted with the share key. Best effort:
    /// the node itself exists either way.
    pub(super) fn share_new_nodes(
        &self,
        snapshot: &Snapshot,
        parent: &str,
        nodes: &[(&str, &[u8])],
    ) {
        let Ok(Some((share_handle, share_key))) = self.share_key_above(snapshot, parent) else {
            return;
        };
        let Some(cr) = super::folder_share::share_keys_cr(&share_handle, &share_key, nodes) else {
            return;
        };
        let _ = self.api(&snapshot.session).send_share_keys(cr);
    }
}

/// The node a `p` reply created.
pub(super) struct CreatedNode {
    pub handle: String,
    pub timestamp: Option<u64>,
}

pub(super) fn created_node(reply: &Value) -> Result<CreatedNode, QuarkError> {
    let node = reply["f"]
        .as_array()
        .and_then(|nodes| nodes.first())
        .ok_or(QuarkError::InvalidResponse)?;
    let handle = node["h"]
        .as_str()
        .filter(|handle| valid_handle(handle))
        .ok_or(QuarkError::InvalidResponse)?;
    Ok(CreatedNode {
        handle: handle.to_owned(),
        timestamp: node["ts"].as_u64(),
    })
}
