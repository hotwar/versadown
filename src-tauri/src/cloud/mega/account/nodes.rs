//! Reading the account's node listing (`{"a":"f","c":1}`).
//!
//! One call returns every node of the account. Only the nodes that are
//! actually shown or downloaded are decrypted, so listing a directory costs
//! one pass over the raw JSON plus the decryption of its direct children.
//! Nodes that no known key opens (for example another user's share) are
//! skipped rather than failing the whole directory.
//!
//! The listing is also the map for every change: which tree a node lives in
//! (own drive, rubbish bin, or somebody else's share), its ancestors and its
//! descendants.

use super::keys::Keyring;
use crate::cloud::mega::crypto::{decrypt_node_attributes, FileKey};
use crate::cloud::mega::folder::valid_handle;
use crate::cloud::QuarkError;
use serde_json::{Map, Value};
use std::collections::HashMap;
use zeroize::Zeroizing;

/// Defensive bound for a hostile or corrupt listing.
const MAX_NODES: usize = 500_000;
/// Longest ancestor chain followed before the listing is treated as corrupt.
const MAX_DEPTH: usize = 512;

const TYPE_FILE: u64 = 0;
const TYPE_FOLDER: u64 = 1;
const TYPE_CLOUD_DRIVE: u64 = 2;
const TYPE_INBOX: u64 = 3;
const TYPE_RUBBISH: u64 = 4;

pub struct ListedNode {
    pub handle: String,
    pub name: String,
    pub is_file: bool,
    pub size: u64,
    pub timestamp: Option<u64>,
    /// The decrypted node key: 32 bytes for a file, 16 for a folder.
    pub key: Zeroizing<Vec<u8>>,
    /// All decrypted attributes (`n` plus anything other clients stored).
    pub attributes: Map<String, Value>,
}

/// Where a node lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tree {
    /// Below the Cloud Drive root.
    Drive,
    /// Below the rubbish bin root.
    Rubbish,
    /// Inbox or a share of another account: not ours to change.
    Foreign,
}

fn raw_nodes(listing: &Value) -> Result<&Vec<Value>, QuarkError> {
    let nodes = listing["f"].as_array().ok_or(QuarkError::InvalidResponse)?;
    if nodes.len() > MAX_NODES {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(nodes)
}

fn root_of_type(listing: &Value, node_type: u64) -> Result<String, QuarkError> {
    raw_nodes(listing)?
        .iter()
        .find(|node| node["t"].as_u64() == Some(node_type))
        .and_then(|node| node["h"].as_str())
        .filter(|handle| valid_handle(handle))
        .map(str::to_owned)
        .ok_or(QuarkError::InvalidResponse)
}

/// Handle of the "Cloud Drive" root node.
pub fn drive_root(listing: &Value) -> Result<String, QuarkError> {
    root_of_type(listing, TYPE_CLOUD_DRIVE)
}

/// Handle of the rubbish bin root node.
pub fn rubbish_root(listing: &Value) -> Result<String, QuarkError> {
    root_of_type(listing, TYPE_RUBBISH)
}

/// The raw JSON of one node.
pub fn raw_node<'a>(listing: &'a Value, handle: &str) -> Result<Option<&'a Value>, QuarkError> {
    Ok(raw_nodes(listing)?
        .iter()
        .find(|node| node["h"].as_str() == Some(handle)))
}

/// Which tree `handle` belongs to; `None` when the handle is not listed.
pub fn tree_of(listing: &Value, handle: &str) -> Result<Option<Tree>, QuarkError> {
    let nodes = raw_nodes(listing)?;
    let index: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|node| Some((node["h"].as_str()?, node)))
        .collect();
    let mut current = match index.get(handle) {
        Some(node) => *node,
        None => return Ok(None),
    };
    for _ in 0..MAX_DEPTH {
        match current["t"].as_u64() {
            Some(TYPE_CLOUD_DRIVE) => return Ok(Some(Tree::Drive)),
            Some(TYPE_RUBBISH) => return Ok(Some(Tree::Rubbish)),
            Some(TYPE_INBOX) => return Ok(Some(Tree::Foreign)),
            _ => {}
        }
        match current["p"].as_str().and_then(|parent| index.get(parent)) {
            Some(parent) => current = *parent,
            // The chain leaves the listing: the top is somebody's share.
            None => return Ok(Some(Tree::Foreign)),
        }
    }
    Err(QuarkError::InvalidResponse)
}

/// Whether `ancestor` is `handle` itself or one of its ancestors.
pub fn is_within(listing: &Value, handle: &str, ancestor: &str) -> Result<bool, QuarkError> {
    let nodes = raw_nodes(listing)?;
    let parents: HashMap<&str, &str> = nodes
        .iter()
        .filter_map(|node| Some((node["h"].as_str()?, node["p"].as_str()?)))
        .collect();
    let mut current = handle;
    for _ in 0..MAX_DEPTH {
        if current == ancestor {
            return Ok(true);
        }
        match parents.get(current) {
            Some(parent) => current = parent,
            None => return Ok(false),
        }
    }
    Err(QuarkError::InvalidResponse)
}

/// `handle` followed by its ancestors up to the top of the listing.
pub fn ancestry(listing: &Value, handle: &str) -> Result<Vec<String>, QuarkError> {
    let nodes = raw_nodes(listing)?;
    let parents: HashMap<&str, &str> = nodes
        .iter()
        .filter_map(|node| Some((node["h"].as_str()?, node["p"].as_str()?)))
        .collect();
    let mut chain = vec![handle.to_owned()];
    let mut current = handle;
    while let Some(parent) = parents.get(current).copied().filter(|p| !p.is_empty()) {
        if chain.len() > MAX_DEPTH {
            return Err(QuarkError::InvalidResponse);
        }
        chain.push(parent.to_owned());
        current = parent;
    }
    Ok(chain)
}

/// Decrypts one listed node (file or folder). `None` for the special roots
/// and for nodes no known key opens.
pub fn decrypt(keyring: &Keyring, node: &Value) -> Option<ListedNode> {
    let handle = node["h"].as_str().filter(|handle| valid_handle(handle))?;
    let is_file = match node["t"].as_u64()? {
        TYPE_FILE => true,
        TYPE_FOLDER => false,
        _ => return None,
    };
    let encrypted_key = node["k"].as_str()?;
    let encoded_attributes = node["a"].as_str()?;
    let key = keyring.node_key(encrypted_key, if is_file { 32 } else { 16 })?;
    let attribute_key = if is_file {
        FileKey::from_bytes(&key)?.aes_key
    } else {
        let folder_key: [u8; 16] = key.as_slice().try_into().ok()?;
        Zeroizing::new(folder_key)
    };
    let attributes = decrypt_node_attributes(&attribute_key, encoded_attributes)?;
    let name = attributes
        .get("n")?
        .as_str()
        .filter(|name| !name.is_empty())?
        .to_owned();
    Some(ListedNode {
        handle: handle.to_owned(),
        name,
        is_file,
        size: if is_file {
            node["s"].as_u64().unwrap_or(0)
        } else {
            0
        },
        timestamp: node["ts"].as_u64(),
        key,
        attributes,
    })
}

fn sort_nodes(nodes: &mut [ListedNode]) {
    nodes.sort_by(|left, right| {
        (left.is_file, left.name.to_lowercase(), &left.handle).cmp(&(
            right.is_file,
            right.name.to_lowercase(),
            &right.handle,
        ))
    });
}

/// The decryptable direct children of `parent`, folders first, then by name.
/// `None` when no node of the listing has that handle.
pub fn children(
    listing: &Value,
    keyring: &Keyring,
    parent: &str,
) -> Result<Option<Vec<ListedNode>>, QuarkError> {
    let nodes = raw_nodes(listing)?;
    if !nodes.iter().any(|node| node["h"].as_str() == Some(parent)) {
        return Ok(None);
    }
    let mut children: Vec<ListedNode> = nodes
        .iter()
        .filter(|node| node["p"].as_str() == Some(parent))
        .filter_map(|node| decrypt(keyring, node))
        .collect();
    sort_nodes(&mut children);
    Ok(Some(children))
}

/// One decryptable node (file or folder) by handle.
pub fn find_node(
    listing: &Value,
    keyring: &Keyring,
    handle: &str,
) -> Result<ListedNode, QuarkError> {
    raw_node(listing, handle)?
        .and_then(|node| decrypt(keyring, node))
        .ok_or(QuarkError::SourceFileMissing)
}

/// One decryptable file node by handle.
pub fn find_file(
    listing: &Value,
    keyring: &Keyring,
    handle: &str,
) -> Result<ListedNode, QuarkError> {
    find_node(listing, keyring, handle).and_then(|node| {
        if node.is_file {
            Ok(node)
        } else {
            Err(QuarkError::SourceFileMissing)
        }
    })
}

/// The shares other users made to this account: the top node of each (its
/// parent is not in the listing) and, for those, the RSA-encrypted share key
/// `sk` that opens everything below.
pub struct IncomingRoot<'a> {
    pub handle: &'a str,
    pub raw: &'a Value,
    pub sealed_key: Option<&'a str>,
}

fn incoming_roots_raw(listing: &Value) -> Result<Vec<IncomingRoot<'_>>, QuarkError> {
    let nodes = raw_nodes(listing)?;
    let handles: std::collections::HashSet<&str> =
        nodes.iter().filter_map(|node| node["h"].as_str()).collect();
    Ok(nodes
        .iter()
        .filter(|node| matches!(node["t"].as_u64(), Some(TYPE_FILE | TYPE_FOLDER)))
        .filter(|node| {
            node["p"]
                .as_str()
                .is_none_or(|parent| !handles.contains(parent))
        })
        .filter(|node| {
            node["su"].as_str().is_some_and(|owner| !owner.is_empty()) || node["sk"].is_string()
        })
        .filter_map(|node| {
            Some(IncomingRoot {
                handle: node["h"].as_str().filter(|handle| valid_handle(handle))?,
                raw: node,
                sealed_key: node["sk"].as_str().filter(|key| !key.is_empty()),
            })
        })
        .collect())
}

/// Handles and RSA-encrypted keys of the incoming shares of a listing.
pub fn incoming_share_blobs(listing: &Value) -> Result<Vec<(String, String)>, QuarkError> {
    Ok(incoming_roots_raw(listing)?
        .into_iter()
        .filter_map(|root| Some((root.handle.to_owned(), root.sealed_key?.to_owned())))
        .collect())
}

/// The decryptable tops of the incoming shares, sorted like a listing.
pub fn incoming_roots(listing: &Value, keyring: &Keyring) -> Result<Vec<ListedNode>, QuarkError> {
    let mut roots: Vec<ListedNode> = incoming_roots_raw(listing)?
        .into_iter()
        .filter_map(|root| decrypt(keyring, root.raw))
        .collect();
    sort_nodes(&mut roots);
    Ok(roots)
}

/// Every decryptable node that carries a public link, with the link's public
/// handle and creation time (the listing's `ph` array), sorted like a listing.
pub fn exported_nodes(
    listing: &Value,
    keyring: &Keyring,
) -> Result<Vec<(ListedNode, String, Option<u64>)>, QuarkError> {
    let links: HashMap<&str, (&str, Option<u64>)> = listing["ph"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|link| {
            let public = link["ph"].as_str().filter(|public| valid_handle(public))?;
            Some((link["h"].as_str()?, (public, link["ts"].as_u64())))
        })
        .collect();
    let mut exported: Vec<(ListedNode, String, Option<u64>)> = raw_nodes(listing)?
        .iter()
        .filter_map(|node| {
            let (public, created) = links.get(node["h"].as_str()?)?;
            Some((decrypt(keyring, node)?, (*public).to_owned(), *created))
        })
        .collect();
    exported.sort_by(|left, right| {
        (left.0.is_file, left.0.name.to_lowercase(), &left.0.handle).cmp(&(
            right.0.is_file,
            right.0.name.to_lowercase(),
            &right.0.handle,
        ))
    });
    Ok(exported)
}

/// Whether the node carries a public link or any outgoing share (the
/// listing's `ph` and `s` arrays).
pub fn is_exported(listing: &Value, handle: &str) -> bool {
    ["ph", "s"].iter().any(|array| {
        listing[array]
            .as_array()
            .into_iter()
            .flatten()
            .any(|entry| entry["h"].as_str() == Some(handle))
    })
}

/// The public handle of the node's link, when it has one.
pub fn link_handle(listing: &Value, handle: &str) -> Option<String> {
    listing["ph"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|link| link["h"].as_str() == Some(handle))
        .and_then(|link| link["ph"].as_str())
        .filter(|public| valid_handle(public))
        .map(str::to_owned)
}

/// The recipients of the node's outgoing shares (`EXP` is the public link's
/// pseudo recipient) and pending shares.
pub fn share_recipients(listing: &Value, handle: &str) -> Vec<String> {
    ["s", "ps"]
        .iter()
        .flat_map(|array| listing[array].as_array().into_iter().flatten())
        .filter(|entry| entry["h"].as_str() == Some(handle))
        .filter_map(|entry| entry["u"].as_str().or(entry["p"].as_str()))
        .map(str::to_owned)
        .collect()
}

/// A node of a subtree with the handle of its parent inside the subtree
/// (`None` for the top).
pub struct SubtreeNode {
    pub node: ListedNode,
    pub parent: Option<String>,
}

/// `root` and everything below it, parents before children. Nodes no known
/// key opens are left out together with what hangs below them and counted in
/// `unreadable`.
pub struct Subtree {
    pub nodes: Vec<SubtreeNode>,
    pub unreadable: usize,
}

pub fn subtree(listing: &Value, keyring: &Keyring, root: &str) -> Result<Subtree, QuarkError> {
    let nodes = raw_nodes(listing)?;
    let mut below: HashMap<&str, Vec<&Value>> = HashMap::new();
    for node in nodes {
        if let Some(parent) = node["p"].as_str() {
            below.entry(parent).or_default().push(node);
        }
    }
    let top = raw_node(listing, root)?.ok_or(QuarkError::SourceFileMissing)?;
    let mut found = Vec::new();
    let mut unreadable = 0;
    let mut queue = std::collections::VecDeque::from([(top, None::<String>)]);
    while let Some((raw, parent)) = queue.pop_front() {
        let Some(node) = decrypt(keyring, raw) else {
            unreadable += 1;
            continue;
        };
        for child in below.get(node.handle.as_str()).into_iter().flatten() {
            queue.push_back((child, Some(node.handle.clone())));
        }
        found.push(SubtreeNode { node, parent });
        if found.len() + unreadable > MAX_NODES {
            return Err(QuarkError::InvalidResponse);
        }
    }
    Ok(Subtree {
        nodes: found,
        unreadable,
    })
}
