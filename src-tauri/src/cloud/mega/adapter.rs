//! Share adapter for public MEGA links: inspect lists the files, resolve
//! returns the temporary ciphertext URL plus the transform that decrypts it.

use super::account::{session_is_paid, MegaSession};
use super::api::{DownloadInfo, MegaApi, NodeScope};
use super::crypto::{decode_base64url, decrypt_node_name, encode_base64url, FileKey};
use super::failure::MegaFailure;
use super::folder::{decode_folder_listing, valid_handle, FolderFile};
use super::link::{parse_mega_link, MegaLink, MegaLinkKind};
use crate::cloud::download_transform::DownloadTransform;
use crate::cloud::share::{
    CloudShareEntry, CloudSharePlan, CloudShareResolvedEntry, CloudShareRootKind, ShareInspect,
    ShareInspection, ShareResolution, ShareResolve,
};
use crate::cloud::{
    CloudHttpTransport, CloudProvider, CredentialStore, QuarkError, ResolvedCloudDownload,
};
use serde_json::{json, Value};
use zeroize::Zeroizing;

pub struct MegaShareAdapter<'a> {
    /// The signed-in account: "save to my drive" (`transfer.rs`) and, when
    /// `account_quota` is set, the session that download URLs are requested
    /// with. Listing a public link never touches it.
    pub credentials: &'a dyn CredentialStore,
    pub http: &'a dyn CloudHttpTransport,
    /// Request download URLs with the signed-in account's session so MEGA
    /// counts the transfer against the account's quota instead of the IP's.
    /// The caller sets it only while that account's session is healthy and
    /// the user allows it; any session problem falls back to anonymous.
    pub account_quota: bool,
}

fn file_key(raw: &[u8]) -> Result<FileKey, QuarkError> {
    FileKey::from_bytes(raw).ok_or(QuarkError::InvalidResponse)
}

impl MegaShareAdapter<'_> {
    fn inspect_file(&self, link: &MegaLink) -> Result<CloudSharePlan, QuarkError> {
        let key = file_key(&link.key)?;
        let info = MegaApi::new(self.http).file_info(&link.id)?;
        let name = decrypt_node_name(&key.aes_key, &info.encrypted_attributes)
            .ok_or(QuarkError::Mega(MegaFailure::KeyInvalid))?;
        let share_id = canonical_share_id(link);
        let entry = CloudShareEntry::new(&share_id, link.id.clone(), name, vec![], Some(info.size));
        let mut plan = CloudSharePlan::new(
            CloudProvider::Mega,
            share_id,
            None,
            vec![entry],
            json!({"kind": "file", "id": link.id, "key": encode_base64url(&link.key)}),
        );
        plan.root_kind = CloudShareRootKind::File {
            entry_id: link.id.clone(),
        };
        Ok(plan)
    }

    fn inspect_folder(&self, link: &MegaLink) -> Result<CloudSharePlan, QuarkError> {
        let folder_key: [u8; 16] = link
            .key
            .as_slice()
            .try_into()
            .map_err(|_| QuarkError::InvalidResponse)?;
        let listing = MegaApi::new(self.http).folder_listing(&link.id)?;
        let files = decode_folder_listing(&folder_key, &listing)?;
        let share_id = canonical_share_id(link);
        let entries = files
            .iter()
            .map(|file| {
                CloudShareEntry::new(
                    &share_id,
                    file.handle.clone(),
                    file.name.clone(),
                    file.directory.clone(),
                    Some(file.size),
                )
            })
            .collect();
        let mut plan = CloudSharePlan::new(
            CloudProvider::Mega,
            share_id,
            None,
            entries,
            folder_context(&link.id, &files),
        );
        plan.root_kind = CloudShareRootKind::Directory;
        Ok(plan)
    }
}

/// Identifies the shared item without its key.
fn canonical_share_id(link: &MegaLink) -> String {
    let kind = match link.kind {
        MegaLinkKind::File => "file",
        MegaLinkKind::Folder => "folder",
    };
    format!("{kind}/{}", link.id)
}

fn folder_context(folder: &str, files: &[FolderFile]) -> Value {
    let nodes = files
        .iter()
        .map(|file| {
            (
                file.handle.clone(),
                json!({"k": encode_base64url(file.key.as_slice())}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({"kind": "folder", "id": folder, "files": nodes})
}

impl ShareInspect for MegaShareAdapter<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        if candidate.provider != CloudProvider::Mega {
            return Err(QuarkError::InvalidResponse);
        }
        let link = parse_mega_link(&candidate.url)?;
        let plan = match link.kind {
            MegaLinkKind::File => self.inspect_file(&link)?,
            MegaLinkKind::Folder => self.inspect_folder(&link)?,
        };
        Ok(ShareInspection::Plan(plan))
    }
}

/// Where the stored context says an entry's key and download scope are.
pub(super) struct EntryTarget {
    pub(super) folder: Option<String>,
    pub(super) handle: String,
    pub(super) key: Zeroizing<Vec<u8>>,
}

pub(super) fn entry_target(entry: &CloudShareResolvedEntry) -> Result<EntryTarget, QuarkError> {
    let context = &entry.plan.provider_context;
    let id = context["id"].as_str().ok_or(QuarkError::InvalidResponse)?;
    let handle = entry.entry.entry_id.as_str();
    if !valid_handle(id) || !valid_handle(handle) {
        return Err(QuarkError::InvalidResponse);
    }
    let (folder, encoded_key) = match context["kind"].as_str() {
        Some("file") if handle == id => (None, context["key"].as_str()),
        Some("folder") => (Some(id.to_owned()), context["files"][handle]["k"].as_str()),
        _ => return Err(QuarkError::InvalidResponse),
    };
    let key = encoded_key
        .and_then(decode_base64url)
        .map(Zeroizing::new)
        .ok_or(QuarkError::InvalidResponse)?;
    Ok(EntryTarget {
        folder,
        handle: handle.to_owned(),
        key,
    })
}

impl MegaShareAdapter<'_> {
    /// The download URL of a node, requested with the account session when
    /// allowed and the account is a paid one (`true` in the result), and
    /// anonymously otherwise: a free account's quota is IP-based either way,
    /// so the session would only be exposed for nothing. A missing, expired or
    /// refused session is logged and falls back to anonymous so the download
    /// still starts; an exhausted account quota is reported as it is.
    fn node_download(&self, scope: NodeScope<'_>) -> Result<(DownloadInfo, bool), QuarkError> {
        let api = MegaApi::new(self.http);
        if self.account_quota {
            match MegaSession::load(self.credentials) {
                Ok(session) => match session_is_paid(self.http, &session) {
                    Ok(true) => match api.download(scope, Some(session.sid())) {
                        Ok(download) => return Ok((download, true)),
                        Err(
                            error @ (QuarkError::Mega(MegaFailure::AccountQuotaExceeded { .. })
                            | QuarkError::Transport(_)),
                        ) => return Err(error),
                        Err(error) => eprintln!(
                            "MEGA 账号会话未被接受（{}），公开链接改用匿名方式下载",
                            session_failure_label(&error)
                        ),
                    },
                    // A free account has no quota of its own to use.
                    Ok(false) => {}
                    Err(error) => eprintln!(
                        "无法确认 MEGA 账号类型（{}），公开链接改用匿名方式下载",
                        session_failure_label(&error)
                    ),
                },
                Err(_) => eprintln!("MEGA 账号会话不可用，公开链接改用匿名方式下载"),
            }
        }
        Ok((api.download(scope, None)?, false))
    }
}

/// A fixed description of why a session request failed: never provider text.
fn session_failure_label(error: &QuarkError) -> &'static str {
    match error {
        QuarkError::AuthorizationExpired => "会话已失效",
        QuarkError::Mega(MegaFailure::LinkUnavailable) => "链接不可用或账号受限",
        _ => "请求被拒绝",
    }
}

impl ShareResolve for MegaShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        if entry.provider != CloudProvider::Mega || entry.plan.provider != CloudProvider::Mega {
            return Err(QuarkError::InvalidResponse);
        }
        let target = entry_target(entry)?;
        let key = file_key(&target.key)?;
        let scope = match target.folder.as_deref() {
            Some(folder) => NodeScope::FolderNode {
                folder,
                node: &target.handle,
            },
            None => NodeScope::PublicFile(&target.handle),
        };
        let (download, account_quota) = self.node_download(scope)?;
        // The listing size and the transfer size must agree: the gateway maps
        // plaintext offsets 1:1 onto ciphertext offsets.
        if entry.entry.size.is_some_and(|size| size != download.size) {
            return Err(QuarkError::InvalidResponse);
        }
        let mut transform =
            DownloadTransform::aes_ctr_path_range(*key.aes_key, key.nonce, download.size);
        if account_quota {
            transform = transform.with_account_quota();
        }
        Ok(ShareResolution::Direct(ResolvedCloudDownload {
            url: download.url,
            name: entry.entry.name.clone(),
            headers: Vec::new(),
            transform: Some(transform),
        }))
    }
}

#[cfg(test)]
mod tests;
