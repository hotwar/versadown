//! Account flows against an in-process fake of MEGA's command endpoint. The
//! cryptographic inputs come from the independently generated vectors in
//! `test_vectors.rs`; the fake only decides which command gets which reply.

use super::client::{ListingCache, MegaAccountClient};
use super::login::{login, LoginRequest};
use super::session::MegaSession;
use super::test_vectors::*;
use crate::cloud::mega::failure::MegaFailure;
use crate::cloud::{
    CloudFileKind, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudStorageCapacity,
    MemoryCredentialStore, QuarkError,
};
use serde_json::{json, Value};
use std::sync::Mutex;

mod copy;
mod import;
mod key_flow;
mod keys;
mod manage;
mod upload;

/// The user-facing text of an error: a failure key for MEGA's own failures.
fn error_text(error: QuarkError) -> String {
    match error {
        QuarkError::Mega(failure) => failure.message(),
        other => other.to_string(),
    }
}

const EMAIL: &str = "alice.example@example.com";

struct Recorded {
    url: String,
    command: Value,
}

type Handler = Box<dyn Fn(&str, &Value) -> (u16, String) + Send + Sync>;

struct FakeMega {
    handler: Handler,
    requests: Mutex<Vec<Recorded>>,
}

impl FakeMega {
    fn new(handler: impl Fn(&str, &Value) -> (u16, String) + Send + Sync + 'static) -> Self {
        Self {
            handler: Box::new(handler),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self, name: &str) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|recorded| recorded.command["a"] == name)
            .map(|recorded| Recorded {
                url: recorded.url.clone(),
                command: recorded.command.clone(),
            })
            .collect()
    }

    fn all_traffic(&self) -> String {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|recorded| format!("{} {}", recorded.url, recorded.command))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl CloudHttpTransport for FakeMega {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        assert_eq!(request.method, "POST");
        assert!(request.url.starts_with("https://g.api.mega.co.nz/cs?id="));
        let body: Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        let command = body[0].clone();
        let (status, reply) = (self.handler)(&request.url, &command);
        self.requests.lock().unwrap().push(Recorded {
            url: request.url,
            command,
        });
        Ok(CloudHttpResponse {
            status,
            body: reply,
            set_cookies: Vec::new(),
        })
    }
}

fn ok(value: Value) -> (u16, String) {
    (200, json!([value]).to_string())
}

fn code(number: i64) -> (u16, String) {
    (200, json!([number]).to_string())
}

#[derive(Clone)]
struct Account {
    /// 1 or 2.
    version: u8,
    expected_proof: &'static str,
    wrapped_master_key: &'static str,
    required_code: Option<&'static str>,
    use_tsid: bool,
}

fn v2_account() -> Account {
    Account {
        version: 2,
        expected_proof: V2_USER_HASH,
        wrapped_master_key: V2_MASTER_KEY_B64,
        required_code: None,
        use_tsid: false,
    }
}

/// A server for one account; commands outside the login flow are answered by
/// `after_login`, which only runs for requests carrying the right session.
fn server(
    account: Account,
    after_login: impl Fn(&Value) -> (u16, String) + Send + Sync + 'static,
) -> FakeMega {
    FakeMega::new(move |url, command| match command["a"].as_str().unwrap_or_default() {
        "us0" if command["user"] == EMAIL => {
            if account.version == 2 {
                ok(json!({"v": 2, "s": V2_SALT_B64}))
            } else {
                ok(json!({"v": 1}))
            }
        }
        "us" => {
            if command["user"] != EMAIL || command["uh"] != account.expected_proof {
                return code(-9);
            }
            if let Some(required) = account.required_code {
                if command["mfa"] != required {
                    return code(-26);
                }
            }
            let mut reply = json!({"k": account.wrapped_master_key, "u": USER_HANDLE});
            if account.use_tsid {
                reply["tsid"] = json!(TSID_B64);
            } else {
                reply["csid"] = json!(CSID_B64);
                reply["privk"] = json!(PRIVK_B64);
            }
            ok(reply)
        }
        _ if !url.contains(&format!("sid={}", expected_sid(&account))) => code(-15),
        "ug" => ok(json!({"u": USER_HANDLE, "email": EMAIL})),
        _ => after_login(command),
    })
}

fn expected_sid(account: &Account) -> &'static str {
    if account.use_tsid {
        TSID_B64
    } else {
        EXPECTED_SID
    }
}

fn request<'a>(email: &'a str, password: &'a str, code: Option<&'a str>) -> LoginRequest<'a> {
    LoginRequest {
        email,
        password,
        second_factor: code,
    }
}

fn mega_failure(result: Result<MegaSession, QuarkError>) -> MegaFailure {
    match result {
        Err(QuarkError::Mega(failure)) => failure,
        Err(other) => panic!("unexpected error {other}"),
        Ok(_) => panic!("login unexpectedly succeeded"),
    }
}

// ---------------------------------------------------------------- login ---

#[test]
fn v2_login_derives_the_session_and_never_sends_the_password() {
    let http = server(v2_account(), |_| code(-2));
    let session = login(&http, &request(" Alice.Example@Example.com ", V2_PASSWORD, None)).unwrap();
    assert_eq!(session.sid(), EXPECTED_SID);
    assert_eq!(*session.master_key().unwrap(), MASTER_KEY);
    assert_eq!(session.user_handle(), USER_HANDLE);
    assert_eq!(session.email(), EMAIL);
    let traffic = http.all_traffic();
    assert!(!traffic.contains(V2_PASSWORD));
    // Only the account call carries the session.
    assert_eq!(http.commands("ug").len(), 1);
    assert!(http.commands("ug")[0].url.contains("&sid="));
    assert!(!http.commands("us")[0].url.contains("sid="));
    assert!(http.commands("us")[0].command.get("mfa").is_none());
}

#[test]
fn v1_login_uses_the_legacy_derivation() {
    for (index, (email, password, _, proof, wrapped)) in V1_CASES.iter().enumerate().take(2) {
        let lowered = email.to_lowercase();
        let (proof, wrapped, handler_email) = (*proof, *wrapped, lowered.clone());
        let http = FakeMega::new(move |url, command| match command["a"].as_str().unwrap() {
            "us0" => ok(json!({"v": 1})),
            "us" if command["user"] == handler_email.as_str() && command["uh"] == proof => ok(
                json!({"k": wrapped, "u": USER_HANDLE, "csid": CSID_B64, "privk": PRIVK_B64}),
            ),
            "us" => code(-9),
            "ug" if url.contains(&format!("sid={EXPECTED_SID}")) => {
                ok(json!({"u": USER_HANDLE, "email": handler_email}))
            }
            _ => code(-15),
        });
        let session = login(&http, &request(email, password, None)).unwrap();
        assert_eq!(session.sid(), EXPECTED_SID, "case {index}");
        assert_eq!(session.email(), lowered);
        assert!(!http.all_traffic().contains(password));
    }
}

#[test]
fn a_wrong_password_is_reported_as_invalid_credentials() {
    let http = server(v2_account(), |_| code(-2));
    assert_eq!(
        mega_failure(login(&http, &request(EMAIL, "not the password", None))),
        MegaFailure::InvalidCredentials
    );
    // Nothing but the two login steps happened.
    assert!(http.commands("ug").is_empty());
}

#[test]
fn blank_or_malformed_input_is_rejected_without_a_request() {
    let http = server(v2_account(), |_| code(-2));
    for (email, password) in [("", "x"), ("no-at-sign", "x"), (EMAIL, ""), ("a\n@b.c", "x")] {
        assert_eq!(
            mega_failure(login(&http, &request(email, password, None))),
            MegaFailure::InvalidCredentials
        );
    }
    assert!(http.all_traffic().is_empty());
}

#[test]
fn two_factor_account_asks_for_a_code_then_accepts_it() {
    let account = Account {
        required_code: Some("123456"),
        ..v2_account()
    };
    let http = server(account, |_| code(-2));
    assert_eq!(
        mega_failure(login(&http, &request(EMAIL, V2_PASSWORD, None))),
        MegaFailure::MfaRequired
    );
    // A blank code counts as none.
    assert_eq!(
        mega_failure(login(&http, &request(EMAIL, V2_PASSWORD, Some("  ")))),
        MegaFailure::MfaRequired
    );
    assert_eq!(
        mega_failure(login(&http, &request(EMAIL, V2_PASSWORD, Some("000000")))),
        MegaFailure::MfaInvalid
    );
    // Not a code at all: refused locally.
    let before = http.commands("us").len();
    assert_eq!(
        mega_failure(login(&http, &request(EMAIL, V2_PASSWORD, Some("12ab56")))),
        MegaFailure::MfaInvalid
    );
    assert_eq!(http.commands("us").len(), before);
    // Spaces from a pasted "123 456" are dropped.
    let session = login(&http, &request(EMAIL, V2_PASSWORD, Some("123 456"))).unwrap();
    assert_eq!(session.sid(), EXPECTED_SID);
    let sent = http.commands("us");
    assert_eq!(sent.last().unwrap().command["mfa"], "123456");
}

#[test]
fn account_level_errors_map_to_their_own_failures() {
    let blocked = FakeMega::new(|_, command| match command["a"].as_str().unwrap() {
        "us0" => ok(json!({"v": 2, "s": V2_SALT_B64})),
        _ => code(-16),
    });
    assert_eq!(
        mega_failure(login(&blocked, &request(EMAIL, V2_PASSWORD, None))),
        MegaFailure::AccountBlocked
    );
    let busy = FakeMega::new(|_, _| code(-3));
    assert!(matches!(
        login(&busy, &request(EMAIL, V2_PASSWORD, None)),
        Err(QuarkError::Mega(MegaFailure::ServiceBusy))
    ));
    let down = FakeMega::new(|_, _| (503, String::new()));
    assert!(matches!(
        login(&down, &request(EMAIL, V2_PASSWORD, None)),
        Err(QuarkError::Mega(MegaFailure::ServiceBusy))
    ));
    let garbage = FakeMega::new(|_, _| (200, "<html>".into()));
    assert!(matches!(
        login(&garbage, &request(EMAIL, V2_PASSWORD, None)),
        Err(QuarkError::InvalidResponse)
    ));
}

#[test]
fn a_v2_prelogin_without_salt_or_an_unknown_version_is_invalid() {
    for prelogin in [json!({"v": 2}), json!({"v": 2, "s": ""}), json!({"v": 7})] {
        let http = FakeMega::new(move |_, _| ok(prelogin.clone()));
        assert!(matches!(
            login(&http, &request(EMAIL, V2_PASSWORD, None)),
            Err(QuarkError::InvalidResponse)
        ));
    }
}

#[test]
fn a_transfer_session_id_is_accepted_only_when_the_master_key_proves_it() {
    let account = Account {
        use_tsid: true,
        ..v2_account()
    };
    let http = server(account, |_| code(-2));
    let session = login(&http, &request(EMAIL, V2_PASSWORD, None)).unwrap();
    assert_eq!(session.sid(), TSID_B64);

    let forged = FakeMega::new(|_, command| match command["a"].as_str().unwrap() {
        "us0" => ok(json!({"v": 2, "s": V2_SALT_B64})),
        _ => ok(json!({"k": V2_MASTER_KEY_B64, "u": USER_HANDLE,
            "tsid": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"})),
    });
    assert!(matches!(
        login(&forged, &request(EMAIL, V2_PASSWORD, None)),
        Err(QuarkError::InvalidResponse)
    ));
}

// --------------------------------------------------------------- account ---

fn signed_in_store() -> MemoryCredentialStore {
    let store = MemoryCredentialStore::default();
    MegaSession::new(
        EXPECTED_SID.into(),
        &MASTER_KEY,
        USER_HANDLE.into(),
        EMAIL.into(),
    )
    .save(&store)
    .unwrap();
    store
}

fn drive_server() -> FakeMega {
    server(v2_account(), |command| match command["a"].as_str().unwrap() {
        "f" => ok(serde_json::from_str(LISTING).unwrap()),
        "uq" => ok(json!({"mstrg": 53687091200u64, "cstrg": 1048576})),
        "g" => match command["n"].as_str().unwrap() {
            "FILEBBBB" => ok(json!({"g": "https://gfs.example/dl/TOKEN", "s": DOWNLOAD_PLAIN_LEN})),
            "FILEAAAA" => ok(json!({"g": "https://gfs.example/dl/AAAA", "s": 999})),
            "FILECCCC" => ok(json!({"g": "http://insecure.example/dl", "s": 77})),
            "FILEDDDD" => code(-9),
            _ => ok(json!({"e": -17, "tl": 1800})),
        },
        _ => code(-2),
    })
}

fn names(directory: &crate::cloud::CloudDirectory) -> Vec<(&str, CloudFileKind)> {
    directory
        .items
        .iter()
        .map(|item| (item.name.as_str(), item.kind))
        .collect()
}

#[test]
fn identity_and_quota_come_from_the_account_endpoints() {
    let http = drive_server();
    let store = signed_in_store();
    let client = MegaAccountClient::new(&http, &store);
    let account = client.account().unwrap();
    assert_eq!(account.id, USER_HANDLE);
    assert_eq!(account.display_name, EMAIL);
    assert_eq!(
        client.storage_capacity().unwrap(),
        CloudStorageCapacity::Limited {
            total_bytes: 53_687_091_200,
            used_bytes: 1_048_576
        }
    );
}

#[test]
fn the_drive_root_lists_decryptable_direct_children_only() {
    let http = drive_server();
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    let root = client.list("0").unwrap();
    assert_eq!(root.id, "0");
    // Inbox, rubbish bin, the foreign-owner node and the trashed file are absent.
    assert_eq!(
        names(&root),
        vec![
            ("Documents é", CloudFileKind::Folder),
            ("Shared folder", CloudFileKind::Folder),
            ("movie.bin", CloudFileKind::File),
        ]
    );
    let movie = root.items.iter().find(|item| item.id == "FILEBBBB").unwrap();
    assert_eq!(movie.size, Some(DOWNLOAD_PLAIN_LEN as u64));
    assert_eq!(movie.parent_id, "0");
    assert_eq!(movie.modified_at.as_deref(), Some("1700000200"));
    assert!(root.items.iter().all(|item| item.id != "FILEZZZZ"));
}

#[test]
fn nested_directories_use_own_and_share_keys() {
    let http = drive_server();
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    let documents = client.list("DIRAAAAA").unwrap();
    assert_eq!(
        names(&documents),
        vec![
            ("Nested", CloudFileKind::Folder),
            ("notes 笔记.txt", CloudFileKind::File)
        ]
    );
    assert_eq!(documents.items[0].parent_id, "DIRAAAAA");
    // FILECCCC opens only with the share key; FILEDDDD lists both alternatives.
    let shared = client.list(SHARE_HANDLE).unwrap();
    assert_eq!(
        names(&shared),
        vec![
            ("dual.dat", CloudFileKind::File),
            ("shared.dat", CloudFileKind::File)
        ]
    );
    assert!(matches!(
        client.list("NOSUCHND"),
        Err(QuarkError::SourceFileMissing)
    ));
}

#[test]
fn walking_a_tree_fetches_the_node_listing_once_per_session() {
    let http = drive_server();
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    client.list("0").unwrap();
    client.list("DIRAAAAA").unwrap();
    client.list("DIRBBBBB").unwrap();
    assert_eq!(http.commands("f").len(), 1);

    // Another session never sees the cached copy.
    let other = MemoryCredentialStore::default();
    let different = MegaSession::new(
        "OTHER-SESSION-ID".into(),
        &MASTER_KEY,
        USER_HANDLE.into(),
        EMAIL.into(),
    );
    different.save(&other).unwrap();
    let rejecting = drive_server();
    assert!(matches!(
        MegaAccountClient::new(&rejecting, &other)
            .with_cache(&cache)
            .list("0"),
        Err(QuarkError::AuthorizationExpired)
    ));
    assert_eq!(rejecting.commands("f").len(), 1);
}

#[test]
fn an_expired_session_reports_authorization_expired() {
    let http = FakeMega::new(|_, _| code(-15));
    let store = signed_in_store();
    let client = MegaAccountClient::new(&http, &store);
    assert!(matches!(client.account(), Err(QuarkError::AuthorizationExpired)));
    assert!(matches!(
        client.list("0"),
        Err(QuarkError::AuthorizationExpired)
    ));
}

#[test]
fn nothing_works_without_a_stored_session() {
    let http = drive_server();
    let store = MemoryCredentialStore::default();
    let client = MegaAccountClient::new(&http, &store);
    assert!(matches!(client.account(), Err(QuarkError::NotAuthorized)));
    assert!(matches!(
        client.resolve_download("FILEBBBB"),
        Err(QuarkError::NotAuthorized)
    ));
    assert!(http.all_traffic().is_empty());
}

fn hex_decode(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

#[test]
fn download_resolves_a_url_and_a_transform_that_decrypts_the_ciphertext() {
    let http = drive_server();
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    let resolved = client.resolve_download("FILEBBBB").unwrap();
    assert_eq!(resolved.url, "https://gfs.example/dl/TOKEN");
    assert_eq!(resolved.name, "movie.bin");
    assert!(resolved.headers.is_empty());
    let transform = resolved.transform.unwrap();
    assert_eq!(transform.total_size(), DOWNLOAD_PLAIN_LEN as u64);

    let mut data = hex_decode(DOWNLOAD_CIPHERTEXT_HEX);
    transform.decryptor(0).apply(&mut data);
    let expected: Vec<u8> = (0..DOWNLOAD_PLAIN_LEN)
        .map(|index| ((index * 31 + 7) & 0xFF) as u8)
        .collect();
    assert_eq!(data, expected);

    let asked = http.commands("g");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].command["n"], "FILEBBBB");
    assert_eq!(asked[0].command["ssl"], 2);
    assert!(asked[0].url.contains(&format!("sid={EXPECTED_SID}")));
    // The file key never goes over the wire.
    let traffic = http.all_traffic();
    assert!(!traffic.contains(&crate::cloud::mega::crypto::encode_base64url(&DOWNLOAD_NODE_KEY)));
}

#[test]
fn download_rejects_inconsistent_or_unusable_replies() {
    let http = drive_server();
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    // Listing says 1234 bytes, the transfer says 999.
    assert!(matches!(
        client.resolve_download("FILEAAAA"),
        Err(QuarkError::InvalidResponse)
    ));
    // Plain HTTP URLs are refused.
    assert!(matches!(
        client.resolve_download("FILECCCC"),
        Err(QuarkError::InvalidResponse)
    ));
    // The file vanished between listing and download.
    assert!(matches!(
        client.resolve_download("FILEDDDD"),
        Err(QuarkError::SourceFileMissing)
    ));
    // Folders, foreign nodes and unknown handles are not downloadable files.
    for handle in ["DIRAAAAA", "FILEZZZZ", "NOSUCHND"] {
        assert!(
            matches!(
                client.resolve_download(handle),
                Err(QuarkError::SourceFileMissing)
            ),
            "{handle}"
        );
    }
}

#[test]
fn download_quota_errors_keep_the_wait_time() {
    let http = server(v2_account(), |command| match command["a"].as_str().unwrap() {
        "f" => ok(serde_json::from_str(LISTING).unwrap()),
        _ => ok(json!({"e": -17, "tl": 1800})),
    });
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    assert!(matches!(
        client.resolve_download("FILEBBBB"),
        Err(QuarkError::Mega(MegaFailure::QuotaExceeded {
            retry_after_secs: Some(1800)
        }))
    ));
}

#[test]
fn only_authenticated_share_keys_are_trusted() {
    use super::keys::Keyring;
    let listing: Value = serde_json::from_str(LISTING).unwrap();
    let keyring = Keyring::new(USER_HANDLE, &MASTER_KEY).with_listing_shares(&listing);
    let shared_node = listing["f"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["h"] == "FILECCCC")
        .unwrap();
    assert!(keyring
        .node_key(shared_node["k"].as_str().unwrap(), 32)
        .is_some());
    // A node wrapped for the forged share is not decryptable.
    let forged = format!("FORGEDHD:{}", "A".repeat(43));
    assert!(keyring.node_key(&forged, 32).is_none());
    // A listing with another master key authenticates nothing.
    let wrong = Keyring::new(USER_HANDLE, &[9; 16]).with_listing_shares(&listing);
    assert!(wrong
        .node_key(shared_node["k"].as_str().unwrap(), 32)
        .is_none());
}

#[test]
fn sign_out_ends_the_server_session_and_never_fails() {
    let http = FakeMega::new(|_, _| (200, "[0]".into()));
    let store = signed_in_store();
    super::revoke_session(&http, &store);
    let sent = http.commands("sml");
    assert_eq!(sent.len(), 1);
    assert!(sent[0].url.contains(&format!("sid={EXPECTED_SID}")));

    // Offline, expired or never signed in: nothing to report.
    let expired = FakeMega::new(|_, _| code(-15));
    super::revoke_session(&expired, &store);
    let empty = MemoryCredentialStore::default();
    super::revoke_session(&http, &empty);
    assert_eq!(http.commands("sml").len(), 1);
}

#[test]
fn the_system_credential_store_accepts_the_mega_session_and_maintenance_keys() {
    use crate::cloud::{credential_key_is_supported, CloudProvider};
    let key = CloudProvider::Mega.key();
    assert!(credential_key_is_supported(key));
    assert!(credential_key_is_supported(&format!(
        "cloud-auth-maintenance-{key}"
    )));
}
