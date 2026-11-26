//! E-mail + password (+ optional second factor) login.
//!
//! 1. `us0` tells which key derivation the account uses (v1 legacy, v2
//!    PBKDF2 with a salt).
//! 2. `us` sends the derived proof `uh` (and the `mfa` code when given) and
//!    returns the master key encrypted with the password key, plus the
//!    session either as an RSA-encrypted `csid` or as a plain `tsid`.
//! 3. `ug` confirms the session and yields the user handle.
//!
//! The password is only used to derive keys; it is never stored or sent.

use super::api::AccountApi;
use super::kdf::{derive_v1, derive_v2, LoginSecrets};
use super::keys::encrypt_ecb;
use super::rsa_sid::decrypt_session_id;
use super::session::MegaSession;
use crate::cloud::mega::crypto::{decode_base64url, decrypt_ecb, encode_base64url};
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::{CloudHttpTransport, QuarkError};
use serde_json::Value;
use zeroize::Zeroizing;

const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_CODE_DIGITS: usize = 12;

pub struct LoginRequest<'a> {
    pub email: &'a str,
    pub password: &'a str,
    pub second_factor: Option<&'a str>,
}

fn invalid_credentials() -> QuarkError {
    QuarkError::Mega(MegaFailure::InvalidCredentials)
}

/// A pasted 2FA code is digits, possibly with spaces ("123 456").
fn normalized_code(code: &str) -> Result<Option<String>, QuarkError> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.is_empty() {
        return Ok(None);
    }
    if digits.len() > MAX_CODE_DIGITS || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(QuarkError::Mega(MegaFailure::MfaInvalid));
    }
    Ok(Some(digits))
}

fn derive_secrets(
    prelogin: &Value,
    email: &str,
    password: &str,
) -> Result<LoginSecrets, QuarkError> {
    // An account without `v` is a v1 account.
    match prelogin["v"].as_u64().unwrap_or(1) {
        1 => Ok(derive_v1(email, password)),
        2 => {
            let salt = prelogin["s"]
                .as_str()
                .and_then(decode_base64url)
                .filter(|salt| !salt.is_empty())
                .ok_or(QuarkError::InvalidResponse)?;
            Ok(derive_v2(password, &salt))
        }
        _ => Err(QuarkError::InvalidResponse),
    }
}

/// Master key from the response's `k`, decrypted with the password key.
fn master_key(response: &Value, password_key: &[u8; 16]) -> Result<Zeroizing<[u8; 16]>, QuarkError> {
    let encrypted = response["k"]
        .as_str()
        .and_then(decode_base64url)
        .ok_or(QuarkError::InvalidResponse)?;
    let plain = Zeroizing::new(decrypt_ecb(password_key, &encrypted).ok_or(QuarkError::InvalidResponse)?);
    let key: [u8; 16] = plain.as_slice().try_into().map_err(|_| QuarkError::InvalidResponse)?;
    Ok(Zeroizing::new(key))
}

/// `tsid` is 32 bytes whose second half is the first half encrypted with the
/// master key; it proves the master key without an RSA key pair.
fn verified_transfer_sid(master_key: &[u8; 16], tsid: &str) -> Option<String> {
    let raw = decode_base64url(tsid)?;
    if raw.len() < 32 {
        return None;
    }
    let (head, tail) = raw.split_at(16);
    let expected = encrypt_ecb(master_key, head)?;
    (expected.as_slice() == &tail[tail.len() - 16..]).then(|| encode_base64url(&raw))
}

fn session_id(response: &Value, master_key: &[u8; 16]) -> Result<String, QuarkError> {
    if let (Some(csid), Some(private_key)) = (response["csid"].as_str(), response["privk"].as_str())
    {
        // A key that does not decrypt the RSA blob means the password was
        // wrong even though the server accepted the proof.
        return decrypt_session_id(master_key, private_key, csid).ok_or_else(invalid_credentials);
    }
    response["tsid"]
        .as_str()
        .and_then(|tsid| verified_transfer_sid(master_key, tsid))
        .ok_or(QuarkError::InvalidResponse)
}

pub fn login(
    http: &dyn CloudHttpTransport,
    request: &LoginRequest<'_>,
) -> Result<MegaSession, QuarkError> {
    let email = request.email.trim().to_lowercase();
    if email.is_empty()
        || !email.contains('@')
        || email.chars().any(char::is_control)
        || request.password.is_empty()
        || request.password.len() > MAX_PASSWORD_BYTES
    {
        return Err(invalid_credentials());
    }
    let second_factor = normalized_code(request.second_factor.unwrap_or_default())?;

    let anonymous = AccountApi::anonymous(http);
    let prelogin = anonymous.prelogin(&email)?;
    let secrets = derive_secrets(&prelogin, &email, request.password)?;
    let response = anonymous.login(&email, &secrets.user_hash, second_factor.as_deref())?;
    let master = master_key(&response, &secrets.password_key)?;
    let sid = Zeroizing::new(session_id(&response, &master)?);

    let info = AccountApi::with_session(http, sid.as_str()).user_info()?;
    let handle = info["u"]
        .as_str()
        .or_else(|| response["u"].as_str())
        .filter(|handle| crate::cloud::mega::folder::valid_handle(handle))
        .ok_or(QuarkError::InvalidResponse)?;
    Ok(MegaSession::new(sid.to_string(), &master, handle.to_owned(), email))
}
