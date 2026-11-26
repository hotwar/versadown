//! Authenticated MEGA command API.
//!
//! Same wire format as the public-link client (`POST /cs?id=N[&sid=SID]` with
//! a one-element JSON array), but the session id is part of the query string
//! and the error numbers are interpreted for an account: `-9` after a login
//! means a wrong password, `-15` an expired session, `-26` a missing second
//! factor. Neither the session id nor any password material is ever copied
//! into an error.
//!
//! Command names and error numbers follow the public megajs 1.3.x reference
//! and MEGA's SDK (`megaapi.h` error constants); no code is copied.

use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::{CloudHttpRequest, CloudHttpTransport, QuarkError};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub(super) const GATEWAY: &str = "https://g.api.mega.co.nz/cs";

const EARGS: i64 = -2;
const EAGAIN: i64 = -3;
const ERATELIMIT: i64 = -4;
const EFAILED: i64 = -5;
const ETOOMANY: i64 = -6;
const ENOENT: i64 = -9;
const ECIRCULAR: i64 = -10;
const EACCESS: i64 = -11;
const EEXPIRED: i64 = -23;
const ESID: i64 = -15;
const EBLOCKED: i64 = -16;
const EOVERQUOTA: i64 = -17;
const ETEMPUNAVAIL: i64 = -18;
const ETOOMANYCONNECTIONS: i64 = -19;
const EMFAREQUIRED: i64 = -26;

/// Waits between attempts while MEGA answers "try again" (`-3`), is rate
/// limiting (`-4`) or a gateway hiccups (HTTP 5xx). The commands retried this
/// way were not executed by the server, so repeating them is safe.
#[cfg(not(test))]
const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(750),
    Duration::from_millis(1500),
    Duration::from_millis(3000),
];
#[cfg(test)]
const RETRY_DELAYS: [Duration; 4] = [Duration::ZERO; 4];

/// What a command was for; decides how ambiguous error numbers read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    /// A login step. `with_code` tells whether a second factor was sent.
    Login { with_code: bool },
    /// Reading the account's own data with an established session.
    Session,
    /// Asking for the download URL of one node.
    NodeDownload,
    /// Changing the account's nodes or shares. `-17` means storage is full.
    Write,
    /// Reading or conditionally writing a user attribute (`^!keys`). A missing
    /// attribute is `-9`; `-23` means the stored version moved on.
    Attribute,
}

pub(super) struct AccountApi<'a> {
    http: &'a dyn CloudHttpTransport,
    sid: Option<&'a str>,
    gateway: &'a str,
}

fn next_request_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let previous = COUNTER.fetch_add(1, Ordering::Relaxed);
    if previous == 0 {
        // Ids only need to differ between requests of one client.
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

fn busy() -> QuarkError {
    QuarkError::Mega(MegaFailure::ServiceBusy)
}

fn api_error(code: i64, purpose: Purpose, retry_after_secs: Option<u64>) -> QuarkError {
    match (code, purpose) {
        (EMFAREQUIRED, Purpose::Login { with_code: true }) => {
            QuarkError::Mega(MegaFailure::MfaInvalid)
        }
        (EMFAREQUIRED, Purpose::Login { with_code: false }) => {
            QuarkError::Mega(MegaFailure::MfaRequired)
        }
        // A refused second factor is reported as a failure, not a missing one.
        (EFAILED, Purpose::Login { with_code: true }) => QuarkError::Mega(MegaFailure::MfaInvalid),
        (ENOENT, Purpose::Login { .. }) => QuarkError::Mega(MegaFailure::InvalidCredentials),
        (ENOENT, Purpose::NodeDownload | Purpose::Write | Purpose::Attribute) => {
            QuarkError::SourceFileMissing
        }
        (EEXPIRED, Purpose::Attribute) => QuarkError::Mega(MegaFailure::KeysConflict),
        (ESID, _) => QuarkError::AuthorizationExpired,
        (EBLOCKED, _) => QuarkError::Mega(MegaFailure::AccountBlocked),
        (EACCESS, Purpose::Write) => QuarkError::Mega(MegaFailure::AccessDenied),
        (ECIRCULAR, Purpose::Write) => QuarkError::Mega(MegaFailure::InvalidMove),
        (EOVERQUOTA, Purpose::Write) => QuarkError::Mega(MegaFailure::StorageFull),
        (EOVERQUOTA, _) => QuarkError::Mega(MegaFailure::QuotaExceeded { retry_after_secs }),
        (EAGAIN | ERATELIMIT | ETOOMANY | ETEMPUNAVAIL | ETOOMANYCONNECTIONS, _) => busy(),
        (EARGS, _) => QuarkError::InvalidResponse,
        (other, _) => QuarkError::Mega(MegaFailure::ApiError { code: other }),
    }
}

/// Error numbers worth repeating the very same command for.
fn retryable(code: i64) -> bool {
    matches!(code, EAGAIN | ERATELIMIT)
}

/// The single result of a command: the value, or the mapped error. A
/// non-negative number (`0`) is the success reply of commands that return no
/// data; `Err((error, retry))` tells whether the same command may be repeated.
fn command_value(response: &str, purpose: Purpose) -> Result<Value, (QuarkError, bool)> {
    let value: Value =
        serde_json::from_str(response).map_err(|_| (QuarkError::InvalidResponse, false))?;
    let result = match value {
        Value::Array(mut items) if items.len() == 1 => items.remove(0),
        other => other,
    };
    if let Some(code) = result.as_i64().filter(|code| *code < 0) {
        return Err((api_error(code, purpose, None), retryable(code)));
    }
    if let Some(code) = result
        .get("e")
        .and_then(Value::as_i64)
        .filter(|code| *code < 0)
    {
        let wait = result.get("tl").and_then(Value::as_u64);
        return Err((api_error(code, purpose, wait), retryable(code)));
    }
    match result {
        Value::Object(_) | Value::String(_) | Value::Number(_) => Ok(result),
        // A versioned attribute write answers with the attribute name and its
        // new version, which may arrive as a pair.
        Value::Array(_) if purpose == Purpose::Attribute => Ok(result),
        _ => Err((QuarkError::InvalidResponse, false)),
    }
}

impl<'a> AccountApi<'a> {
    pub fn anonymous(http: &'a dyn CloudHttpTransport) -> Self {
        Self::with_gateway(http, None, GATEWAY)
    }

    pub fn with_session(http: &'a dyn CloudHttpTransport, sid: &'a str) -> Self {
        Self::with_gateway(http, Some(sid), GATEWAY)
    }

    pub fn with_gateway(
        http: &'a dyn CloudHttpTransport,
        sid: Option<&'a str>,
        gateway: &'a str,
    ) -> Self {
        Self { http, sid, gateway }
    }

    fn attempt(&self, command: &Value, purpose: Purpose) -> Result<Value, (QuarkError, bool)> {
        let mut url = format!("{}?id={}", self.gateway, next_request_id());
        if let Some(sid) = self.sid {
            url.push_str("&sid=");
            url.push_str(&crate::cloud::types::encoded(sid));
        }
        let response = self
            .http
            .send(CloudHttpRequest {
                method: "POST",
                url,
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                body: Some(Value::Array(vec![command.clone()]).to_string()),
            })
            .map_err(|error| (error, false))?;
        if response.status != 200 {
            return Err((busy(), response.status >= 500));
        }
        command_value(&response.body, purpose)
    }

    /// Sends one command, repeating it while MEGA asks to try again. The
    /// reply may be an object, a string (a handle) or a number (`0`).
    pub fn call_value(&self, command: Value, purpose: Purpose) -> Result<Value, QuarkError> {
        let mut delays = RETRY_DELAYS.iter();
        loop {
            match self.attempt(&command, purpose) {
                Ok(value) => return Ok(value),
                Err((error, true)) => match delays.next() {
                    Some(delay) => std::thread::sleep(*delay),
                    None => return Err(error),
                },
                Err((error, false)) => return Err(error),
            }
        }
    }

    /// Like [`Self::call_value`] for commands that answer with an object.
    pub fn call(&self, command: Value, purpose: Purpose) -> Result<Value, QuarkError> {
        let value = self.call_value(command, purpose)?;
        if value.is_object() {
            Ok(value)
        } else {
            Err(QuarkError::InvalidResponse)
        }
    }

    /// Pre-login: which key derivation the account uses (and its salt).
    pub fn prelogin(&self, email: &str) -> Result<Value, QuarkError> {
        self.call(
            json!({"a": "us0", "user": email}),
            Purpose::Login { with_code: false },
        )
    }

    /// The login proof exchange.
    pub fn login(
        &self,
        email: &str,
        user_hash: &str,
        second_factor: Option<&str>,
    ) -> Result<Value, QuarkError> {
        let mut command = json!({"a": "us", "user": email, "uh": user_hash});
        if let Some(code) = second_factor {
            command["mfa"] = Value::String(code.to_owned());
        }
        self.call(
            command,
            Purpose::Login {
                with_code: second_factor.is_some(),
            },
        )
    }

    pub fn user_info(&self) -> Result<Value, QuarkError> {
        self.call(json!({"a": "ug"}), Purpose::Session)
    }

    /// Storage figures plus the plan level (`utype`), see `tier.rs`.
    pub fn quota(&self) -> Result<Value, QuarkError> {
        self.call(json!({"a": "uq", "strg": 1, "pro": 1}), Purpose::Session)
    }

    pub fn nodes(&self) -> Result<Value, QuarkError> {
        self.call(json!({"a": "f", "c": 1}), Purpose::Session)
    }

    /// Ends the session on the server (`sml`). Its reply carries no data.
    pub fn logout(&self) -> Result<(), QuarkError> {
        match self.call_value(json!({"a": "sml"}), Purpose::Session) {
            Ok(_) | Err(QuarkError::InvalidResponse) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn node_download(&self, handle: &str) -> Result<Value, QuarkError> {
        self.call(
            json!({"a": "g", "g": 1, "ssl": 2, "n": handle}),
            Purpose::NodeDownload,
        )
    }

    /// `p`: create nodes below `parent` (folders, or files whose bytes were
    /// uploaded and are identified by their completion token). The reply lists
    /// the created nodes under `f`.
    pub fn put_nodes(&self, parent: &str, nodes: Vec<Value>) -> Result<Value, QuarkError> {
        self.call(json!({"a": "p", "t": parent, "n": nodes}), Purpose::Write)
    }

    /// `a`: replace a node's sealed attributes.
    pub fn set_attributes(&self, handle: &str, sealed: &str) -> Result<(), QuarkError> {
        self.call_value(json!({"a": "a", "n": handle, "at": sealed}), Purpose::Write)
            .map(|_| ())
    }

    /// Move a node, carrying the subtree keys for destination shares in
    /// the same request so success never depends on a later key publication.
    pub fn move_node(
        &self,
        handle: &str,
        target: &str,
        share_keys: Option<Value>,
    ) -> Result<(), QuarkError> {
        let mut command = json!({"a": "m", "n": handle, "t": target});
        if let Some(cr) = share_keys {
            command["cr"] = cr;
        }
        self.call_value(command, Purpose::Write).map(|_| ())
    }

    /// `d`: delete a node and everything below it, permanently.
    pub fn delete_node(&self, handle: &str) -> Result<(), QuarkError> {
        self.call_value(json!({"a": "d", "n": handle}), Purpose::Write)
            .map(|_| ())
    }

    /// `u`: where to upload `size` bytes. The reply carries the upload URL `p`.
    pub fn upload_url(&self, size: u64) -> Result<Value, QuarkError> {
        self.call(json!({"a": "u", "s": size, "ssl": 2}), Purpose::Write)
    }

    /// `l`: export a node. The reply is the public handle of the link.
    pub fn export_link(&self, handle: &str) -> Result<String, QuarkError> {
        match self.call_value(json!({"a": "l", "n": handle}), Purpose::Write)? {
            Value::String(public_handle) => Ok(public_handle),
            _ => Err(QuarkError::InvalidResponse),
        }
    }

    /// `l` with `d`: withdraw a node's public link.
    pub fn remove_link(&self, handle: &str) -> Result<(), QuarkError> {
        self.call_value(json!({"a": "l", "n": handle, "d": 1}), Purpose::Write)
            .map(|_| ())
    }

    /// `uga`: one attribute of the account's own user. The reply carries the
    /// value (`av`, base64) and its version (`v`); `None` when it was never set.
    pub fn user_attribute(
        &self,
        user_handle: &str,
        name: &str,
    ) -> Result<Option<(Vec<u8>, String)>, QuarkError> {
        let command = json!({"a": "uga", "u": user_handle, "ua": name, "v": 1});
        let reply = match self.call(command, Purpose::Attribute) {
            Ok(reply) => reply,
            Err(QuarkError::SourceFileMissing) => return Ok(None),
            Err(error) => return Err(error),
        };
        let value = reply["av"]
            .as_str()
            .and_then(crate::cloud::mega::crypto::decode_base64url)
            .ok_or(QuarkError::InvalidResponse)?;
        let version = reply["v"]
            .as_str()
            .filter(|version| !version.is_empty())
            .ok_or(QuarkError::InvalidResponse)?;
        Ok(Some((value, version.to_owned())))
    }

    /// `upv`: replace an attribute if its stored version is still `version`
    /// (otherwise `-23`, reported as [`MegaFailure::KeysConflict`]).
    pub fn put_user_attribute(
        &self,
        name: &str,
        value: &[u8],
        version: &str,
    ) -> Result<(), QuarkError> {
        let encoded = crate::cloud::mega::crypto::encode_base64url(value);
        let mut command = json!({"a": "upv"});
        command[name] = json!([encoded, version]);
        self.call_value(command, Purpose::Attribute).map(|_| ())
    }

    /// `s2`: create (or update) the folder's exported-link share. `cr` holds
    /// the keys of the nodes below it, encrypted with the share key, when the
    /// share is new. With a key manager the share key is recorded in `^!keys`
    /// and the command carries all-zero placeholders for `ok` and `ha`.
    pub fn create_link_share(&self, handle: &str, cr: Option<Value>) -> Result<(), QuarkError> {
        let placeholder = crate::cloud::mega::crypto::encode_base64url(&[0u8; 16]);
        let mut command = json!({
            "a": "s2", "n": handle, "ok": placeholder, "ha": placeholder,
            "s": [{"u": "EXP", "r": 0}],
        });
        if let Some(cr) = cr {
            command["cr"] = cr;
        }
        let reply = self.call_value(command, Purpose::Write)?;
        // An `ok` in the answer means the server already holds a different
        // share key for this folder: do not link with ours.
        if reply.get("ok").is_some() {
            return Err(QuarkError::Mega(MegaFailure::KeysUnsupported));
        }
        Ok(())
    }

    /// `s2` removing the exported-link share of a folder.
    pub fn remove_link_share(&self, handle: &str) -> Result<(), QuarkError> {
        self.call_value(
            json!({"a": "s2", "n": handle, "s": [{"u": "EXP"}]}),
            Purpose::Write,
        )
        .map(|_| ())
    }

    /// `k`: supply node keys encrypted for an existing share (`cr`).
    pub fn send_share_keys(&self, key_requests: Value) -> Result<(), QuarkError> {
        self.call_value(json!({"a": "k", "cr": key_requests}), Purpose::Write)
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(body: &str, purpose: Purpose) -> QuarkError {
        command_value(body, purpose).unwrap_err().0
    }

    #[test]
    fn write_error_numbers_are_read_as_change_failures() {
        let write = Purpose::Write;
        assert!(matches!(
            failure("[-11]", write),
            QuarkError::Mega(MegaFailure::AccessDenied)
        ));
        assert!(matches!(
            failure("[-17]", write),
            QuarkError::Mega(MegaFailure::StorageFull)
        ));
        assert!(matches!(
            failure("[-10]", write),
            QuarkError::Mega(MegaFailure::InvalidMove)
        ));
        assert!(matches!(
            failure("[-9]", write),
            QuarkError::SourceFileMissing
        ));
        assert!(matches!(
            failure("[-15]", write),
            QuarkError::AuthorizationExpired
        ));
        assert!(matches!(
            failure("[-16]", write),
            QuarkError::Mega(MegaFailure::AccountBlocked)
        ));
        // The same number reads as a transfer quota when downloading.
        assert!(matches!(
            failure("[-17]", Purpose::NodeDownload),
            QuarkError::Mega(MegaFailure::QuotaExceeded { .. })
        ));
        assert!(matches!(
            failure("[-11]", Purpose::Session),
            QuarkError::Mega(MegaFailure::ApiError { code: -11 })
        ));
    }

    #[test]
    fn only_try_again_and_rate_limit_are_retryable_and_data_free_replies_succeed() {
        let (_, retry) = command_value("[-3]", Purpose::Write).unwrap_err();
        assert!(retry);
        let (_, retry) = command_value("[-4]", Purpose::Write).unwrap_err();
        assert!(retry);
        for number in [-2, -9, -11, -15, -17] {
            let (_, retry) = command_value(&format!("[{number}]"), Purpose::Write).unwrap_err();
            assert!(!retry, "{number}");
        }
        assert_eq!(command_value("[0]", Purpose::Write).unwrap(), 0);
        assert_eq!(
            command_value(r#"["HANDLE12"]"#, Purpose::Write).unwrap(),
            "HANDLE12"
        );
        assert!(command_value(r#"[{"f":[]}]"#, Purpose::Write)
            .unwrap()
            .is_object());
    }

    #[test]
    fn attribute_commands_read_a_missing_value_a_stale_version_and_a_pair_reply() {
        let attribute = Purpose::Attribute;
        assert!(matches!(
            failure("[-9]", attribute),
            QuarkError::SourceFileMissing
        ));
        let (error, retry) = command_value("[-23]", attribute).unwrap_err();
        assert!(matches!(error, QuarkError::Mega(MegaFailure::KeysConflict)));
        assert!(!retry, "a version clash is re-read, never re-sent");
        assert!(matches!(
            failure("[-15]", attribute),
            QuarkError::AuthorizationExpired
        ));
        // The write answers with the attribute name and its new version.
        assert!(command_value(r#"[["^!keys","ver2"]]"#, attribute)
            .unwrap()
            .is_array());
        assert!(command_value(r#"[{"^!keys":"ver2"}]"#, attribute)
            .unwrap()
            .is_object());
        // Other commands never accept an array.
        assert!(matches!(
            failure(r#"[["x","y"]]"#, Purpose::Write),
            QuarkError::InvalidResponse
        ));
        // `-23` means nothing special outside attribute commands.
        assert!(matches!(
            failure("[-23]", Purpose::Write),
            QuarkError::Mega(MegaFailure::ApiError { code: -23 })
        ));
    }

    #[test]
    fn login_error_numbers_are_read_as_account_failures() {
        let first = Purpose::Login { with_code: false };
        let second = Purpose::Login { with_code: true };
        assert!(matches!(
            failure("[-9]", first),
            QuarkError::Mega(MegaFailure::InvalidCredentials)
        ));
        assert!(matches!(
            failure("-26", first),
            QuarkError::Mega(MegaFailure::MfaRequired)
        ));
        assert!(matches!(
            failure("[-26]", second),
            QuarkError::Mega(MegaFailure::MfaInvalid)
        ));
        assert!(matches!(
            failure("[-5]", second),
            QuarkError::Mega(MegaFailure::MfaInvalid)
        ));
        assert!(matches!(
            failure("[-16]", first),
            QuarkError::Mega(MegaFailure::AccountBlocked)
        ));
    }

    #[test]
    fn session_error_numbers_are_read_as_session_failures() {
        assert!(matches!(
            failure("[-15]", Purpose::Session),
            QuarkError::AuthorizationExpired
        ));
        assert!(matches!(
            failure("[-9]", Purpose::NodeDownload),
            QuarkError::SourceFileMissing
        ));
        assert!(matches!(
            failure("[-3]", Purpose::Session),
            QuarkError::Mega(MegaFailure::ServiceBusy)
        ));
        assert!(matches!(
            failure(r#"[{"e":-17,"tl":600}]"#, Purpose::NodeDownload),
            QuarkError::Mega(MegaFailure::QuotaExceeded {
                retry_after_secs: Some(600)
            })
        ));
    }

    #[test]
    fn unknown_codes_and_malformed_bodies_never_echo_the_response() {
        assert_eq!(
            failure("[-99]", Purpose::Session).to_string(),
            "cloud.error.mega.apiError:-99"
        );
        assert_eq!(
            failure("[-3]", Purpose::Session).to_string(),
            "cloud.error.mega.serviceBusy"
        );
        for body in ["", "oops", "[]", "[1,2]", "null", "[true]"] {
            assert!(matches!(
                failure(body, Purpose::Session),
                QuarkError::InvalidResponse
            ));
        }
    }
}
