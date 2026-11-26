//! Uploading a local file into the account.
//!
//! MEGA stores ciphertext only, so the file is encrypted on the way out:
//!
//! 1. Pick a random AES-128 key and 8-byte nonce for the file.
//! 2. Stream the file once through the MEGA MAC (CBC-MAC per chunk, chained
//!    into a condensed 8-byte meta-MAC). This is a plain local read.
//! 3. `u` returns an upload URL; the file is POSTed piece by piece at its
//!    byte offset, each piece AES-128-CTR encrypted while it streams from disk
//!    ([`UploadBody::AesCtr`]), so only the piece being sent is ever buffered.
//!    The pieces are MEGA's MAC chunks (128 KiB growing to 1 MiB). The last
//!    reply is the completion token.
//! 4. `p` turns the token into a node: attributes sealed with the file key,
//!    the packed key (nonce, meta-MAC, AES key) wrapped with the master key.
//!
//! Progress, pause, cancel and bandwidth limits come from the shared upload
//! body (`UploadBody::FileSlice` under `Controlled`). Written from the public
//! megajs 1.3.x reference (`lib/storage.mjs`, `lib/mutable-file.mjs`) and
//! MEGA's SDK behaviour; no code is copied.

use super::client::MegaAccountClient;
use super::manage::{created_node, random_key};
use super::seal::{
    attribute_key, name_attributes, pack_file_key, seal_attributes, validate_name, wrap_key,
};
use crate::cloud::mega::crypto::{chunk_ranges, condensed_mac, FileKey};
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::upload::https_transfer_url;
use crate::cloud::{
    is_upload_cancelled, CloudFile, CloudFileKind, CloudHttpTransport, CloudUploadTransport,
    QuarkError, UploadBody, UploadFileSnapshot, UploadHttpRequest,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::time::Duration;
use zeroize::Zeroizing;

/// Waits between attempts of one piece (the server may answer `-3`, or the
/// connection may drop). Pieces are idempotent: they carry their own offset.
#[cfg(not(test))]
const PIECE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(8),
];
#[cfg(test)]
const PIECE_RETRY_DELAYS: [Duration; 3] = [Duration::ZERO; 3];

const MAC_READ_BUFFER: usize = 1024 * 1024;
/// A completion token is a base64url string of a few dozen characters.
const MAX_TOKEN_LEN: usize = 128;

const EAGAIN: i64 = -3;
const EOVERQUOTA: i64 = -17;

/// Reads for the local MAC pass, checking for cancellation before each read.
struct CheckedRead<'a, R> {
    inner: R,
    http: &'a dyn CloudHttpTransport,
}

impl<R: Read> Read for CheckedRead<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.http
            .check_transfer()
            .map_err(|_| io::Error::other("upload cancelled"))?;
        self.inner.read(buffer)
    }
}

fn unreadable() -> QuarkError {
    QuarkError::Mega(MegaFailure::LocalFileUnreadable)
}

/// What the upload server answered to one piece.
enum PieceReply {
    /// An intermediate piece was accepted.
    Accepted,
    /// The last piece: the completion token.
    Token(String),
}

fn piece_reply(status: u16, body: &str, last: bool) -> Result<PieceReply, PieceFailure> {
    let body = body.trim();
    if let Ok(code) = body.parse::<i64>() {
        if code < 0 {
            return Err(match code {
                EAGAIN => PieceFailure::Retry,
                EOVERQUOTA => PieceFailure::Fatal(QuarkError::Mega(MegaFailure::StorageFull)),
                other => PieceFailure::Fatal(QuarkError::Mega(MegaFailure::ApiError { code: other })),
            });
        }
    }
    if status >= 500 {
        return Err(PieceFailure::Retry);
    }
    if status != 200 {
        return Err(PieceFailure::Fatal(QuarkError::Mega(
            MegaFailure::UploadHttpError { status },
        )));
    }
    if !last {
        return Ok(PieceReply::Accepted);
    }
    let valid = !body.is_empty()
        && body.len() <= MAX_TOKEN_LEN
        && body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(PieceReply::Token(body.to_owned()))
    } else {
        Err(PieceFailure::Fatal(QuarkError::InvalidResponse))
    }
}

enum PieceFailure {
    Retry,
    Fatal(QuarkError),
}

impl MegaAccountClient<'_> {
    /// Encrypts and uploads `path` into `parent_id`, returning the new node.
    pub fn upload_file(
        &self,
        upload: &dyn CloudUploadTransport,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, QuarkError> {
        let file = UploadFileSnapshot::capture(path)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(unreadable)?
            .trim()
            .to_owned();
        validate_name(&name)?;
        let size = file.len();

        let snapshot = self.snapshot()?;
        let parent = self.writable_parent(&snapshot.listing, parent_id)?;
        let master = snapshot.session.master_key()?;

        let aes_key = random_key::<16>();
        let nonce = *random_key::<8>();
        let meta_mac = self.file_mac(path, &aes_key, nonce, size)?;
        file.validate()?;

        let api = self.api(&snapshot.session);
        let url = api.upload_url(size)?;
        let url = https_transfer_url(url["p"].as_str().ok_or(QuarkError::InvalidResponse)?)?;
        let token = self.send_pieces(upload, &url, &file, &aes_key, nonce, size)?;
        file.validate()?;

        let packed = pack_file_key(&aes_key, &nonce, &meta_mac);
        let sealed =
            seal_attributes(&aes_key, &name_attributes(&name)).ok_or(QuarkError::Storage)?;
        let wrapped = wrap_key(&master, &*packed).ok_or(QuarkError::Storage)?;
        debug_assert_eq!(attribute_key(&*packed).as_deref(), Some(&*aes_key));
        let reply = api.put_nodes(
            &parent,
            vec![json!({"h": token, "t": 0, "a": sealed, "k": wrapped})],
        );
        self.cache.invalidate();
        let created = created_node(&reply?)?;
        self.share_new_nodes(
            &snapshot,
            &parent,
            &[(created.handle.as_str(), &packed[..])],
        );
        Ok(CloudFile {
            id: created.handle,
            parent_id: parent_id.to_owned(),
            name,
            kind: CloudFileKind::File,
            size: Some(size),
            modified_at: created.timestamp.map(|seconds| seconds.to_string()),
        })
    }

    /// The meta-MAC of the file as it is on disk right now.
    fn file_mac(
        &self,
        path: &Path,
        aes_key: &[u8; 16],
        nonce: [u8; 8],
        size: u64,
    ) -> Result<[u8; 8], QuarkError> {
        let key = FileKey {
            aes_key: Zeroizing::new(*aes_key),
            nonce,
            meta_mac: [0; 8],
        };
        let reader = CheckedRead {
            inner: BufReader::with_capacity(
                MAC_READ_BUFFER,
                File::open(path).map_err(|_| unreadable())?,
            ),
            http: self.http,
        };
        condensed_mac(&key, reader, size).map_err(|_| {
            // A cancellation surfaces as its own error; anything else is an
            // unreadable or truncated file.
            self.http.check_transfer().err().unwrap_or_else(unreadable)
        })
    }

    /// POSTs every piece and returns the completion token of the last one.
    fn send_pieces(
        &self,
        upload: &dyn CloudUploadTransport,
        url: &str,
        file: &UploadFileSnapshot,
        aes_key: &[u8; 16],
        nonce: [u8; 8],
        size: u64,
    ) -> Result<String, QuarkError> {
        let mut token = None;
        for (offset, length) in chunk_ranges(size) {
            let last = offset + length == size;
            let reply = self.send_piece(upload, url, file, aes_key, nonce, offset, length, last)?;
            if let PieceReply::Token(value) = reply {
                token = Some(value);
            }
        }
        token.ok_or(QuarkError::InvalidResponse)
    }

    #[allow(clippy::too_many_arguments)]
    fn send_piece(
        &self,
        upload: &dyn CloudUploadTransport,
        url: &str,
        file: &UploadFileSnapshot,
        aes_key: &[u8; 16],
        nonce: [u8; 8],
        offset: u64,
        length: u64,
        last: bool,
    ) -> Result<PieceReply, QuarkError> {
        let mut delays = PIECE_RETRY_DELAYS.iter();
        loop {
            self.http.check_transfer()?;
            let body = UploadBody::AesCtr {
                body: Box::new(file.slice(offset, length)?),
                key: Zeroizing::new(*aes_key),
                nonce,
                offset,
            };
            let outcome = upload.send_upload(UploadHttpRequest {
                method: "POST",
                url: format!("{url}/{offset}"),
                headers: BTreeMap::new(),
                body,
            });
            let failure = match outcome {
                Ok(response) => match piece_reply(response.status, &response.body, last) {
                    Ok(reply) => return Ok(reply),
                    Err(failure) => failure,
                },
                Err(error) if is_upload_cancelled(&error) => return Err(error),
                Err(error @ QuarkError::Transport(_)) => PieceFailure::Fatal(error),
                Err(error) => return Err(error),
            };
            let retryable = matches!(failure, PieceFailure::Retry)
                || matches!(&failure, PieceFailure::Fatal(QuarkError::Transport(_)));
            match (retryable, delays.next(), failure) {
                (true, Some(delay), _) => std::thread::sleep(*delay),
                (_, _, PieceFailure::Fatal(error)) => return Err(error),
                (_, _, PieceFailure::Retry) => {
                    return Err(QuarkError::Mega(MegaFailure::ServiceBusy))
                }
            }
        }
    }
}
