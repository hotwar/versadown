//! Saving a public MEGA link into the signed-in account ("transfer").
//!
//! A link's node keys are encrypted for the link (the file key itself for a
//! file link, the folder key for a folder link). Saving it is a put-nodes
//! request (`p`) that names the shared nodes and carries each node's key
//! encrypted for the account instead: files keep their key, folders get a
//! fresh one (their attributes are sealed again), exactly as a copy inside the
//! account does (`copy.rs`). A file link names its node by the link's public
//! handle (`ph`); the nodes of a folder link are named by their own handles
//! (`h`), which MEGA accepts for any node inside an exported folder. Written from MEGA's open source SDK
//! (`MegaClient::putnodes_prepareCopy`, `CommandPutNodes`); no code is copied.

use super::client::MegaAccountClient;
use super::copy::{created_with_keys, plan_copy, CopyPlan};
use super::keys::Keyring;
use super::manage::{created_node, Snapshot};
use super::nodes::{subtree, ListedNode, Subtree};
use super::seal::{attribute_key, wrap_key};
use crate::cloud::mega::api::MegaApi;
use crate::cloud::mega::crypto::{decode_base64url, decrypt_ecb, decrypt_node_attributes, FileKey};
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::mega::folder::valid_handle;
use crate::cloud::mega::link::{MegaLink, MegaLinkKind};
use crate::cloud::{CloudFile, CloudFileKind, QuarkError};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use zeroize::Zeroizing;

fn invalid() -> QuarkError {
    QuarkError::Mega(MegaFailure::KeyInvalid)
}

/// The node of a file link, ready to be named in a put-nodes request.
struct FileLinkNode {
    entry: Value,
    key: Vec<u8>,
    top: ListedNode,
}

/// The name in a file's sealed attributes; opening them proves the key right.
fn file_name(key: &FileKey, sealed: &str) -> Result<String, QuarkError> {
    decrypt_node_attributes(&key.aes_key, sealed)
        .and_then(|attributes| attributes.get("n")?.as_str().map(str::to_owned))
        .filter(|name| !name.is_empty())
        .ok_or_else(invalid)
}

/// A file as the caller sees it afterwards (the handle is replaced by the
/// created node's).
fn file_top(handle: &str, name: String, size: u64, key: &[u8]) -> ListedNode {
    ListedNode {
        handle: handle.to_owned(),
        name,
        is_file: true,
        size,
        timestamp: None,
        key: Zeroizing::new(key.to_vec()),
        attributes: Default::default(),
    }
}

/// The owner name under which a folder link's key is filed in the keyring.
const LINK_OWNER: &str = "link";

/// A node's `k` lists alternatives (`owner:key/owner:key`), only one of which
/// is encrypted with the folder key. Rewrites it to that single alternative:
/// the one whose result opens the node's attributes.
fn link_node_key(folder_key: &[u8; 16], node: &Value) -> Option<String> {
    let is_file = node["t"].as_u64()? == 0;
    let attributes = node["a"].as_str()?;
    node["k"].as_str()?.split('/').find_map(|alternative| {
        let (_, encoded) = alternative.split_once(':')?;
        let encrypted = decode_base64url(encoded)?;
        let raw = Zeroizing::new(decrypt_ecb(folder_key, &encrypted)?);
        let attribute_key =
            attribute_key(&raw).filter(|_| raw.len() == if is_file { 32 } else { 16 })?;
        decrypt_node_attributes(&attribute_key, attributes)?;
        Some(format!("{LINK_OWNER}:{encoded}"))
    })
}

/// A folder link's whole tree, readable with the link key.
pub(super) fn folder_tree(
    link: &MegaLink,
    listing: &Value,
    master: &[u8; 16],
) -> Result<Subtree, QuarkError> {
    let folder_key: [u8; 16] = link.key.as_slice().try_into().map_err(|_| invalid())?;
    let nodes = listing["f"].as_array().ok_or(QuarkError::InvalidResponse)?;
    let readable: Vec<Value> = nodes
        .iter()
        .filter(|node| matches!(node["t"].as_u64(), Some(0 | 1)))
        .filter_map(|node| {
            let mut node = node.clone();
            node["k"] = Value::String(link_node_key(&folder_key, &node)?);
            Some(node)
        })
        .collect();
    let expected = nodes
        .iter()
        .filter(|node| matches!(node["t"].as_u64(), Some(0 | 1)))
        .count();
    if readable.is_empty() {
        // Not one node opens: this is not the link's key.
        return Err(invalid());
    }
    if readable.len() != expected {
        // A copy that silently lacks nodes would be worse than none.
        return Err(QuarkError::InvalidResponse);
    }
    let keyring = Keyring::new("", master)
        .with_incoming_shares(&[(LINK_OWNER.to_owned(), Zeroizing::new(folder_key))]);
    let handles: BTreeSet<&str> = readable
        .iter()
        .filter_map(|node| node["h"].as_str())
        .collect();
    let root = readable
        .iter()
        .find(|node| {
            node["t"].as_u64() == Some(1)
                && node["p"]
                    .as_str()
                    .is_none_or(|parent| !handles.contains(parent))
        })
        .and_then(|node| node["h"].as_str().filter(|handle| valid_handle(handle)))
        .ok_or_else(invalid)?;
    let trimmed = json!({"f": readable});
    let tree = subtree(&trimmed, &keyring, root)?;
    if tree.nodes.is_empty() {
        return Err(invalid());
    }
    Ok(tree)
}

fn cloud_file(node: &ListedNode, handle: &str, parent_id: &str) -> CloudFile {
    CloudFile {
        id: handle.to_owned(),
        parent_id: parent_id.to_owned(),
        name: node.name.clone(),
        kind: if node.is_file {
            CloudFileKind::File
        } else {
            CloudFileKind::Folder
        },
        size: node.is_file.then_some(node.size),
        modified_at: None,
    }
}

impl MegaAccountClient<'_> {
    fn file_link_node(
        &self,
        link: &MegaLink,
        master: &[u8; 16],
    ) -> Result<FileLinkNode, QuarkError> {
        let key = FileKey::from_bytes(&link.key).ok_or_else(invalid)?;
        let info = MegaApi::new(self.http).file_info(&link.id)?;
        // The attributes stay sealed as the link has them (the key is the
        // same).
        let name = file_name(&key, &info.encrypted_attributes)?;
        let wrapped = wrap_key(master, &link.key).ok_or(QuarkError::Storage)?;
        Ok(FileLinkNode {
            entry: json!({
                "ph": link.id, "t": 0,
                "a": info.encrypted_attributes, "k": wrapped,
            }),
            key: link.key.to_vec(),
            top: file_top(&link.id, name, info.size, &link.key),
        })
    }

    /// Saves a public file or folder link below `parent_id` (a folder of the
    /// drive, or the drive root). Returns the created top node.
    pub fn save_public_link(
        &self,
        link: &MegaLink,
        parent_id: &str,
    ) -> Result<CloudFile, QuarkError> {
        let snapshot = self.snapshot()?;
        let result = (|| {
            let master = snapshot.session.master_key()?;
            let (plan, top) = match link.kind {
                MegaLinkKind::File => {
                    let node = self.file_link_node(link, &master)?;
                    let plan = CopyPlan {
                        entries: vec![node.entry],
                        keys: vec![Zeroizing::new(node.key.clone())],
                    };
                    (plan, node.top)
                }
                MegaLinkKind::Folder => {
                    let listing = MegaApi::new(self.http).folder_listing(&link.id)?;
                    let tree = folder_tree(link, &listing, &master)?;
                    let plan = plan_copy(std::slice::from_ref(&tree), &master)?;
                    let top = tree.nodes.into_iter().next().ok_or_else(invalid)?.node;
                    (plan, top)
                }
            };
            self.send_import(&snapshot, parent_id, &plan, &top)
        })();
        self.cache.invalidate();
        result
    }

    /// Saves one file of a public folder link below `parent_id`, given its
    /// handle and 32-byte key (the folder key is not needed: the key opens
    /// the file's own attributes, which stay sealed as the link has them).
    pub fn save_folder_file(
        &self,
        folder_id: &str,
        handle: &str,
        key: &[u8],
        parent_id: &str,
    ) -> Result<CloudFile, QuarkError> {
        let file_key = FileKey::from_bytes(key).ok_or_else(invalid)?;
        if !valid_handle(handle) {
            return Err(QuarkError::InvalidResponse);
        }
        let listing = MegaApi::new(self.http).folder_listing(folder_id)?;
        let node = listing["f"]
            .as_array()
            .ok_or(QuarkError::InvalidResponse)?
            .iter()
            .find(|node| node["h"].as_str() == Some(handle) && node["t"].as_u64() == Some(0))
            .ok_or(QuarkError::SourceFileMissing)?;
        let sealed = node["a"].as_str().ok_or(QuarkError::InvalidResponse)?;
        let name = file_name(&file_key, sealed)?;
        let snapshot = self.snapshot()?;
        let result = (|| {
            let master = snapshot.session.master_key()?;
            let plan = CopyPlan {
                entries: vec![json!({
                    "h": handle, "t": 0, "a": sealed,
                    "k": wrap_key(&master, key).ok_or(QuarkError::Storage)?,
                })],
                keys: vec![Zeroizing::new(key.to_vec())],
            };
            let top = file_top(handle, name, node["s"].as_u64().unwrap_or(0), key);
            self.send_import(&snapshot, parent_id, &plan, &top)
        })();
        self.cache.invalidate();
        result
    }

    fn send_import(
        &self,
        snapshot: &Snapshot,
        parent_id: &str,
        plan: &CopyPlan,
        top: &ListedNode,
    ) -> Result<CloudFile, QuarkError> {
        let parent = self.writable_parent(&snapshot.listing, parent_id)?;
        let reply = self
            .api(&snapshot.session)
            .put_nodes(&parent, plan.entries.clone())?;
        let created = created_node(&reply)?;
        if let Some(pairs) = created_with_keys(&reply, plan) {
            self.share_new_nodes(snapshot, &parent, &pairs);
        }
        Ok(cloud_file(top, &created.handle, parent_id))
    }
}
