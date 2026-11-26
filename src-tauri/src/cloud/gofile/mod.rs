//! Gofile (`gofile.io/d/<code>`).
//!
//! Gofile has no account here and no public API we may call: its share page
//! runs in an embedded browser window and the user operates it (password,
//! "download"). This module only recognises the page, decides which window
//! requests may be taken over ([`capture`]) and turns one captured file
//! request into an ordinary download ([`adapter`]). It never reproduces or
//! calls anything the page does to obtain a download address.
//!
//! The captured address and its visitor cookie are secrets: they live only in
//! the encrypted share-plan store, never in a task source, log or error.

mod adapter;
mod capture;
mod failure;
mod link;

pub(crate) use adapter::GofileShareAdapter;
pub(crate) use capture::{is_file_request, is_page_navigation_allowed, CapturedDownload};
pub use failure::GofileFailure;
pub(crate) use link::share_page_url;
