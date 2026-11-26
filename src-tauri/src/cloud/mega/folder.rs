//! Decoding of a folder link's node listing (`{"a":"f","c":1,"r":1}`).
//!
//! Every node key in the listing is AES-ECB encrypted with the folder key.
//! Nodes that this key does not decrypt (they belong to another share) are
//! ignored; if the folder's own root cannot be decrypted the key is wrong.
//!
//! The link's id is the share's public handle, which differs from the handle of
//! the folder node at its root (verified against the live API), so the root is
//! found structurally: the first folder in the listing that is not nested in
//! another decryptable node.

use super::crypto::{decode_base64url, decrypt_ecb, decrypt_node_name, FileKey};
use super::failure::MegaFailure;
use crate::cloud::QuarkError;
use serde_json::Value;
use std::collections::HashMap;
use zeroize::Zeroizing;

/// Defensive bounds for a hostile or corrupt listing.
const MAX_NODES: usize = 50_000;
const MAX_DEPTH: usize = 128;

const NODE_TYPE_FILE: u64 = 0;
const NODE_TYPE_FOLDER: u64 = 1;

pub struct FolderFile {
    pub handle: String,
    pub name: String,
    /// Names of the folders between the link's root and the file.
    pub directory: Vec<String>,
    pub size: u64,
    /// The decrypted 32-byte file key.
    pub key: Zeroizing<[u8; 32]>,
}

struct Node {
    parent: String,
    name: String,
    is_file: bool,
    size: u64,
    key: Option<Zeroizing<[u8; 32]>>,
}

pub fn valid_handle(handle: &str) -> bool {
    !handle.is_empty()
        && handle
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Decrypt one node: returns its name and (for files) the 32-byte key.
fn decrypt_node(
    folder_key: &[u8; 16],
    node: &Value,
    is_file: bool,
) -> Option<(String, Option<Zeroizing<[u8; 32]>>)> {
    let attributes = node["a"].as_str()?;
    // `k` may list several alternatives ("owner:key/owner2:key2").
    for alternative in node["k"].as_str()?.split('/') {
        let (_, encoded) = alternative.rsplit_once(':')?;
        let Some(encrypted) = decode_base64url(encoded) else {
            continue;
        };
        let Some(raw) = decrypt_ecb(folder_key, &encrypted).map(Zeroizing::new) else {
            continue;
        };
        if is_file && raw.len() == 32 {
            let file_key = FileKey::from_bytes(&raw)?;
            if let Some(name) = decrypt_node_name(&file_key.aes_key, attributes) {
                let mut key = [0u8; 32];
                key.copy_from_slice(&raw);
                return Some((name, Some(Zeroizing::new(key))));
            }
        } else if !is_file && raw.len() == 16 {
            let key: [u8; 16] = raw.as_slice().try_into().ok()?;
            if let Some(name) = decrypt_node_name(&key, attributes) {
                return Some((name, None));
            }
        }
    }
    None
}

fn parse_nodes(
    folder_key: &[u8; 16],
    listing: &Value,
) -> Result<(HashMap<String, Node>, Vec<String>), QuarkError> {
    let raw_nodes = listing["f"].as_array().ok_or(QuarkError::InvalidResponse)?;
    if raw_nodes.len() > MAX_NODES {
        return Err(QuarkError::InvalidResponse);
    }
    let mut nodes = HashMap::new();
    let mut order = Vec::new();
    for raw in raw_nodes {
        let handle = raw["h"].as_str().ok_or(QuarkError::InvalidResponse)?;
        if !valid_handle(handle) {
            return Err(QuarkError::InvalidResponse);
        }
        let is_file = match raw["t"].as_u64() {
            Some(NODE_TYPE_FILE) => true,
            Some(NODE_TYPE_FOLDER) => false,
            // Inbox / trash / root-of-cloud nodes never belong to a link.
            _ => continue,
        };
        let Some((name, key)) = decrypt_node(folder_key, raw, is_file) else {
            continue;
        };
        let node = Node {
            parent: raw["p"].as_str().unwrap_or_default().to_owned(),
            name,
            is_file,
            size: raw["s"].as_u64().unwrap_or(0),
            key,
        };
        if nodes.insert(handle.to_owned(), node).is_some() {
            return Err(QuarkError::InvalidResponse);
        }
        order.push(handle.to_owned());
    }
    Ok((nodes, order))
}

/// Handle of the folder node the link points at.
fn root_handle<'a>(nodes: &HashMap<String, Node>, order: &'a [String]) -> Option<&'a str> {
    order
        .iter()
        .find(|handle| {
            let node = &nodes[handle.as_str()];
            !node.is_file && !nodes.contains_key(node.parent.as_str())
        })
        .map(String::as_str)
}

/// Folder names from the link's root down to (excluding) `node`'s own name.
fn directory_of(nodes: &HashMap<String, Node>, root: &str, node: &Node) -> Option<Vec<String>> {
    let mut path = Vec::new();
    let mut parent = node.parent.as_str();
    while parent != root {
        if path.len() >= MAX_DEPTH {
            return None;
        }
        let folder = nodes.get(parent).filter(|folder| !folder.is_file)?;
        path.push(folder.name.clone());
        parent = folder.parent.as_str();
    }
    path.reverse();
    Some(path)
}

/// All files below the folder link, in a stable order.
pub fn decode_folder_listing(
    folder_key: &[u8; 16],
    listing: &Value,
) -> Result<Vec<FolderFile>, QuarkError> {
    let (nodes, order) = parse_nodes(folder_key, listing)?;
    let Some(folder_id) = root_handle(&nodes, &order) else {
        return Err(QuarkError::Mega(MegaFailure::KeyInvalid));
    };
    let mut files = nodes
        .iter()
        .filter(|(_, node)| node.is_file)
        .filter_map(|(handle, node)| {
            Some(FolderFile {
                handle: handle.clone(),
                name: node.name.clone(),
                directory: directory_of(&nodes, folder_id, node)?,
                size: node.size,
                key: node.key.clone()?,
            })
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        (&left.directory, &left.name, &left.handle).cmp(&(
            &right.directory,
            &right.name,
            &right.handle,
        ))
    });
    Ok(files)
}
