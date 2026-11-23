//! Account operations that read: identity, quota, directory listing (own
//! drive, rubbish bin, shares from other users) and the download URL of one
//! file. Changes live in `manage.rs`, `upload.rs` and `share.rs`.

use super::api::AccountApi;
use super::keys::{Keyring, ShareKey};
use super::nodes::{
    children, drive_root, find_file, incoming_roots, incoming_share_blobs, rubbish_root,
};
use super::rsa_sid::RsaKey;
use super::session::MegaSession;
use crate::cloud::download_transform::DownloadTransform;
use crate::cloud::mega::crypto::FileKey;
use crate::cloud::{
    CloudAccount, CloudAuthMethod, CloudDirectory, CloudFile, CloudFileKind, CloudHttpTransport,
    CloudProvider, CloudStorageCapacity, CredentialStore, ProviderRegistry, QuarkError,
    ResolvedCloudDownload,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The node listing is one large call. Walking a directory tree (downloading
/// a folder) would repeat it for every level, so a fresh copy is reused for a
/// few seconds. It holds only encrypted metadata and is keyed by a hash of the
/// session id.
const LISTING_TTL: Duration = Duration::from_secs(10);

/// The raw `f` reply plus the share keys that only the account's RSA key can
/// open (folders other users shared with it). Derefs to the raw reply.
pub struct Listing {
    raw: Value,
    incoming_keys: Vec<ShareKey>,
}

impl Deref for Listing {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.raw
    }
}

struct CachedListing {
    session: [u8; 32],
    fetched: Instant,
    listing: Arc<Listing>,
}

#[derive(Default)]
pub struct ListingCache {
    slot: Mutex<Option<CachedListing>>,
}

fn session_fingerprint(sid: &str) -> [u8; 32] {
    Sha256::digest(sid.as_bytes()).into()
}

impl ListingCache {
    /// Forgets the cached listing; every change of the account calls this so
    /// the next read sees it.
    pub fn invalidate(&self) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = None;
        }
    }

    fn get_or_fetch(
        &self,
        sid: &str,
        fetch: impl FnOnce() -> Result<Listing, QuarkError>,
    ) -> Result<Arc<Listing>, QuarkError> {
        let session = session_fingerprint(sid);
        let mut slot = self.slot.lock().map_err(|_| QuarkError::Storage)?;
        if let Some(cached) = slot.as_ref() {
            if cached.session == session && cached.fetched.elapsed() < LISTING_TTL {
                return Ok(Arc::clone(&cached.listing));
            }
        }
        *slot = None;
        let listing = Arc::new(fetch()?);
        *slot = Some(CachedListing {
            session,
            fetched: Instant::now(),
            listing: Arc::clone(&listing),
        });
        Ok(listing)
    }
}

fn global_cache() -> &'static ListingCache {
    static CACHE: std::sync::OnceLock<ListingCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(ListingCache::default)
}

/// Best-effort server-side sign-out of the stored session; failures (offline,
/// already expired) are deliberately ignored because the local sign-out
/// proceeds either way.
pub fn revoke_session(http: &dyn CloudHttpTransport, store: &dyn CredentialStore) {
    if let Ok(session) = MegaSession::load(store) {
        let _ = AccountApi::with_session(http, session.sid()).logout();
    }
}

fn cloud_file(node: super::nodes::ListedNode, directory_id: &str) -> CloudFile {
    CloudFile {
        id: node.handle,
        parent_id: directory_id.to_owned(),
        name: node.name,
        kind: if node.is_file {
            CloudFileKind::File
        } else {
            CloudFileKind::Folder
        },
        size: node.is_file.then_some(node.size),
        modified_at: node.timestamp.map(|seconds| seconds.to_string()),
    }
}

/// The decryptable children of `parent_handle`, presented under `directory_id`.
pub(super) fn directory_of(
    listing: &Value,
    keyring: &Keyring,
    parent_handle: &str,
    directory_id: &str,
) -> Result<CloudDirectory, QuarkError> {
    let nodes = children(listing, keyring, parent_handle)?.ok_or(QuarkError::SourceFileMissing)?;
    Ok(CloudDirectory {
        id: directory_id.to_owned(),
        items: nodes
            .into_iter()
            .map(|node| cloud_file(node, directory_id))
            .collect(),
    })
}

pub struct MegaAccountClient<'a> {
    pub(super) http: &'a dyn CloudHttpTransport,
    pub(super) store: &'a dyn CredentialStore,
    pub(super) cache: &'a ListingCache,
}

/// The ids the registry reserves for MEGA's special directories.
pub(super) struct SpecialIds {
    pub root: &'static str,
    pub recycle_bin: &'static str,
    pub incoming_shares: &'static str,
}

pub(super) fn special_ids() -> SpecialIds {
    let descriptor = ProviderRegistry::global().get(CloudProvider::Mega);
    SpecialIds {
        root: descriptor.root_directory_id,
        recycle_bin: descriptor.recycle_bin_directory_id,
        incoming_shares: descriptor
            .incoming_shares_directory_id
            .unwrap_or(crate::cloud::mega::MEGA_INCOMING_SHARES_ID),
    }
}

impl<'a> MegaAccountClient<'a> {
    pub fn new(http: &'a dyn CloudHttpTransport, store: &'a dyn CredentialStore) -> Self {
        Self {
            http,
            store,
            cache: global_cache(),
        }
    }

    #[cfg(test)]
    pub fn with_cache(mut self, cache: &'a ListingCache) -> Self {
        self.cache = cache;
        self
    }

    pub fn account(&self) -> Result<CloudAccount, QuarkError> {
        let session = MegaSession::load(self.store)?;
        let info = AccountApi::with_session(self.http, session.sid()).user_info()?;
        let id = info["u"]
            .as_str()
            .filter(|handle| !handle.is_empty())
            .ok_or(QuarkError::InvalidResponse)?;
        let email = info["email"].as_str().unwrap_or_else(|| session.email());
        Ok(CloudAccount {
            provider: CloudProvider::Mega,
            id: id.to_owned(),
            display_name: email.to_owned(),
            auth_method: CloudAuthMethod::Password,
        })
    }

    pub fn storage_capacity(&self) -> Result<CloudStorageCapacity, QuarkError> {
        let session = MegaSession::load(self.store)?;
        let quota = AccountApi::with_session(self.http, session.sid()).quota()?;
        super::tier::remember(session.sid(), &quota);
        Ok(CloudStorageCapacity::Limited {
            total_bytes: quota["mstrg"].as_u64().ok_or(QuarkError::InvalidResponse)?,
            used_bytes: quota["cstrg"].as_u64().ok_or(QuarkError::InvalidResponse)?,
        })
    }

    pub(super) fn listing(&self, session: &MegaSession) -> Result<Arc<Listing>, QuarkError> {
        self.cache.get_or_fetch(session.sid(), || {
            let api = AccountApi::with_session(self.http, session.sid());
            let raw = api.nodes()?;
            let incoming_keys = self.incoming_share_keys(&api, session, &raw)?;
            Ok(Listing { raw, incoming_keys })
        })
    }

    /// Opens the RSA-encrypted share keys of folders other users shared with
    /// this account. The private key is only fetched when there are any.
    fn incoming_share_keys(
        &self,
        api: &AccountApi<'_>,
        session: &MegaSession,
        raw: &Value,
    ) -> Result<Vec<ShareKey>, QuarkError> {
        let blobs = incoming_share_blobs(raw)?;
        if blobs.is_empty() {
            return Ok(Vec::new());
        }
        let info = api.user_info()?;
        let master = session.master_key()?;
        let Some(rsa) = info["privk"]
            .as_str()
            .and_then(|private_key| RsaKey::open(&master, private_key))
        else {
            // Without the RSA key the shares stay invisible; the own drive
            // does not depend on it.
            return Ok(Vec::new());
        };
        Ok(blobs
            .into_iter()
            .filter_map(|(handle, sealed)| Some((handle, rsa.share_key(&sealed)?)))
            .collect())
    }

    pub(super) fn keyring(session: &MegaSession, listing: &Listing) -> Result<Keyring, QuarkError> {
        let master = session.master_key()?;
        Ok(Keyring::new(session.user_handle(), &master)
            .with_listing_shares(listing)
            .with_incoming_shares(&listing.incoming_keys))
    }

    /// `parent_id` is the registry's id for the drive root, the recycle bin or
    /// the incoming shares, otherwise a node handle from an earlier listing.
    pub fn list(&self, parent_id: &str) -> Result<CloudDirectory, QuarkError> {
        let session = MegaSession::load(self.store)?;
        let listing = self.listing(&session)?;
        let keyring = Self::keyring(&session, &listing)?;
        let ids = special_ids();
        if parent_id == ids.incoming_shares {
            return Ok(CloudDirectory {
                id: parent_id.to_owned(),
                items: incoming_roots(&listing, &keyring)?
                    .into_iter()
                    .map(|node| cloud_file(node, parent_id))
                    .collect(),
            });
        }
        let parent_handle = if parent_id == ids.root {
            drive_root(&listing)?
        } else if parent_id == ids.recycle_bin {
            rubbish_root(&listing)?
        } else {
            parent_id.to_owned()
        };
        directory_of(&listing, &keyring, &parent_handle, parent_id)
    }

    /// Temporary ciphertext URL of a file plus the transform that decrypts it.
    pub fn resolve_download(&self, file_id: &str) -> Result<ResolvedCloudDownload, QuarkError> {
        let session = MegaSession::load(self.store)?;
        let listing = self.listing(&session)?;
        let keyring = Self::keyring(&session, &listing)?;
        let node = find_file(&listing, &keyring, file_id)?;
        let key = FileKey::from_bytes(&node.key).ok_or(QuarkError::InvalidResponse)?;
        let reply =
            AccountApi::with_session(self.http, session.sid()).node_download(&node.handle)?;
        let url = reply["g"]
            .as_str()
            .filter(|url| url.starts_with("https://"))
            .ok_or(QuarkError::InvalidResponse)?;
        // The gateway maps plaintext offsets 1:1 onto ciphertext offsets, so
        // the listing and the transfer must describe the same object.
        let size = reply["s"].as_u64().ok_or(QuarkError::InvalidResponse)?;
        if size != node.size {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(ResolvedCloudDownload {
            url: url.to_owned(),
            name: node.name,
            headers: Vec::new(),
            transform: Some(DownloadTransform::aes_ctr_path_range(
                *key.aes_key,
                key.nonce,
                size,
            )),
        })
    }
}
