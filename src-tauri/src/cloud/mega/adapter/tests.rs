use super::*;
use crate::cloud::mega::test_vectors::*;
use crate::cloud::share::{provider_share_adapter, test_support::Http};
use crate::cloud::MemoryCredentialStore;
use crate::parser::CloudShareCandidate;

const FILE_ID: &str = "AbC12345";
const FOLDER_ID: &str = "ROOTHNDL";

fn candidate(url: String) -> CloudShareCandidate {
    CloudShareCandidate {
        provider: CloudProvider::Mega,
        url,
        access_code: None,
    }
}

fn file_candidate() -> CloudShareCandidate {
    candidate(format!("https://mega.nz/file/{FILE_ID}#{FILE_KEY_B64}"))
}

fn folder_candidate(key: &str) -> CloudShareCandidate {
    candidate(format!("https://mega.nz/folder/{FOLDER_ID}#{key}"))
}

fn inspect(http: &Http, candidate: &CloudShareCandidate) -> Result<CloudSharePlan, QuarkError> {
    let credentials = MemoryCredentialStore::default();
    let adapter = provider_share_adapter(CloudProvider::Mega, &credentials, http).unwrap();
    match adapter.inspect(candidate)? {
        ShareInspection::Plan(plan) => Ok(plan),
        ShareInspection::AccessCodeRequired => panic!("MEGA never asks for an access code"),
    }
}

fn resolve(
    http: &Http,
    plan: &CloudSharePlan,
    entry: &CloudShareEntry,
) -> Result<ShareResolution, QuarkError> {
    let credentials = MemoryCredentialStore::default();
    let adapter = provider_share_adapter(CloudProvider::Mega, &credentials, http).unwrap();
    adapter.resolve(&CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        plan: plan.clone(),
        entry: entry.clone(),
    })
}

fn file_listing_reply() -> Value {
    json!([{"s": PLAIN_LEN, "at": FILE_ATTR_B64}])
}

fn folder_listing_reply() -> Value {
    let listing: Value = serde_json::from_str(FOLDER_LISTING).unwrap();
    json!([listing])
}

fn entry_named<'a>(plan: &'a CloudSharePlan, name: &str) -> &'a CloudShareEntry {
    plan.entries
        .iter()
        .find(|entry| entry.name == name)
        .unwrap()
}

fn expect_failure(result: Result<impl Sized, QuarkError>, expected: MegaFailure) {
    match result {
        Err(QuarkError::Mega(failure)) => assert_eq!(failure, expected),
        Err(other) => panic!("unexpected error {other}"),
        Ok(_) => panic!("expected {expected:?}"),
    }
}

fn direct(resolution: ShareResolution) -> ResolvedCloudDownload {
    match resolution {
        ShareResolution::Direct(download) => download,
        ShareResolution::TransferRequired(_) => panic!("MEGA never transfers"),
    }
}

#[test]
fn single_file_inspection_decrypts_the_name_and_hides_the_key_from_the_network() {
    let http = Http::new(vec![file_listing_reply()]);
    let plan = inspect(&http, &file_candidate()).unwrap();
    assert_eq!(plan.entries.len(), 1);
    assert_eq!(plan.entries[0].name, FILE_NAME);
    assert_eq!(plan.entries[0].size, Some(PLAIN_LEN as u64));
    assert!(plan.entries[0].relative_directory.is_empty());
    assert!(matches!(plan.root_kind, CloudShareRootKind::File { .. }));
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .url
        .starts_with("https://g.api.mega.co.nz/cs?id="));
    assert_eq!(
        requests[0].body.as_deref(),
        Some(&*format!(r#"[{{"a":"g","p":"{FILE_ID}"}}]"#))
    );
    for text in [
        requests[0].url.as_str(),
        requests[0].body.as_deref().unwrap(),
    ] {
        assert!(!text.contains(FILE_KEY_B64));
    }
}

#[test]
fn single_file_resolution_returns_a_ciphertext_url_with_the_decrypting_transform() {
    let http = Http::new(vec![
        file_listing_reply(),
        json!([{"g": "https://gfs270.userstorage.mega.co.nz/dl/TOKEN", "s": PLAIN_LEN}]),
    ]);
    let plan = inspect(&http, &file_candidate()).unwrap();
    let download = direct(resolve(&http, &plan, &plan.entries[0].clone()).unwrap());
    assert_eq!(
        download.url,
        "https://gfs270.userstorage.mega.co.nz/dl/TOKEN"
    );
    assert_eq!(download.name, FILE_NAME);
    assert!(download.headers.is_empty());
    match download.transform.unwrap() {
        DownloadTransform::AesCtrPathRange {
            key,
            nonce,
            total_size,
            ..
        } => {
            assert_eq!(*key, FILE_AES_KEY);
            assert_eq!(nonce, FILE_NONCE);
            assert_eq!(total_size, PLAIN_LEN as u64);
        }
    }
    let requests = http.requests.lock().unwrap();
    let body = requests[1].body.as_deref().unwrap();
    assert!(body.contains(r#""g":1"#) && body.contains(&format!(r#""p":"{FILE_ID}""#)));
    // `ssl: 2` is what makes the API return an https storage URL.
    assert!(body.contains(r#""ssl":2"#));
    assert!(!body.contains(FILE_KEY_B64));
}

#[test]
fn a_cleartext_download_url_is_rejected() {
    let http = Http::new(vec![
        file_listing_reply(),
        json!([{"g": "http://gfs270.userstorage.mega.co.nz/dl/TOKEN", "s": PLAIN_LEN}]),
    ]);
    let plan = inspect(&http, &file_candidate()).unwrap();
    assert!(matches!(
        resolve(&http, &plan, &plan.entries[0].clone()),
        Err(QuarkError::InvalidResponse)
    ));
}

#[test]
fn a_wrong_file_key_is_reported_as_invalid() {
    let http = Http::new(vec![file_listing_reply()]);
    let mut wrong = FILE_KEY;
    wrong[3] ^= 0x40;
    let url = format!(
        "https://mega.nz/file/{FILE_ID}#{}",
        encode_base64url(&wrong)
    );
    expect_failure(inspect(&http, &candidate(url)), MegaFailure::KeyInvalid);
}

#[test]
fn a_link_without_a_key_fails_locally_without_any_request() {
    for url in [
        format!("https://mega.nz/file/{FILE_ID}"),
        format!("https://mega.nz/file/{FILE_ID}#"),
        format!("https://mega.nz/folder/{FOLDER_ID}#short"),
    ] {
        let http = Http::new(vec![]);
        expect_failure(inspect(&http, &candidate(url)), MegaFailure::LinkIncomplete);
        assert!(http.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn quota_exhaustion_maps_to_the_dedicated_error_while_inspecting_and_resolving() {
    let http = Http::new(vec![json!([-17])]);
    expect_failure(
        inspect(&http, &file_candidate()),
        MegaFailure::QuotaExceeded {
            retry_after_secs: None,
        },
    );
    let http = Http::new(vec![file_listing_reply(), json!([{"e": -17, "tl": 1800}])]);
    let plan = inspect(&http, &file_candidate()).unwrap();
    expect_failure(
        resolve(&http, &plan, &plan.entries[0].clone()),
        MegaFailure::QuotaExceeded {
            retry_after_secs: Some(1800),
        },
    );
}

#[test]
fn a_removed_link_is_reported_as_unavailable() {
    let http = Http::new(vec![json!([-9])]);
    expect_failure(
        inspect(&http, &file_candidate()),
        MegaFailure::LinkUnavailable,
    );
}

#[test]
fn a_transfer_size_that_differs_from_the_listing_is_rejected() {
    let http = Http::new(vec![
        file_listing_reply(),
        json!([{"g": "https://gfs.example/dl/T", "s": PLAIN_LEN + 1}]),
    ]);
    let plan = inspect(&http, &file_candidate()).unwrap();
    assert!(matches!(
        resolve(&http, &plan, &plan.entries[0].clone()),
        Err(QuarkError::InvalidResponse)
    ));
}

#[test]
fn folder_inspection_lists_nested_files_and_skips_nodes_of_other_shares() {
    let http = Http::new(vec![folder_listing_reply()]);
    let plan = inspect(&http, &folder_candidate(FOLDER_KEY_B64)).unwrap();
    assert!(matches!(plan.root_kind, CloudShareRootKind::Directory));
    let listed = plan
        .entries
        .iter()
        .map(|entry| {
            (
                entry.relative_directory.join("/"),
                entry.name.as_str(),
                entry.size,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        vec![
            ("".to_owned(), "a.bin", Some(10)),
            ("2024".to_owned(), "b.txt", Some(20)),
            ("2024/deep dir".to_owned(), "c.mp4", Some(4_194_305)),
        ]
    );
    let requests = http.requests.lock().unwrap();
    assert!(requests[0].url.ends_with(&format!("&n={FOLDER_ID}")));
    assert_eq!(
        requests[0].body.as_deref(),
        Some(r#"[{"a":"f","c":1,"r":1}]"#)
    );
    assert!(!requests[0].url.contains(FOLDER_KEY_B64));
}

/// Real folder links carry the share's public handle, which is not the handle
/// of the root folder node in the listing.
#[test]
fn folder_link_id_may_differ_from_the_root_node_handle() {
    let http = Http::new(vec![
        folder_listing_reply(),
        json!([{"g": "https://gfs.example/dl/NESTED", "s": 20}]),
    ]);
    let public_link = candidate(format!("https://mega.nz/folder/PUBLICHD#{FOLDER_KEY_B64}"));
    let plan = inspect(&http, &public_link).unwrap();
    let names = plan
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["a.bin", "b.txt", "c.mp4"]);
    let entry = entry_named(&plan, "b.txt").clone();
    direct(resolve(&http, &plan, &entry).unwrap());
    let requests = http.requests.lock().unwrap();
    assert!(requests[0].url.ends_with("&n=PUBLICHD"));
    assert!(requests[1].url.ends_with("&n=PUBLICHD"));
}

#[test]
fn a_wrong_folder_key_is_reported_as_invalid() {
    let http = Http::new(vec![folder_listing_reply()]);
    let mut wrong = FOLDER_KEY;
    wrong[0] ^= 0x01;
    expect_failure(
        inspect(&http, &folder_candidate(&encode_base64url(&wrong))),
        MegaFailure::KeyInvalid,
    );
}

#[test]
fn folder_quota_exhaustion_is_reported_while_listing() {
    let http = Http::new(vec![json!([-17])]);
    expect_failure(
        inspect(&http, &folder_candidate(FOLDER_KEY_B64)),
        MegaFailure::QuotaExceeded {
            retry_after_secs: None,
        },
    );
}

#[test]
fn folder_file_resolution_uses_the_node_key_and_the_folder_scope() {
    let http = Http::new(vec![
        folder_listing_reply(),
        json!([{"g": "https://gfs.example/dl/NESTED", "s": 20}]),
    ]);
    let plan = inspect(&http, &folder_candidate(FOLDER_KEY_B64)).unwrap();
    let entry = entry_named(&plan, "b.txt").clone();
    let download = direct(resolve(&http, &plan, &entry).unwrap());
    let expected = FileKey::from_bytes(&NODE_KEY_FILEBBBB).unwrap();
    match download.transform.unwrap() {
        DownloadTransform::AesCtrPathRange {
            key,
            nonce,
            total_size,
            ..
        } => {
            assert_eq!(*key, *expected.aes_key);
            assert_eq!(nonce, expected.nonce);
            assert_eq!(total_size, 20);
        }
    }
    let requests = http.requests.lock().unwrap();
    assert!(requests[1].url.ends_with(&format!("&n={FOLDER_ID}")));
    let body = requests[1].body.as_deref().unwrap();
    assert!(body.contains(r#""n":"FILEBBBB""#) && body.contains(r#""g":1"#));
}

#[test]
fn plan_debug_output_never_contains_the_link_key() {
    let http = Http::new(vec![file_listing_reply()]);
    let plan = inspect(&http, &file_candidate()).unwrap();
    assert!(!format!("{plan:?}").contains(FILE_KEY_B64));
    assert!(!format!("{:?}", file_candidate()).contains(FILE_KEY_B64));
}

// ---- Downloading with the signed-in account's quota ----

use crate::cloud::mega::account::MegaSession;
use crate::cloud::share::{provider_share_adapter_with, ShareAdapterOptions};

/// A store holding a session whose id is unique to `name`: the plan level is
/// cached per session, so tests must not share one.
fn credentials_with_session(name: &str) -> (MemoryCredentialStore, String) {
    let sid = format!("SESSION-{name}-SECRET");
    let credentials = MemoryCredentialStore::default();
    MegaSession::new(sid.clone(), &[7; 16], "USERHNDL".into(), "a@b.c".into())
        .save(&credentials)
        .unwrap();
    (credentials, sid)
}

fn paid() -> Value {
    json!([{"utype": 4, "mxfer": 1_000_000_000u64}])
}

fn free() -> Value {
    json!([{"utype": 0, "tah": [1, 2, 3]}])
}

fn resolve_with(
    credentials: &MemoryCredentialStore,
    http: &Http,
    account_quota: bool,
    plan: &CloudSharePlan,
    entry: &CloudShareEntry,
) -> Result<ShareResolution, QuarkError> {
    let adapter = provider_share_adapter_with(
        CloudProvider::Mega,
        credentials,
        http,
        ShareAdapterOptions {
            mega_account_quota: account_quota,
        },
    )
    .unwrap();
    adapter.resolve(&CloudShareResolvedEntry {
        provider: CloudProvider::Mega,
        plan: plan.clone(),
        entry: entry.clone(),
    })
}

fn uses_account_quota(download: &ResolvedCloudDownload) -> bool {
    download.transform.as_ref().unwrap().uses_account_quota()
}

fn file_plan() -> CloudSharePlan {
    inspect(&Http::new(vec![file_listing_reply()]), &file_candidate()).unwrap()
}

fn folder_plan() -> CloudSharePlan {
    inspect(
        &Http::new(vec![folder_listing_reply()]),
        &folder_candidate(FOLDER_KEY_B64),
    )
    .unwrap()
}

fn granted(url: &str) -> Value {
    json!([{"g": url, "s": PLAIN_LEN}])
}

fn commands(http: &Http) -> Vec<String> {
    http.requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| {
            let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
            body[0]["a"].as_str().unwrap().to_owned()
        })
        .collect()
}

#[test]
fn a_paid_account_requests_a_public_file_with_the_session() {
    let (credentials, sid) = credentials_with_session("file");
    let plan = file_plan();
    let http = Http::new(vec![paid(), granted("https://gfs.example/dl/T")]);
    let download =
        direct(resolve_with(&credentials, &http, true, &plan, &plan.entries[0]).unwrap());
    assert!(uses_account_quota(&download));
    assert_eq!(commands(&http), ["uq", "g"]);
    let requests = http.requests.lock().unwrap();
    // The plan level is asked with the session, never with the link.
    assert!(requests[0].url.ends_with(&format!("&sid={sid}")));
    assert!(requests[0].body.as_deref().unwrap().contains(r#""pro":1"#));
    assert!(requests[1].url.ends_with(&format!("&sid={sid}")));
    assert!(!requests[1].url.contains("&n="));
    let body = requests[1].body.as_deref().unwrap();
    assert!(body.contains(&format!(r#""p":"{FILE_ID}""#)) && body.contains(r#""g":1"#));
    // The session never travels in the body, and the link key never in the URL.
    assert!(!body.contains(&sid));
    for request in requests.iter() {
        assert!(!request.url.contains(FILE_KEY_B64));
    }
}

#[test]
fn a_paid_account_carries_the_folder_scope_and_the_session_together() {
    let (credentials, sid) = credentials_with_session("folder");
    let plan = folder_plan();
    let entry = entry_named(&plan, "b.txt").clone();
    let http = Http::new(vec![
        paid(),
        json!([{"g": "https://gfs.example/dl/N", "s": 20}]),
    ]);
    let download = direct(resolve_with(&credentials, &http, true, &plan, &entry).unwrap());
    assert!(uses_account_quota(&download));
    let requests = http.requests.lock().unwrap();
    assert!(requests[1]
        .url
        .ends_with(&format!("&n={FOLDER_ID}&sid={sid}")));
    assert!(requests[1]
        .body
        .as_deref()
        .unwrap()
        .contains(r#""n":"FILEBBBB""#));
}

#[test]
fn a_free_account_stays_anonymous_and_asks_for_the_level_only_once() {
    let (credentials, sid) = credentials_with_session("free");
    let plan = file_plan();
    let http = Http::new(vec![
        free(),
        granted("https://gfs.example/dl/ONE"),
        granted("https://gfs.example/dl/TWO"),
    ]);
    for _ in 0..2 {
        let download =
            direct(resolve_with(&credentials, &http, true, &plan, &plan.entries[0]).unwrap());
        assert!(!uses_account_quota(&download));
    }
    // The level is cached for the session: one `uq`, then two plain `g`.
    assert_eq!(commands(&http), ["uq", "g", "g"]);
    let requests = http.requests.lock().unwrap();
    assert!(requests[0].url.contains(&format!("sid={sid}")));
    assert!(!requests[1].url.contains("sid=") && !requests[2].url.contains("sid="));
}

#[test]
fn no_session_is_sent_when_the_setting_is_off_even_with_a_paid_account() {
    let (credentials, _) = credentials_with_session("off");
    let plan = file_plan();
    let http = Http::new(vec![granted("https://gfs.example/dl/T")]);
    let download =
        direct(resolve_with(&credentials, &http, false, &plan, &plan.entries[0]).unwrap());
    assert!(!uses_account_quota(&download));
    assert_eq!(commands(&http), ["g"]);
    assert!(!http.requests.lock().unwrap()[0].url.contains("sid="));
}

#[test]
fn without_a_stored_session_the_request_stays_anonymous() {
    let credentials = MemoryCredentialStore::default();
    let plan = file_plan();
    let http = Http::new(vec![granted("https://gfs.example/dl/T")]);
    let download =
        direct(resolve_with(&credentials, &http, true, &plan, &plan.entries[0]).unwrap());
    assert!(!uses_account_quota(&download));
    assert_eq!(commands(&http), ["g"]);
}

#[test]
fn a_session_the_level_check_rejects_falls_back_to_anonymous() {
    let (credentials, _) = credentials_with_session("expired");
    let plan = file_plan();
    let http = Http::new(vec![json!([-15]), granted("https://gfs.example/dl/ANON")]);
    let download =
        direct(resolve_with(&credentials, &http, true, &plan, &plan.entries[0]).unwrap());
    assert_eq!(download.url, "https://gfs.example/dl/ANON");
    assert!(!uses_account_quota(&download));
    assert!(!http.requests.lock().unwrap()[1].url.contains("sid="));
}

#[test]
fn a_refused_session_falls_back_to_an_anonymous_request() {
    for (index, refusal) in [json!([-15]), json!([-16]), json!([-9])]
        .into_iter()
        .enumerate()
    {
        let (credentials, sid) = credentials_with_session(&format!("refused{index}"));
        let plan = file_plan();
        let http = Http::new(vec![
            paid(),
            refusal,
            granted("https://gfs.example/dl/ANON"),
        ]);
        let download =
            direct(resolve_with(&credentials, &http, true, &plan, &plan.entries[0]).unwrap());
        assert_eq!(download.url, "https://gfs.example/dl/ANON");
        assert!(!uses_account_quota(&download));
        assert_eq!(commands(&http), ["uq", "g", "g"]);
        let requests = http.requests.lock().unwrap();
        assert!(requests[1].url.contains(&format!("&sid={sid}")));
        assert!(!requests[2].url.contains("sid="));
    }
}

#[test]
fn a_link_that_is_really_gone_still_fails_after_the_fallback() {
    let (credentials, _) = credentials_with_session("gone");
    let plan = file_plan();
    let http = Http::new(vec![paid(), json!([-9]), json!([-9])]);
    expect_failure(
        resolve_with(&credentials, &http, true, &plan, &plan.entries[0]),
        MegaFailure::LinkUnavailable,
    );
}

#[test]
fn an_exhausted_account_quota_is_reported_as_the_accounts_without_falling_back() {
    let plan = file_plan();
    for (index, (reply, wait)) in [
        (json!([-17]), None),
        (json!([{"e": -17, "tl": 1800}]), Some(1800)),
    ]
    .into_iter()
    .enumerate()
    {
        let (credentials, _) = credentials_with_session(&format!("quota{index}"));
        let http = Http::new(vec![paid(), reply]);
        expect_failure(
            resolve_with(&credentials, &http, true, &plan, &plan.entries[0]),
            MegaFailure::AccountQuotaExceeded {
                retry_after_secs: wait,
            },
        );
        assert_eq!(http.requests.lock().unwrap().len(), 2);
    }
    // Anonymously the same answer keeps the IP-quota wording.
    let (credentials, _) = credentials_with_session("quota-anonymous");
    let http = Http::new(vec![json!([-17])]);
    expect_failure(
        resolve_with(&credentials, &http, false, &plan, &plan.entries[0]),
        MegaFailure::QuotaExceeded {
            retry_after_secs: None,
        },
    );
}

#[test]
fn busy_account_downloads_do_not_fall_back_to_anonymous() {
    let plan = file_plan();
    for code in [-3, -4, -18] {
        let (credentials, sid) = credentials_with_session(&format!("busy{code}"));
        let http = Http::new(vec![paid(), json!([code])]);
        expect_failure(
            resolve_with(&credentials, &http, true, &plan, &plan.entries[0]),
            MegaFailure::ServiceBusy,
        );
        assert_eq!(commands(&http), ["uq", "g"]);
        assert!(http.requests.lock().unwrap()[1]
            .url
            .contains(&format!("&sid={sid}")));
    }
}

#[test]
fn every_resolution_of_a_task_source_requests_the_session_again() {
    let (credentials, sid) = credentials_with_session("again");
    let plan = file_plan();
    let http = Http::new(vec![
        paid(),
        granted("https://gfs.example/dl/FIRST"),
        granted("https://gfs.example/dl/SECOND"),
    ]);
    for expected in ["FIRST", "SECOND"] {
        let download =
            direct(resolve_with(&credentials, &http, true, &plan, &plan.entries[0]).unwrap());
        assert!(download.url.ends_with(expected));
        assert!(uses_account_quota(&download));
    }
    assert_eq!(commands(&http), ["uq", "g", "g"]);
    for request in http.requests.lock().unwrap().iter().skip(1) {
        assert!(request.url.ends_with(&format!("&sid={sid}")));
    }
}

#[test]
fn inspecting_never_sends_the_session() {
    let (credentials, _) = credentials_with_session("inspect");
    for (replies, candidate) in [
        (vec![file_listing_reply()], file_candidate()),
        (
            vec![folder_listing_reply()],
            folder_candidate(FOLDER_KEY_B64),
        ),
    ] {
        let http = Http::new(replies);
        let adapter = provider_share_adapter_with(
            CloudProvider::Mega,
            &credentials,
            &http,
            ShareAdapterOptions {
                mega_account_quota: true,
            },
        )
        .unwrap();
        adapter.inspect(&candidate).unwrap();
        assert!(!http.requests.lock().unwrap()[0].url.contains("sid="));
    }
}

#[test]
fn the_plan_context_never_holds_the_session() {
    let plan = file_plan();
    assert!(!plan.provider_context.to_string().contains("SESSION-"));
}
