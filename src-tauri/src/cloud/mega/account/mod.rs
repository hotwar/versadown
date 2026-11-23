//! MEGA account access: sign in with e-mail and password, browse the account's
//! own files, download them, and manage them (new folders, rename, move, copy,
//! rubbish bin, upload, public links, saving a public link into the drive).
//!
//! Sessions are the session id plus the master key,
//! kept in the encrypted credential store; the password itself is never
//! stored, so an expired session asks the user to sign in again. Downloads
//! reuse the public-link decryption path: the URL serves AES-128-CTR
//! ciphertext and a `DownloadTransform` lets the local range gateway decrypt.
//!
//! Protocol and key handling are written from the public megajs 1.3.x
//! reference (`lib/crypto/*`, `lib/storage.mjs`); no code is copied.

mod api;
mod client;
mod copy;
mod folder_share;
mod gcm;
mod import;
mod kdf;
mod key_container;
mod key_manager;
mod keys;
mod login;
mod manage;
mod nodes;
mod rsa_sid;
mod seal;
mod session;
mod share;
mod tier;
mod upload;
mod verify;

#[cfg(test)]
#[allow(dead_code)]
#[rustfmt::skip]
pub(crate) mod test_vectors;
#[cfg(test)]
#[allow(dead_code, clippy::type_complexity)]
#[rustfmt::skip]
pub(crate) mod manage_vectors;
#[cfg(test)]
#[allow(dead_code, clippy::type_complexity)]
#[rustfmt::skip]
pub(crate) mod keys_vectors;
#[cfg(test)]
mod live;
#[cfg(test)]
mod tests;

pub use client::{revoke_session, MegaAccountClient};
pub use login::{login, LoginRequest};
pub use session::MegaSession;
pub use tier::session_is_paid;
