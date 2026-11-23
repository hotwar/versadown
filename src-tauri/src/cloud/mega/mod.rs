//! MEGA: public-link downloads (no account needed) and, in [`account`], a
//! signed-in drive (browse, download, manage).
//!
//! MEGA stores end-to-end encrypted data: the temporary download URL serves
//! AES-128-CTR ciphertext, and the key lives only in the link's `#fragment`.
//! The share adapter therefore returns a [`DownloadTransform`] so the local
//! range gateway decrypts on the fly and aria2 only ever sees plaintext.
//!
//! The link key is as sensitive as the file: it is kept in the encrypted share
//! plan store and must never reach logs, errors or UI.
//!
//! [`DownloadTransform`]: crate::cloud::download_transform::DownloadTransform

pub mod account;
mod adapter;
mod api;
pub mod crypto;
mod failure;
mod folder;
mod link;
mod transfer;
mod verify;

#[cfg(test)]
mod live_tests;

#[cfg(test)]
#[allow(dead_code)]
#[rustfmt::skip]
pub(crate) mod test_vectors;

/// Virtual directory ids the account client maps to MEGA's rubbish bin and to
/// the folders other people shared with the account.
pub const MEGA_RECYCLE_BIN_ID: &str = "__versa_down_mega_recycle_bin__";
pub const MEGA_INCOMING_SHARES_ID: &str = "__versa_down_mega_incoming_shares__";

/// Task status message (a localization key) of a public-link download that was
/// requested with the signed-in account and so uses its transfer quota.
pub const ACCOUNT_QUOTA_NOTICE: &str = "download.mega.accountQuota";

pub use adapter::MegaShareAdapter;
pub use link::{parse_mega_link, MegaLink, MegaLinkKind};
pub use failure::MegaFailure;
pub use verify::verify_downloaded_file;
