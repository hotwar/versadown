//! Client for MEGA's public (account-less) command API.
//!
//! Every request goes through the application's `CloudHttpTransport`, so the
//! user's proxy settings apply. Responses are either a bare negative error
//! number or an array holding one result (itself a number or an object).

use super::failure::MegaFailure;
use crate::cloud::{CloudHttpRequest, CloudHttpTransport, QuarkError};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

const GATEWAY: &str = "https://g.api.mega.co.nz/cs";

// Error numbers from the public API reference (negated in the wire format).
const EAGAIN: i64 = -3;
const ERATELIMIT: i64 = -4;
const EEXPIRED: i64 = -8;
const ENOENT: i64 = -9;
const ESID: i64 = -15;
const EBLOCKED: i64 = -16;
const EOVERQUOTA: i64 = -17;
const ETEMPUNAVAIL: i64 = -18;

/// Where a node lives: a public file link, or a node inside a folder link.
#[derive(Clone, Copy)]
pub enum NodeScope<'a> {
    PublicFile(&'a str),
    FolderNode { folder: &'a str, node: &'a str },
}

pub struct FileInfo {
    pub size: u64,
    pub encrypted_attributes: String,
}

pub struct DownloadInfo {
    pub url: String,
    pub size: u64,
}

pub struct MegaApi<'a> {
    http: &'a dyn CloudHttpTransport,
}

fn next_request_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let previous = COUNTER.fetch_add(1, Ordering::Relaxed);
    if previous == 0 {
        // The API only needs ids to differ between requests of one client.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(1)
            % 1_000_000_000;
        COUNTER.store(seed + 1, Ordering::Relaxed);
        return seed;
    }
    previous
}

/// Map an API error number to the error shown to the user.
fn api_error(code: i64, retry_after_secs: Option<u64>) -> QuarkError {
    match code {
        // Only reachable when a request carried a session id: the API no
        // longer accepts it.
        ESID => QuarkError::AuthorizationExpired,
        EOVERQUOTA => QuarkError::Mega(MegaFailure::QuotaExceeded { retry_after_secs }),
        ENOENT | EEXPIRED | EBLOCKED => QuarkError::Mega(MegaFailure::LinkUnavailable),
        EAGAIN | ERATELIMIT | ETEMPUNAVAIL => {
            QuarkError::Transport("MEGA 服务暂时繁忙，请稍后重试".into())
        }
        other => QuarkError::Provider(format!("MEGA 错误码 {other}")),
    }
}

/// Result of one command: the object, or the mapped error.
fn command_result(response: &str) -> Result<Value, QuarkError> {
    let value: Value = serde_json::from_str(response).map_err(|_| QuarkError::InvalidResponse)?;
    let result = match value {
        Value::Array(mut items) if items.len() == 1 => items.remove(0),
        other => other,
    };
    if let Some(code) = result.as_i64().filter(|code| *code < 0) {
        return Err(api_error(code, None));
    }
    if let Some(code) = result
        .get("e")
        .and_then(Value::as_i64)
        .filter(|code| *code < 0)
    {
        let wait = result.get("tl").and_then(Value::as_u64);
        return Err(api_error(code, wait));
    }
    if result.is_object() {
        Ok(result)
    } else {
        Err(QuarkError::InvalidResponse)
    }
}

impl<'a> MegaApi<'a> {
    pub fn new(http: &'a dyn CloudHttpTransport) -> Self {
        Self { http }
    }

    fn call(
        &self,
        folder_scope: Option<&str>,
        sid: Option<&str>,
        command: Value,
    ) -> Result<Value, QuarkError> {
        let mut url = format!("{GATEWAY}?id={}", next_request_id());
        if let Some(folder) = folder_scope {
            url.push_str("&n=");
            url.push_str(folder);
        }
        // A folder link and a session id travel together: the web client and
        // the SDK both send `n=<folder>&sid=<sid>` for a signed-in user.
        if let Some(sid) = sid {
            url.push_str("&sid=");
            url.push_str(&crate::cloud::types::encoded(sid));
        }
        let response = self.http.send(CloudHttpRequest {
            method: "POST",
            url,
            headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
            body: Some(Value::Array(vec![command]).to_string()),
        })?;
        if response.status != 200 {
            return Err(QuarkError::Transport(
                "MEGA 服务暂时不可用，请稍后重试".into(),
            ));
        }
        command_result(&response.body)
    }

    /// Size and encrypted attributes of a public file; costs no transfer quota.
    pub fn file_info(&self, handle: &str) -> Result<FileInfo, QuarkError> {
        let result = self.call(None, None, json!({"a": "g", "p": handle}))?;
        Ok(FileInfo {
            size: result["s"].as_u64().ok_or(QuarkError::InvalidResponse)?,
            encrypted_attributes: result["at"]
                .as_str()
                .ok_or(QuarkError::InvalidResponse)?
                .to_owned(),
        })
    }

    /// Temporary download URL (ciphertext) of a file or folder node. With a
    /// session id the URL is issued against that account's transfer quota
    /// instead of the caller's IP quota, and an exhausted quota is reported
    /// as the account's.
    pub fn download(
        &self,
        scope: NodeScope<'_>,
        sid: Option<&str>,
    ) -> Result<DownloadInfo, QuarkError> {
        let (folder, command) = match scope {
            NodeScope::PublicFile(handle) => {
                (None, json!({"a": "g", "g": 1, "ssl": 2, "p": handle}))
            }
            NodeScope::FolderNode { folder, node } => {
                (Some(folder), json!({"a": "g", "g": 1, "ssl": 2, "n": node}))
            }
        };
        let result = self
            .call(folder, sid, command)
            .map_err(|error| match error {
                QuarkError::Mega(MegaFailure::QuotaExceeded { retry_after_secs })
                    if sid.is_some() =>
                {
                    QuarkError::Mega(MegaFailure::AccountQuotaExceeded { retry_after_secs })
                }
                other => other,
            })?;
        let url = result["g"].as_str().ok_or(QuarkError::InvalidResponse)?;
        // `ssl: 2` makes the API hand out an https URL (`ssl: 0` yields plain
        // http, verified against the live API). Never fall back to cleartext.
        if !url.starts_with("https://") {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(DownloadInfo {
            url: url.to_owned(),
            size: result["s"].as_u64().ok_or(QuarkError::InvalidResponse)?,
        })
    }

    /// Full recursive node listing of a folder link.
    pub fn folder_listing(&self, folder: &str) -> Result<Value, QuarkError> {
        self.call(Some(folder), None, json!({"a": "f", "c": 1, "r": 1}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_and_wrapped_error_numbers_are_mapped() {
        for body in ["-17", "[-17]"] {
            assert!(matches!(
                command_result(body),
                Err(QuarkError::Mega(MegaFailure::QuotaExceeded {
                    retry_after_secs: None
                }))
            ));
        }
        assert!(matches!(
            command_result("[-9]"),
            Err(QuarkError::Mega(MegaFailure::LinkUnavailable))
        ));
        assert!(matches!(
            command_result("[-3]"),
            Err(QuarkError::Transport(_))
        ));
    }

    #[test]
    fn quota_error_inside_an_object_carries_the_wait() {
        assert!(matches!(
            command_result(r#"[{"e":-17,"tl":1800}]"#),
            Err(QuarkError::Mega(MegaFailure::QuotaExceeded {
                retry_after_secs: Some(1800)
            }))
        ));
    }

    #[test]
    fn malformed_responses_are_invalid() {
        for body in ["", "not json", "[]", "[1,2]", "[\"x\"]", "7"] {
            assert!(
                matches!(command_result(body), Err(QuarkError::InvalidResponse)),
                "{body}"
            );
        }
    }
}
