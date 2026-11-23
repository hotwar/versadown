//! Public links of the account's own files and folders.
//!
//! * A file link is `https://mega.nz/file/<public handle>#<file key>`: the
//!   `l` command exports the node and returns the public handle, and the
//!   32-byte node key is the fragment.
//! * A folder link is `https://mega.nz/folder/<public handle>#<share key>`.
//!   It needs the folder's share key. Accounts with MEGA's key manager (the
//!   `^!keys` attribute, every account created since 2023) keep share keys
//!   there and refuse the legacy way of creating one (`s2` with a real `ok`
//!   answers `-3`): `folder_share.rs` records a new key in `^!keys` first.
//!   For an older account only folders that already have a share key in the
//!   listing (`ok`) are linked; creating one otherwise fails with
//!   [`MegaFailure::FolderShareUnsupported`] and nothing is half-created.
//! * A link is withdrawn with `l` and `d`; for a key-manager account the
//!   folder's link share is removed as well (`folder_share.rs`).
//! * The public handles of the account's links come in the listing's `ph`
//!   array (`h` node, `ph` public handle, `ts` creation time).
//!
//! Written from MEGA's SDK behaviour (`exportnode`) and the public megajs
//! 1.3.x reference; no code is copied. Links carry neither a pass code nor an
//! expiry here.

use super::client::MegaAccountClient;
use super::manage::Snapshot;
use super::nodes::{exported_nodes, tree_of, ListedNode, Tree};
use crate::cloud::mega::crypto::encode_base64url;
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::{CloudFileKind, CloudShare, CloudShareOptions, QuarkError};

pub(super) fn link(kind: &str, public_handle: &str, key: &[u8]) -> String {
    format!(
        "https://mega.nz/{kind}/{public_handle}#{}",
        encode_base64url(key)
    )
}

pub(super) fn share_of(
    node: &ListedNode,
    public_handle: &str,
    created_at: Option<u64>,
    key: &[u8],
) -> CloudShare {
    CloudShare {
        id: node.handle.clone(),
        url: link(
            if node.is_file { "file" } else { "folder" },
            public_handle,
            key,
        ),
        name: node.name.clone(),
        kind: if node.is_file {
            CloudFileKind::File
        } else {
            CloudFileKind::Folder
        },
        source_file_ids: vec![node.handle.clone()],
        pass_code: None,
        created_at: created_at.map(|seconds| seconds.to_string()),
        expires_at: None,
    }
}

impl MegaAccountClient<'_> {
    pub fn create_share(
        &self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, QuarkError> {
        if options
            .pass_code
            .as_deref()
            .is_some_and(|code| !code.trim().is_empty())
            || options.generate_pass_code
            || options.expires_in_days.is_some()
        {
            return Err(QuarkError::Mega(MegaFailure::ShareOptionsUnsupported));
        }
        let [file_id] = file_ids else {
            return Err(QuarkError::Mega(MegaFailure::ShareSingleOnly));
        };
        let snapshot = self.snapshot()?;
        let result = self.export_node(&snapshot, file_id);
        self.cache.invalidate();
        result
    }

    fn export_node(&self, snapshot: &Snapshot, file_id: &str) -> Result<CloudShare, QuarkError> {
        let node = self.own_node(snapshot, file_id, &[Tree::Drive])?;
        if !node.is_file {
            return self.export_folder(snapshot, &node);
        }
        let public_handle = self.api(&snapshot.session).export_link(&node.handle)?;
        Ok(share_of(&node, &public_handle, None, &node.key))
    }

    pub fn list_shares(&self) -> Result<Vec<CloudShare>, QuarkError> {
        let snapshot = self.snapshot()?;
        let mut shares = Vec::new();
        // `^!keys` is read once, and only when a folder link needs it.
        let mut container = None;
        for (node, public_handle, created_at) in
            exported_nodes(&snapshot.listing, &snapshot.keyring)?
        {
            if tree_of(&snapshot.listing, &node.handle)? != Some(Tree::Drive) {
                continue;
            }
            if node.is_file {
                shares.push(share_of(&node, &public_handle, created_at, &node.key));
            } else if let Some(key) =
                self.folder_share_key(&snapshot, &node.handle, &mut container)?
            {
                shares.push(share_of(&node, &public_handle, created_at, &*key));
            }
        }
        Ok(shares)
    }

    /// Withdraws the links of the given nodes. Already unshared nodes are fine.
    pub fn cancel_shares(&self, share_ids: &[String]) -> Result<(), QuarkError> {
        let snapshot = self.snapshot()?;
        let result = share_ids.iter().try_for_each(|id| {
            let raw = self.own_raw(&snapshot, id, &[Tree::Drive])?;
            match self.api(&snapshot.session).remove_link(id) {
                // The node has no link (any more): the goal is reached.
                Err(QuarkError::SourceFileMissing) => {}
                other => other?,
            }
            if raw["t"].as_u64() == Some(1) {
                self.withdraw_folder_share(&snapshot, id)?;
            }
            Ok(())
        });
        self.cache.invalidate();
        result
    }
}
