//! Copying nodes (`p` with the source handles).
//!
//! A copy is a put-nodes request (one per copied node) whose entries name existing nodes by handle
//! (`h`). Every node of the copied tree must come with its own key encrypted
//! for the account (`k`, AES-ECB under the master key) and its attributes
//! sealed under that key (`a`); children point at their parent's source handle
//! (`p`) and only the tops go below the target (`t`). Files keep their key,
//! each folder gets a fresh one, and the restore marker `rr` of a node that
//! came out of the rubbish bin is dropped, as MEGA's own clients do
//! (`MegaClient::putnodes_prepareCopy` in `megaclient.cpp`; no code is
//! copied). Copies below a shared folder also get their share keys (`cr`).
//!
//! Sources are the account's own drive and folders shared with it (their keys
//! open with the share key and are re-encrypted for this account).

use super::client::MegaAccountClient;
use super::manage::{random_key, Snapshot};
use super::nodes::{raw_node, subtree, tree_of, ListedNode, Subtree, Tree};
use super::seal::{attribute_key, seal_attributes, wrap_key};
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::mega::folder::valid_handle;
use crate::cloud::QuarkError;
use serde_json::{json, Value};
use zeroize::Zeroizing;

/// The attribute under which a node remembers where it was before it went to
/// the rubbish bin.
const RESTORE_ATTRIBUTE: &str = "rr";
/// The most nodes one copy request carries.
const MAX_COPY_NODES: usize = 5_000;

/// What a copy sends, and the keys of the nodes it creates (in request order).
pub(super) struct CopyPlan {
    pub entries: Vec<Value>,
    pub keys: Vec<Zeroizing<Vec<u8>>>,
}

fn new_key(node: &ListedNode) -> Zeroizing<Vec<u8>> {
    if node.is_file {
        node.key.clone()
    } else {
        Zeroizing::new(random_key::<16>().to_vec())
    }
}

/// The request entries for copying `trees` (each a node with what is below
/// it), wrapped for the account with `master_key`.
pub(super) fn plan_copy(trees: &[Subtree], master_key: &[u8; 16]) -> Result<CopyPlan, QuarkError> {
    let total: usize = trees.iter().map(|tree| tree.nodes.len()).sum();
    if total > MAX_COPY_NODES {
        return Err(QuarkError::Mega(MegaFailure::CopyTooLarge));
    }
    let mut entries = Vec::with_capacity(total);
    let mut keys = Vec::with_capacity(total);
    for tree in trees {
        for member in &tree.nodes {
            let node = &member.node;
            let key = new_key(node);
            let mut attributes = node.attributes.clone();
            attributes.remove(RESTORE_ATTRIBUTE);
            let sealed = attribute_key(&key)
                .and_then(|attribute_key| seal_attributes(&attribute_key, &attributes))
                .ok_or(QuarkError::Storage)?;
            let wrapped = wrap_key(master_key, &key).ok_or(QuarkError::Storage)?;
            let mut entry = json!({
                "h": node.handle,
                "t": if node.is_file { 0 } else { 1 },
                "a": sealed,
                "k": wrapped,
            });
            if let Some(parent) = &member.parent {
                entry["p"] = Value::String(parent.clone());
            }
            entries.push(entry);
            keys.push(key);
        }
    }
    Ok(CopyPlan { entries, keys })
}

/// Pairs each created node of a `p` reply with the key it was sent with. The
/// reply lists the new nodes in request order; the sealed attributes (equal
/// to what was sent) confirm that before any key is attributed to a handle.
pub(super) fn created_with_keys<'a>(
    reply: &'a Value,
    plan: &'a CopyPlan,
) -> Option<Vec<(&'a str, &'a [u8])>> {
    let created = reply["f"].as_array()?;
    if created.len() != plan.entries.len() {
        return None;
    }
    created
        .iter()
        .zip(&plan.entries)
        .zip(&plan.keys)
        .map(|((node, sent), key)| {
            let same = node["a"].as_str()? == sent["a"].as_str()?;
            let handle = node["h"].as_str().filter(|handle| valid_handle(handle))?;
            same.then_some((handle, &key[..]))
        })
        .collect()
}

impl MegaAccountClient<'_> {
    /// Copies nodes of the drive (or of a folder shared with the account)
    /// below a folder of the drive.
    pub fn copy_files(&self, file_ids: &[String], parent_id: &str) -> Result<(), QuarkError> {
        if file_ids.is_empty() {
            return Ok(());
        }
        let snapshot = self.snapshot()?;
        let result = self.copy_into(&snapshot, file_ids, parent_id);
        self.cache.invalidate();
        result
    }

    fn copy_into(
        &self,
        snapshot: &Snapshot,
        file_ids: &[String],
        parent_id: &str,
    ) -> Result<(), QuarkError> {
        let parent = self.writable_parent(&snapshot.listing, parent_id)?;
        let mut trees = Vec::with_capacity(file_ids.len());
        for id in file_ids {
            trees.push(self.copy_source(snapshot, id)?);
        }
        let master = snapshot.session.master_key()?;
        // One request per selected node: a node selected together with the
        // folder it is in must not appear twice in one request. Everything is
        // checked and planned before the first request goes out.
        let plans = trees
            .iter()
            .map(|tree| plan_copy(std::slice::from_ref(tree), &master))
            .collect::<Result<Vec<_>, _>>()?;
        for plan in plans {
            let reply = self
                .api(&snapshot.session)
                .put_nodes(&parent, plan.entries.clone())?;
            if let Some(created) = created_with_keys(&reply, &plan) {
                self.share_new_nodes(snapshot, &parent, &created);
            }
        }
        Ok(())
    }

    /// The node and everything below it, complete and readable.
    fn copy_source(&self, snapshot: &Snapshot, id: &str) -> Result<Subtree, QuarkError> {
        let raw = valid_handle(id)
            .then(|| raw_node(&snapshot.listing, id))
            .transpose()?
            .flatten()
            .ok_or(QuarkError::SourceFileMissing)?;
        // Only files and folders: never the drive, inbox or rubbish roots.
        if !matches!(raw["t"].as_u64(), Some(0 | 1)) {
            return Err(QuarkError::Mega(MegaFailure::AccessDenied));
        }
        match tree_of(&snapshot.listing, id)? {
            Some(Tree::Drive | Tree::Foreign) => {}
            Some(Tree::Rubbish) => return Err(QuarkError::Mega(MegaFailure::AccessDenied)),
            None => return Err(QuarkError::SourceFileMissing),
        }
        let tree = subtree(&snapshot.listing, &snapshot.keyring, id)?;
        match (tree.nodes.first(), tree.unreadable) {
            (None, _) => Err(QuarkError::SourceFileMissing),
            // Something below the node cannot be decrypted: a copy would be
            // silently incomplete.
            (Some(_), 0) => Ok(tree),
            (Some(_), _) => Err(QuarkError::InvalidResponse),
        }
    }
}
