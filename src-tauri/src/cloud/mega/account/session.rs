//! The persisted MEGA login: session id plus master key.
//!
//! The password is never stored. The master key is what decrypts every node
//! key, so the whole record lives only in the encrypted credential store and
//! is wiped from memory when dropped.

use crate::cloud::mega::crypto::{decode_base64url, encode_base64url};
use crate::cloud::{CloudProvider, CredentialStore, QuarkError};
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Serialize, Deserialize)]
pub struct MegaSession {
    sid: String,
    master_key: String,
    user_handle: String,
    email: String,
}

impl MegaSession {
    pub fn new(
        sid: String,
        master_key: &[u8; 16],
        user_handle: String,
        email: String,
    ) -> Self {
        Self {
            sid,
            master_key: encode_base64url(master_key),
            user_handle,
            email,
        }
    }

    pub fn sid(&self) -> &str {
        &self.sid
    }

    pub fn user_handle(&self) -> &str {
        &self.user_handle
    }

    pub fn email(&self) -> &str {
        &self.email
    }

    pub fn master_key(&self) -> Result<Zeroizing<[u8; 16]>, QuarkError> {
        let raw = Zeroizing::new(decode_base64url(&self.master_key).ok_or(QuarkError::Storage)?);
        let key: [u8; 16] = raw.as_slice().try_into().map_err(|_| QuarkError::Storage)?;
        Ok(Zeroizing::new(key))
    }

    pub fn save(&self, store: &dyn CredentialStore) -> Result<(), QuarkError> {
        let bytes = Zeroizing::new(serde_json::to_vec(self).map_err(|_| QuarkError::Storage)?);
        store.save(CloudProvider::Mega.key(), &bytes)
    }

    /// The stored session, or `NotAuthorized` when nobody is signed in.
    pub fn load(store: &dyn CredentialStore) -> Result<Self, QuarkError> {
        let bytes = Zeroizing::new(
            store
                .load(CloudProvider::Mega.key())?
                .ok_or(QuarkError::NotAuthorized)?,
        );
        serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)
    }
}

impl Drop for MegaSession {
    fn drop(&mut self) {
        self.sid.zeroize();
        self.master_key.zeroize();
    }
}

// Neither the session id nor the master key may reach logs.
impl fmt::Debug for MegaSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MegaSession")
            .field("user_handle", &self.user_handle)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::MemoryCredentialStore;

    #[test]
    fn round_trips_through_the_credential_store_without_leaking_in_debug() {
        let store = MemoryCredentialStore::default();
        let session = MegaSession::new("SID".into(), &[7; 16], "USER".into(), "a@b.c".into());
        session.save(&store).unwrap();
        let loaded = MegaSession::load(&store).unwrap();
        assert_eq!(loaded.sid(), "SID");
        assert_eq!(*loaded.master_key().unwrap(), [7; 16]);
        let debug = format!("{loaded:?}");
        assert!(!debug.contains("SID") && !debug.contains("a@b.c"));
        assert!(!debug.contains(&encode_base64url(&[7; 16])));
    }

    #[test]
    fn missing_or_corrupt_session_is_reported() {
        let store = MemoryCredentialStore::default();
        assert!(matches!(
            MegaSession::load(&store),
            Err(QuarkError::NotAuthorized)
        ));
        store.save("mega", b"{").unwrap();
        assert!(matches!(MegaSession::load(&store), Err(QuarkError::Storage)));
    }
}
